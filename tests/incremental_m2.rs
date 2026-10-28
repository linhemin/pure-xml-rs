//! M2 PR-B 增量读取回归（计划 §1、§4），2026-10-21 由 Tester 编写。
//!
//! 等价性：同一份字节经过 `from_bytes`、`from_vec`、各种切块的 `from_reader`，事件序列和错误
//! （kind、limit、actual、line、column、offset）必须完全相同。例外只有一条（计划 §0.1 v7）：
//! - Io：一次性解析没有 IO；流式出 Io 时，之前的事件是完整序列的前缀，offset = 已成功读入的字节数。
//!
//! TotalInput 按位置判定（Lead 11:51 裁定，D31）：只有解析真正需要 offset = limit 的那个字节时才报，
//! position 指向这个字节，actual = limit + 1；位置更靠前的错误优先，from_bytes 也一样。所以它和其他
//! 错误一样要求事件序列和错误完全相同，不再是例外（v7 以前只比较最终错误）。
//!
//! position 口径（计划 §0.1 v5）：按字节计的 TokenLength、TotalInput、InternalSubsetLength 指向
//! 越界的那个字节本身；按字符计的 TextLength、AttrValueLength、NameLength 指向越界字符之后。
//!
//! 慢测试（256 MiB、1 GiB 生成式输入、线性复杂度）只在 release 下编译（`#[cfg(not(debug_assertions))]`），
//! debug 下跑缩小版。W3C 语料来自 `PUREXML_W3C_DIR`（默认 tests/w3c/cache/xmlconf），缺失时跳过，
//! 设 `PUREXML_W3C_STRICT=1` 则判失败。

mod common;

use common::{run_guarded, Guarded};
use purexml::{Error, ErrorKind, LimitKind, ParserConfig, Position, Reader};
use std::io::{self, BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

// ───────────────────────────── 结果快照 ─────────────────────────────

type End = Result<(), (ErrorKind, Position)>;

#[derive(Debug, Clone, PartialEq)]
struct Snap {
    events: Vec<String>,
    end: End,
}

/// 事件上限：防止解析器出错时无限产出事件把测试拖死。
const MAX_EVENTS: usize = 20_000_000;

macro_rules! snap {
    ($reader:expr) => {{
        let reader = &mut $reader;
        let mut events = Vec::new();
        let end = loop {
            match reader.read_event() {
                Ok(Some(e)) => {
                    events.push(format!("{e:?}"));
                    assert!(events.len() < MAX_EVENTS, "事件数失控");
                }
                Ok(None) => break Ok(()),
                Err(e) => break Err((e.kind.clone(), e.position)),
            }
        };
        Snap { events, end }
    }};
}

/// 只统计事件个数（大文档用，不保存事件文本）。
macro_rules! count {
    ($reader:expr) => {{
        let reader = &mut $reader;
        let mut n = 0usize;
        let end: End = loop {
            match reader.read_event() {
                Ok(Some(_)) => n += 1,
                Ok(None) => break Ok(()),
                Err(e) => break Err((e.kind.clone(), e.position)),
            }
        };
        (n, end)
    }};
}

fn bytes(d: &[u8], c: &ParserConfig) -> Snap {
    let mut r = Reader::from_bytes(d).with_config(c.clone());
    snap!(r)
}

fn vec_path(d: &[u8], c: &ParserConfig) -> Snap {
    let mut r = Reader::from_vec(d.to_vec()).with_config(c.clone());
    snap!(r)
}

// ───────────────────────────── 11 种测试读取器 ─────────────────────────────

/// 读取器的行为。所有读取器都直接实现 `BufRead`（fill_buf 每次返回不同长度），
/// 也可以再套一层 `BufReader::with_capacity`。
#[derive(Clone, Debug)]
enum Plan {
    /// ByteReader：每次 1 字节
    Byte,
    /// ScheduleReader：按序列循环给块长
    Schedule(Vec<usize>),
    /// CutReader(k)：在第 k 个字节处切成两块
    Cut(usize),
    /// ShortReader：每次只给请求的一部分（随机 1..=请求长度）
    Short(u64),
    /// ZeroThenData(k)：给出前 k 字节后返回 Ok(0)，之后（不合法地）继续给数据
    ZeroThenData(usize),
    /// InterruptReader：以 permille/1000 的概率返回 Interrupted
    Interrupt { permille: u64, seed: u64 },
    /// 前 n 次调用连续返回 Interrupted
    InterruptBurst(usize),
    /// WouldBlockReader(n)：第 n 次调用返回 WouldBlock，之后恢复
    WouldBlock(usize),
    /// FailAt { call: n }
    FailCall(usize, io::ErrorKind),
    /// FailAt { byte: x }：读到偏移 x 时一直返回错误
    FailByte(usize, io::ErrorKind),
    /// PanicReader(n)：第 n 次调用 panic
    Panic(usize),
    /// LyingReader：`read` 报告的字节数大于目标缓冲
    Lying,
}

struct Feed<'a> {
    data: &'a [u8],
    pos: usize,
    plan: Plan,
    rng: u64,
    sched: usize,
    chunk_end: usize,
    zero_done: bool,
    calls: Arc<AtomicUsize>,
    delivered: Arc<AtomicUsize>,
    /// 返回过 Ok(0) 之后又被调用的次数（应为 0）
    after_eof: Arc<AtomicUsize>,
    eof_seen: bool,
}

fn xorshift(s: &mut u64) -> u64 {
    *s ^= *s << 13;
    *s ^= *s >> 7;
    *s ^= *s << 17;
    *s
}

impl<'a> Feed<'a> {
    fn new(data: &'a [u8], plan: Plan) -> Self {
        let seed = match &plan {
            Plan::Short(s) => *s,
            Plan::Interrupt { seed, .. } => *seed,
            _ => 0x9E37_79B9_7F4A_7C15,
        };
        Self {
            data,
            pos: 0,
            plan,
            rng: seed | 1,
            sched: 0,
            chunk_end: 0,
            zero_done: false,
            calls: Arc::new(AtomicUsize::new(0)),
            delivered: Arc::new(AtomicUsize::new(0)),
            after_eof: Arc::new(AtomicUsize::new(0)),
            eof_seen: false,
        }
    }

    /// 本次调用允许给出的最多字节数；Err 表示本次调用返回错误。
    fn next_len(&mut self, want: usize) -> io::Result<usize> {
        let call = self.calls.fetch_add(1, Ordering::SeqCst) + 1;
        if self.eof_seen {
            self.after_eof.fetch_add(1, Ordering::SeqCst);
        }
        let n = self.next_len_inner(want, call)?;
        if n == 0 && want > 0 {
            self.eof_seen = true;
        }
        Ok(n)
    }

    fn next_len_inner(&mut self, want: usize, call: usize) -> io::Result<usize> {
        let rem = self.data.len() - self.pos;
        let n = match &self.plan {
            Plan::Byte => 1,
            Plan::Schedule(s) => {
                let n = s[self.sched % s.len()];
                self.sched += 1;
                n
            }
            Plan::Cut(k) => {
                if self.pos < *k {
                    k - self.pos
                } else {
                    rem
                }
            }
            Plan::Short(_) => (xorshift(&mut self.rng) as usize % want.max(1)) + 1,
            Plan::ZeroThenData(k) => {
                if self.pos >= *k && !self.zero_done {
                    self.zero_done = true;
                    return Ok(0);
                }
                if self.pos < *k {
                    k - self.pos
                } else {
                    rem
                }
            }
            Plan::Interrupt { permille, .. } => {
                if xorshift(&mut self.rng) % 1000 < *permille {
                    return Err(io::Error::new(io::ErrorKind::Interrupted, "interrupted"));
                }
                (xorshift(&mut self.rng) as usize % 97) + 1
            }
            Plan::InterruptBurst(n) => {
                if call <= *n {
                    return Err(io::Error::new(io::ErrorKind::Interrupted, "interrupted"));
                }
                rem
            }
            Plan::WouldBlock(n) => {
                if call == *n {
                    return Err(io::Error::new(io::ErrorKind::WouldBlock, "would block"));
                }
                13
            }
            Plan::FailCall(n, kind) => {
                if call == *n {
                    return Err(io::Error::new(*kind, "injected"));
                }
                11
            }
            Plan::FailByte(x, kind) => {
                if self.pos >= *x {
                    return Err(io::Error::new(*kind, "injected"));
                }
                (x - self.pos).min(5)
            }
            Plan::Panic(n) => {
                if call == *n {
                    panic!("PanicReader：第 {n} 次调用");
                }
                17
            }
            Plan::Lying => usize::MAX,
        };
        Ok(n.min(rem).min(want))
    }
}

impl Read for Feed<'_> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if let Plan::Lying = self.plan {
            self.calls.fetch_add(1, Ordering::SeqCst);
            // 错误的实现：报告的字节数比目标缓冲还大
            return Ok(buf.len() + 1);
        }
        let n = self.next_len(buf.len())?;
        buf[..n].copy_from_slice(&self.data[self.pos..self.pos + n]);
        self.pos += n;
        self.delivered.fetch_add(n, Ordering::SeqCst);
        Ok(n)
    }
}

impl BufRead for Feed<'_> {
    fn fill_buf(&mut self) -> io::Result<&[u8]> {
        if self.chunk_end <= self.pos {
            let n = self.next_len(usize::MAX)?;
            self.chunk_end = self.pos + n;
        }
        Ok(&self.data[self.pos..self.chunk_end])
    }
    fn consume(&mut self, amt: usize) {
        let amt = amt.min(self.chunk_end - self.pos);
        self.pos += amt;
        self.delivered.fetch_add(amt, Ordering::SeqCst);
    }
}

struct Run {
    snap: Snap,
    calls: usize,
    delivered: usize,
    after_eof: usize,
}

/// 直接把 Feed 当 BufRead 传入（cap = None），或者套 BufReader::with_capacity(cap)。
fn stream(d: &[u8], c: &ParserConfig, plan: Plan, cap: Option<usize>) -> Run {
    let feed = Feed::new(d, plan);
    let (calls, delivered, after_eof) = (
        feed.calls.clone(),
        feed.delivered.clone(),
        feed.after_eof.clone(),
    );
    let snap = match cap {
        None => {
            let mut r = Reader::from_reader_with_config(feed, c.clone()).expect("from_reader");
            snap!(r)
        }
        Some(cap) => {
            let mut r =
                Reader::from_reader_with_config(BufReader::with_capacity(cap, feed), c.clone())
                    .expect("from_reader");
            snap!(r)
        }
    };
    Run {
        snap,
        calls: calls.load(Ordering::SeqCst),
        delivered: delivered.load(Ordering::SeqCst),
        after_eof: after_eof.load(Ordering::SeqCst),
    }
}

#[track_caller]
fn assert_same(label: &str, base: &Snap, got: &Snap) {
    // v7（D31 裁定）：TotalInput 不再只比最终错误，事件序列也必须相同。
    if base == got {
        return;
    }
    let i = base
        .events
        .iter()
        .zip(&got.events)
        .position(|(a, b)| a != b)
        .unwrap_or(base.events.len().min(got.events.len()));
    panic!(
        "{label}：与 from_bytes 不一致。事件数 {} vs {}，第一个不同在第 {i} 项：{:?} vs {:?}；结尾 {:?} vs {:?}",
        base.events.len(),
        got.events.len(),
        base.events.get(i).map(|s| s.chars().take(120).collect::<String>()),
        got.events.get(i).map(|s| s.chars().take(120).collect::<String>()),
        base.end,
        got.end
    );
}

fn seeded_schedule(seed: u64) -> Vec<usize> {
    let ns = [2usize, 3, 7, 64, 4096];
    let n = ns[(seed as usize) % ns.len()];
    let mut s = seed.wrapping_mul(0x2545_F491_4F6C_DD1D) | 1;
    (0..16)
        .map(|_| (xorshift(&mut s) as usize % n) + 1)
        .collect()
}

/// 计划 §1.2 的切法。返回检查过的路径数。
fn check_doc(label: &str, d: &[u8], c: &ParserConfig) -> usize {
    check_doc_depth(label, d, c, Depth::Full)
}

/// 切块覆盖的深度。Full：每个切点、64 种随机切块、7 种 BufReader 容量；
/// Quick（默认套件用）：切点按步长取样（首尾和 0..=8 全保留）、8 种随机切块、3 种容量。
/// 两者每条路径上的断言完全相同，只是路径少；Full 放在 #[ignore] 用例里每轮用 --ignored 跑。
#[derive(Clone, Copy, PartialEq)]
enum Depth {
    Full,
    Quick,
}

fn check_doc_depth(label: &str, d: &[u8], c: &ParserConfig, depth: Depth) -> usize {
    let quick = depth == Depth::Quick;
    let base = bytes(d, c);
    assert_eq!(base, vec_path(d, c), "{label}：from_vec 与 from_bytes 不同");
    let mut n = 1;
    let big = d.len() > 64 << 10;
    // 逐字节，直接 BufRead 和套 BufReader 两种形态
    let run = stream(d, c, Plan::Byte, None);
    assert_same(&format!("{label} [逐字节]"), &base, &run.snap);
    assert_eq!(run.after_eof, 0, "{label}：Ok(0) 之后又调用了 read");
    assert_same(
        &format!("{label} [逐字节+BufReader(1)]"),
        &base,
        &stream(d, c, Plan::Byte, Some(1)).snap,
    );
    n += 2;
    if d.len() <= 4096 {
        let step = if quick { (d.len() / 32).max(1) } else { 1 };
        for k in (0..=d.len()).filter(|&k| k % step == 0 || k <= 8 || k + 8 >= d.len()) {
            let run = stream(d, c, Plan::Cut(k), None);
            assert_same(&format!("{label} [切点 {k}]"), &base, &run.snap);
            assert_eq!(
                run.after_eof, 0,
                "{label} [切点 {k}]：Ok(0) 之后又调用了 read"
            );
            n += 1;
        }
    }
    let rounds = if quick {
        8
    } else if big {
        16
    } else {
        64
    };
    for seed in 0..rounds {
        let plan = Plan::Schedule(seeded_schedule(seed));
        assert_same(
            &format!("{label} [随机切块 {seed}]"),
            &base,
            &stream(d, c, plan, None).snap,
        );
        n += 1;
    }
    let caps: &[usize] = if big || quick {
        &[1, 7, 4096]
    } else {
        &[1, 2, 3, 7, 16, 64, 8192]
    };
    for &cap in caps {
        assert_same(
            &format!("{label} [Short+BufReader({cap})]"),
            &base,
            &stream(d, c, Plan::Short(cap as u64 + 1), Some(cap)).snap,
        );
        n += 1;
    }
    n
}

// ───────────────────────────── 语料 ─────────────────────────────

fn w3c_root() -> PathBuf {
    std::env::var_os("PUREXML_W3C_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/w3c/cache/xmlconf"))
}

fn files_in(dir: &Path, xml_only: bool) -> Vec<PathBuf> {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut v: Vec<_> = rd
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_file())
        .filter(|p| !xml_only || p.extension().is_some_and(|x| x == "xml"))
        .filter(|p| !p.file_name().unwrap().to_string_lossy().ends_with(".md"))
        .collect();
    v.sort();
    v
}

