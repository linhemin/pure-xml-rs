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
        for k in 0..=d.len() {
            let run = stream(d, c, Plan::Cut(k), None);
            assert_same(&format!("{label} [切点 {k}]"), &base, &run.snap);
            assert_eq!(
                run.after_eof, 0,
                "{label} [切点 {k}]：Ok(0) 之后又调用了 read"
            );
            n += 1;
        }
    }
    let rounds = if big { 16 } else { 64 };
    for seed in 0..rounds {
        let plan = Plan::Schedule(seeded_schedule(seed));
        assert_same(
            &format!("{label} [随机切块 {seed}]"),
            &base,
            &stream(d, c, plan, None).snap,
        );
        n += 1;
    }
    let caps: &[usize] = if big {
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

#[test]
fn corpus_matches_from_bytes_on_all_chunkings() {
    let docs = corpus();
    assert!(docs.len() >= 90, "语料太少：{}", docs.len());
    let mut paths = 0;
    for (name, d) in &docs {
        paths += check_doc(name, d, &ParserConfig::default());
    }
    eprintln!("语料 {} 份，{paths} 条路径", docs.len());
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
#[test]
fn d21_documents_replay_on_all_paths() {
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
        let s = four_paths_counted(label, doc.as_bytes(), &c);
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
fn four_paths_counted(label: &str, d: &[u8], c: &ParserConfig) -> (usize, End) {
    let mut r = Reader::from_bytes(d).with_config(c.clone());
    let base = count!(r);
    let mut r = Reader::from_vec(d.to_vec()).with_config(c.clone());
    assert_eq!(base, count!(r), "{label}：from_vec");
    let mut plans = vec![Plan::Byte];
    plans.extend((0..16).map(|s| Plan::Schedule(seeded_schedule(s + 2000))));
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
