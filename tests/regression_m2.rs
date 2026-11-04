//! M2 PR-A（D11、D23）回归用例，2026-10-21 由 Tester 编写。
//!
//! - D11：属性值长度在累加过程中检查。按 Lead 裁定（D-Q4），快路径和慢路径的 `actual`
//!   统一为“首次超过时的长度”（上限 + 越界字符的 UTF-8 字节数，前提是之前正好等于上限），
//!   刚好等于上限仍然通过；不能先把超长的值缓冲完再报错。
//! - D23：PI target、一般实体名、参数实体名、记号名（含 ATTLIST NOTATION 和 NDATA）必须是 NCName；
//!   实体引用也按 NCName 读，`&a:b;` 报名字错误（Syntax），不再是“未定义实体”；
//!   DOCTYPE 名必须是 QName（`a:b` 合法，`r:`、`:r`、`a:b:c` 不合法）。
//! - D23 fuzz 样本（r8/r9/r9b，去重后 9 个）全部应报 Syntax，from_bytes 与逐字节 from_reader 一致。
//!
//! - D26：注释、CDATA、PI 的内容逐字符检查 `max_text_len`，在第一个越界字符处停下，报 TextLength，
//!   `actual` 为计入该字符后的长度；未闭合的超长注释/PI 同样报 TextLength，且不缓冲尾巴。
//! - position 口径（R1b 实测统一）：所有长度限额（TextLength、AttrValueLength）的 position 都是
//!   **越界字符之后的游标**（offset = 越界字符起点 + 其 UTF-8 字节数，column 同样指向其后）。
//!   两条路径（from_bytes、逐字节 from_reader）的 kind、limit、actual、position 都必须相同。
//!
//! 只按错误类别、限额种类、limit/actual 断言；名字错误与未定义实体同为 Syntax，
//! 区分两者只能看 message，这里只做“不含 undefined entity”的反向检查。

mod common;

use common::{run_guarded, ErrorClass, Failure, Guarded, Limits};
use std::io::{BufReader, Read};
use std::time::Duration;

fn lim(max_text_len: usize) -> Limits {
    Limits {
        max_text_len: Some(max_text_len),
        ..Limits::default()
    }
}

/// 从 Read 逐字节喂给 from_reader（BufReader 容量 1）。
struct OneByte<'a>(&'a [u8]);
impl Read for OneByte<'_> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if self.0.is_empty() || buf.is_empty() {
            return Ok(0);
        }
        buf[0] = self.0[0];
        self.0 = &self.0[1..];
        Ok(1)
    }
}

/// (类别, 限额种类, limit, actual, (line, column, offset))
type Key = (
    ErrorClass,
    Option<String>,
    Option<u64>,
    Option<u64>,
    (u64, u64, u64),
);

fn key_of(e: &purexml::Error) -> Key {
    let p = (e.position.line, e.position.column, e.position.offset);
    let (c, k, l, a) = match &e.kind {
        purexml::ErrorKind::SecurityLimit {
            kind,
            limit,
            actual,
        } => (
            ErrorClass::SecurityLimit,
            Some(format!("{kind:?}")),
            Some(*limit),
            Some(*actual),
        ),
        purexml::ErrorKind::Syntax => (ErrorClass::Syntax, None, None, None),
        purexml::ErrorKind::Encoding => (ErrorClass::Encoding, None, None, None),
        purexml::ErrorKind::ExternalEntityForbidden => {
            (ErrorClass::ExternalEntityForbidden, None, None, None)
        }
        _ => (ErrorClass::Io, None, None, None),
    };
    (c, k, l, a, p)
}

/// from_reader 路径（逐字节）的最终结果。M1 的 from_reader 构造时就可能报错，同样计入。
fn via_reader(input: &[u8], limits: Limits) -> Result<(), Key> {
    let mut r = purexml::Reader::from_reader_with_config(
        BufReader::with_capacity(1, OneByte(input)),
        limits.config(),
    )
    .map_err(|e| key_of(&e))?;
    loop {
        match r.read_event() {
            Ok(Some(_)) => {}
            Ok(None) => return Ok(()),
            Err(e) => return Err(key_of(&e)),
        }
    }
}

fn key(f: &Failure) -> Key {
    (
        f.class,
        f.limit.clone(),
        f.limit_value,
        f.actual,
        (f.line, f.column, f.offset),
    )
}

