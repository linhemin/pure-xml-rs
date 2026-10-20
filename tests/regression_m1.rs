//! M1 第二轮（修复后）对抗性回归用例，2026-10-21 由 Tester 编写。
//!
//! - `d13_*` / `d10_*` / `d1_*` / `d2_*` / `d12_*`：针对已修复缺陷的变体，修复后应当全部通过。
//! - `d14_*` ～ `d17_*`、`d19_*`：第二轮新发现的缺陷（D18 栈溢出见 `stack_depth_m1.rs`），修复前会失败，按约定保持开启。
//!
//! 缺陷编号和说明见 M1-test-report.md。

mod common;

use common::{run_guarded, structure, ErrorClass, Guarded, Node};
use std::time::{Duration, Instant};

fn class(x: &str) -> Result<(), ErrorClass> {
    common::parse_default(x.as_bytes())
        .map(|_| ())
        .map_err(|f| f.class)
}

#[track_caller]
fn assert_syntax(x: &str) {
    assert_eq!(class(x), Err(ErrorClass::Syntax), "应报 Syntax：{x}");
}

#[track_caller]
fn assert_wf(x: &str) {
    assert_eq!(class(x), Ok(()), "应良构：{x}");
}

/// 运行并返回 (结果, 峰值内存字节)；超时或 panic 直接失败。
fn guarded(name: &str, secs: u64, s: String) -> (Result<(), ErrorClass>, usize) {
    match run_guarded(name, Duration::from_secs(secs), move || {
        common::parse_default(s.as_bytes())
            .map(|_| ())
            .map_err(|f| f.class)
    }) {
        Guarded::Done {
            value, peak_bytes, ..
        } => (value, peak_bytes),
        Guarded::TimedOut => panic!("{name}：超过 {secs} 秒"),
        Guarded::Panicked(m) => panic!("{name}：panic {m}"),
    }
}

fn first_start(x: &str) -> Node {
    structure(x.as_bytes()).unwrap().remove(0)
}

fn start(ns: Option<&str>, local: &str, attrs: &[(&str, &str)]) -> Node {
    Node::Start {
        name: (ns.map(str::to_string), local.to_string()),
        attrs: attrs
            .iter()
            .map(|(k, v)| ((None, k.to_string()), v.to_string()))
            .collect(),
    }
}

// ───────────── D13 变体：命名空间 URI 不得逐次复制 ─────────────

/// 100 个不同前缀，各绑定一个不同的 64 KiB URI，再各用一次（输入约 6.5 MB）
#[test]
fn d13_many_distinct_long_uris() {
    let mut s = String::from("<r");
    for i in 0..100 {
        s.push_str(&format!(" xmlns:p{i}=\"{}\"", "u".repeat(64 << 10)));
    }
    s.push('>');
    for i in 0..100 {
        s.push_str(&format!("<p{i}:e p{i}:a=\"1\"/>"));
    }
    s.push_str("</r>");
    let n = s.len();
    let (r, peak) = guarded("d13a", 10, s);
    assert_eq!(r, Ok(()));
    assert!(peak < 3 * n, "峰值 {peak} > 3 × 输入 {n}");
}

/// 1 MiB 默认命名空间 + 5 万个子元素（每个元素都要解析到默认命名空间）
#[test]
fn d13_long_default_namespace() {
    let s = format!(
        "<r xmlns=\"{}\">{}</r>",
        "u".repeat(1 << 20),
        "<e a=\"1\"/>".repeat(50_000)
    );
    let n = s.len();
    let (r, peak) = guarded("d13b", 5, s);
    assert_eq!(r, Ok(()));
    assert!(peak < 4 * n, "峰值 {peak} > 4 × 输入 {n}");
}

/// 每个子元素都重新声明同一前缀（16 KiB URI），声明和使用都应是线性开销
#[test]
fn d13_redeclare_on_every_element() {
    let uri = "u".repeat(16 << 10);
    let mut s = format!("<r xmlns:p=\"{uri}\">");
    for _ in 0..1000 {
        s.push_str(&format!("<p:e xmlns:p=\"{uri}\" p:a=\"1\"/>"));
    }
    s.push_str("</r>");
    let (r, peak) = guarded("d13c", 5, s);
    assert_eq!(r, Ok(()));
    assert!(peak < 8 << 20, "峰值 {peak} 字节");
}