fn corpus() -> Vec<(String, Vec<u8>)> {
    let mut out = Vec::new();
    let root = w3c_root().join("xmltest");
    let mut w3c = 0;
    for dir in ["valid/sa", "not-wf/sa", "valid/not-sa"] {
        for p in files_in(&root.join(dir), true) {
            out.push((
                format!("w3c:{dir}/{}", p.file_name().unwrap().to_string_lossy()),
                std::fs::read(&p).unwrap(),
            ));
            w3c += 1;
        }
    }
    if w3c == 0 {
        assert!(
            std::env::var_os("PUREXML_W3C_STRICT").is_none(),
            "PUREXML_W3C_STRICT：找不到 W3C 语料 {}",
            root.display()
        );
        eprintln!(
            "没有 W3C 语料（{}），只跑攻击样本和 fuzz 回归样本",
            root.display()
        );
    }
    let base = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data");
    for p in files_in(&base.join("attacks"), true) {
        out.push((
            format!("attack:{}", p.file_name().unwrap().to_string_lossy()),
            std::fs::read(&p).unwrap(),
        ));
    }
    for p in files_in(&base.join("fuzz-regressions"), false) {
        out.push((
            format!("fuzz:{}", p.file_name().unwrap().to_string_lossy()),
            std::fs::read(&p).unwrap(),
        ));
    }
    out
}

/// 计划 §1.2 “必须专门构造的切点”和“定界扫描器的专项切点”。每份都 ≤ 4 KiB，所以会遍历全部切点。
fn crafted() -> Vec<(&'static str, Vec<u8>)> {
    let s = |x: &str| x.as_bytes().to_vec();
    vec![
        // UTF-8 多字节字符在各种位置（2/3/4 字节）
        ("utf8-text", s("<r>é€😀x é€😀</r>")),
        ("utf8-names", s("<é€😀 a€😀=\"v\"><é€😀:x xmlns:é€😀=\"u\"/></é€😀>")),
        ("utf8-attr", s("<r a=\"é€😀\" b='😀&amp;é'/>")),
        ("utf8-comment-cdata-pi", s("<r><!--é€😀--><![CDATA[é€😀]]><?pé€😀 é€😀?></r>")),
        ("utf8-doctype-entity", s("<!DOCTYPE é€😀 [<!ENTITY e \"é€😀\">]><é€😀>&e;</é€😀>")),
        ("utf8-bad-mid", vec![b'<', b'r', b'>', 0xE2, 0x82, b'x', b'<', b'/', b'r', b'>']),
        ("utf8-bad-lead", vec![b'<', b'r', b'>', 0xFF, b'<', b'/', b'r', b'>']),
        ("utf8-truncated-eof", vec![b'<', b'r', b'>', 0xF0, 0x9F, 0x98]),
        ("utf8-bad-in-name", vec![b'<', b'r', 0xC3, b'>', b'<', b'/', b'r', b'>']),
        // BOM
        ("bom-utf8", [&[0xEF, 0xBB, 0xBF][..], b"<r/>"].concat()),
        ("bom-utf16be", [&[0xFE, 0xFF][..], &[0, b'<', 0, b'r', 0, b'/', 0, b'>']].concat()),
        ("bom-utf16le", [&[0xFF, 0xFE][..], &[b'<', 0, b'r', 0, b'/', 0, b'>', 0]].concat()),
        ("bom-only", vec![0xEF, 0xBB, 0xBF]),
        // CRLF
        ("crlf", s("<r a=\"1\r\n2\">x\r\ny\rz\r\n<!--a\r\nb--><?p a\r\nb?><![CDATA[\r\n]]></r>\r\n")),
        ("cr-eof", s("<r/>\r")),
        ("crlf-pi-gap", s("<r><?p\r\nd?><?q\rd?></r>")),
        // 标签和结束符
        ("tags", s("<r><a/><b></b><!--c--><![CDATA[x]]]]><?p d?></r>")),
        ("cdata-brackets", s("<r><![CDATA[]]]><![CDATA[]]]]><![CDATA[a]b]]c]]></r>")),
        ("comment-dashes", s("<r><!-- - -- --></r>")),
        ("empty-pi", s("<r><?p?><?q ?></r>")),
        // 实体和字符引用
        ("refs", s("<r>&amp;&#x10FFFF;&#65;&lt;&gt;&quot;&apos;</r>")),
        ("charref-zeros", s("<r>&#0000000000000065;&#x00000041;</r>")),
        ("named-entity", s("<!DOCTYPE r [<!ENTITY name \"v\">]><r a=\"&name;\">&name;</r>")),
        ("entity-markup-then-text", s("<!DOCTYPE r [<!ENTITY e \"<a>x</a>\">]><r>1&e;2&e;3</r>")),
        ("entity-empty", s("<!DOCTYPE r [<!ENTITY e \"\">]><r>&e;<a/>&e;&e;x&e;</r>")),
        ("undefined-entity", s("<r>&nope;</r>")),
        // 属性值
        ("attr-forms", s("<r a = \"1\" b='2'\tc=\"&amp;\" d=\"\t\n x\"/>")),
        ("attr-gt-lt", s("<r a=\">\" b='>'/><x a=\"<\"/>")),
        ("attr-dup", s("<r a=\"1\" a=\"2\"/>")),
        // XML 声明
        ("xmldecl", s("<?xml version=\"1.0\" encoding=\"UTF-8\" standalone='yes'?><r/>")),
        ("xmldecl-late", s(" <?xml version=\"1.0\"?><r/>")),
        ("xmldecl-q", s("<?xml version=\"1.0\"?><?xml-stylesheet x?><r/>")),
        ("xmldecl-gt-in-quote", s("<?xml version=\"1.0\" encoding=\"UTF-8?>\"?><r/>")),
        // DOCTYPE 内部子集
        (
            "subset",
            s("<!DOCTYPE r [<!ENTITY a \"]>\"><!ATTLIST r x CDATA ']>'><!--]>--><?p ]>?><!ENTITY % pe \"x\">%pe;<!ELEMENT r ANY>]><r>&a;</r>"),
        ),
        ("subset-unterminated", s("<!DOCTYPE r [<!ENTITY a \"x\">")),
        ("doctype-external", s("<!DOCTYPE r SYSTEM \"r.dtd\"><r/>")),
        // 文档末尾
        ("epilog-ws-comment", s("<r/>  <!--c-->\n  <?p?>  \n\n")),
        ("truncated-tag", s("<r><a")),
        ("truncated-attr", s("<r a=\"1")),
        ("truncated-comment", s("<r><!--abc")),
        ("unclosed", s("<r><a></a>")),
        ("empty", s("")),
        ("ws-only", s("   \n ")),
        ("text-outside", s("x<r/>")),
        ("two-roots", s("<r/><s/>")),
        ("bang-junk", s("<r><!x></r>")),
        ("lt-in-text", s("<r>a<b</r>")),
        ("gt-gt", s("<r>]]></r>")),
        ("ns", s("<a xmlns=\"urn:a\" xmlns:b=\"urn:b\"><b:c b:x=\"1\"/><u:x/></a>")),
    ]
}

/// 小上限，让 probe 和各种限额在小文档里就触发。
fn tight() -> ParserConfig {
    ParserConfig::default()
        .with_max_token_len(24)
        .with_max_internal_subset_len(20)
        .with_max_text_len(6)
        .with_max_name_len(4)
}

#[test]
fn crafted_cut_points_match_from_bytes() {
    let mut paths = 0;
    for (name, d) in crafted() {
        paths += check_doc(&format!("crafted:{name}"), &d, &ParserConfig::default());
        paths += check_doc(&format!("crafted:{name} (小上限)"), &d, &tight());
    }
    eprintln!("crafted：{paths} 条路径");
}

fn corpus_on_chunkings(depth: Depth) {
    let docs = corpus();
    assert!(docs.len() >= 90, "语料太少：{}", docs.len());
    let mut paths = 0;
    for (name, d) in &docs {
        paths += check_doc_depth(name, d, &ParserConfig::default(), depth);
    }
    eprintln!("语料 {} 份，{paths} 条路径", docs.len());
}

/// 默认套件：同一批语料，切点取样 + 8 种随机切块 + 3 种容量。
#[test]
fn corpus_matches_from_bytes_on_sampled_chunkings() {
    corpus_on_chunkings(Depth::Quick);
}

/// 完整版：每个切点、64 种随机切块、7 种容量（debug 约 50 s）。
#[test]
#[ignore = "slow: 语料全切点（debug 约 50 s），需要跑 --ignored"]
fn corpus_matches_from_bytes_on_all_chunkings() {
    corpus_on_chunkings(Depth::Full);
}

#[test]
fn corpus_matches_with_tight_limits() {
    // 同一批语料在小上限下（每个限额都会在 probe 边界触发），只做逐字节和 16 种随机切块
    let c = tight();
    for (name, d) in corpus() {
        let base = bytes(&d, &c);
        assert_eq!(base, vec_path(&d, &c), "{name}：from_vec");
        assert_same(
            &format!("{name} [小上限 逐字节]"),
            &base,
            &stream(&d, &c, Plan::Byte, None).snap,
        );
        for seed in 0..16 {
            let plan = Plan::Schedule(seeded_schedule(seed + 100));
            assert_same(
                &format!("{name} [小上限 随机 {seed}]"),
                &base,
                &stream(&d, &c, plan, None).snap,
            );
        }
    }
}

// ───────────────────────────── 计划 §1.3 短读、0、Interrupted、WouldBlock ─────────────────────────────

fn sample_docs() -> Vec<Vec<u8>> {
    vec![
        b"<?xml version=\"1.0\"?><!DOCTYPE r [<!ENTITY e \"<a>x</a>\">]><r a=\"1\">t&e;<!--c--><![CDATA[d]]><?p q?></r>".to_vec(),
        format!("<r>{}</r>", "<i a=\"1\">x &amp; y</i>".repeat(2000)).into_bytes(),
    ]
}

#[test]
fn zero_read_is_eof_and_later_data_is_ignored() {
    let c = ParserConfig::default();
    for d in sample_docs() {
        for k in [0, 1, 5, d.len() / 2, d.len()] {
            let run = stream(&d, &c, Plan::ZeroThenData(k), None);
            let prefix = bytes(&d[..k], &c);
            assert_same(&format!("ZeroThenData({k})"), &prefix, &run.snap);
            // Ok(0) 之后不再调用 read，后面的数据不被读取
            assert_eq!(run.after_eof, 0, "ZeroThenData({k})：EOF 之后又调用了 read");
            assert_eq!(
                run.delivered, k,
                "ZeroThenData({k})：读取了 Ok(0) 之后的数据"
            );
        }
    }
}

#[test]
fn interrupted_is_retried_transparently() {
    let c = ParserConfig::default();
    for d in sample_docs() {
        let base = bytes(&d, &c);
        for permille in [300, 900] {
            for seed in 1..=4 {
                let run = stream(&d, &c, Plan::Interrupt { permille, seed }, None);
                assert_same(
                    &format!("Interrupted p={permille} seed={seed}"),
                    &base,
                    &run.snap,
                );
            }
        }
        let run = stream(&d, &c, Plan::InterruptBurst(1000), None);
        assert_same("连续 1000 次 Interrupted", &base, &run.snap);
    }
}

#[track_caller]
fn assert_io_at(label: &str, got: &Snap, full: &Snap, d: &[u8], at: usize) {
    let Err((kind, pos)) = &got.end else {
        panic!("{label}：应报 Io，实际 {:?}", got.end);
    };
    assert_eq!(*kind, ErrorKind::Io, "{label}：应报 Io");
    assert_eq!(
        pos.offset, at as u64,
        "{label}：Io 的 offset 应等于已成功读入的字节数"
    );
    let want = position_after(&d[..at]);
    assert_eq!(
        (pos.line, pos.column),
        (want.line, want.column),
        "{label}：line/column 应与 offset 一致"
    );
    assert!(
        got.events.len() <= full.events.len() && got.events[..] == full.events[..got.events.len()],
        "{label}：Io 之前的事件不是完整序列的前缀"
    );
}

/// 与解析器相同的行列口径：CR、LF、CRLF 各算一行；列按 Unicode 标量（不计续字节）。
fn position_after(prefix: &[u8]) -> Position {
    let (mut line, mut column, mut cr) = (1u64, 1u64, false);
    for &b in prefix {
        if cr {
            cr = false;
            if b == b'\n' {
                continue;
            }
        }
        if b == b'\r' {
            line += 1;
            column = 1;
            cr = true;
        } else if b == b'\n' {
            line += 1;
            column = 1;
        } else if b & 0xC0 != 0x80 {
            column += 1;
        }
    }
    Position {
        line,
        column,
        offset: prefix.len() as u64,
    }
}

#[test]
fn would_block_is_io_and_fuses() {
    let c = ParserConfig::default();
    for d in sample_docs() {
        let full = bytes(&d, &c);
        for n in [1usize, 2, 5, 40] {
            let feed = Feed::new(&d, Plan::WouldBlock(n));
            let (calls, delivered) = (feed.calls.clone(), feed.delivered.clone());
            let mut r = Reader::from_reader_with_config(feed, c.clone()).unwrap();
            let got = snap!(r);
            let at = delivered.load(Ordering::SeqCst);
            if got.end.is_ok() {
                // 文档在第 n 次调用之前已经读完
                assert!(calls.load(Ordering::SeqCst) < n, "WouldBlock({n})");
                continue;
            }
            assert_io_at(&format!("WouldBlock({n})"), &got, &full, &d, at);
            let before = calls.load(Ordering::SeqCst);
            let e0 = r.read_event().unwrap_err();
            for _ in 0..10 {
                assert_eq!(r.read_event().unwrap_err(), e0, "WouldBlock 之后应熔断");
            }
            assert_eq!(
                calls.load(Ordering::SeqCst),
                before,
                "熔断后不应再调用底层读取器"
            );
        }
    }
}

// ───────────────────────────── 计划 §1.4 Io 错误注入 ─────────────────────────────

#[test]
fn injected_io_errors_report_bytes_read() {
    let c = ParserConfig::default();
    let kinds = [
        io::ErrorKind::Other,
        io::ErrorKind::UnexpectedEof,
        io::ErrorKind::InvalidData,
        io::ErrorKind::BrokenPipe,
    ];
    let docs = [
        sample_docs()[0].clone(),
        b"<r><a></b></r>".to_vec(),
        b"<r>\r\n\xC3\xA9 &bad; </r>".to_vec(),
    ];
    for d in &docs {
        let full = bytes(d, &c);
        // FailAt { byte: x }
        for x in 0..=d.len() {
            for kind in kinds {
                let run = stream(d, &c, Plan::FailByte(x, kind), None);
                let err_offset = full.end.as_ref().err().map(|(_, p)| p.offset as usize);
                match &run.snap.end {
                    Err((ErrorKind::Io, _)) => {
                        assert_eq!(run.delivered, x, "FailByte({x})：已读字节数");
                        assert_io_at(&format!("FailByte({x}) {kind:?}"), &run.snap, &full, d, x);
                    }
                    other => {
                        // 只有在出错位置之前已经读到的字节足以报出原来的错误时，才可以不是 Io
                        assert_eq!(
                            other, &full.end,
                            "FailByte({x})：既不是 Io 也不是原来的结果"
                        );
                        let e = err_offset.expect("良构文档在 IO 出错时不可能成功");
                        assert!(
                            x >= e,
                            "FailByte({x})：原错误在 {e}，IO 在它之前出错却没有报 Io"
                        );
                    }
                }
            }
        }
        // FailAt { call: n }
        for n in 1..=200usize {
            let run = stream(d, &c, Plan::FailCall(n, io::ErrorKind::Other), None);
            if let Err((ErrorKind::Io, _)) = &run.snap.end {
                assert_io_at(
                    &format!("FailCall({n})"),
                    &run.snap,
                    &full,
                    d,
                    run.delivered,
                );
            } else {
                assert_eq!(run.snap.end, full.end, "FailCall({n})");
                assert!(
                    run.calls < n || full.end.is_err(),
                    "FailCall({n})：第 {n} 次调用应已发生"
                );
            }
        }
    }
}

