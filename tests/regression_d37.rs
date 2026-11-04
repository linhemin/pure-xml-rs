//! D37 回归（安全缺陷，Lead 裁定单独开 PR，基于 main 257c900，分支 cursor/purexml-d37-text-gt-limit）。
//!
//! 缺陷：文本快路径（`try_plain_text`）遇到 `>` 时只更新 `]]>` 状态就 `continue`，跳过了
//! `max_text_len` 检查。越界字符是 `>` 时不报错：一串 `>`（或者以 `>` 结尾的文本）可以绕过 max_text_len；
//! 越界字符是 `>`、后面还有普通字符时，要等到下一个普通字符才报，actual 和 position 都偏大。
//!
//! 口径（与 257c900 现有用例一致，见 tests/regression_m2.rs 的 `text_over` 和
//! `length_limits_share_one_position_rule`，也与 M2 的 L 表一致，两者没有差别）：
//! - TextLength 按 UTF-8 字节计；`limit` = max_text_len，`actual` = 计入首个越界字符后的长度；
//! - position 是越界字符之后的游标：offset = 越界字符起点 + 它的字节数，line/column 也指向其后
//!   （column 按 Unicode 标量计、1 起）。
//!
//! 路径：`from_bytes`，以及 `from_reader_with_config` 的三种读取方式（整块 `&[u8]`、
//! `BufReader::with_capacity(1)` 逐字节、7 字节块）。257c900 的 from_reader 先把输入整块读入
//! 再解析，所以四条路径的事件序列和错误（kind、limit、actual、line、column、offset）必须完全相同。
//! 这里不断言“构造时已读完全部输入”：PR #3 改成流式读取以后这条不再成立，而 D37 合入 main 后
//! 这个文件会随 merge 进入 PR #3。
//!
//! 只用 M1 的公开 API（`Reader::from_bytes`、`Reader::from_reader_with_config`、`ParserConfig`），
//! 不依赖 tests/common 和 M2 的辅助函数。默认 16 MiB 上限的用例只在 release 下编译（与 d28_default_cap 相同）。

use purexml::{ErrorKind, LimitKind, ParserConfig, Position, Reader};
use std::io::{BufReader, Read};

type End = Result<(), (ErrorKind, Position)>;

#[derive(Debug, PartialEq)]
struct Outcome {
    events: Vec<String>,
    end: End,
}

/// 事件数上限，防止解析器出错时无限产出事件。
const MAX_EVENTS: usize = 1_000_000;

macro_rules! drain {
    ($reader:expr) => {{
        let r = &mut $reader;
        let mut events = Vec::new();
        let end = loop {
            match r.read_event() {
                Ok(Some(e)) => {
                    events.push(format!("{e:?}"));
                    assert!(events.len() < MAX_EVENTS, "事件数失控");
                }
                Ok(None) => break Ok(()),
                Err(e) => break Err((e.kind.clone(), e.position)),
            }
        };
        Outcome { events, end }
    }};
}

/// 每次 read 最多给出 `n` 字节的读取器。
struct Chunked<'a> {
    data: &'a [u8],
    n: usize,
}

impl Read for Chunked<'_> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let k = self.n.min(buf.len()).min(self.data.len());
        buf[..k].copy_from_slice(&self.data[..k]);
        self.data = &self.data[k..];
        Ok(k)
    }
}

fn via_reader<R: std::io::BufRead>(r: R, c: &ParserConfig) -> Outcome {
    let mut r = Reader::from_reader_with_config(r, c.clone()).expect("from_reader 构造");
    drain!(r)
}