/// 两条路径都跑，要求一致，返回 from_bytes 的失败。
#[track_caller]
fn both(input: &str, limits: Limits) -> Result<(), Failure> {
    let a = common::parse_with(input.as_bytes(), limits).map(|_| ());
    let b = via_reader(input.as_bytes(), limits);
    assert_eq!(
        a.as_ref().map(|_| ()).map_err(key),
        b,
        "from_bytes 与逐字节 from_reader 不一致：{input:?}"
    );
    a
}

#[track_caller]
fn attr_over(input: &str, max: usize, actual: u64) {
    let f = both(input, lim(max)).expect_err("应报 AttrValueLength");
    assert_eq!(f.class, ErrorClass::SecurityLimit, "{input:?} {f:?}");
    assert_eq!(
        f.limit.as_deref(),
        Some("AttrValueLength"),
        "{input:?} {f:?}"
    );
    assert_eq!(f.limit_value, Some(max as u64), "{input:?}");
    assert_eq!(
        f.actual,
        Some(actual),
        "actual 应为首次超过时的长度：{input:?}"
    );
}

#[track_caller]
fn attr_ok(input: &str, max: usize) {
    assert!(
        both(input, lim(max)).is_ok(),
        "刚好等于上限应通过：{input:?}"
    );
}

fn attr(v: &str) -> String {
    format!("<r a=\"{v}\"/>")
}

// ───────────── D11：刚好等于上限通过，超过时 actual = 首次超过的长度 ─────────────

#[test]
fn d11_fast_path_exact_limit_and_one_over() {
    let x = |n| "x".repeat(n);
    attr_ok(&attr(&x(8)), 8);
    attr_over(&attr(&x(9)), 8, 9);
    // 越界之后还有很长的尾巴：actual 仍是 9，不是整个值的长度
    attr_over(&attr(&x(100_000)), 8, 9);
    // 单引号
    attr_ok(&format!("<r a='{}'/>", x(8)), 8);
    attr_over(&format!("<r a='{}'/>", x(9)), 8, 9);
    // 空值、上限 0
    attr_ok("<r a=\"\"/>", 0);
    attr_over("<r a=\"x\"/>", 0, 1);
}

#[test]
fn d11_multibyte_char_crossing_the_limit() {
    // 之前正好等于上限，越界字符是 2、3、4 字节：actual = 上限 + 字符字节数
    for (ch, n) in [("é", 2u64), ("€", 3), ("😀", 4)] {
        attr_over(&attr(&format!("{}{ch}", "x".repeat(8))), 8, 8 + n);
        // 越界字符后面还有尾巴：actual 不变（M1 报整个值的长度）
        attr_over(
            &attr(&format!("{}{ch}{}", "x".repeat(8), "y".repeat(100))),
            8,
            8 + n,
        );
        // 多字节字符正好填满上限：通过
        attr_ok(&attr(&format!("{}{ch}", "x".repeat(8 - n as usize))), 8);
        // 多字节字符跨过上限（之前 7 字节）：actual = 7 + n
        attr_over(&attr(&format!("{}{ch}", "x".repeat(7))), 8, 7 + n);
    }
}

#[test]
fn d11_slow_path_same_rule() {
    let x = |n| "x".repeat(n);
    // 字符引用、预定义实体、需要规范化的空白都走慢路径
    attr_ok(&attr(&format!("&amp;{}", x(7))), 8);
    attr_over(&attr(&format!("&amp;{}", x(8))), 8, 9);
    attr_over(&attr(&format!("{}&#233;", x(8))), 8, 10);
    attr_over(&attr(&format!("{}&#x20AC;", x(8))), 8, 11);
    attr_over(&attr(&format!("{}\t", x(8))), 8, 9);
    attr_ok(&attr(&format!("{}\t", x(7))), 8);
    // 快慢两条路径逻辑值相同，actual 必须相同
    attr_over(&attr(&format!("{}y", x(8))), 8, 9);
    attr_over(&attr(&format!("&#120;{}y", x(7))), 8, 9);
    // 越界发生在实体展开中途
    let ent = |v: &str| format!("<!DOCTYPE r [<!ENTITY e \"xxxx\">]><r a=\"{v}\"/>");
    attr_ok(&ent("&e;&e;"), 8);
    attr_over(&ent("&e;&e;&e;"), 8, 9);
    attr_over(&ent(&format!("{}&e;", x(7))), 8, 9);
}