#[test]
fn io_error_fuses_and_reader_is_not_called_again() {
    let d = sample_docs()[1].clone();
    let feed = Feed::new(&d, Plan::FailByte(1000, io::ErrorKind::BrokenPipe));
    let calls = feed.calls.clone();
    let mut r = Reader::from_reader(feed).unwrap();
    let e = loop {
        match r.read_event() {
            Ok(Some(_)) => {}
            Ok(None) => panic!("应报 Io"),
            Err(e) => break e,
        }
    };
    assert_eq!(e.kind, ErrorKind::Io);
    assert_eq!(e.position.offset, 1000);
    let before = calls.load(Ordering::SeqCst);
    for _ in 0..10 {
        assert_eq!(r.read_event().unwrap_err(), e);
    }
    assert_eq!(
        calls.load(Ordering::SeqCst),
        before,
        "熔断后不应再调用底层读取器"
    );
}

#[test]
fn panicking_reader_propagates_and_leaves_no_global_state() {
    let d = sample_docs()[1].clone();
    for n in [1usize, 2, 10] {
        let d2 = d.clone();
        let r = std::panic::catch_unwind(move || {
            let mut r = Reader::from_reader(Feed::new(&d2, Plan::Panic(n))).unwrap();
            while r.read_event().unwrap().is_some() {}
        });
        assert!(r.is_err(), "PanicReader({n})：panic 应原样传出");
    }
    let c = ParserConfig::default();
    assert_same(
        "panic 之后的新 Reader",
        &bytes(&d, &c),
        &stream(&d, &c, Plan::Byte, None).snap,
    );
}

#[test]
fn lying_reader_is_io_not_a_panic() {
    let d = sample_docs()[0].clone();
    let run = stream(&d, &ParserConfig::default(), Plan::Lying, None);
    assert!(
        matches!(
            run.snap.end,
            Err((ErrorKind::Io, _)) | Err((ErrorKind::Syntax, _))
        ),
        "LyingReader：应报 Io 或 Syntax，实际 {:?}",
        run.snap.end
    );
    assert!(run.snap.events.is_empty());
}

#[test]
fn reader_is_send_when_source_is_send_and_accepts_non_send_sources() {
    fn send<T: Send>() {}
    send::<Reader<purexml::IoInput<BufReader<std::fs::File>>>>();
    send::<Reader<purexml::IoInput<&[u8]>>>();
    // !Send 的 BufRead 也可以用
    struct NotSend(#[allow(dead_code)] std::rc::Rc<()>, &'static [u8]);
    impl Read for NotSend {
        fn read(&mut self, b: &mut [u8]) -> io::Result<usize> {
            self.1.read(b)
        }
    }
    let mut r =
        Reader::from_reader(BufReader::new(NotSend(std::rc::Rc::new(()), b"<r/>"))).unwrap();
    assert!(r.read_event().unwrap().is_some());
}

// ───────────────────────────── 计划 §1.7 熔断 ─────────────────────────────

fn error_samples() -> Vec<(&'static str, Vec<u8>, ParserConfig)> {
    let d = ParserConfig::default();
    let s = |x: &str| x.as_bytes().to_vec();
    vec![
        ("Syntax", s("<r><a></b></r>"), d.clone()),
        (
            "Encoding",
            vec![b'<', b'r', b'>', 0xFF, b'<', b'/', b'r', b'>'],
            d.clone(),
        ),
        (
            "EEF",
            s("<!DOCTYPE r [<!ENTITY e SYSTEM \"x\">]><r>&e;</r>"),
            d.clone(),
        ),
        (
            "Depth",
            s("<a><a><a><a></a></a></a></a>"),
            d.clone().with_max_depth(3),
        ),
        (
            "Attributes",
            s("<r a=\"1\" b=\"2\" c=\"3\"/>"),
            d.clone().with_max_attrs(2),
        ),
        ("NameLength", s("<abcdef/>"), d.clone().with_max_name_len(3)),
        (
            "TextLength",
            s("<r>abcdefgh</r>"),
            d.clone().with_max_text_len(4),
        ),
        (
            "AttrValueLength",
            s("<r a=\"abcdefgh\"/>"),
            d.clone().with_max_text_len(4),
        ),
        (
            "TotalInput",
            s("<r>abcdefgh</r>"),
            d.clone().with_max_total_input(8),
        ),
        (
            "TokenLength",
            s("<r a=\"&amp;&amp;&amp;&amp;&amp;\"/>"),
            d.clone().with_max_token_len(12),
        ),
        (
            "InternalSubsetLength",
            s("<!DOCTYPE r [<!ELEMENT r ANY>]><r/>"),
            d.clone().with_max_internal_subset_len(5),
        ),
        (
            "EntityDepth",
            s("<!DOCTYPE r [<!ENTITY a \"&b;\"><!ENTITY b \"&c;\"><!ENTITY c \"x\">]><r>&a;</r>"),
            d.clone().with_max_entity_depth(2),
        ),
        (
            "EntityExpansions",
            s("<!DOCTYPE r [<!ENTITY a \"x\">]><r>&a;&a;&a;</r>"),
            d.clone().with_max_entity_expansions(2),
        ),
        (
            "entity-syntax",
            s("<!DOCTYPE r [<!ENTITY a \"<b>\">]><r>&a;</r>"),
            d.clone(),
        ),
    ]
}

#[test]
fn every_error_class_fuses_on_every_path() {
    for (name, doc, c) in error_samples() {
        let check =
            |label: &str, first: Error, mut next: Box<dyn FnMut() -> Result<bool, Error> + '_>| {
                for i in 0..10 {
                    match next() {
                        Err(e) => {
                            assert_eq!(e, first, "{name} {label}：第 {} 次再调用的错误不同", i + 1)
                        }
                        Ok(some) => panic!(
                            "{name} {label}：出错后又返回了 Ok({})",
                            if some { "Some" } else { "None" }
                        ),
                    }
                }
            };
        macro_rules! fuse_path {
            ($label:expr, $reader:expr) => {{
                let mut r = $reader;
                let first = loop {
                    match r.read_event() {
                        Ok(Some(_)) => {}
                        Ok(None) => panic!("{name} {}：应出错", $label),
                        Err(e) => break e,
                    }
                };
                check(
                    $label,
                    first.clone(),
                    Box::new(|| r.read_event().map(|e| e.is_some())),
                );
                first
            }};
        }
        let a = fuse_path!(
            "from_bytes",
            Reader::from_bytes(&doc).with_config(c.clone())
        );
        let b = fuse_path!(
            "from_vec",
            Reader::from_vec(doc.clone()).with_config(c.clone())
        );
        let s1 = fuse_path!(
            "逐字节",
            Reader::from_reader_with_config(Feed::new(&doc, Plan::Byte), c.clone()).unwrap()
        );
        let s2 = fuse_path!(
            "64 KiB",
            Reader::from_reader_with_config(
                BufReader::with_capacity(64 << 10, &doc[..]),
                c.clone()
            )
            .unwrap()
        );
        for (label, e) in [("from_vec", &b), ("逐字节", &s1), ("64 KiB", &s2)] {
            assert_eq!(
                (&e.kind, e.position),
                (&a.kind, a.position),
                "{name} {label}：错误与 from_bytes 不同"
            );
        }
        // 底层读取器在熔断后不再被调用
        let feed = Feed::new(&doc, Plan::Byte);
        let calls = feed.calls.clone();
        let mut r = Reader::from_reader_with_config(feed, c.clone()).unwrap();
        while let Ok(Some(_)) = r.read_event() {}
        let before = calls.load(Ordering::SeqCst);
        for _ in 0..10 {
            let _ = r.read_event();
        }
        assert_eq!(
            calls.load(Ordering::SeqCst),
            before,
            "{name}：熔断后又调用了底层读取器"
        );
    }
}

// ───────────────────────────── 计划 §4：所有限额，四条路径 ─────────────────────────────

/// 一次性、逐字节、16 种随机切块、from_vec 四条路径；返回 from_bytes 的结果。
fn four_paths(label: &str, d: &[u8], c: &ParserConfig) -> Snap {
    let base = bytes(d, c);
    assert_eq!(base, vec_path(d, c), "{label}：from_vec 与 from_bytes 不同");
    assert_same(
        &format!("{label} [逐字节]"),
        &base,
        &stream(d, c, Plan::Byte, None).snap,
    );
    for seed in 0..16 {
        let plan = Plan::Schedule(seeded_schedule(seed + 1000));
        assert_same(
            &format!("{label} [随机 {seed}]"),
            &base,
            &stream(d, c, plan, None).snap,
        );
    }
    base
}

#[track_caller]
fn limit_of(label: &str, s: &Snap) -> (LimitKind, u64, u64, Position) {
    match &s.end {
        Err((
            ErrorKind::SecurityLimit {
                kind,
                limit,
                actual,
            },
            p,
        )) => (*kind, *limit, *actual, *p),
        other => panic!("{label}：应报 SecurityLimit，实际 {other:?}"),
    }
}

#[track_caller]
fn pass(label: &str, d: &[u8], c: &ParserConfig) {
    let s = four_paths(label, d, c);
    assert!(
        s.end.is_ok(),
        "{label}：刚好等于上限应通过，实际 {:?}",
        s.end
    );
}

/// 超 1 失败：断言 kind、limit、actual 和 position.offset。
#[track_caller]
fn fail(
    label: &str,
    d: &[u8],
    c: &ParserConfig,
    kind: LimitKind,
    limit: u64,
    actual: u64,
    offset: u64,
) {
    let s = four_paths(label, d, c);
    let (k, l, a, p) = limit_of(label, &s);
    assert_eq!(
        (k, l, a),
        (kind, limit, actual),
        "{label}：kind/limit/actual"
    );
    assert_eq!(p.offset, offset, "{label}：position.offset（{p:?}）");
    let want = position_after(&d[..offset as usize]);
    assert_eq!(
        (p.line, p.column),
        (want.line, want.column),
        "{label}：line/column 与 offset 不一致"
    );
}

#[test]
fn token_length_boundaries() {
    let m = 64u64;
    let c = ParserConfig::default().with_max_token_len(m);
    // 开始标签正好 m 字节通过；多 1 字节失败，position = 标签起点 + m（越界的那个字节本身）
    let tag = |len: usize| format!("<r a=\"{}\"/>", "v".repeat(len - 9));
    let d = format!("<doc>{}</doc>", tag(m as usize));
    pass("TokenLength 开始标签 = m", d.as_bytes(), &c);
    let d = format!("<doc>{}</doc>", tag(m as usize + 1));
    fail(
        "TokenLength 开始标签 m+1",
        d.as_bytes(),
        &c,
        LimitKind::TokenLength,
        m,
        m + 1,
        5 + m,
    );
    // 文本 token（含 & 使框定器不走纯文本探测）
    let text = |len: usize| format!("&amp;{}", "t".repeat(len - 5));
    let d = format!("<doc>{}</doc>", text(m as usize));
    pass("TokenLength 文本 = m", d.as_bytes(), &c);
    let d = format!("<doc>{}</doc>", text(m as usize + 1));
    fail(
        "TokenLength 文本 m+1",
        d.as_bytes(),
        &c,
        LimitKind::TokenLength,
        m,
        m + 1,
        5 + m,
    );
    // CRLF 文本：规范化后只有一半，原始字节触发 TokenLength
    let d = format!("<doc>{}\r</doc>", "\r\n".repeat(32));
    fail(
        "TokenLength CRLF 文本",
        d.as_bytes(),
        &c,
        LimitKind::TokenLength,
        m,
        m + 1,
        5 + m,
    );
    // 字符引用前导零：输出 1 个字符，原始长度 m+1
    let d = format!("<doc>&#{}65;</doc>", "0".repeat(m as usize - 4));
    fail(
        "TokenLength 前导零字符引用",
        d.as_bytes(),
        &c,
        LimitKind::TokenLength,
        m,
        m + 1,
        5 + m,
    );
    // prolog 里的 token 包含前导空白
    let d = format!(
        "{}<!--{}--><r/>",
        " ".repeat(10),
        "c".repeat(m as usize - 17)
    );
    pass("TokenLength 前导空白 + 注释 = m", d.as_bytes(), &c);
    let d = format!(
        "{}<!--{}--><r/>",
        " ".repeat(10),
        "c".repeat(m as usize - 16)
    );
    fail(
        "TokenLength 前导空白 + 注释 m+1",
        d.as_bytes(),
        &c,
        LimitKind::TokenLength,
        m,
        m + 1,
        m,
    );
    // 未闭合的长 token
    let d = format!("<doc><a b=\"{}", "&amp;".repeat(40));
    fail(
        "TokenLength 未闭合",
        d.as_bytes(),
        &c,
        LimitKind::TokenLength,
        m,
        m + 1,
        5 + m,
    );
}

#[test]
fn token_length_error_precedence() {
    // token 内在 m 之前已经有别的错误：报那个错误，两条路径相同
    let c = ParserConfig::default().with_max_token_len(64);
    for (label, d) in [
        (
            "非法字符",
            format!("<r a=\"\u{1}{}\"/>", "&amp;".repeat(20)),
        ),
        (
            "TextLength",
            format!("<r>{}&amp;{}</r>", "t".repeat(10), "u".repeat(100)),
        ),
    ] {
        let c = if label == "TextLength" {
            c.clone().with_max_text_len(8)
        } else {
            c.clone()
        };
        let s = four_paths(label, d.as_bytes(), &c);
        assert!(
            !matches!(
                s.end,
                Err((
                    ErrorKind::SecurityLimit {
                        kind: LimitKind::TokenLength,
                        ..
                    },
                    _
                ))
            ),
            "{label}：应报 token 内更早的错误，实际 {:?}",
            s.end
        );
    }
    let mut bad = b"<r a=\"".to_vec();
    bad.push(0xFF);
    bad.extend_from_slice("&amp;".repeat(20).as_bytes());
    bad.extend_from_slice(b"\"/>");
    let s = four_paths("非法 UTF-8 字节", &bad, &c);
    assert!(
        matches!(s.end, Err((ErrorKind::Encoding, _))),
        "{:?}",
        s.end
    );
}

#[test]
fn internal_subset_length_boundaries() {
    let m = 40u64;
    let c = ParserConfig::default().with_max_internal_subset_len(m);
    let subset = |len: usize| format!("<!ENTITY e \"{}\">", "v".repeat(len - 14));
    let d = format!("<!DOCTYPE r [{}]><r/>", subset(m as usize));
    pass("InternalSubsetLength = m", d.as_bytes(), &c);
    let d = format!("<!DOCTYPE r [{}]><r/>", subset(m as usize + 1));
    // 子集从 '[' 之后（偏移 13）开始，越界字节是第 m 个（0 起计）
    fail(
        "InternalSubsetLength m+1",
        d.as_bytes(),
        &c,
        LimitKind::InternalSubsetLength,
        m,
        m + 1,
        13 + m,
    );
    // 越界发生在声明关键字中途
    let d = format!(
        "<!DOCTYPE r [{}<!ENTITY f \"x\">]><r/>",
        " ".repeat(m as usize - 3)
    );
    fail(
        "InternalSubsetLength 关键字中途",
        d.as_bytes(),
        &c,
        LimitKind::InternalSubsetLength,
        m,
        m + 1,
        13 + m,
    );
    // 引号里的 ']' 和 '>' 不结束子集，但计入长度
    let d = format!(
        "<!DOCTYPE r [<!ENTITY e \"]>{}\">]><r/>",
        "v".repeat(m as usize - 16)
    );
    pass("InternalSubsetLength 引号里的 ]> = m", d.as_bytes(), &c);
    // 未闭合
    let d = format!("<!DOCTYPE r [{}", " ".repeat(100));
    fail(
        "InternalSubsetLength 未闭合",
        d.as_bytes(),
        &c,
        LimitKind::InternalSubsetLength,
        m,
        m + 1,
        13 + m,
    );
}