/// from_bytes 和三种 from_reader 结果必须完全相同；返回 from_bytes 的结果。
fn all_paths(label: &str, d: &[u8], c: &ParserConfig) -> Outcome {
    let mut r = Reader::from_bytes(d).with_config(c.clone());
    let base = drain!(r);
    let readers: [(&str, Outcome); 3] = [
        ("from_reader 整块", via_reader(d, c)),
        (
            "from_reader 逐字节",
            via_reader(BufReader::with_capacity(1, Chunked { data: d, n: 1 }), c),
        ),
        (
            "from_reader 7 字节块",
            via_reader(BufReader::with_capacity(7, Chunked { data: d, n: 7 }), c),
        ),
    ];
    for (name, got) in readers {
        assert!(
            got == base,
            "{label} [{name}]：与 from_bytes 不同。事件数 {} vs {}；结尾 {:?} vs {:?}",
            base.events.len(),
            got.events.len(),
            base.end,
            got.end
        );
    }
    base
}

/// `prefix` 之后的游标位置（line/column 按 Unicode 标量计，1 起；只用 `\n` 换行）。
fn position_after(prefix: &[u8]) -> Position {
    let s = std::str::from_utf8(prefix).expect("样本是 UTF-8");
    let (mut line, mut column) = (1u64, 1u64);
    for ch in s.chars() {
        if ch == '\n' {
            line += 1;
            column = 1;
        } else {
            column += 1;
        }
    }
    Position {
        line,
        column,
        offset: prefix.len() as u64,
    }
}

fn cfg(max: u64) -> ParserConfig {
    ParserConfig::default().with_max_text_len(max)
}

/// 断言报 TextLength：limit = max，actual = 首次越界时的字节数，position 在越界字符之后。
#[track_caller]
fn text_over(label: &str, d: &[u8], c: &ParserConfig, actual: u64, offset: u64) {
    let o = all_paths(label, d, c);
    match &o.end {
        Err((
            ErrorKind::SecurityLimit {
                kind: LimitKind::TextLength,
                limit,
                actual: a,
            },
            p,
        )) => {
            assert_eq!(
                (*limit, *a),
                (c.max_text_len, actual),
                "{label}：limit/actual"
            );
            assert_eq!(
                *p,
                position_after(&d[..offset as usize]),
                "{label}：position 应在越界字符之后"
            );
        }
        other => panic!("{label}：应报 TextLength，实际 {other:?}"),
    }
}

#[track_caller]
fn passes(label: &str, d: &[u8], c: &ParserConfig) {
    let o = all_paths(label, d, c);
    assert!(o.end.is_ok(), "{label}：应通过，实际 {:?}", o.end);
}

/// 文本从 `start` 开始，按 max 字节计：返回 (actual, 越界字符之后的 offset)。
fn first_overflow(text: &str, start: usize, max: u64) -> (u64, u64) {
    let mut acc = 0u64;
    for ch in text.chars() {
        acc += ch.len_utf8() as u64;
        if acc > max {
            return (acc, start as u64 + acc);
        }
    }
    panic!("样本 {text:?} 没有超过 {max}");
}

/// 在 `<r>` 后放 `text`，断言在第一个越界字符处报 TextLength。
#[track_caller]
fn text_body_over(label: &str, text: &str, max: u64) {
    let d = format!("<r>{text}</r>");
    let (actual, offset) = first_overflow(text, 3, max);
    text_over(label, d.as_bytes(), &cfg(max), actual, offset);
}

const MAX: u64 = 12;

/// 纯 `>` 文本：13 个起就报 TextLength 12/13 @16（257c900 上整串放行）。
#[test]
fn d37_gt_only_text_hits_text_length() {
    for n in [13usize, 14, 100, 4096, 20_000] {
        text_body_over(&format!("`>`×{n}"), &">".repeat(n), MAX);
    }
}