#[test]
fn d11_attlist_default_value_uses_the_same_rule() {
    let doc = |v: &str| format!("<!DOCTYPE r [<!ATTLIST r a CDATA \"{v}\">]><r/>");
    attr_ok(&doc(&"x".repeat(8)), 8);
    attr_over(&doc(&"x".repeat(9)), 8, 9);
    attr_over(&doc(&format!("{}&#233;", "x".repeat(8))), 8, 10);
    attr_over(
        &doc(&format!("{}&#233;{}", "x".repeat(8), "y".repeat(100))),
        8,
        10,
    );
}

/// 慢路径：超长之后的尾巴不能被缓冲（M1 会先缓冲整个值再报错）。
#[test]
fn d11_slow_path_does_not_buffer_the_tail() {
    let max = 64 << 10;
    let value = format!("&amp;{}", "x".repeat(32 << 20));
    let s = attr(&value);
    let n = s.len();
    match run_guarded("d11-tail", Duration::from_secs(10), move || {
        common::parse_with(s.as_bytes(), lim(max))
            .map(|_| ())
            .map_err(|f| (f.limit, f.actual))
    }) {
        Guarded::Done {
            value, peak_bytes, ..
        } => {
            assert_eq!(
                value,
                Err((Some("AttrValueLength".to_string()), Some(max as u64 + 1)))
            );
            assert!(
                peak_bytes < 2 << 20,
                "峰值 {peak_bytes} 字节，输入 {n}：尾巴被缓冲了"
            );
        }
        Guarded::TimedOut => panic!("超时"),
        Guarded::Panicked(m) => panic!("panic {m}"),
    }
}

// ───────────── D23：NCName / QName ─────────────

#[track_caller]
fn name_error(input: &str) {
    let f = both(input, Limits::default()).expect_err("应报 Syntax");
    assert_eq!(f.class, ErrorClass::Syntax, "{input:?} {f:?}");
    assert!(
        !f.detail.contains("undefined entity"),
        "应是名字错误，不是未定义实体：{input:?} {}",
        f.detail
    );
}

#[test]
fn d23_pi_targets_are_ncnames() {
    for x in [
        "<r><?a:b x?></r>",
        "<r><?a: x?></r>",
        "<r><?:a x?></r>",
        "<?p:q?><r/>",
        "<?xml:x y?><r/>",
        "<?xmlns: ?><r/>",
        "<?:ml version=\"1.0\"?><r/>",
        "<r/><?a:b?>",
        "<!DOCTYPE r [<?a:b x?>]><r/>",
        "<!DOCTYPE r [<!ENTITY e \"<?a:b x?>\">]><r>&e;</r>",
    ] {
        name_error(x);
    }
    for x in [
        "<r><?ab x?></r>",
        "<?xml-stylesheet href=\"a\"?><r/>",
        "<r><?a.b-c_d x?></r>",
    ] {
        assert!(both(x, Limits::default()).is_ok(), "{x}");
    }
}

#[test]
fn d23_entity_and_parameter_entity_names_are_ncnames() {
    for x in [
        "<!DOCTYPE r [<!ENTITY a:b \"x\">]><r/>",
        "<!DOCTYPE r [<!ENTITY :a \"x\">]><r/>",
        "<!DOCTYPE r [<!ENTITY a: \"x\">]><r/>",
        "<!DOCTYPE r [<!ENTITY % a:b \"x\">]><r/>",
        "<!DOCTYPE r [<!ENTITY % :a \"x\">]><r/>",
        // 名字先于 SYSTEM 检查：是 Syntax，不是 ExternalEntityForbidden
        "<!DOCTYPE r [<!ENTITY a:b SYSTEM \"x\">]><r/>",
        "<!DOCTYPE r [<!ENTITY % a:b SYSTEM \"x\">]><r/>",
    ] {
        name_error(x);
    }
    assert!(both(
        "<!DOCTYPE r [<!ENTITY a_b \"x\">]><r>&a_b;</r>",
        Limits::default()
    )
    .is_ok());
    assert!(both("<!DOCTYPE r [<!ENTITY % p \"x\">]><r/>", Limits::default()).is_ok());
}