// ───────────── D10 变体：前缀查找不得随绑定数增长 ─────────────

/// 同一前缀在 255 层每层都重新绑定 + 10 万个带前缀子元素
#[test]
fn d10_same_prefix_rebound_every_level() {
    let mut s = "<e xmlns:p=\"u\">".repeat(255);
    s.push_str(&"<p:k p:a=\"1\"/>".repeat(100_000));
    s.push_str(&"</e>".repeat(255));
    assert_eq!(guarded("d10a", 10, s).0, Ok(()));
}

/// 单层 1023 个不同前缀，10 万次查询最后声明的前缀和第一个前缀
#[test]
fn d10_wide_single_level() {
    let mut s = String::from("<e");
    for d in 0..1023 {
        s.push_str(&format!(" xmlns:p{d}=\"u{d}\""));
    }
    s.push('>');
    for _ in 0..50_000 {
        s.push_str("<p1022:k p0:a=\"1\"/>");
    }
    s.push_str("</e>");
    assert_eq!(guarded("d10b", 10, s).0, Ok(()));
}

/// 反复打开、关闭带 1023 个声明的元素：出栈也必须是 O(声明数)
#[test]
fn d10_push_pop_many_bindings() {
    let mut el = String::from("<e");
    for d in 0..1023 {
        el.push_str(&format!(" xmlns:p{d}=\"u\""));
    }
    el.push_str("><p5:k/></e>");
    let s = format!("<r>{}</r>", el.repeat(300));
    assert_eq!(guarded("d10c", 10, s).0, Ok(()));
}

// ───────────── D1 变体：实体替换文本必须自身匹配 content ─────────────