/// `>` 与普通字符、`]`、`]]`（后面不跟 `>`）混合，越界字符是 `>` 或越界时 `>` 已计入：
/// 都报 TextLength 12/13，position 在第 13 个字节之后。
#[test]
fn d37_gt_mixed_text_hits_text_length() {
    let samples: Vec<(&str, String)> = vec![
        ("12 个 a + `>`", format!("{}>", "a".repeat(12))),
        ("12 个 a + `>` + b", format!("{}>b", "a".repeat(12))),
        ("11 个字符 + `>>` + b", "abcdefghijk>>b".to_string()),
        ("`>a`×7", ">a".repeat(7)),
        ("`a>`×7", "a>".repeat(7)),
        ("`>]`×7", ">]".repeat(7)),
        ("`]>`×7", "]>".repeat(7)),
        ("`]]a`×4 + `>`", format!("{}>", "]]a".repeat(4))),
        ("`]]a`×4 + `>x`", format!("{}>x", "]]a".repeat(4))),
        ("`>]]a`×4", ">]]a".repeat(4)),
        ("空格×12 + `>`", format!("{}>", " ".repeat(12))),
        ("`>`×12 + `]`", format!("{}]", ">".repeat(12))),
        ("`>`×12 + `]]`", format!("{}]]", ">".repeat(12))),
        ("`>`×12 + a", format!("{}a", ">".repeat(12))),
        ("`> `×20", "> ".repeat(20)),
        ("`]>]]a`×3（`]]` 后不跟 `>`）", "]>]]a".repeat(3)),
    ];
    for (name, t) in &samples {
        text_body_over(name, t, MAX);
    }
}

/// position 的行列：越界字符之后。含换行、多字节字符、前面已有子元素的文本。
#[test]
fn d37_gt_text_position_line_column() {
    // 换行：5 个 `\n` + 8 个 `>`，第 13 个字节是 `>`
    text_body_over(
        "换行 + `>`",
        &format!("{}{}", "\n".repeat(5), ">".repeat(8)),
        MAX,
    );
    // 多字节：6 个 é（12 字节）+ `>`，越界字符是 `>`
    text_body_over("é×6 + `>`", &format!("{}>", "\u{e9}".repeat(6)), MAX);
    // 多字节字符本身越界：11 个 `>` + é（13 字节，actual 13）
    text_body_over("`>`×11 + é", &format!("{}\u{e9}", ">".repeat(11)), MAX);
    // `>`×12 + é：越界的是 é，actual = 14
    text_body_over("`>`×12 + é", &format!("{}\u{e9}", ">".repeat(12)), MAX);
    // 子元素之后的文本：从 offset 7 开始
    let d = format!("<r><x/>{}</r>", ">".repeat(13));
    text_over("子元素后 `>`×13", d.as_bytes(), &cfg(MAX), 13, 20);
    // 第二段文本越界：第一段刚好 12
    let d = format!("<r>{}<x/>{}</r>", ">".repeat(12), ">".repeat(13));
    text_over("第二段 `>`×13", d.as_bytes(), &cfg(MAX), 13, 32);
}

/// 刚好等于上限时通过（257c900 上也通过）。
#[test]
fn d37_text_exactly_at_limit_passes() {
    let c = cfg(MAX);
    for t in [
        ">".repeat(12),
        format!("{}>", "a".repeat(11)),
        ">]".repeat(6),
        format!("{}>", "]]a".repeat(3) + "ab"),
        format!("{}>>", "\u{e9}".repeat(5)),
        format!("{}>", "\n".repeat(11)),
    ] {
        assert_eq!(t.len(), 12, "{t:?}");
        passes(
            &format!("刚好 12 {t:?}"),
            format!("<r>{t}</r>").as_bytes(),
            &c,
        );
    }
    // 每段文本单独计：两段各 12 个 `>`
    let d = format!(
        "<r>{}<x/>{}<!--c-->{}</r>",
        ">".repeat(12),
        ">".repeat(12),
        ">".repeat(12)
    );
    passes("三段各 12", d.as_bytes(), &c);
    // `]]>` 仍是 Syntax（不受影响）
    let o = all_paths("`]]>`", b"<r>]]></r>", &c);
    assert!(
        matches!(o.end, Err((ErrorKind::Syntax, _))),
        "`]]>`：{:?}",
        o.end
    );
}