#[test]
fn d23_entity_references_are_ncnames() {
    for x in [
        "<r>&a:b;</r>",
        "<r>&:a;</r>",
        "<r>&a:;</r>",
        "<r a=\"&a:b;\"/>",
        "<!DOCTYPE r [<!ENTITY a:b \"x\">]><r>&a:b;</r>",
        "<!DOCTYPE r [<!ENTITY e \"&a:b;\">]><r/>",
        "<!DOCTYPE r [<!ENTITY e \"&a:b;\">]><r>&e;</r>",
        "<!DOCTYPE r [<!ATTLIST r a CDATA \"&a:b;\">]><r/>",
    ] {
        name_error(x);
    }
    // 未声明的合法名字仍然是未定义实体
    let f = both("<r>&missing;</r>", Limits::default()).unwrap_err();
    assert_eq!(f.class, ErrorClass::Syntax);
}

#[test]
fn d23_notation_names_are_ncnames() {
    for x in [
        "<!DOCTYPE r [<!NOTATION a:b SYSTEM \"x\">]><r/>",
        "<!DOCTYPE r [<!NOTATION a:b PUBLIC \"p\">]><r/>",
        "<!DOCTYPE r [<!ATTLIST r a NOTATION (a:b) #IMPLIED>]><r/>",
        "<!DOCTYPE r [<!ATTLIST r a NOTATION (n|a:b) #IMPLIED>]><r/>",
        // NDATA 的记号名：名字非法是 Syntax，优先于外部实体的 ExternalEntityForbidden
        "<!DOCTYPE r [<!ENTITY e SYSTEM \"u\" NDATA a:b>]><r/>",
        "<!DOCTYPE r [<!ENTITY e PUBLIC \"p\" \"u\" NDATA a:b>]><r/>",
        "<!DOCTYPE r [<!ENTITY e SYSTEM \"u\" NDATA :n>]><r/>",
    ] {
        name_error(x);
    }
    assert!(both(
        "<!DOCTYPE r [<!NOTATION n SYSTEM \"x\">]><r/>",
        Limits::default()
    )
    .is_ok());
    // 合法的 NDATA 记号名：外部实体仍然禁止，不加载
    let f = both(
        "<!DOCTYPE r [<!ENTITY e SYSTEM \"u\" NDATA n>]><r/>",
        Limits::default(),
    )
    .unwrap_err();
    assert_eq!(f.class, ErrorClass::ExternalEntityForbidden);
    let f = both(
        "<!DOCTYPE r [<!ENTITY e PUBLIC \"p\" \"u\" NDATA n>]><r/>",
        Limits::default(),
    )
    .unwrap_err();
    assert_eq!(f.class, ErrorClass::ExternalEntityForbidden);
}

#[test]
fn d23_doctype_name_is_a_qname() {
    for x in [
        "<!DOCTYPE r:><r/>",
        "<!DOCTYPE :r><r/>",
        "<!DOCTYPE a:b:c><r/>",
        "<!DOCTYPE a::b><r/>",
        "<!DOCTYPE r: SYSTEM \"x.dtd\"><r/>",
        "<!DOCTYPE r: PUBLIC \"p\" \"x.dtd\"><r/>",
        "<!DOCTYPE r: [<!ENTITY a \"x\">]><r/>",
    ] {
        name_error(x);
    }
    for x in [
        "<!DOCTYPE a:b><a:b xmlns:a=\"urn:a\"/>",
        "<!DOCTYPE a:b SYSTEM \"x.dtd\"><a:b xmlns:a=\"urn:a\"/>",
        "<!DOCTYPE a:b [<!ATTLIST a:b c CDATA \"d\">]><a:b xmlns:a=\"urn:a\"/>",
        "<!DOCTYPE r><r/>",
    ] {
        assert!(both(x, Limits::default()).is_ok(), "{x}");
    }
    let s = common::parse_default(b"<!DOCTYPE a:b><a:b xmlns:a=\"urn:a\"/>").unwrap();
    assert_eq!(s.doctype.unwrap().0, "a:b");
}