#[test]
fn total_input_boundaries() {
    let m = 4096u64;
    let c = ParserConfig::default().with_max_total_input(m);
    let doc = |len: usize| format!("<r>{}</r>", "x".repeat(len - 7));
    pass("TotalInput = m", doc(m as usize).as_bytes(), &c);
    let d = doc(m as usize + 1);
    let s = four_paths("TotalInput m+1", d.as_bytes(), &c);
    let (k, l, a, p) = limit_of("TotalInput", &s);
    assert_eq!((k, l, a, p.offset), (LimitKind::TotalInput, m, m + 1, m));
    assert_eq!((p.line, p.column), (1, m + 1));
    // 多行、切点落在 4096 前后
    let d = format!("<r>{}</r>", "ab\r\n".repeat(1100));
    let s = four_paths("TotalInput 多行", d.as_bytes(), &c);
    let (_, _, a, p) = limit_of("TotalInput 多行", &s);
    assert_eq!((a, p), (m + 1, position_after(&d.as_bytes()[..m as usize])));
}

/// D31（R2b fuzz 发现，Lead 11:51 裁定）：TotalInput 按位置判定。输入总长超过 max_total_input，
/// 但在 offset = limit 之前就有别的错误时，四条路径（含各种块长）都报那个更早的错误。
/// 修复前：from_bytes 一开始就按总长报 TotalInput，流式则随块长变化。
#[test]
fn d31_total_input_does_not_depend_on_chunking() {
    let c = ParserConfig::default()
        .with_max_text_len(12)
        .with_max_total_input(512);
    let docs = [
        // 文本在 offset 3 开始，第 13 个字符（offset 15）越界，position = 16；总长 597
        (
            format!("<r>{}</r>{}", "t".repeat(30), " ".repeat(560)),
            16u64,
        ),
        // 文本在 offset 498 开始，越界字符在 510，position = 511（limit 前一个字节）；总长 576
        (
            format!(
                "<r>{}<a>{}</a></r>{}",
                "<b/>".repeat(123),
                "t".repeat(30),
                " ".repeat(40)
            ),
            511,
        ),
    ];
    for (i, (d, off)) in docs.iter().enumerate() {
        let label = format!("D31 文档 {i}");
        let base = all_chunkings(&label, d.as_bytes(), &c);
        let (k, l, a, p) = limit_of(&label, &base);
        assert_eq!(
            (k, l, a, p.offset),
            (LimitKind::TextLength, 12, 13, *off),
            "{label}：应报更早的 TextLength"
        );
        assert_eq!(p, position_after(&d.as_bytes()[..*off as usize]));
    }
}

/// 四条路径 + 固定块长 1/7/511/512/513/8192 和整块；全部与 from_bytes 完全相同。
fn all_chunkings(label: &str, d: &[u8], c: &ParserConfig) -> Snap {
    let base = four_paths(label, d, c);
    for n in [1usize, 7, 511, 512, 513, 8192, d.len().max(1)] {
        assert_same(
            &format!("{label} [块长 {n}]"),
            &base,
            &stream(d, c, Plan::Schedule(vec![n]), None).snap,
        );
    }
    base
}

/// TotalInput 本身（D31 裁定）：没有更早的错误时，四条路径和各种块长都报 TotalInput，
/// position = limit 那个字节，actual = limit + 1，之前的事件完全相同；刚好等于 limit 时通过；
/// 更靠后的错误（offset > limit）让位给 TotalInput。
#[test]
fn total_input_is_positional_on_all_chunkings() {
    let m = 512u64;
    let c = ParserConfig::default()
        .with_max_text_len(12)
        .with_max_total_input(m);
    let pad = |len: usize| {
        format!("<r>{}</r>", "<b/>".repeat((len - 7) / 4)) + &" ".repeat((len - 7) % 4)
    };
    // 边界：刚好 m 字节通过
    let exact = pad(m as usize);
    assert_eq!(exact.len(), m as usize);
    let s = all_chunkings("TotalInput = m", exact.as_bytes(), &c);
    assert!(s.end.is_ok(), "TotalInput = m 应通过，实际 {:?}", s.end);
    let cases = [
        // m+1：越界的是结尾后的空白
        ("m+1（尾部空白）", pad(m as usize) + " "),
        // 第 m 个字节落在一个注释 token 中间
        (
            "落在 token 中间",
            // 注释占 507..524，第 512 个字节在注释内部；注释内容 10 个字符，不触发 max_text_len
            format!("<r>{}<!--{}--></r>", "<b/>".repeat(126), "c".repeat(10)),
        ),
        // 多行：line/column 按主输入计算
        ("多行", format!("<r>{}</r>", "<b/>\r\n".repeat(120))),
        // 更靠后的错误（文本在 offset 600 才越界）让位给 TotalInput
        (
            "后面的错误让位",
            format!("<r>{}<a>{}</a></r>", "<b/>".repeat(150), "t".repeat(30)),
        ),
    ];
    for (name, d) in cases {
        assert!(d.len() as u64 > m, "{name}：样本必须超过 limit");
        let label = format!("TotalInput {name}");
        let base = all_chunkings(&label, d.as_bytes(), &c);
        let (k, l, a, p) = limit_of(&label, &base);
        assert_eq!(
            (k, l, a, p.offset),
            (LimitKind::TotalInput, m, m + 1, m),
            "{label}"
        );
        assert_eq!(
            p,
            position_after(&d.as_bytes()[..m as usize]),
            "{label}：行列"
        );
        // 位置判定：limit 之前已经完整的 token 都要产出事件
        assert!(!base.events.is_empty(), "{label}：limit 之前的事件不应丢掉");
    }
}

/// D32（R2b fuzz 发现，reader_1byte）：内部子集里的 `<!-->`。定界扫描器把开头 `<!--` 的
/// `--` 和后面的 `>` 当成了 `-->`，在 `]>` 处提前结束 token；引擎仍在未闭合的注释里，
/// 读到窗口末尾就误报 TokenLength（32 MiB 上限、文档只有几十字节），位置还随块长变化。
/// main 257c900 上报的是 Syntax。四条路径必须一致，并且不能是 TokenLength。
#[test]
fn d32_subset_comment_opener_is_not_a_terminator() {
    let c = ParserConfig::default();
    for d in [
        "<!DOCTYPE r [<!-->]><r/>",
        "<!DOCTYPE r [<!-->ENTITY ]>\n<r>&e;",
        "\n<!DOCTYPE r [\n<!-->ENTITY e \"<?x<r>&e;&e;&e;&e;^&e;;\">\n]>\n<r>&e;e;&",
    ] {
        // 四条路径 + 各种块长的 kind 和 position 完全一致（assert_same），并且是 Syntax
        let s = all_chunkings(&format!("D32 {d:?}"), d.as_bytes(), &c);
        assert!(
            matches!(s.end, Err((ErrorKind::Syntax, _))),
            "D32 {d:?}：应报 Syntax（未闭合的注释），实际 {:?}",
            s.end
        );
    }
    // 合法的对照：注释里出现 `]>` 不结束内部子集
    pass("D32 合法", b"<!DOCTYPE r [<!-- ]> -->]><r/>", &c);
}

/// R2c（6894e02 审查）：在 XML 声明、PI、DOCTYPE、注释、CDATA 的开头、中间关键字和结尾附近，
/// 用 max_token_len、max_total_input = 1..=64 把探测截在每一个字节上，四条路径加各种块长必须完全一致；
/// 报 TokenLength / TotalInput 时 limit、actual 按口径（actual = limit + 1，TotalInput 的 offset = limit）。
/// 默认限额下：合法文档必须通过；任何前缀（EOF 截断）和畸形输入都不能报 TokenLength / TotalInput。
fn probe_cut_docs() -> Vec<(&'static str, bool)> {
    vec![
        (
            "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?><r/>",
            true,
        ),
        ("<?xml version='1.0'?>\n<?pi data?><r/>", true),
        ("<?xml version=\"1.0\" encoding=\"?>\"?><r/>", false),
        ("<?xml version=\"1.0\" encodin=\"x\"?><r/>", false),
        ("<?xml version=\"1.0\" stand?><r/>", false),
        ("<?xml version=\"1.0\" ?<r/>", false),
        ("<?xml version=\"1.0\" standalone=\"maybe\"?><r/>", false),
        (
            "<!DOCTYPE r [<!ENTITY e \"x\"><!-- c --><?p d?>]><r>&e;</r>",
            true,
        ),
        ("<!DOCTYPE r PUBLIC \"p\" \"s\"><r/>", true),
        ("<!DOCTYPE r [<!-->]><r/>", false),
        ("<!DOCTYPE r [<?>]><r/>", false),
        (
            "<!DOCTYPE r [<!ELEMENT r ANY><!ATTLIST r a CDATA \"d\">]><r/>",
            true,
        ),
        ("<r><!-- comment --><![CDATA[x]]>y<?p q?></r>", true),
        ("<r><!-- a -- b --></r>", false),
        ("<r><![CDAT[x]]></r>", false),
        ("<![CDATA[x]]><r/>", false),
        ("<r><?xml x?></r>", false),
        ("<r a=\"1\" b='2'>t&amp;&#x41;</r><!--e--><?p?>", true),
    ]
}

fn check_limit_shape(label: &str, s: &Snap, c: &ParserConfig) {
    if let Err((
        ErrorKind::SecurityLimit {
            kind,
            limit,
            actual,
        },
        p,
    )) = &s.end
    {
        match kind {
            LimitKind::TokenLength => {
                assert_eq!(
                    (*limit, *actual),
                    (c.max_token_len, c.max_token_len + 1),
                    "{label}"
                );
            }
            LimitKind::TotalInput => {
                assert_eq!(
                    (*limit, *actual, p.offset),
                    (c.max_total_input, c.max_total_input + 1, c.max_total_input),
                    "{label}"
                );
            }
            _ => {}
        }
    }
}

fn is_cap(e: &End) -> bool {
    matches!(
        e,
        Err((
            ErrorKind::SecurityLimit {
                kind: LimitKind::TokenLength | LimitKind::TotalInput,
                ..
            },
            _
        ))
    )
}

#[test]
fn probe_cuts_in_keywords_match_on_all_paths() {
    let d = ParserConfig::default();
    for (doc, valid) in probe_cut_docs() {
        let b = doc.as_bytes();
        let n = b.len() as u64;
        let base = all_chunkings(&format!("默认 {doc:?}"), b, &d);
        assert_eq!(base.end.is_ok(), valid, "默认 {doc:?}：{:?}", base.end);
        // 每个前缀（EOF 截断）：默认限额下不能报 TokenLength / TotalInput
        for k in 0..b.len() {
            let label = format!("默认 {doc:?} 前缀 {k}");
            let s = all_chunkings(&label, &b[..k], &d);
            assert!(!is_cap(&s.end), "{label}：不应报上限，实际 {:?}", s.end);
        }
        let base_off = err_offset(&base);
        for t in 1..=64u64 {
            for (name, c) in [
                ("max_token_len", d.clone().with_max_token_len(t)),
                ("max_total_input", d.clone().with_max_total_input(t)),
            ] {
                let label = format!("{name}={t} {doc:?}");
                let s = all_chunkings(&label, b, &c);
                check_limit_shape(&label, &s, &c);
                if t >= n {
                    assert_eq!(s, base, "{label}：上限不小于文档长度时应与默认一致");
                    continue;
                }
                // Lead 13:56 统一原则：默认配置下的错误在 offset ≥ t（判断要读到 t 或之后），
                // 或者文档合法，TotalInput 就在 t 上先报。
                if name == "max_total_input" && base_off >= t {
                    let (k, l, a, p) = limit_of(&label, &s);
                    assert_eq!(
                        (k, l, a, p.offset),
                        (LimitKind::TotalInput, t, t + 1, t),
                        "{label}"
                    );
                }
                // 合法文档只能通过或报 TokenLength，不能变成 Syntax / Encoding。
                if name == "max_token_len" && valid {
                    assert!(
                        s.end.is_ok() || limit_of(&label, &s).0 == LimitKind::TokenLength,
                        "{label}：合法文档只能通过或报 TokenLength，实际 {:?}",
                        s.end
                    );
                }
            }
        }
    }
}

/// D33（Lead 13:56 裁定，统一原则）：按字节计的限额只看位置，在越界的那个字节上报，
/// 先于任何需要读到该字节及之后的 Syntax/Encoding 判断。五类都写死 kind、actual、position：
/// (a) 只设 tot：TotalInput @t（流式不能误报 32 MiB 的 TokenLength）；
/// (b) `encodin` 截在 @20：TokenLength；(c) 声明结尾 `?>` 截在 `?`：TokenLength；
/// (d) 同一位置只设 tot：TotalInput；(e) 注释里的 `--` 跨过 tot：TotalInput。
#[test]
fn d33_keyword_cut_agrees_on_all_paths() {
    let d = ParserConfig::default();
    let decl: &[u8] = b"<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?><r/>";
    for t in [6u64, 7, 12, 20, 33, 47] {
        fail(
            &format!("D33(a) tot={t}"),
            decl,
            &d.clone().with_max_total_input(t),
            LimitKind::TotalInput,
            t,
            t + 1,
            t,
        );
    }
    fail(
        "D33(b) tok=20 encodin",
        b"<?xml version=\"1.0\" encodin=\"x\"?><r/>",
        &d.clone().with_max_token_len(20),
        LimitKind::TokenLength,
        20,
        21,
        20,
    );
    fail(
        "D33(c) tok=53 standalone ?>",
        decl,
        &d.clone().with_max_token_len(53),
        LimitKind::TokenLength,
        53,
        54,
        53,
    );
    fail(
        "D33(c) tok=19 version ?>",
        b"<?xml version='1.0'?>\n<?pi data?><r/>",
        &d.clone().with_max_token_len(19),
        LimitKind::TokenLength,
        19,
        20,
        19,
    );
    fail(
        "D33(d) tot=53",
        decl,
        &d.clone().with_max_total_input(53),
        LimitKind::TotalInput,
        53,
        54,
        53,
    );
    fail(
        "D33(e) tot=10 注释 --",
        b"<r><!-- a -- b --></r>",
        &d.clone().with_max_total_input(10),
        LimitKind::TotalInput,
        10,
        11,
        10,
    );
}

#[test]
fn char_counted_limits_point_after_the_character() {
    // NameLength / TextLength / AttrValueLength：position = 越界字符之后（1/2/3/4 字节）
    for ch in ["y", "é", "€", "😀"] {
        let n = ch.len() as u64;
        let c = ParserConfig::default().with_max_name_len(8);
        let d = format!("<{}{ch}z/>", "a".repeat(8));
        fail(
            &format!("NameLength {n}B"),
            d.as_bytes(),
            &c,
            LimitKind::NameLength,
            8,
            8 + n,
            1 + 8 + n,
        );
        pass(
            &format!("NameLength = {n}B"),
            format!("<{}{ch}/>", "a".repeat(8 - n as usize)).as_bytes(),
            &c,
        );
        let c = ParserConfig::default().with_max_text_len(8);
        let d = format!("<r>{}{ch}zz</r>", "t".repeat(8));
        fail(
            &format!("TextLength {n}B"),
            d.as_bytes(),
            &c,
            LimitKind::TextLength,
            8,
            8 + n,
            3 + 8 + n,
        );
        let d = format!("<r a=\"{}{ch}zz\"/>", "v".repeat(8));
        fail(
            &format!("AttrValueLength {n}B"),
            d.as_bytes(),
            &c,
            LimitKind::AttrValueLength,
            8,
            8 + n,
            6 + 8 + n,
        );
        for (pre, post) in [
            ("<r><!--", "zz--></r>"),
            ("<r><![CDATA[", "zz]]></r>"),
            ("<r><?p ", "zz?></r>"),
        ] {
            let d = format!("{pre}{}{ch}{post}", "c".repeat(8));
            let off = (pre.len() + 8) as u64 + n;
            fail(
                &format!("TextLength {pre} {n}B"),
                d.as_bytes(),
                &c,
                LimitKind::TextLength,
                8,
                8 + n,
                off,
            );
        }
    }
}