/// 对照：实体展开文本、字符引用走慢路径，257c900 上已经逐字符检查，必须保持不变。
#[test]
fn d37_entity_and_char_ref_text_unaffected() {
    let c = cfg(MAX);
    // 内部实体展开出 13 个 `>`：TextLength 12/13，position 是引用 `&` 所在位置（M1 口径：
    // 展开内的错误报最外层引用的位置）
    let d = format!(
        "<!DOCTYPE r [<!ENTITY g \"{}\">]><r>&g;</r>",
        ">".repeat(13)
    );
    let amp = d.find("&g;").unwrap();
    entity_over("实体 `>`×13", d.as_bytes(), &c, amp);
    let d = format!(
        "<!DOCTYPE r [<!ENTITY g \"{}\">]><r>&g;</r>",
        ">".repeat(12)
    );
    passes("实体 `>`×12", d.as_bytes(), &c);
    // 字符引用 `&gt;`×13：越界的是第 13 个引用，position 在该引用之后
    let d = format!("<r>{}</r>", "&gt;".repeat(13));
    text_over("`&gt;`×13", d.as_bytes(), &c, 13, 3 + 13 * 4);
    let d = format!("<r>{}</r>", "&gt;".repeat(12));
    passes("`&gt;`×12", d.as_bytes(), &c);
    // 普通字符和引用混合
    let d = format!("<r>{}&gt;</r>", ">".repeat(12));
    text_over("`>`×12 + `&gt;`", d.as_bytes(), &c, 13, 3 + 12 + 4);
}

#[track_caller]
fn entity_over(label: &str, d: &[u8], c: &ParserConfig, amp: usize) {
    let o = all_paths(label, d, c);
    match &o.end {
        Err((
            ErrorKind::SecurityLimit {
                kind: LimitKind::TextLength,
                limit,
                actual,
            },
            p,
        )) => {
            assert_eq!(
                (*limit, *actual),
                (c.max_text_len, c.max_text_len + 1),
                "{label}"
            );
            assert_eq!(*p, position_after(&d[..amp]), "{label}：position");
        }
        other => panic!("{label}：应报 TextLength，实际 {other:?}"),
    }
}

/// 对照：CDATA 走逐字符检查，`>` 也计入，257c900 上已经正确。
#[test]
fn d37_cdata_unaffected() {
    let c = cfg(MAX);
    let pre = "<r><![CDATA[";
    for t in [
        ">".repeat(13),
        "]>".repeat(7),
        format!("{}>", "a".repeat(12)),
    ] {
        let d = format!("{pre}{t}]]></r>");
        let (actual, offset) = first_overflow(&t, pre.len(), MAX);
        text_over(&format!("CDATA {t:?}"), d.as_bytes(), &c, actual, offset);
    }
    for t in [">".repeat(12), "]>".repeat(6)] {
        passes(
            &format!("CDATA 刚好 12 {t:?}"),
            format!("{pre}{t}]]></r>").as_bytes(),
            &c,
        );
    }
}

/// 默认上限 16 MiB：16 MiB + 1 个 `>` 必须报 TextLength（257c900 上整串放行）。只在 release 下编译。
#[cfg(not(debug_assertions))]
#[test]
fn d37_default_cap_gt_run_over_16_mib() {
    let m = purexml::DEFAULT_MAX_TEXT_LEN;
    let c = ParserConfig::default();
    assert_eq!(c.max_text_len, m);
    let d = format!("<r>{}</r>", ">".repeat(m as usize + 1));
    text_over(
        "默认上限 `>`×(16 MiB+1)",
        d.as_bytes(),
        &c,
        m + 1,
        3 + m + 1,
    );
    let d = format!("<r>{}></r>", "a".repeat(m as usize));
    text_over(
        "默认上限 a×16 MiB + `>`",
        d.as_bytes(),
        &c,
        m + 1,
        3 + m + 1,
    );
}

/// 默认上限：刚好 16 MiB 个 `>` 通过。只在 release 下编译。
#[cfg(not(debug_assertions))]
#[test]
fn d37_default_cap_gt_run_exactly_16_mib_passes() {
    let m = purexml::DEFAULT_MAX_TEXT_LEN as usize;
    let d = format!("<r>{}</r>", ">".repeat(m));
    passes(
        "默认上限 `>`×16 MiB",
        d.as_bytes(),
        &ParserConfig::default(),
    );
}