#[test]
fn d1_nested_entity_open_then_close_inside() {
    assert_syntax(r#"<!DOCTYPE r [<!ENTITY c "</a>"><!ENTITY o "<a>&c;">]><r>&o;</r>"#);
}

#[test]
fn d1_nested_entity_close_then_open() {
    assert_syntax(r#"<!DOCTYPE r [<!ENTITY o "<a>"><!ENTITY c "&o;</a>">]><r>&c;</r>"#);
}

/// w 的替换文本“&o;&c;”整体平衡，但 o、c 各自不平衡：每个实体都必须单独良构
#[test]
fn d1_split_across_two_sibling_entities() {
    assert_syntax(
        r#"<!DOCTYPE r [<!ENTITY o "<a>"><!ENTITY c "</a>"><!ENTITY w "&o;&c;">]><r>&w;</r>"#,
    );
}

#[test]
fn d1_nested_balanced_ok() {
    assert_wf(r#"<!DOCTYPE r [<!ENTITY b "<b/>"><!ENTITY a "<a>&b;</a>">]><r>&a;</r>"#);
}

#[test]
fn d1_cdata_pi_comment_inside_entity() {
    assert_syntax(r#"<!DOCTYPE r [<!ENTITY e "<![CDATA[x">]><r>&e;]]></r>"#);
    assert_syntax(r#"<!DOCTYPE r [<!ENTITY e "x]]>">]><r><![CDATA[a&e;</r>"#);
    assert_syntax(r#"<!DOCTYPE r [<!ENTITY e "<?p x">]><r>&e;?></r>"#);
    assert_wf(r#"<!DOCTYPE r [<!ENTITY e "<![CDATA[<x>]]>">]><r>&e;</r>"#);
    assert_wf(r#"<!DOCTYPE r [<!ENTITY e "<?p x?>">]><r>&e;</r>"#);
    assert_wf(r#"<!DOCTYPE r [<!ENTITY e "<!--c-->">]><r>&e;</r>"#);
}

/// 字符引用、实体引用不能跨实体边界拼出来
#[test]
fn d1_reference_split_across_entity() {
    assert_syntax(r#"<!DOCTYPE r [<!ENTITY e "&#38;#6">]><r>&e;0;</r>"#);
    assert_syntax(r#"<!DOCTYPE r [<!ENTITY e "&#38;am">]><r>&e;p;</r>"#);
    assert_wf(r#"<!DOCTYPE r [<!ENTITY e "&#38;amp;">]><r>&e;</r>"#);
}

/// 实体里的结束标签不能关掉外层元素；实体里不能有 DOCTYPE 或 xml 声明
#[test]
fn d1_entity_cannot_close_outer_or_declare() {
    assert_syntax(r#"<!DOCTYPE r [<!ENTITY e "</r><r>">]><r>&e;</r>"#);
    assert_syntax(r#"<!DOCTYPE r [<!ENTITY e "<?xml version='1.0'?>">]><r>&e;</r>"#);
    assert_syntax(r#"<!DOCTYPE r [<!ENTITY e "<!DOCTYPE x>">]><r>&e;</r>"#);
}

/// “]]>”：实体里字面出现不行；实体只给出“]]”，后面的 ">" 在实体外，是合法的
#[test]
fn d1_cdata_end_and_entity_boundary() {
    assert_syntax(r#"<!DOCTYPE r [<!ENTITY e "]]&#62;">]><r>&e;</r>"#);
    assert_wf(r#"<!DOCTYPE r [<!ENTITY e "]]">]><r>&e;></r>"#);
    assert_wf("<r>]]&#62;</r>");
    assert_wf("<r>]]&gt;</r>");
    assert_syntax("<r>]]></r>");
}

/// 被引用的实体无论出现在哪里都必须良构（§2.1、§4.3.2）：替换文本是“]]>”的实体，
/// 即使只在属性值里引用也应拒绝；从未引用则不影响
#[test]
fn d1_referenced_entity_must_be_well_formed_even_in_attribute() {
    assert_syntax(r#"<!DOCTYPE r [<!ENTITY e "]]&#62;">]><r v="&e;"/>"#);
    assert_wf(r#"<!DOCTYPE r [<!ENTITY e "]]&#62;">]><r/>"#);
    assert_wf(r#"<!DOCTYPE r [<!ENTITY e "<a">]><r/>"#);
}

// ───────────── D2 变体：属性值里经实体（含嵌套）引入的 '<' ─────────────

#[test]
fn d2_nested_entities_in_attribute() {
    assert_syntax(r#"<!DOCTYPE r [<!ENTITY b "<"><!ENTITY a "x&b;">]><r v="&a;"/>"#);
    assert_syntax(
        r#"<!DOCTYPE r [<!ENTITY c "a&#60;b"><!ENTITY b "&c;"><!ENTITY a "&b;">]><r v="&a;"/>"#,
    );
    // ATTLIST 默认值同样适用
    assert_syntax(r#"<!DOCTYPE r [<!ENTITY l "<"><!ATTLIST r v CDATA "&l;">]><r/>"#);
}

/// 双重转义的 '<'、引号、'&' 在属性里是合法的；实体替换文本里的字面 TAB 要规范化为空格，字符引用产生的 TAB 保留
#[test]
fn d2_attribute_entity_values() {
    let one = |x: &str| match first_start(x) {
        Node::Start { attrs, .. } => attrs[0].1.clone(),
        other => panic!("{other:?}"),
    };
    assert_eq!(
        one(r#"<!DOCTYPE r [<!ENTITY a "&#38;#60;">]><r v="&a;"/>"#),
        "<"
    );
    assert_eq!(one(r#"<!DOCTYPE r [<!ENTITY q '"'>]><r v="&q;"/>"#), "\"");
    assert_eq!(
        one(r#"<!DOCTYPE r [<!ENTITY e "a&#38;#38;b">]><r v="&e;"/>"#),
        "a&b"
    );
    assert_eq!(
        one(r#"<!DOCTYPE r [<!ENTITY t "a&#9;b">]><r v="&t;"/>"#),
        "a b"
    );
    assert_eq!(
        one(r#"<!DOCTYPE r [<!ENTITY t "a&#38;#9;b">]><r v="&t;"/>"#),
        "a\tb"
    );
}

// ───────────── D12 变体：默认属性与限额、命名空间 ─────────────

fn attlist_doc(n: usize, explicit: usize) -> String {
    let mut s = String::from("<!DOCTYPE e [");
    for i in 0..n {
        s.push_str(&format!("<!ATTLIST e d{i} CDATA \"v\">"));
    }
    s.push_str("]><e");
    for i in 0..explicit {
        s.push_str(&format!(" x{i}=\"1\""));
    }
    s.push_str("/>");
    s
}

/// 补上的默认属性计入 max_attrs（1024，严格大于才报错）
#[test]
fn d12_defaults_count_toward_max_attrs() {
    let lim = |x: String| {
        common::parse_default(x.as_bytes())
            .map(|_| ())
            .map_err(|f| (f.class, f.limit))
    };
    assert_eq!(lim(attlist_doc(1024, 0)), Ok(()));
    assert_eq!(
        lim(attlist_doc(1025, 0)),
        Err((ErrorClass::SecurityLimit, Some("Attributes".to_string())))
    );
    assert_eq!(lim(attlist_doc(1000, 24)), Ok(()));
    assert_eq!(
        lim(attlist_doc(1000, 25)),
        Err((ErrorClass::SecurityLimit, Some("Attributes".to_string())))
    );
}

/// 默认值里的 xmlns / xmlns:p 参与命名空间处理
#[test]
fn d12_xmlns_defaults_bind_namespaces() {
    let got = structure(
        br#"<!DOCTYPE r [<!ATTLIST r xmlns CDATA "urn:d"><!ATTLIST c xmlns:p CDATA "urn:p">]><r><c><p:x/></c></r>"#,
    )
    .unwrap();
    assert_eq!(got[0], start(Some("urn:d"), "r", &[]));
    assert_eq!(got[1], start(Some("urn:d"), "c", &[]));
    assert_eq!(got[2], start(Some("urn:p"), "x", &[]));
}

/// 同一属性重复声明 10 万次：只有第一次生效，也不应放大
#[test]
fn d12_duplicate_attlist_first_wins() {
    let mut s = String::from("<!DOCTYPE r [<!ATTLIST e a CDATA \"first\">");
    s.push_str(&"<!ATTLIST e a CDATA \"later\">".repeat(100_000));
    s.push_str("]><r>");
    s.push_str(&"<e/>".repeat(10_000));
    s.push_str("</r>");
    assert_eq!(guarded("d12dup", 5, s.clone()).0, Ok(()));
    let small = "<!DOCTYPE e [<!ATTLIST e a CDATA \"first\"><!ATTLIST e a CDATA \"later\">]><e/>";
    assert_eq!(first_start(small), start(None, "e", &[("a", "first")]));
}

// ───────────── D14（新）：ATTLIST 默认值逐元素整串复制 → CPU 放大 ─────────────

/// 4 MiB 默认值 + 2 万个空元素（输入约 4.3 MB）。线性实现只需毫秒级；
/// 当前实现每个元素复制两次默认值（`value.clone()` 收集一次，`RawStr::Owned` 再给事件一次），约 160 GB memcpy
#[test]
fn d14_attlist_default_not_copied_per_element() {
    let s = format!(
        "<!DOCTYPE r [<!ATTLIST e a CDATA \"{}\">]><r>{}</r>",
        "v".repeat(4 << 20),
        "<e/>".repeat(20_000)
    );
    let (r, peak) = guarded("d14", 5, s);
    assert_eq!(r, Ok(()));
    assert!(peak < 32 << 20, "峰值 {peak} 字节");
}

// ───────────── D15（新）：默认属性的“是否已显式给出”检查是 O(显式 × 默认) 且每次比较都分配 ─────────────

/// 1024 个默认属性 + 每个元素显式给全 1024 个同名属性。相同输入去掉 ATTLIST 作为对照，
/// 两者耗时之比应在常数范围内（当前约 60 倍）
#[test]
fn d15_default_presence_check_not_quadratic() {
    let mut el = String::from("<e");
    for i in 0..1024 {
        el.push_str(&format!(" a{i}=\"\""));
    }
    el.push_str("/>");
    let body = format!("<r>{}</r>", el.repeat(50));
    let mut dtd = String::from("<!DOCTYPE r [");
    for i in 0..1024 {
        dtd.push_str(&format!("<!ATTLIST e a{i} CDATA \"v\">"));
    }
    dtd.push_str("]>");
    let with = format!("{dtd}{body}");
    let without = format!("<!DOCTYPE r []>{body}");
    let time = |x: &str| {
        let t = Instant::now();
        assert_eq!(class(x), Ok(()));
        t.elapsed()
    };
    let _ = time(&without); // 预热
    let a = time(&with);
    let b = time(&without);
    assert!(
        a < b * 5 + Duration::from_millis(50),
        "带 ATTLIST {a:?} vs 不带 {b:?}：默认属性检查随 显式×默认 增长"
    );
}

// ───────────── D16（新）：Writer 出错时已写出半截内容；XmlDecl / Doctype 字段不校验 ─────────────

#[test]
fn d16_writer_error_leaves_no_partial_output() {
    use purexml::Writer;
    let mut w = Writer::new(Vec::new());
    assert!(w.write_start("r", &[("a", "ok"), ("b", "\u{1}")]).is_err());
    assert!(
        w.get_ref().is_empty(),
        "出错后 sink 里留下了 {:?}",
        String::from_utf8_lossy(w.get_ref())
    );
}

#[test]
fn d16_writer_validates_xmldecl_and_doctype() {
    use purexml::{Doctype, Event, Writer, XmlDecl, XmlText};
    let mut w = Writer::new(Vec::new());
    let decl = XmlDecl {
        version: XmlText::borrowed("1.0\"?><evil/>"),
        encoding: None,
        standalone: None,
    };
    assert!(
        w.write_event(&Event::XmlDecl(decl)).is_err(),
        "非法 version 应报错"
    );
    let mut w = Writer::new(Vec::new());
    let dt = Doctype {
        name: XmlText::borrowed("r"),
        system_id: Some(XmlText::borrowed("s")),
        public_id: Some(XmlText::borrowed("<bad>")),
        has_internal_subset: false,
    };
    assert!(
        w.write_event(&Event::Doctype(dt)).is_err(),
        "非法 PubidChar 应报错"
    );
}

// ───────────── D17（新）：ATTLIST 默认属性名不做 QName 校验 ─────────────
//
// writer_roundtrip fuzz 发现（crash-42afd53c）：`a:`、`:a` 这类不是 QName 的名字被当成本地名补到开始标签上，
// 显式写 `<e a:="v"/>` 时会被拒绝，补默认值这条路径却放过了；结果是产生命名空间不良构的事件，Writer 写不出来。

#[test]
fn d17_default_attribute_names_must_be_qnames() {
    assert_syntax(r#"<!DOCTYPE e [<!ATTLIST e a: CDATA "v">]><e/>"#);
    assert_syntax(r#"<!DOCTYPE e [<!ATTLIST e :a CDATA "v">]><e/>"#);
    assert_syntax(r#"<!DOCTYPE e [<!ATTLIST e a:b:c CDATA "v">]><e xmlns:a="urn:a"/>"#);
    // 合法的带前缀默认属性照常解析
    assert_wf(r#"<!DOCTYPE e [<!ATTLIST e p:a CDATA "v">]><e xmlns:p="urn:p"/>"#);
}

// ───────────── D19：属性去重按 URI 内容哈希 → CPU 随 属性数 × URI 长度 增长 ─────────────

/// 1 MiB 命名空间 URI，10 个元素各带 1000 个该前缀的属性（输入约 1.1 MB）。
/// URI 已驻留（Rc<str>），去重只需比较指针；实现目前对每个属性都把整条 URI 哈希一遍，
/// 实测约 0.6 s / 元素，即每个元素约 1 GiB 的哈希量。要求 2 秒内完成。
#[test]
fn d19_expanded_name_dedup_not_proportional_to_uri_length() {
    let mut el = String::from("<p:e");
    for i in 0..1000 {
        el.push_str(&format!(" p:a{i}=\"1\""));
    }
    el.push_str("/>");
    let s = format!(
        "<r xmlns:p=\"{}\">{}</r>",
        "u".repeat(1 << 20),
        el.repeat(10)
    );
    let t = Instant::now();
    let (r, _) = guarded("d19", 30, s);
    assert_eq!(r, Ok(()));
    let e = t.elapsed();
    assert!(
        e < Duration::from_secs(2),
        "用时 {e:?}：属性去重随 URI 长度线性增长"
    );
}