#[test]
fn structural_and_entity_limits_on_four_paths() {
    let d = ParserConfig::default();
    let deep = |n: usize| format!("{}{}", "<a>".repeat(n), "</a>".repeat(n));
    pass(
        "Depth = 5",
        deep(5).as_bytes(),
        &d.clone().with_max_depth(5),
    );
    let s = four_paths("Depth 6", deep(6).as_bytes(), &d.clone().with_max_depth(5));
    assert_eq!(limit_of("Depth", &s).0, LimitKind::ElementDepth);
    let attrs = |n: usize| {
        format!(
            "<r {}/>",
            (0..n)
                .map(|i| format!("a{i}=\"v\""))
                .collect::<Vec<_>>()
                .join(" ")
        )
    };
    pass(
        "Attributes = 3",
        attrs(3).as_bytes(),
        &d.clone().with_max_attrs(3),
    );
    let s = four_paths(
        "Attributes 4",
        attrs(4).as_bytes(),
        &d.clone().with_max_attrs(3),
    );
    assert_eq!(limit_of("Attributes", &s).0, LimitKind::Attributes);
    let ents = |n: usize| format!("<!DOCTYPE r [<!ENTITY a \"x\">]><r>{}</r>", "&a;".repeat(n));
    pass(
        "EntityExpansions = 5",
        ents(5).as_bytes(),
        &d.clone().with_max_entity_expansions(5),
    );
    let s = four_paths(
        "EntityExpansions 6",
        ents(6).as_bytes(),
        &d.clone().with_max_entity_expansions(5),
    );
    assert_eq!(
        limit_of("EntityExpansions", &s).0,
        LimitKind::EntityExpansions
    );
    let chain = "<!DOCTYPE r [<!ENTITY a \"&b;\"><!ENTITY b \"&c;\"><!ENTITY c \"x\">]><r>&a;</r>";
    pass(
        "EntityDepth = 3",
        chain.as_bytes(),
        &d.clone().with_max_entity_depth(3),
    );
    let s = four_paths(
        "EntityDepth 2",
        chain.as_bytes(),
        &d.clone().with_max_entity_depth(2),
    );
    assert_eq!(limit_of("EntityDepth", &s).0, LimitKind::EntityDepth);
    let big = |n: usize| format!("<!DOCTYPE r [<!ENTITY a \"{}\">]><r>&a;</r>", "x".repeat(n));
    pass(
        "ExpandedBytes = 100",
        big(100).as_bytes(),
        &d.clone().with_max_expanded_bytes(100),
    );
    let s = four_paths(
        "ExpandedBytes 101",
        big(101).as_bytes(),
        &d.clone().with_max_expanded_bytes(100),
    );
    assert_eq!(limit_of("ExpandedBytes", &s).0, LimitKind::ExpandedBytes);
    // 比例：64 KiB 地板之上
    let ratio = format!(
        "<!DOCTYPE r [<!ENTITY a \"{}\">]><r>{}</r>",
        "x".repeat(8192),
        "&a;".repeat(20)
    );
    let s = four_paths(
        "ExpansionRatio",
        ratio.as_bytes(),
        &d.clone().with_max_expansion_ratio(2),
    );
    assert_eq!(limit_of("ExpansionRatio", &s).0, LimitKind::ExpansionRatio);
    // 实体展开中途越界的长度类限额：position 是最外层引用的 '&'，各路径相同
    let ent_text = "<!DOCTYPE r [<!ENTITY a \"0123456789\">]><r>xx&a;</r>";
    let s = four_paths(
        "TextLength 实体中途",
        ent_text.as_bytes(),
        &d.clone().with_max_text_len(8),
    );
    assert_eq!(limit_of("TextLength 实体中途", &s).0, LimitKind::TextLength);
}

/// D21 的 6 个测试用的文档，在四条路径上重放，结果与 from_bytes 完全相同。
/// 默认套件：同样 11 份文档、同样断言，流式路径为逐字节 + 2 种随机切块。
#[test]
fn d21_documents_replay_on_sampled_paths() {
    d21_replay(2);
}

/// 完整版：逐字节 + 16 种随机切块（debug 约 20 s）。
#[test]
#[ignore = "slow: d21 大文档 × 17 条流式路径（debug 约 20 s），需要跑 --ignored"]
fn d21_documents_replay_on_all_paths() {
    d21_replay(16);
}

fn d21_replay(schedules: u64) {
    let d = ParserConfig::default();
    let attlist = |n: usize, name: &dyn Fn(usize) -> String, v: &str| {
        let mut s = String::from("<!DOCTYPE r [");
        for i in 0..n {
            s.push_str(&format!("<!ATTLIST e {} CDATA \"{v}\">", name(i)));
        }
        s.push_str("]>");
        s
    };
    let cases: Vec<(&str, String, ParserConfig, bool)> = vec![
        (
            "d21_default_attribute_amplification_bounded",
            format!(
                "{}<r>{}</r>",
                attlist(1024, &|i| format!("attribute_name_{i:04}"), "v"),
                "<e/>".repeat(50_000)
            ),
            d.clone(),
            false,
        ),
        (
            "d21_name_bytes_variants (1 KiB 名字)",
            format!(
                "{}<r>{}</r>",
                attlist(1024, &|i| format!("n{i:04}{}", "x".repeat(1019)), ""),
                "<e/>".repeat(20_000)
            ),
            d.clone(),
            false,
        ),
        (
            "d21_name_bytes_variants (空值)",
            format!(
                "{}<r>{}</r>",
                attlist(1024, &|i| format!("attribute_name_{i:04}"), ""),
                "<e/>".repeat(50_000)
            ),
            d.clone(),
            false,
        ),
        (
            "d21_ratio_floor (1000×65)",
            format!(
                "<!DOCTYPE r [<!ATTLIST e n{} CDATA \"\">]><r>{}</r>",
                "x".repeat(999),
                "<e/>".repeat(65)
            ),
            d.clone(),
            true,
        ),
        (
            "d21_ratio_floor (1024×64)",
            format!(
                "<!DOCTYPE r [<!ATTLIST e n{} CDATA \"\">]><r>{}</r>",
                "x".repeat(1023),
                "<e/>".repeat(64)
            ),
            d.clone(),
            true,
        ),
        (
            "d21_ratio_floor (1000×66)",
            format!(
                "<!DOCTYPE r [<!ATTLIST e n{} CDATA \"\">]><r>{}</r>",
                "x".repeat(999),
                "<e/>".repeat(66)
            ),
            d.clone(),
            false,
        ),
        (
            "d21_shared_large_default",
            format!(
                "<!DOCTYPE r [<!ATTLIST e a CDATA \"{}\">]><r>{}</r>",
                "v".repeat(1 << 20),
                "<e/>".repeat(10_000)
            ),
            d.clone(),
            false,
        ),
        (
            "d21_ratio_zero (600×100)",
            format!(
                "<!DOCTYPE r [<!ATTLIST e a CDATA \"{}\">]><r>{}</r>",
                "v".repeat(100),
                "<e/>".repeat(600)
            ),
            d.clone().with_max_expansion_ratio(0),
            true,
        ),
        (
            "d21_ratio_zero (649×100)",
            format!(
                "<!DOCTYPE r [<!ATTLIST e a CDATA \"{}\">]><r>{}</r>",
                "v".repeat(100),
                "<e/>".repeat(649)
            ),
            d.clone().with_max_expansion_ratio(0),
            false,
        ),
        (
            "d21_ratio_max (2000×1 MiB)",
            format!(
                "<!DOCTYPE r [<!ATTLIST e a CDATA \"{}\">]><r>{}</r>",
                "v".repeat(1 << 20),
                "<e/>".repeat(2000)
            ),
            d.clone().with_max_expansion_ratio(u64::MAX),
            true,
        ),
        (
            "d21_default_injection_not_subject_to_absolute_cap",
            format!(
                "<!DOCTYPE r [<!ATTLIST e a CDATA \"{}\">]><r>{}</r>",
                "v".repeat(100),
                format!("<e>{}</e>", "t".repeat(100)).repeat(20_000)
            ),
            d.clone(),
            true,
        ),
    ];
    for (label, doc, c, ok) in cases {
        let s = four_paths_counted(label, doc.as_bytes(), &c, schedules);
        if ok {
            assert_eq!(s.1, Ok(()), "{label}：应通过");
        } else {
            assert!(
                matches!(
                    s.1,
                    Err((
                        ErrorKind::SecurityLimit {
                            kind: LimitKind::ExpansionRatio,
                            ..
                        },
                        _
                    ))
                ),
                "{label}：应报 ExpansionRatio，实际 {:?}",
                s.1
            );
        }
    }
}

/// four_paths 的计数版（大文档不保存事件文本，只比较事件数和结尾）。
fn four_paths_counted(label: &str, d: &[u8], c: &ParserConfig, schedules: u64) -> (usize, End) {
    let mut r = Reader::from_bytes(d).with_config(c.clone());
    let base = count!(r);
    let mut r = Reader::from_vec(d.to_vec()).with_config(c.clone());
    assert_eq!(base, count!(r), "{label}：from_vec");
    let mut plans = vec![Plan::Byte];
    plans.extend((0..schedules).map(|s| Plan::Schedule(seeded_schedule(s + 2000))));
    for plan in plans {
        let mut r = Reader::from_reader_with_config(Feed::new(d, plan.clone()), c.clone()).unwrap();
        assert_eq!(base, count!(r), "{label}：{plan:?}");
    }
    base
}

// ───────────────────────────── 计划 §1.5 内存上限 ─────────────────────────────

/// GenReader：现场生成 head + item × n + tail，不占内存。
struct GenReader {
    parts: [Vec<u8>; 3],
    reps: u64,
    part: usize,
    rep: u64,
    off: usize,
    pulled: Arc<AtomicUsize>,
}

impl GenReader {
    fn new(head: &str, item: &str, reps: u64, tail: &str) -> Self {
        Self {
            parts: [head.into(), item.into(), tail.into()],
            reps,
            part: 0,
            rep: 0,
            off: 0,
            pulled: Arc::new(AtomicUsize::new(0)),
        }
    }
    fn len(&self) -> u64 {
        self.parts[0].len() as u64
            + self.parts[1].len() as u64 * self.reps
            + self.parts[2].len() as u64
    }
}

impl Read for GenReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let mut n = 0;
        while n < buf.len() && self.part < 3 {
            if self.part == 1 && self.rep >= self.reps {
                self.part = 2;
                self.off = 0;
                continue;
            }
            let src = &self.parts[self.part];
            let k = (src.len() - self.off).min(buf.len() - n);
            buf[n..n + k].copy_from_slice(&src[self.off..self.off + k]);
            n += k;
            self.off += k;
            if self.off == src.len() {
                self.off = 0;
                if self.part == 1 {
                    self.rep += 1;
                } else {
                    self.part += 1;
                }
            }
        }
        self.pulled.fetch_add(n, Ordering::SeqCst);
        Ok(n)
    }
}

impl BufRead for GenReader {
    fn fill_buf(&mut self) -> io::Result<&[u8]> {
        unreachable!("IoInput 只用 read")
    }
    fn consume(&mut self, _: usize) {}
}

const ITEM: &str = "<item a=\"1\" b=\"x\">text &amp; more</item>";

fn peak_for(mib: u64) -> (usize, u64, usize) {
    let reps = (mib << 20) / ITEM.len() as u64;
    match run_guarded(&format!("mem-{mib}"), Duration::from_secs(600), move || {
        let g = GenReader::new("<root>", ITEM, reps, "</root>");
        let len = g.len();
        let pulled = g.pulled.clone();
        let mut r = Reader::from_reader(BufReader::with_capacity(8192, g)).unwrap();
        let (n, end) = count!(r);
        assert_eq!(end, Ok(()), "{mib} MiB");
        assert_eq!(pulled.load(Ordering::SeqCst) as u64, len, "应读完全部输入");
        (n, len, r.buffered_capacity())
    }) {
        Guarded::Done {
            value: (n, len, cap),
            peak_bytes,
            ..
        } => {
            assert_eq!(n as u64, reps * 3 + 2, "{mib} MiB 事件数");
            eprintln!("{mib} MiB（{len} 字节）：峰值 {peak_bytes} 字节，结束时窗口容量 {cap}");
            (peak_bytes, len, cap)
        }
        Guarded::TimedOut => panic!("{mib} MiB 超时"),
        Guarded::Panicked(m) => panic!("{mib} MiB panic {m}"),
    }
}

#[track_caller]
fn assert_flat(sizes: &[u64]) {
    let peaks: Vec<usize> = sizes.iter().map(|&m| peak_for(m).0).collect();
    for (m, p) in sizes.iter().zip(&peaks) {
        assert!(*p <= 4 << 20, "{m} MiB：峰值 {p} 字节 > 4 MiB");
    }
    let (lo, hi) = (*peaks.iter().min().unwrap(), *peaks.iter().max().unwrap());
    assert!(
        hi - lo <= 64 << 10,
        "各档峰值相差 {} 字节 > 64 KiB：{peaks:?}",
        hi - lo
    );
}

#[cfg(debug_assertions)]
#[test]
fn memory_is_flat_in_document_size_debug() {
    assert_flat(&[4, 16]);
}

#[cfg(not(debug_assertions))]
#[test]
fn memory_is_flat_in_document_size_16_64_256_mib() {
    assert_flat(&[16, 64, 256]);
}

#[test]
fn buffer_shrinks_after_a_large_token() {
    // 4 MiB 文本之后跟 8 MiB 小 token：结束时窗口容量回落
    let g = GenReader::new(
        &format!("<r>{}", "t".repeat(4 << 20)),
        "<e/>",
        2 << 20,
        "</r>",
    );
    let mut r = Reader::from_reader(g).unwrap();
    let (_, end) = count!(r);
    assert_eq!(end, Ok(()));
    let cap = r.buffered_capacity();
    assert!(cap <= 1 << 20, "大 token 之后窗口容量没有收缩：{cap}");
}

#[test]
fn one_gib_unterminated_comment_stops_early() {
    // 1 GiB 不闭合注释（生成式，不进内存）：报 TextLength（16 MiB + 1），读入 ≤ 16 MiB + 64 KiB
    let reps = (1u64 << 30) / 1024;
    let comment_item = "c".repeat(1024);
    match run_guarded("1gib-comment", Duration::from_secs(120), move || {
        let g = GenReader::new("<r><!--", &comment_item, reps, "");
        let pulled = g.pulled.clone();
        let mut r = Reader::from_reader(g).unwrap();
        let (_, end) = count!(r);
        (end, pulled.load(Ordering::SeqCst))
    }) {
        Guarded::Done {
            value: (end, pulled),
            peak_bytes,
            elapsed,
        } => {
            let max = purexml::DEFAULT_MAX_TEXT_LEN;
            match end {
                Err((
                    ErrorKind::SecurityLimit {
                        kind: LimitKind::TextLength,
                        limit,
                        actual,
                    },
                    p,
                )) => {
                    assert_eq!((limit, actual), (max, max + 1));
                    assert_eq!(p.offset, 7 + max + 1, "position 应在越界字符之后");
                }
                other => panic!("应报 TextLength，实际 {other:?}"),
            }
            assert!(
                pulled as u64 <= max + 7 + (64 << 10),
                "读入 {pulled} 字节，超过 16 MiB + 64 KiB"
            );
            assert!(peak_bytes <= (33 << 20), "峰值 {peak_bytes}");
            eprintln!("1 GiB 不闭合注释：读入 {pulled} 字节，峰值 {peak_bytes}，用时 {elapsed:?}");
        }
        Guarded::TimedOut => panic!("超时"),
        Guarded::Panicked(m) => panic!("panic {m}"),
    }
}