/// r8/r9/r9b 的 writer_roundtrip D23 样本（去重后 9 个），以及手工最小化的模式。
#[test]
fn d23_fuzz_samples_rejected() {
    let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/data/fuzz-regressions");
    let files = [
        "writer_roundtrip-crash-543336e4-min.xml",
        "writer_roundtrip-crash-a91aac12-min.xml",
        "writer_roundtrip-crash-63fb89f9.bin",
        "writer_roundtrip-crash-f2ef4724.bin",
        "writer_roundtrip-crash-12a2f21b.bin",
        "writer_roundtrip-crash-22c64e90.bin",
        "writer_roundtrip-crash-5c3a3134.bin",
        "writer_roundtrip-crash-a7187392.bin",
        "writer_roundtrip-crash-fd3c5623.bin",
    ];
    for f in files {
        let data = std::fs::read(format!("{dir}/{f}")).unwrap_or_else(|e| panic!("{f}: {e}"));
        let a = common::parse_default(&data).map(|_| ());
        let b = via_reader(&data, Limits::default());
        assert_eq!(
            a.as_ref().map(|_| ()).map_err(key),
            b,
            "{f}：两条路径不一致"
        );
        assert_eq!(
            a.map_err(|x| x.class),
            Err(ErrorClass::Syntax),
            "{f}：应报 Syntax"
        );
    }
    // 各样本的最小模式（哈希前 8 位）
    for x in [
        "<?xmlns: x?><r/>",                                    // 12a2f21b
        "<!DOCTYPE r: [<!ENTITY a \"x\">]><r/>",               // 22c64e90
        "<r><?a: x?></r>",                                     // 543336e4
        "<!DOCTYPE r [<!ENTITY a \"<?xmlA:x?>\">]><r>&a;</r>", // 543336e4（r9b 变体：PI 在实体值里）
        "<?:ml version=\"1.0\"?><r/>",                         // 5c3a3134
        "<!DOCTYPE rxmlns: PUBLIC \"p\" \"x.dtd\"><r/>",       // 63fb89f9
        "<!DOCTYPE rYPE333:><r/>",                             // a7187392
        "<!DOCTYPE r: SYSTEM \"x.dtd\"><r/>",                  // a91aac12
        "<!DOCTYPE docxml: [<!ELEMENT doc (#PCDATA)>]><doc/>", // f2ef4724
        "<doc><?:p d?></doc>",                                 // fd3c5623
    ] {
        name_error(x);
    }
}

// ───────────── D26：注释、CDATA、PI 逐字符检查 ─────────────

/// 断言报 TextLength，actual 为首次超过时的长度，position 为越界字符之后的游标（offset）。
#[track_caller]
fn text_over(input: &str, max: usize, actual: u64, offset: u64) {
    let f = both(input, lim(max)).expect_err("应报 TextLength");
    assert_eq!(f.class, ErrorClass::SecurityLimit, "{input:?} {f:?}");
    assert_eq!(f.limit.as_deref(), Some("TextLength"), "{input:?} {f:?}");
    assert_eq!(f.limit_value, Some(max as u64), "{input:?}");
    assert_eq!(
        f.actual,
        Some(actual),
        "actual 应为首次超过时的长度：{input:?}"
    );
    assert_eq!(
        f.offset, offset,
        "position 应为越界字符之后的游标：{input:?} {f:?}"
    );
}

#[track_caller]
fn text_ok(input: &str, max: usize) {
    assert!(
        both(input, lim(max)).is_ok(),
        "刚好等于上限应通过：{input:?}"
    );
}

/// (名字, 前缀, 后缀) 三种 token；后缀带 `zz` 尾巴，验证不是“读完再报”。
const D26_TOKENS: [(&str, &str, &str); 3] = [
    ("comment", "<r><!--", "zz--></r>"),
    ("cdata", "<r><![CDATA[", "zz]]></r>"),
    ("pi", "<r><?p ", "zz?></r>"),
];

#[test]
fn d26_exact_limit_passes() {
    for (_, pre, post) in D26_TOKENS {
        let close = &post[2..];
        // 纯 ASCII 和以多字节字符结尾，都刚好 8 字节
        for body in [
            "x".repeat(8),
            format!("{}é", "x".repeat(6)),
            format!("x{}", "😀"),
        ] {
            let body = if body.len() == 5 {
                format!("xxx{body}")
            } else {
                body
            };
            assert_eq!(body.len(), 8);
            text_ok(&format!("{pre}{body}{close}"), 8);
        }
    }
}