#[test]
fn overlong_token_stops_within_one_chunk() {
    // 超长 token：报错前读入的字节 ≤ token 起点 + 上限 + 一个块（READ_CHUNK 8 KiB + 源块）
    let m = 1u64 << 20;
    let c = ParserConfig::default().with_max_token_len(m);
    for (label, head, item) in [
        ("属性值", "<r a=\"", "&amp;"),
        ("文本", "<r>&amp;", "tttt"),
        ("注释", "<r><!--", "cccc"),
        ("开始标签", "<r ", "    "),
    ] {
        let g = GenReader::new(head, item, 1 << 22, "");
        let pulled = g.pulled.clone();
        let mut r = Reader::from_reader_with_config(
            g,
            c.clone()
                .with_max_text_len(u64::MAX >> 1)
                .with_max_attrs(usize::MAX),
        )
        .unwrap();
        let (_, end) = count!(r);
        let got = pulled.load(Ordering::SeqCst) as u64;
        assert!(
            matches!(
                end,
                Err((
                    ErrorKind::SecurityLimit {
                        kind: LimitKind::TokenLength,
                        ..
                    },
                    _
                ))
            ) || matches!(end, Err((ErrorKind::SecurityLimit { .. }, _))),
            "{label}：{end:?}"
        );
        assert!(
            got <= m + 3 + (16 << 10),
            "{label}：读入 {got} 字节，超过上限 + 一个块"
        );
    }
}

// ───────────────────────────── 线性复杂度（release） ─────────────────────────────

#[cfg(not(debug_assertions))]
#[test]
fn byte_by_byte_reading_is_linear() {
    use std::time::Instant;
    let time = |d: &[u8], plan: Plan| {
        let mut best = Duration::MAX;
        for _ in 0..3 {
            let t = Instant::now();
            let mut r = Reader::from_reader(Feed::new(d, plan.clone())).unwrap();
            let (_, end) = count!(r);
            assert_eq!(end, Ok(()));
            best = best.min(t.elapsed());
        }
        best
    };
    let t4 = time(
        format!("<r>{}</r>", "x".repeat(4 << 20)).as_bytes(),
        Plan::Byte,
    );
    let t8 = time(
        format!("<r>{}</r>", "x".repeat(8 << 20)).as_bytes(),
        Plan::Byte,
    );
    eprintln!(
        "8 MiB 文本逐字节 {t8:?}，4 MiB {t4:?}，比 {:.2}",
        t8.as_secs_f64() / t4.as_secs_f64()
    );
    assert!(
        t8.as_secs_f64() <= t4.as_secs_f64() * 2.5,
        "8 MiB/4 MiB = {t8:?}/{t4:?}"
    );
    // 同一个 1 MiB 文档：逐字节 ≤ 64 KiB 块的 20 倍
    let doc = format!("<r>{}</r>", ITEM.repeat((1 << 20) / ITEM.len()));
    let tb = time(doc.as_bytes(), Plan::Byte);
    let tc = time(doc.as_bytes(), Plan::Schedule(vec![64 << 10]));
    eprintln!(
        "1 MiB 文档逐字节 {tb:?}，64 KiB 块 {tc:?}，比 {:.2}",
        tb.as_secs_f64() / tc.as_secs_f64()
    );
    assert!(
        tb.as_secs_f64() <= tc.as_secs_f64() * 20.0,
        "逐字节 {tb:?} vs 64 KiB {tc:?}"
    );
    // 1 个大 token 和很多小 token：时间随大小线性
    let t1 = time(
        format!("<r>{}</r>", "<e/>".repeat(1 << 18)).as_bytes(),
        Plan::Schedule(vec![7]),
    );
    let t2 = time(
        format!("<r>{}</r>", "<e/>".repeat(1 << 19)).as_bytes(),
        Plan::Schedule(vec![7]),
    );
    eprintln!(
        "小 token 2 MiB {t2:?}，1 MiB {t1:?}，比 {:.2}",
        t2.as_secs_f64() / t1.as_secs_f64()
    );
    assert!(t2.as_secs_f64() <= t1.as_secs_f64() * 2.5);
}

// ───────────────────────────── 已知问题（PR-B 回归发现） ─────────────────────────────

/// D27：空实体展开之后的下一个 token，from_bytes 没有按 max_token_len 定界。
#[test]
fn d27_token_after_empty_expansion_is_still_framed() {
    let c = ParserConfig::default().with_max_token_len(64);
    let v = "&amp;".repeat(10);
    let d = format!("<!DOCTYPE r [<!ENTITY e \"\">]><r>&e;<a x=\"{v}\" y=\"{v}\"/></r>");
    let s = four_paths("D27", d.as_bytes(), &c);
    assert_eq!(limit_of("D27", &s).0, LimitKind::TokenLength);
}

/// D28：小 token 流里窗口没有压缩（< 64 KiB 不压缩），max_token_len 较小时误报 TokenLength。
#[test]
fn d28_small_tokens_do_not_hit_a_small_token_cap() {
    let c = ParserConfig::default().with_max_token_len(4096);
    let d = format!("<r>{}<!--{}--></r>", "<x/>".repeat(5000), "c".repeat(3990));
    let s = four_paths("D28", d.as_bytes(), &c);
    assert_eq!(s.end, Ok(()), "所有 token 都 < 4096 字节，不应报错");
}

/// D28 的默认上限版本：前面有 < 64 KiB 的已消费内容时，接近 32 MiB 的 token 被误报。
#[cfg(not(debug_assertions))]
#[test]
fn d28_default_cap_token_near_the_limit() {
    let m = purexml::DEFAULT_MAX_TOKEN_LEN as usize;
    let d = format!(
        "<r>{}<!--{}--></r>",
        "<x/>".repeat(10_000),
        "c".repeat(m - (16 << 10))
    );
    let c = ParserConfig::default().with_max_text_len(u64::MAX >> 1);
    let mut r = Reader::from_reader_with_config(
        Feed::new(d.as_bytes(), Plan::Schedule(vec![8192])),
        c.clone(),
    )
    .unwrap();
    let streamed = count!(r);
    let mut r = Reader::from_bytes(d.as_bytes()).with_config(c);
    assert_eq!(streamed, count!(r), "默认上限下接近 32 MiB 的 token");
}

/// D34（R2c）：带前缀的 QName 按整个 QName 逐字符计 NameLength（L7），
/// 一越过上限就报错：actual = 首次超过的长度，position = 越界字符之后；四条路径一致。
/// 6894e02 上 from_bytes 分别计前缀和本地名、只在末尾检查总长（报得更晚、actual 偏大）。
#[test]
fn d34_prefixed_qname_counts_the_whole_name() {
    let c6 = ParserConfig::default().with_max_name_len(6);
    let c8 = ParserConfig::default().with_max_name_len(8);
    fail(
        "D34 xmlns:abcdefgh",
        b"<d xmlns:abcdefgh=\"u\"/>",
        &c6,
        LimitKind::NameLength,
        6,
        7,
        10,
    );
    fail(
        "D34 xmlns:ab",
        b"<d xmlns:ab=\"u\" ab:cdefghij=\"v\"/>",
        &c6,
        LimitKind::NameLength,
        6,
        7,
        10,
    );
    fail(
        "D34 属性 ab:cdefghij",
        b"<d xmlns:ab=\"u\" ab:cdefghij=\"v\"/>",
        &c8,
        LimitKind::NameLength,
        8,
        9,
        25,
    );
    fail(
        "D34 元素 abc:defghij",
        b"<abc:defghij xmlns:abc=\"u\"/>",
        &c8,
        LimitKind::NameLength,
        8,
        9,
        10,
    );
    // 元素名 a:bcdefg 刚好 8 字符，属性名 xmlns:a 7 字符：通过。
    pass("D34 a:bcdefg = 8", b"<a:bcdefg xmlns:a=\"u\"/>", &c8);
    // 元素名 abc:defg 是 8 字符，但属性名 xmlns:abc 是 9 字符：按整个 QName 计长，
    // 在属性名的第 9 个字符之后报 NameLength 8/9 @19。
    fail(
        "D34 abc:defg 的属性名 xmlns:abc = 9",
        b"<abc:defg xmlns:abc=\"u\"/>",
        &c8,
        LimitKind::NameLength,
        8,
        9,
        19,
    );
    let (p, l) = ("p".repeat(600), "l".repeat(600));
    let d = format!("<{p}:{l} xmlns:{p}=\"u\"/>");
    fail(
        "D34 默认配置 600:600",
        d.as_bytes(),
        &ParserConfig::default(),
        LimitKind::NameLength,
        1024,
        1025,
        1026,
    );
}

/// D37（R2c，main 上即已存在）：文本里的 `>` 也计入 TextLength。
/// 6894e02/main 的 from_bytes 文本快路径遇到 `>` 直接 continue，跳过长度检查，
/// 一串 `>` 可以绕过 max_text_len。
#[test]
fn d37_gt_in_text_counts_toward_text_length() {
    let c = ParserConfig::default().with_max_text_len(12);
    let run = format!("<d>{}</d>", ">".repeat(30));
    fail(
        "D37 `>`×30",
        run.as_bytes(),
        &c,
        LimitKind::TextLength,
        12,
        13,
        16,
    );
    fail(
        "D37 第 13 字节是 `>`",
        b"<d>abcdefghijkl></d>",
        &c,
        LimitKind::TextLength,
        12,
        13,
        16,
    );
    fail(
        "D37 `>` 后还有文本",
        b"<d>abcdefghijkl>mnop</d>",
        &c,
        LimitKind::TextLength,
        12,
        13,
        16,
    );
    fail(
        "D37 `]>`",
        b"<d>abcdefghijk]></d>",
        &c,
        LimitKind::TextLength,
        12,
        13,
        16,
    );
    fail(
        "D37 EOF 前",
        b"<d>abcdefghijkl>",
        &c,
        LimitKind::TextLength,
        12,
        13,
        16,
    );
    pass("D37 = 12", b"<d>abcdefghijk></d>", &c);
    let big = format!("<d>{}</d>", ">".repeat(5000));
    fail(
        "D37 text=1000 `>`×5000",
        big.as_bytes(),
        &ParserConfig::default().with_max_text_len(1000),
        LimitKind::TextLength,
        1000,
        1001,
        1004,
    );
}

/// D36（Lead 13:56：只设部分限额时不得误报）：只设名字或文本上限、没设 max_token_len 时，
/// 任何路径都不能报 TokenLength；按 L7 和字符口径写死期望（`:` 是 QName 的第 7 个字符）。
#[test]
fn d36_no_false_token_length_from_name_or_text_budget() {
    let n6 = ParserConfig::default().with_max_name_len(6);
    let t12 = ParserConfig::default().with_max_text_len(12);
    type Case<'a> = (&'a [u8], &'a ParserConfig, LimitKind, u64, u64, u64);
    let cases: [Case; 5] = [
        (b"<abcdef:", &n6, LimitKind::NameLength, 6, 7, 8),
        (b"<dxmlns:a", &n6, LimitKind::NameLength, 6, 7, 8),
        (b"<d>abcdefghijkl>", &t12, LimitKind::TextLength, 12, 13, 16),
        (
            b"<d>abcdefghijkl></d>",
            &t12,
            LimitKind::TextLength,
            12,
            13,
            16,
        ),
        (b"<d>wwwwwwr svo?>", &t12, LimitKind::TextLength, 12, 13, 16),
    ];
    for (d, c, kind, limit, actual, offset) in cases {
        let label = format!("D36 {:?}", String::from_utf8_lossy(d));
        all_chunkings(&label, d, c);
        fail(&label, d, c, kind, limit, actual, offset);
    }
}

/// D35（Lead 13:56：报 TokenLength，不解码越界字节）：max_token_len 的那个字节是任何
/// 非法或不完整的 UTF-8 首字节时，注释、CDATA、PI、文本、属性值里都报 TokenLength 48/49，
/// position = 越界的那个字节。
#[test]
fn d35_utf8_lead_at_token_cap_agrees_on_all_paths() {
    let c = ParserConfig::default().with_max_token_len(48);
    for (ctx, at) in d35_contexts() {
        for bad in d35_bad_bytes() {
            let d = d35_doc(ctx, at, bad);
            let label = format!("D35 {ctx} {bad:02X?}");
            all_chunkings(&label, &d, &c);
            fail(&label, &d, &c, LimitKind::TokenLength, 48, 49, at as u64);
        }
    }
}

/// (前缀, 越界字节的 offset)：token 起点是 3（`<r>` 之后）或 0（开始标签）。
fn d35_contexts() -> [(&'static str, usize); 5] {
    [
        ("<r><!--", 51),
        ("<r><![CDATA[", 51),
        ("<r><?p ", 51),
        ("<r>", 51),
        ("<r a=\"", 48),
    ]
}

fn d35_bad_bytes() -> [&'static [u8]; 4] {
    [&[0xE4, 0xB8], &[0xC0, 0x80], &[0xFF], &[0x80]]
}

fn d35_doc(ctx: &str, at: usize, bad: &[u8]) -> Vec<u8> {
    let mut d = ctx.as_bytes().to_vec();
    d.extend(std::iter::repeat(b'a').take(at - ctx.len()));
    d.extend_from_slice(bad);
    d.extend_from_slice(b"zzzz</r>");
    d
}

// ───────────── R2c（4d7ebe2：限额检查改为每个 token 预算一次截止索引）─────────────

/// 扫描用的轻量四路径：from_bytes、from_vec、逐字节、块长 7、3 种随机切块，结果必须相同。
fn quick_paths(label: &str, d: &[u8], c: &ParserConfig) -> Snap {
    let base = bytes(d, c);
    assert_eq!(base, vec_path(d, c), "{label}：from_vec 与 from_bytes 不同");
    let mut plans = vec![
        ("逐字节".to_string(), Plan::Byte),
        ("块长 7".to_string(), Plan::Schedule(vec![7])),
    ];
    for seed in 0..3 {
        plans.push((
            format!("随机 {seed}"),
            Plan::Schedule(seeded_schedule(seed + 2000)),
        ));
    }
    for (name, plan) in plans {
        assert_same(
            &format!("{label} [{name}]"),
            &base,
            &stream(d, c, plan, None).snap,
        );
    }
    base
}

fn err_offset(s: &Snap) -> u64 {
    match &s.end {
        Err((_, p)) => p.offset,
        Ok(()) => u64::MAX,
    }
}

/// 判定一个限额错误“需要读到的那个字节”（Lead 13:56 L0）：
/// - 按字节计的限额：position 就是需要的那个字节；
/// - 按字符计的限额：position 在越界字符之后，需要的是前一个字节；
///   但如果越界字符来自一般实体引用 `&name;` 的展开，position 报在引用起始的 `&`，
///   而判定要读到引用末尾的 `;`，需要的字节是 `;` 的偏移。
///   `d[offset] == '&'` 时也可能是 `&` 前一个普通字符越界（position 同样落在 `&`），
///   所以不能只看 `&`：用只含前缀 `d[..offset]` 的对照解析区分——前一个字符越界时，
///   前缀解析在同一位置报同一个限额（kind、limit、actual 都相同）；越界来自展开时，
///   前缀里没有这个引用，不会在该位置报该限额。两者都不像时直接失败，不退化成“二者之一”。
///   字符引用 `&#..;` 的 position 在 `;` 之后，仍是 offset - 1。
/// - 实体类限额（ExpandedBytes 等）的检测字节和 position 不同（O-1），返回 None。
fn needed_byte(label: &str, d: &[u8], c: &ParserConfig, s: &Snap) -> Option<u64> {
    let Err((
        ErrorKind::SecurityLimit {
            kind,
            limit,
            actual,
        },
        p,
    )) = &s.end
    else {
        return None;
    };
    let offset = p.offset;
    match kind {
        LimitKind::TotalInput | LimitKind::TokenLength | LimitKind::InternalSubsetLength => {
            Some(offset)
        }
        LimitKind::TextLength | LimitKind::AttrValueLength | LimitKind::NameLength => {
            let o = offset as usize;
            let general_ref = d.get(o) == Some(&b'&') && d.get(o + 1) != Some(&b'#');
            if !general_ref {
                return Some(offset.saturating_sub(1));
            }
            let semi = o + d[o..]
                .iter()
                .position(|&b| b == b';')
                .unwrap_or_else(|| panic!("{label}：offset {o} 处的 `&` 之后没有 `;`"));
            let prefix = bytes(&d[..o], c);
            match &prefix.end {
                Err((
                    ErrorKind::SecurityLimit {
                        kind: k,
                        limit: l,
                        actual: a,
                    },
                    q,
                )) if k == kind && q.offset == offset => {
                    assert_eq!(
                        (l, a),
                        (limit, actual),
                        "{label}：前缀对照在同一位置报 {k:?}，但 limit/actual 不同，无法判定"
                    );
                    Some(offset - 1)
                }
                Err((ErrorKind::SecurityLimit { kind: k, .. }, q))
                    if q.offset >= offset && !(k == kind && q.offset == offset) =>
                {
                    panic!(
                        "{label}：前缀对照在 offset {} 报 {k:?}，越过了截断点 {offset}，无法判定",
                        q.offset
                    )
                }
                _ => Some(semi as u64),
            }
        }
        _ => None,
    }
}

/// `needed_byte` 本身的对照：`&` 处报的按字符计限额，区分“越界字符来自展开”（需要 `;`）
/// 和“`&` 前一个普通字符越界”（需要 `&` 前一个字节）；字符引用仍是 offset - 1。
#[test]
fn needed_byte_tells_expansion_from_plain_char() {
    let e = "<!DOCTYPE r [<!ENTITY a \"xy\">]>";
    let cases: [(String, u64, LimitKind, u64, u64, u64); 9] = [
        // (文档, max_text_len, kind, actual, position, 需要的字节)
        (
            format!("{e}<r>abc&a;</r>"),
            3,
            LimitKind::TextLength,
            4,
            37,
            39,
        ), // 展开的 x
        (
            format!("{e}<r>abc&a;</r>"),
            4,
            LimitKind::TextLength,
            5,
            37,
            39,
        ), // 展开的 y
        (
            format!("{e}<r>abc&a;</r>"),
            2,
            LimitKind::TextLength,
            3,
            37,
            36,
        ), // & 前的 c
        (
            "<!DOCTYPE r [<!ENTITY a \"<q>xyz</q>\">]><r>abc&a;</r>".to_string(),
            2,
            LimitKind::TextLength,
            3,
            45,
            44,
        ),
        (
            "<!DOCTYPE r [<!ENTITY a \"xy\"><!ENTITY b \"&a;&a;\">]><r>a&b;</r>".to_string(),
            4,
            LimitKind::TextLength,
            5,
            55,
            57, // 嵌套展开：需要外层引用的 `;`
        ),
        (
            "<r>ab&#99;d</r>".to_string(),
            2,
            LimitKind::TextLength,
            3,
            10,
            9,
        ),
        (
            "<r>ab&#99;d</r>".to_string(),
            1,
            LimitKind::TextLength,
            2,
            5,
            4,
        ),
        (
            format!("{e}<r t=\"abc&a;\"/>"),
            3,
            LimitKind::AttrValueLength,
            4,
            40,
            42,
        ),
        (
            format!("{e}<r t=\"abc&a;\"/>"),
            2,
            LimitKind::AttrValueLength,
            3,
            40,
            39,
        ),
    ];
    for (doc, max, kind, actual, pos, need) in cases {
        let c = ParserConfig::default().with_max_text_len(max);
        let s = quick_paths(&format!("{doc:?} max={max}"), doc.as_bytes(), &c);
        let (k, l, a, p) = limit_of(&doc, &s);
        assert_eq!(
            (k, l, a, p.offset),
            (kind, max, actual, pos),
            "{doc:?} max={max}"
        );
        assert_eq!(
            needed_byte(&doc, doc.as_bytes(), &c, &s),
            Some(need),
            "{doc:?} max={max}"
        );
    }
}

/// 同一字节的优先级（Lead 13:56）：TotalInput > TokenLength > 按字符计的限额。
fn priority(kind: LimitKind) -> u8 {
    match kind {
        LimitKind::TotalInput => 0,
        LimitKind::TokenLength => 1,
        LimitKind::TextLength | LimitKind::AttrValueLength | LimitKind::NameLength => 2,
        _ => 1,
    }
}

fn limit_end(s: &Snap) -> Option<(LimitKind, u64)> {
    match &s.end {
        Err((ErrorKind::SecurityLimit { kind, .. }, p)) => Some((*kind, p.offset)),
        _ => None,
    }
}

/// 两个限额同时设置时的期望（Lead 13:56 统一原则）：
/// - 只有一边报了限额：结果就是那一边（另一边的 Syntax/Encoding 必然要读到更后面的字节）；
/// - 两边都报限额：需要的字节更靠前的赢；同一字节按 TotalInput > TokenLength > 按字符计；
///   InternalSubsetLength 与 TokenLength 同字节、以及实体类限额（O-1 另有用例）只要求是二者之一；
/// - 两边都没报限额：两边相同，结果也相同。
///
/// 返回“同一字节、按优先级判定”的次数。
fn earlier_wins(
    label: &str,
    d: &[u8],
    a: &ParserConfig,
    b: &ParserConfig,
    both: &ParserConfig,
) -> usize {
    let sa = quick_paths(&format!("{label} 只设 A"), d, a);
    let sb = quick_paths(&format!("{label} 只设 B"), d, b);
    let s = quick_paths(&format!("{label} 都设"), d, both);
    check_limit_shape(label, &s, both);
    match (limit_end(&sa), limit_end(&sb)) {
        (None, None) => {
            assert_eq!(sa, sb, "{label}：两边都没报限额，结果应相同");
            assert_eq!(s, sa, "{label}");
        }
        (Some(_), None) => assert_eq!(s, sa, "{label}：只有 A 报限额"),
        (None, Some(_)) => assert_eq!(s, sb, "{label}：只有 B 报限额"),
        (Some((ka, _)), Some((kb, _))) => match (
            needed_byte(&format!("{label} 只设 A"), d, a, &sa),
            needed_byte(&format!("{label} 只设 B"), d, b, &sb),
        ) {
            (Some(na), Some(nb)) if na != nb => {
                let want = if na < nb { &sa } else { &sb };
                assert_eq!(&s, want, "{label}：需要的字节 A {na} / B {nb}，先到的赢");
            }
            (Some(_), Some(_)) if priority(ka) != priority(kb) => {
                let want = if priority(ka) < priority(kb) {
                    &sa
                } else {
                    &sb
                };
                assert_eq!(
                    &s, want,
                    "{label}：同一字节，按优先级 TotalInput > TokenLength > 按字符计"
                );
                return 1;
            }
            _ => assert!(
                s == sa || s == sb,
                "{label}：结果应是二者之一，实际 {:?}",
                s.end
            ),
        },
    }
    0
}

fn limit_docs() -> Vec<String> {
    let mut v: Vec<String> = probe_cut_docs()
        .into_iter()
        .map(|(d, _)| d.to_string())
        .collect();
    v.push(format!("<r>{}</r>", "a".repeat(40)));
    v.push(format!(
        "<r a=\"{}\" b='{}'/>",
        "v".repeat(20),
        "w".repeat(20)
    ));
    v.push(format!(
        "<r><!--{}--><?p {}?><![CDATA[{}]]></r>",
        "c".repeat(12),
        "d".repeat(12),
        "e".repeat(12)
    ));
    v.push(
        "<r>\u{e9}\u{20ac}\u{1f600}\u{e9}\u{20ac}\u{1f600}\u{e9}\u{20ac}\u{1f600}</r>".to_string(),
    );
    v
}

/// max_token_len 和 max_total_input 谁先到、两者恰好相等：t、u = 1..=48 全组合。
/// 先到的赢；落在同一字节时报 TotalInput（Lead 13:56 裁定 1）。
///
/// 默认套件只跑 |t-u| <= 3 的对角带加 t、u 取 1 或 48 的边；全组合在 #[ignore] 版里。
#[test]
fn token_vs_total_input_earlier_wins_near_ties() {
    token_vs_total(|t, u| t.abs_diff(u) <= 3 || t == 1 || u == 1 || t == 48 || u == 48);
}

#[test]
#[ignore = "slow: t、u 全组合 2304 × 22 份文档（debug 约 10 s），需要跑 --ignored"]
fn token_vs_total_input_earlier_wins() {
    token_vs_total(|_, _| true);
}

fn token_vs_total(keep: fn(u64, u64) -> bool) {
    let d = ParserConfig::default();
    let mut ties = 0;
    for doc in limit_docs() {
        let b = doc.as_bytes();
        for t in 1..=48u64 {
            for u in (1..=48u64).filter(|&u| keep(t, u)) {
                let label = format!("tok={t} tot={u} {doc:?}");
                ties += earlier_wins(
                    &label,
                    b,
                    &d.clone().with_max_token_len(t),
                    &d.clone().with_max_total_input(u),
                    &d.clone().with_max_token_len(t).with_max_total_input(u),
                );
            }
        }
    }
    println!("token_vs_total_input_earlier_wins：同位置 {ties} 次");
}

/// 内部子集和实体展开的限额与 token/total 上限组合：同样要求“更早的那个赢”、四条路径一致。
///
/// 默认套件：v 步长 10、t 步长 6；完整步长（5、3）在 #[ignore] 版里。
#[test]
fn subset_and_entity_limits_with_token_and_total_sampled() {
    subset_and_entity(10, 6);
}

#[test]
#[ignore = "slow: v 步长 5 × t 步长 3 全组合（debug 约 7 s），需要跑 --ignored"]
fn subset_and_entity_limits_with_token_and_total() {
    subset_and_entity(5, 3);
}

fn subset_and_entity(v_step: usize, t_step: usize) {
    let d = ParserConfig::default();
    let docs = [
        "<!DOCTYPE r [<!ENTITY a \"0123456789\"><!ENTITY b \"&a;&a;\"><!-- c --><?p d?>]><r>x&b;y&a;</r>".to_string(),
        format!("<!DOCTYPE r [<!ENTITY a \"{}\">]><r a=\"&a;\">&a;&a;</r>", "x".repeat(30)),
        "<!DOCTYPE r [<!ELEMENT r ANY><!ATTLIST r a CDATA \"dflt\">]><r/>".to_string(),
    ];
    type Mk = fn(u64) -> ParserConfig;
    let others: [(&str, Mk); 4] = [
        ("sub", |v| {
            ParserConfig::default().with_max_internal_subset_len(v)
        }),
        ("exp", |v| {
            ParserConfig::default().with_max_expanded_bytes(v)
        }),
        ("ents", |v| {
            ParserConfig::default().with_max_entity_expansions(v / 8)
        }),
        ("text", |v| ParserConfig::default().with_max_text_len(v)),
    ];
    for doc in &docs {
        let b = doc.as_bytes();
        for (on, mk) in &others {
            for v in (1..=96u64).step_by(v_step) {
                for t in (1..=96u64).step_by(t_step) {
                    for (cn, cap) in [
                        ("tok", d.clone().with_max_token_len(t)),
                        ("tot", d.clone().with_max_total_input(t)),
                    ] {
                        let o = mk(v);
                        let mut both = o.clone();
                        if cn == "tok" {
                            both = both.with_max_token_len(t);
                        } else {
                            both = both.with_max_total_input(t);
                        }
                        earlier_wins(&format!("{on}={v} {cn}={t} {doc:?}"), b, &o, &cap, &both);
                    }
                }
            }
        }
    }
}

/// 限额取极大值（u64::MAX、u64::MAX-1、usize::MAX）不能溢出：结果与默认配置完全相同；
/// 只设一个小上限、另一个取极大值时，结果与只设小上限相同。token 起点不在 0（前面有 1000 字节注释）。
#[test]
fn extreme_limits_do_not_overflow() {
    let d = ParserConfig::default();
    let mut docs = limit_docs();
    docs.push(format!(
        "<!--{}--><r>{}</r>",
        "p".repeat(1000),
        "q".repeat(100)
    ));
    let huge = [u64::MAX, u64::MAX - 1, usize::MAX as u64, u64::MAX / 2 + 1];
    for doc in &docs {
        let b = doc.as_bytes();
        let base = all_chunkings(&format!("默认 {doc:?}"), b, &d);
        for h in huge {
            let all = d
                .clone()
                .with_max_token_len(h)
                .with_max_total_input(h)
                .with_max_internal_subset_len(h)
                .with_max_text_len(h)
                .with_max_expanded_bytes(h)
                .with_max_entity_expansions(h)
                .with_max_expansion_ratio(h)
                .with_max_name_len(usize::MAX)
                .with_max_depth(usize::MAX)
                .with_max_attrs(usize::MAX)
                .with_max_entity_depth(usize::MAX);
            let s = all_chunkings(&format!("全部 {h} {doc:?}"), b, &all);
            assert_eq!(s.end, base.end, "全部 {h} {doc:?}");
            for small in [1u64, 5, 13, 40] {
                let tok = d.clone().with_max_token_len(small);
                let tot = d.clone().with_max_total_input(small);
                let a = quick_paths("只设 tok", b, &tok);
                let s = quick_paths("tok 小 tot 极大", b, &tok.clone().with_max_total_input(h));
                assert_eq!(s, a, "tok={small} tot={h} {doc:?}");
                let a = quick_paths("只设 tot", b, &tot);
                let s = quick_paths("tot 小 tok 极大", b, &tot.clone().with_max_token_len(h));
                assert_eq!(s, a, "tot={small} tok={h} {doc:?}");
            }
        }
        // 0：只要求四条路径一致、形状正确
        for c in [
            d.clone().with_max_token_len(0),
            d.clone().with_max_total_input(0),
            d.clone().with_max_token_len(0).with_max_total_input(0),
        ] {
            let s = all_chunkings(&format!("0 {doc:?}"), b, &c);
            check_limit_shape("0", &s, &c);
        }
    }
}

/// D38（R2c；Lead 13:56：内部子集长度和 token 上限都要及时报）。内部子集从 offset 13 开始。
/// ①类型关键字 `CDATA` 被截在中间：sub=v 报 InternalSubsetLength v/v+1 @13+v；tok=t 报 TokenLength t/t+1 @t。
/// ②上限落在默认值字面量里：InternalSubsetLength 37/38 @50（不能扫完字面量才报）。
#[test]
fn d38_attlist_in_subset_limits_agree() {
    let doc = "<!DOCTYPE r [<!ELEMENT r ANY><!ATTLIST r a CDATA \"d\">]><r/>";
    let d = ParserConfig::default();
    for v in [30u64, 31, 32, 33] {
        let c = d.clone().with_max_internal_subset_len(v);
        all_chunkings(&format!("D38① sub={v}"), doc.as_bytes(), &c);
        fail(
            &format!("D38① sub={v}"),
            doc.as_bytes(),
            &c,
            LimitKind::InternalSubsetLength,
            v,
            v + 1,
            13 + v,
        );
    }
    for t in [44u64, 45, 46] {
        let c = d.clone().with_max_token_len(t);
        all_chunkings(&format!("D38① tok={t}"), doc.as_bytes(), &c);
        fail(
            &format!("D38① tok={t}"),
            doc.as_bytes(),
            &c,
            LimitKind::TokenLength,
            t,
            t + 1,
            t,
        );
    }
    for lit in ["d", "dxxxxx"] {
        let doc = format!("<!DOCTYPE r [<!ELEMENT r ANY><!ATTLIST r a CDATA \"{lit}\">]><r/>");
        fail(
            &format!("D38② {lit}"),
            doc.as_bytes(),
            &d.clone().with_max_internal_subset_len(37),
            LimitKind::InternalSubsetLength,
            37,
            38,
            50,
        );
    }
}

/// O-1（Lead 13:56）：ExpandedBytes 与 TotalInput 按原始输入位置先到先报，位置相同时 TotalInput 优先。
/// `&b;` 占 80..=82，ExpandedBytes 要读完 `;`（82）才能判定，position 报在 `&`（80）。
/// tot ≤ 82：TotalInput @tot；tot ≥ 83：ExpandedBytes 1/6 @80。之前的 2 个事件相同。
#[test]
fn o1_expanded_bytes_vs_total_input() {
    let doc = b"<!DOCTYPE r [<!ENTITY a \"0123456789\"><!ENTITY b \"&a;&a;\"><!-- c --><?p d?>]><r>x&b;y&a;</r>";
    let exp = ParserConfig::default().with_max_expanded_bytes(1);
    let s = all_chunkings("O-1 只设 exp", doc, &exp);
    let (k, l, a, p) = limit_of("O-1 只设 exp", &s);
    assert_eq!((k, l, a, p.offset), (LimitKind::ExpandedBytes, 1, 6, 80));
    for t in [80u64, 81, 82] {
        let c = exp.clone().with_max_total_input(t);
        let got = all_chunkings(&format!("O-1 tot={t}"), doc, &c);
        assert_eq!(got.events, s.events, "O-1 tot={t}：之前的事件");
        fail(
            &format!("O-1 tot={t}"),
            doc,
            &c,
            LimitKind::TotalInput,
            t,
            t + 1,
            t,
        );
    }
    for t in [83u64, 84, 200] {
        let c = exp.clone().with_max_total_input(t);
        let got = all_chunkings(&format!("O-1 tot={t}"), doc, &c);
        assert_eq!(got, s, "O-1 tot={t}：应与只设 exp 相同");
    }
}

/// 三重触发（Lead 13:56 优先级）：`<r>` 之后的文本，第 13 个字节在 offset 15。
/// max_text_len=12（需要 15）、max_token_len=12（越界字节 15）、max_total_input=15 同时落在 15：报 TotalInput。
/// 去掉 tot：TokenLength 12/13 @15（TokenLength > 按字符计）；只设 text：TextLength 12/13 @16；
/// text=12 + tot=16：TextLength 先到（需要的是 15）。
#[test]
fn triple_trigger_on_one_byte_reports_total_input() {
    let doc = b"<r>abcdefghijklmnop</r>";
    let d = ParserConfig::default();
    let all3 = d
        .clone()
        .with_max_text_len(12)
        .with_max_token_len(12)
        .with_max_total_input(15);
    fail("三重", doc, &all3, LimitKind::TotalInput, 15, 16, 15);
    fail(
        "tok+text",
        doc,
        &d.clone().with_max_text_len(12).with_max_token_len(12),
        LimitKind::TokenLength,
        12,
        13,
        15,
    );
    fail(
        "tot+text",
        doc,
        &d.clone().with_max_text_len(12).with_max_total_input(15),
        LimitKind::TotalInput,
        15,
        16,
        15,
    );
    fail(
        "只设 text",
        doc,
        &d.clone().with_max_text_len(12),
        LimitKind::TextLength,
        12,
        13,
        16,
    );
    fail(
        "text + tot=16",
        doc,
        &d.clone().with_max_text_len(12).with_max_total_input(16),
        LimitKind::TextLength,
        12,
        13,
        16,
    );
}

/// 完全不设限额时，同样的输入有另一种合法结果：D33/D35/D36/D38 的文档在默认配置下
/// 报的是 Syntax 或 Encoding（或者通过），不能出现任何 SecurityLimit；四条路径一致。
#[test]
fn default_config_keeps_syntax_and_encoding_outcomes() {
    let d = ParserConfig::default();
    type Want = Option<(&'static str, u64)>;
    let cases: Vec<(Vec<u8>, Want)> = vec![
        (
            b"<?xml version=\"1.0\" encodin=\"x\"?><r/>".to_vec(),
            Some(("Syntax", 20)),
        ),
        (b"<r><!-- a -- b --></r>".to_vec(), Some(("Syntax", 10))),
        (b"<abcdef:".to_vec(), Some(("Syntax", 8))),
        (b"<dxmlns:a".to_vec(), Some(("Syntax", 9))),
        (b"<d>abcdefghijkl>".to_vec(), Some(("Syntax", 16))),
        (b"<d>wwwwwwr svo?>".to_vec(), Some(("Syntax", 16))),
        (
            b"<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?><r/>".to_vec(),
            None,
        ),
        (
            b"<!DOCTYPE r [<!ELEMENT r ANY><!ATTLIST r a CDATA \"d\">]><r/>".to_vec(),
            None,
        ),
        (format!("<d>{}</d>", ">".repeat(30)).into_bytes(), None),
    ];
    let mut all = cases;
    for (ctx, at) in d35_contexts() {
        for bad in d35_bad_bytes() {
            all.push((d35_doc(ctx, at, bad), Some(("Encoding", at as u64))));
        }
    }
    for (doc, want) in all {
        let label = format!("默认 {:?}", String::from_utf8_lossy(&doc));
        let s = all_chunkings(&label, &doc, &d);
        match (want, &s.end) {
            (None, Ok(())) => {}
            (Some((name, off)), Err((k, p))) => {
                assert!(
                    format!("{k:?}").starts_with(name) && p.offset == off,
                    "{label}：应为 {name} @{off}，实际 {k:?} @{}",
                    p.offset
                );
            }
            (w, e) => panic!("{label}：应为 {w:?}，实际 {e:?}"),
        }
    }
}

// ───────────── Encoding 与按字节计限额（L0：Encoding 的“需要的字节”是能证明序列非法的那个字节）─────────────

/// r2c 样本 c12c5a0（reader_1byte，tight 配置）：内部子集字面量里 `F2 BE BE 02`（偏移 63–66），
/// 要读到 `02`（66）才能证明非法。TokenLength 的边界字节不晚于 66，所以按 L0 报 TokenLength 48/49，
/// 结果与把这 4 个字节换成 ASCII 的对照文档相同（kind、limit、actual、offset、事件；line/column
/// 按本文档算），四路径全切块一致。
///
/// 注意：解析器把 `<!DOCTYPE` 前面的序言空白（偏移 17 的 `\n`）算进 DOCTYPE token，起点是 17，
/// 边界字节是 65；若起点按 `<!DOCTYPE`（18）算则是 66。绝对偏移 66 单独放在
/// `c12c5a0_doctype_token_starts_at_markup`（#[ignore]，待 Lead 裁定 token 起点口径）。
#[test]
fn encoding_vs_byte_limit_c12c5a0_reports_token_length() {
    let d: &[u8] = C12C5A0;
    let c = c12c5a0_config();
    let s = all_chunkings("c12c5a0", d, &c);
    let (k, l, a, p) = limit_of("c12c5a0", &s);
    assert_eq!((k, l, a), (LimitKind::TokenLength, 48, 49), "c12c5a0");
    assert!(
        p.offset <= 66,
        "c12c5a0：边界字节 {} 应不晚于证明字节 66",
        p.offset
    );
    let mut ctl = d.to_vec();
    ctl[63..67].fill(b'a');
    let base = all_chunkings("c12c5a0 对照", &ctl, &c);
    assert_eq!(s.events, base.events, "c12c5a0 事件同对照");
    match &base.end {
        Err((ErrorKind::SecurityLimit { .. }, bp)) => {
            assert_eq!(bp.offset, p.offset, "c12c5a0 offset 同对照")
        }
        e => panic!("c12c5a0 对照应报限额，实际 {e:?}"),
    }
    assert_eq!(p, position_after(&d[..p.offset as usize]), "c12c5a0 行列");
    let free = all_chunkings("c12c5a0 不设限额", d, &ParserConfig::default());
    assert!(
        matches!(&free.end, Err((ErrorKind::Encoding, _))),
        "c12c5a0 不设限额应报 Encoding：{:?}",
        free.end
    );
}

/// 同一样本的绝对偏移：若 DOCTYPE token 从 `<!DOCTYPE`（18）起算，边界字节就是证明字节 02（66），
/// 期望 TokenLength 48/49 @66（Lead 11:03 给出的期望值）。94d3c46 / 2431508 都报 @65（起点算到了
/// 序言空白 17）。是否计入序言空白待 Lead 裁定，裁定后去掉 #[ignore] 或改期望。
#[test]
fn c12c5a0_doctype_token_starts_at_markup() {
    let d: &[u8] = C12C5A0;
    let s = all_chunkings("c12c5a0 绝对偏移", d, &c12c5a0_config());
    let (k, l, a, p) = limit_of("c12c5a0 绝对偏移", &s);
    assert_eq!(
        (k, l, a, p.offset),
        (LimitKind::TokenLength, 48, 49, 66),
        "c12c5a0 绝对偏移"
    );
    assert_eq!(p, position_after(&d[..66]), "c12c5a0 行列");
}

const C12C5A0: &[u8] = b"\x0a<?x vion=\x221.0\x22?>\x0a<!DOCTYPE r [\x0a<!ENTITY a \x22@AAAAAAAAApp&amp;pp\xf2\xbe\xbe\x02\x00\x00\x00\x00\x00\x00\x00;&a?m\x0d\x0a;</r[>\x0a";

fn c12c5a0_config() -> ParserConfig {
    ParserConfig::default()
        .with_max_token_len(48)
        .with_max_internal_subset_len(40)
        .with_max_text_len(12)
        .with_max_name_len(6)
        .with_max_total_input(512)
}

/// 非法 UTF-8 序列和“能证明非法的字节”在序列里的下标（Unicode 最大子部分规则）。
fn bad_utf8_seqs() -> Vec<(&'static [u8], usize)> {
    vec![
        // 1 字节就能证明：C0/C1、F5–FF 首字节、孤立续字节
        (b"\xc0", 0),
        (b"\xc1", 0),
        (b"\xf5", 0),
        (b"\xf8", 0),
        (b"\xff", 0),
        (b"\x80", 0),
        (b"\xbf", 0),
        // 第 2 字节越出首字节允许的范围
        (b"\xe0\x80", 1),
        (b"\xed\xa0", 1),
        (b"\xf0\x80", 1),
        (b"\xf4\x90", 1),
        // 过早断开的续字节：2、3、4 字节前缀后跟非续字节
        (b"\xc3A", 1),
        (b"\xe4A", 1),
        (b"\xe4\xb8A", 2),
        (b"\xe0\xa0A", 2),
        (b"\xf0\x9fA", 2),
        (b"\xf0\x9f\x98A", 3),
        (b"\xf4\x8f\xbfA", 3),
        (b"\xf2\xbe\xbe\x02", 3), // c12c5a0 的序列
    ]
}

/// (上下文名, 序列之前的部分, 序列之后的部分)
fn encoding_contexts() -> [(&'static str, &'static str, &'static str); 4] {
    [
        ("文本", "<r>aaaaaaaaaa", "zzzz</r>"),
        ("属性值", "<r x=\"aaaaaaaaaa", "zzzz\"/>"),
        ("注释", "<r><!--aaaaaaaaaa", "zzzz--></r>"),
        (
            "子集字面量",
            "<!DOCTYPE r [<!ENTITY e \"aaaaaaaaaa",
            "zzzz\">]><r/>",
        ),
    ]
}

/// 对每个上下文 × 非法序列，让 TokenLength / TotalInput 的边界字节扫过序列前后各 3 个字节：
/// - 边界字节 ≤ 证明字节：报限额，结果与“把序列换成同长 ASCII”的对照文档完全相同
///   （kind、limit、actual、position、之前的事件）；
/// - 边界字节 > 证明字节：报 Encoding，结果与不设限额时完全相同。
///
/// `paths` 决定比对哪些路径（四路径硬断言）。返回检查的组合数。
fn encoding_vs_byte_limit(paths: fn(&str, &[u8], &ParserConfig) -> Snap) -> usize {
    let dflt = ParserConfig::default();
    let mut n = 0;
    for (ctx, pre, post) in encoding_contexts() {
        for (seq, k) in bad_utf8_seqs() {
            let mut d = pre.as_bytes().to_vec();
            let start = d.len();
            d.extend_from_slice(seq);
            d.extend_from_slice(post.as_bytes());
            let mut ctl = d.clone();
            ctl[start..start + seq.len()].fill(b'a');
            let proof = (start + k) as u64;
            let lo = start as u64 - 3;
            let hi = (start + seq.len() + 3) as u64;
            let label = format!("{ctx} {seq:02x?}");
            let free = paths(&format!("{label} 不设限额"), &d, &dflt);
            assert!(
                matches!(&free.end, Err((ErrorKind::Encoding, _))),
                "{label}：不设限额应报 Encoding，实际 {:?}",
                free.end
            );
            let mut seen = Vec::new();
            let mut cfgs: Vec<(String, ParserConfig)> = (lo..=hi)
                .map(|u| (format!("tot={u}"), dflt.clone().with_max_total_input(u)))
                .collect();
            cfgs.extend(
                (1..=d.len() as u64)
                    .map(|t| (format!("tok={t}"), dflt.clone().with_max_token_len(t))),
            );
            for (cn, c) in cfgs {
                let lbl = format!("{label} {cn}");
                let base = paths(&format!("{lbl} 对照"), &ctl, &c);
                let b = match &base.end {
                    Err((ErrorKind::SecurityLimit { kind, .. }, p))
                        if matches!(kind, LimitKind::TokenLength | LimitKind::TotalInput)
                            && (lo..=hi).contains(&p.offset) =>
                    {
                        p.offset
                    }
                    _ => continue,
                };
                check_limit_shape(&lbl, &base, &c);
                seen.push((cn.starts_with("tok"), b));
                let s = paths(&lbl, &d, &c);
                if b <= proof {
                    // kind、limit、actual、offset、之前的事件同对照文档；line/column 按本文档的字节前缀算
                    // （多字节序列的首字节算一列，续字节不算，对照文档里同位置是 ASCII，列数会不同）
                    let want = Snap {
                        events: base.events.clone(),
                        end: match &base.end {
                            Err((k, _)) => Err((k.clone(), position_after(&d[..b as usize]))),
                            Ok(()) => unreachable!(),
                        },
                    };
                    assert_eq!(
                        s, want,
                        "{lbl}：边界字节 {b} ≤ 证明非法的字节 {proof}，应报限额（同对照文档）"
                    );
                } else {
                    assert_eq!(
                        s, free,
                        "{lbl}：证明非法的字节 {proof} < 边界字节 {b}，应报 Encoding（同不设限额）"
                    );
                }
                n += 1;
            }
            // 不能是空转：两种限额的边界都要扫过整个窗口
            for tok in [false, true] {
                let mut bs: Vec<u64> = seen.iter().filter(|x| x.0 == tok).map(|x| x.1).collect();
                bs.sort();
                bs.dedup();
                assert_eq!(
                    bs,
                    (lo..=hi).collect::<Vec<_>>(),
                    "{label}：{} 的边界没有扫满窗口",
                    if tok { "TokenLength" } else { "TotalInput" }
                );
            }
        }
    }
    n
}

#[test]
fn encoding_vs_byte_limit_needed_byte_sampled() {
    let n = encoding_vs_byte_limit(quick_paths);
    println!("encoding_vs_byte_limit_needed_byte_sampled：{n} 组");
}

#[test]
#[ignore = "slow: 四路径 + 16 种随机切块 + 固定块长全切块，需要跑 --ignored"]
fn encoding_vs_byte_limit_needed_byte() {
    let n = encoding_vs_byte_limit(all_chunkings);
    println!("encoding_vs_byte_limit_needed_byte：{n} 组");
}