#[test]
fn d26_first_overflow_actual_and_position() {
    for (_, pre, post) in D26_TOKENS {
        for ch in ["y", "é", "€", "😀"] {
            let n = ch.len() as u64;
            // 之前正好 8 字节：actual = 8 + n，position = 越界字符之后
            let s = format!("{pre}{}{ch}{post}", "x".repeat(8));
            let at = (pre.len() + 8) as u64;
            text_over(&s, 8, 8 + n, at + n);
            // 之前 7 字节，多字节字符跨过上限：actual = 7 + n（1 字节时不越界，越界的是尾巴的第一个 z）
            let s = format!("{pre}{}{ch}{post}", "x".repeat(7));
            let at = (pre.len() + 7) as u64;
            if n == 1 {
                text_over(&s, 8, 9, at + 2);
            } else {
                text_over(&s, 8, 7 + n, at + n);
            }
        }
    }
}

#[test]
fn d26_normalizing_path_same_rule() {
    // 注释里有 \r 时走规范化缓冲路径（\r 计 1 字节）；越界字符起点 = 7（前缀）+ 1 + 7 = 15
    for ch in ["y", "é", "€", "😀"] {
        let n = ch.len() as u64;
        let s = format!("<r><!--\r{}{ch}zz--></r>", "x".repeat(7));
        text_over(&s, 8, 8 + n, 15 + n);
    }
    text_ok(&format!("<r><!--\r\n{}--></r>", "x".repeat(7)), 8);
}

#[test]
fn d26_unterminated_overlong_token_reports_text_length() {
    for pre in ["<r><!--", "<r><?p ", "<r><![CDATA["] {
        let s = format!("{pre}{}", "x".repeat(20));
        text_over(&s, 8, 9, (pre.len() + 9) as u64);
        // 未超限的未闭合 token 仍是语法错误
        let f = both(&format!("{pre}xxxx"), lim(8)).expect_err("未闭合");
        assert_eq!(f.class, ErrorClass::Syntax, "{pre} {f:?}");
    }
}

#[test]
fn d26_does_not_buffer_the_tail() {
    let max = 64 << 10;
    // 纯路径和规范化路径（带 \r）各一份，后面 32 MiB 尾巴，不闭合
    for pre in [
        "<r><!--",
        "<r><!--\r",
        "<r><?p ",
        "<r><?p \r",
        "<r><![CDATA[\r",
    ] {
        let s = format!("{pre}{}", "x".repeat(32 << 20));
        let n = s.len();
        match run_guarded("d26-tail", Duration::from_secs(10), move || {
            common::parse_with(s.as_bytes(), lim(max))
                .map(|_| ())
                .map_err(|f| (f.limit, f.actual))
        }) {
            Guarded::Done {
                value, peak_bytes, ..
            } => {
                assert_eq!(
                    value,
                    Err((Some("TextLength".to_string()), Some(max as u64 + 1))),
                    "{pre:?}"
                );
                assert!(
                    peak_bytes < 2 << 20,
                    "{pre:?}：峰值 {peak_bytes} 字节，输入 {n}：尾巴被缓冲了"
                );
            }
            Guarded::TimedOut => panic!("{pre:?} 超时"),
            Guarded::Panicked(m) => panic!("{pre:?} panic {m}"),
        }
    }
}

#[test]
fn length_limits_share_one_position_rule() {
    // 普通文本、属性值、注释、CDATA、PI：越界字符 1/2/3/4 字节，position 都在越界字符之后
    let shapes: [(&str, &str, &str); 5] = [
        ("TextLength", "<r>", "zz</r>"),
        ("AttrValueLength", "<r a=\"", "zz\"/>"),
        ("TextLength", "<r><!--", "zz--></r>"),
        ("TextLength", "<r><![CDATA[", "zz]]></r>"),
        ("TextLength", "<r><?p ", "zz?></r>"),
    ];
    for (kind, pre, post) in shapes {
        for ch in ["y", "é", "€", "😀"] {
            let n = ch.len() as u64;
            let s = format!("{pre}{}{ch}{post}", "x".repeat(8));
            let f = both(&s, lim(8)).expect_err("应越界");
            assert_eq!(f.limit.as_deref(), Some(kind), "{s:?}");
            assert_eq!(f.actual, Some(8 + n), "{s:?}");
            let after = (pre.len() + 8) as u64 + n;
            assert_eq!(f.offset, after, "{s:?}：offset 应在越界字符之后");
            // 单行输入：column（按字符计，1 起）= 越界字符之后的字符位置
            let col = (pre.chars().count() + 8 + 1 + 1) as u64;
            assert_eq!(f.column, col, "{s:?}：column 应在越界字符之后");
        }
    }
}
