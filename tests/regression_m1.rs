//! M1 第二轮（修复后）对抗性回归用例，2026-10-21 由 Tester 编写。
//!
//! - `d13_*` / `d10_*` / `d1_*` / `d2_*` / `d12_*`：针对已修复缺陷的变体，修复后应当全部通过。
//! - `d14_*` ～ `d17_*`、`d19_*`：第二轮新发现的缺陷（D18 栈溢出见 `stack_depth_m1.rs`），修复前会失败，按约定保持开启。
//!
//! - 第三轮及之后：D14、D20、D21、D22 按 Lead 裁决 M1 内修，测试保持开启；D16 已修好，不再 ignore；
//!   只有 D23 移到 M2，对应测试标 `#[ignore = "M2: D23"]`（断言不变），用 `cargo test -- --ignored` 运行。
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
    // 严格 D21 口径下 4 MiB × 2 万次注入远超默认 10 倍比例；本用例只测“不逐元素复制”，故显式调高比例
    let cfg = purexml::ParserConfig::default().with_max_expansion_ratio(1_000_000);
    let (r, peak, _) = parse_cfg_guarded("d14", 5, s, cfg);
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

// ═════════════ 第三轮（ddaaacc）新增 ═════════════
//
// 按 ddaaacc 的 XmlText API 编写。各缺陷的实测数据见 M1-test-report.md 第三轮。

fn failure(x: &str) -> Option<common::Failure> {
    common::parse_default(x.as_bytes()).err()
}

// ───────────── D14 残留：字面默认值每个元素都重新做一遍 UTF-8 校验 ─────────────
//
// D21 严格口径定下以后（每次注入都计“名字 + 值的完整字节”），16 MiB 默认值 × 1 万个元素
// 在默认配置下会被 ExpansionRatio 正确拒绝。所以 D14 的主用例显式调高 `ParserConfig::max_expansion_ratio`
// （793b612 中的字段：`pub max_expansion_ratio: u64`，构造器 `with_max_expansion_ratio(u64)`，默认 10），
// 只考察“不重复校验、不重复复制”；另有一个密度较低、默认配置下就在 10 倍以内的变体。

/// 用指定配置把文档拉完，在 run_guarded 线程里运行，返回 (结果, 峰值内存, 用时)。
fn parse_cfg_guarded(
    name: &str,
    secs: u64,
    s: String,
    cfg: purexml::ParserConfig,
) -> (Result<(), String>, usize, Duration) {
    let t = Instant::now();
    match run_guarded(name, Duration::from_secs(secs), move || {
        let mut r = purexml::Reader::from_bytes(s.as_bytes()).with_config(cfg);
        loop {
            match r.read_event() {
                Ok(Some(_)) => {}
                Ok(None) => return Ok(()),
                Err(e) => return Err(format!("{:?}", e.kind)),
            }
        }
    }) {
        Guarded::Done {
            value, peak_bytes, ..
        } => (value, peak_bytes, t.elapsed()),
        Guarded::TimedOut => panic!("{name}：超过 {secs} 秒"),
        Guarded::Panicked(m) => panic!("{name}：panic {m}"),
    }
}

/// 16 MiB 字面 CDATA 默认值，加 1 万个 `<e/>`（输入约 16.8 MB，其中元素部分只有 40 KB）。
/// ddaaacc 每次物化都对整段调用 `std::str::from_utf8`，release 实测 7.1–7.9 s（约 0.7 ms/元素）。
///
/// 严格口径下，注入量约 10 000 × 16 MiB ≈ 1.7e11 字节，需要比例 ≥ 约 10 001，这里设为 100 000
/// （consumed × ratio 约 1.7e12，不会溢出 u64）。
/// 阈值用同一 DTD、只有 1 个 `<e/>` 的对照自标定：1 万个元素的用时不得超过 `对照 × 1.5 + 500 ms`；
/// 峰值内存 < 64 MiB（没有逐元素复制 16 MiB）。线性实现多出的 1 万个空元素只需几毫秒。
#[test]
fn d14_literal_default_not_revalidated_per_element() {
    let doc = |n: usize| {
        format!(
            "<!DOCTYPE r [<!ATTLIST e a CDATA \"{}\">]><r>{}</r>",
            "v".repeat((16 << 20) - 64),
            "<e/>".repeat(n)
        )
    };
    let cfg = purexml::ParserConfig::default().with_max_expansion_ratio(100_000);
    let (r1, _, base) = parse_cfg_guarded("d14r3-base", 120, doc(1), cfg.clone());
    assert_eq!(r1, Ok(()));
    let (r, peak, e) = parse_cfg_guarded("d14r3", 120, doc(10_000), cfg);
    assert_eq!(r, Ok(()), "调高比例后应放行");
    assert!(peak < 64 << 20, "峰值 {peak}");
    let limit = base.mul_f64(1.5) + Duration::from_millis(500);
    assert!(
        e < limit,
        "1 万个元素用时 {e:?}，对照（1 个元素）{base:?}，阈值 {limit:?}：默认值随元素数整段重扫"
    );
}

/// 低密度变体（默认配置，本来就在 10 倍以内）：1 MiB 默认值，100 个 `<e>` 各带 120 KiB 文本。
/// 严格口径下注入量与已消费输入之比趋近 1 MiB / 120 KiB ≈ 8.5 倍，低于 10 倍，必须放行。
/// “不重复复制”直接断言：100 个元素拿到的默认值指向同一块内存（共享，不是每个元素复制一份），
/// 长度正确，内容正确（指针和长度都相同，所以只需检查第一个的内容）。
/// 共享断言已经直接证明没有复制；时间断言只是辅助，用来捕捉逐元素重扫，只在 release
/// （`cfg(not(debug_assertions))`）下生效，阈值为对照 × 3 + 1 s。对照是同一 DTD、元素名换成 `<f>`
/// （不触发注入）的同长文档。计时只包含解析，不包含测试自己的内容检查。
/// 第四轮稳定性修正：原版把 100 × 1 MiB 的逐字节内容检查算进了计时，对照又不含 1 MiB 的 DTD，
/// debug 下在本机 5/5 次失败（约 1.07 s，阈值约 0.85 s）。
#[test]
fn d14_low_density_default_shared_under_default_ratio() {
    let dtd = format!(
        "<!DOCTYPE r [<!ATTLIST e a CDATA \"{}\">]>",
        "v".repeat(1 << 20)
    );
    let text = "t".repeat(120 << 10);
    let with = format!("{dtd}<r>{}</r>", format!("<e>{text}</e>").repeat(100));
    let control = format!("{dtd}<r>{}</r>", format!("<f>{text}</f>").repeat(100));
    let scan = |s: &str| -> (Vec<(usize, usize)>, Option<bool>, Duration) {
        let t = Instant::now();
        let mut r = purexml::Reader::from_bytes(s.as_bytes());
        let mut seen = Vec::new();
        let mut first_ok = None;
        while let Some(ev) = r.read_event().expect("默认配置下应放行") {
            if let purexml::Event::StartElement(el) = ev {
                if let Some(a) = el.attribute("a") {
                    if first_ok.is_none() {
                        first_ok = Some(a.value.bytes().all(|b| b == b'v'));
                    }
                    seen.push((a.value.as_ptr() as usize, a.value.len()));
                }
            }
        }
        (seen, first_ok, t.elapsed())
    };
    let _ = scan(&control);
    let (none, _, base) = scan(&control);
    assert!(none.is_empty());
    let (seen, first_ok, e) = scan(&with);
    assert_eq!(seen.len(), 100, "每个 <e> 都应补上默认值");
    assert_eq!(first_ok, Some(true), "默认值内容错误");
    assert!(seen.iter().all(|&(_, n)| n == 1 << 20));
    assert!(
        seen.iter().all(|&(p, _)| p == seen[0].0),
        "默认值没有共享：各元素的值指向不同内存（逐元素复制）"
    );
    if cfg!(not(debug_assertions)) {
        let limit = base * 3 + Duration::from_secs(1);
        assert!(e < limit, "用时 {e:?}，对照 {base:?}，阈值 {limit:?}");
    }
}

// ───────────── D21（新）：默认属性的注入不受放大预算约束 ─────────────
//
// Lead 第三轮裁决的口径：注入的默认属性字节与实体展开**共用同一个放大计数**，只计入 10 倍比例预算
// （累计 ≤ 64 KiB 时不查比例），**不计入** 1 MiB 绝对上限（max_expanded_bytes）。
// 严格口径（Lead 已定）：**每次**注入默认属性，都把“名字字节 + 值的完整字节”计入比例分子；
// 共享值（Arc，不复制）也不打折，每次注入都计全长。下面所有 D21 断言都按这个口径写。

fn d21_failure_fast(name: &str, s: String, budget: Duration) -> common::Failure {
    let t = Instant::now();
    let r = match run_guarded(name, Duration::from_secs(60), move || {
        common::parse_default(s.as_bytes()).map(|_| ())
    }) {
        Guarded::Done { value, .. } => value,
        Guarded::TimedOut => panic!("{name}：超过 60 秒"),
        Guarded::Panicked(m) => panic!("{name}：panic {m}"),
    };
    let e = t.elapsed();
    let f = r.expect_err(&format!(
        "{name}：应报 SecurityLimit，实际解析成功（用时 {e:?}）"
    ));
    assert_eq!(f.class, ErrorClass::SecurityLimit, "{name}：{f:?}");
    assert!(e < budget, "{name}：用时 {e:?}，应在 {budget:?} 内报错");
    f
}

/// 1024 个默认属性（值为 1 字节 `v`），加 5 万个 `<e/>`，输入约 243 KB。
/// 只算值字节，每个 `<e/>` 就注入 1024 字节，几百个元素后累计超过 64 KiB 且超过 10 倍比例，
/// 应当很快（< 1 s）报 SecurityLimit(ExpansionRatio)。ddaaacc 上的同类样本（值为空串）要 13.2 s 且成功。
/// 注：第三轮报告里的原始样本默认值是空串，只有“计入名字”才会触发，已移到 d21_name_bytes_*。
#[test]
fn d21_default_attribute_amplification_bounded() {
    let mut d = String::from("<!DOCTYPE r [");
    for i in 0..1024 {
        d.push_str(&format!("<!ATTLIST e attribute_name_{i:04} CDATA \"v\">"));
    }
    d.push_str("]>");
    let s = format!("{d}<r>{}</r>", "<e/>".repeat(50_000));
    assert!(s.len() < 250_000, "样本大小 {}", s.len());
    let f = d21_failure_fast("d21", s, Duration::from_secs(1));
    assert_eq!(f.limit.as_deref(), Some("ExpansionRatio"), "{f:?}");
}

/// 计费口径（Lead 已确认）：每个注入的默认属性按“属性名字节数 + 值字节数”计入共用放大计数，
/// 只受 10 倍比例约束（累计 ≤ 64 KiB 不检查）。下面两个样本的默认值都是空串，只靠名字字节触发：
/// - 1024 个 1 KiB 长名字的默认属性 × 2 万个 `<e/>`：ddaaacc 上 22.5 s 且成功；
/// - 第三轮原始样本：1024 个默认属性（值为空串）× 5 万个 `<e/>`（242 KB）：ddaaacc 上 13.2 s 且成功。
///
/// 两者都应在 1 s 内报 SecurityLimit(ExpansionRatio)。
#[test]
fn d21_name_bytes_variants_fail_fast() {
    let mut d = String::from("<!DOCTYPE r [");
    for i in 0..1024 {
        d.push_str(&format!(
            "<!ATTLIST e n{i:04}{} CDATA \"\">",
            "x".repeat(1024 - 5)
        ));
    }
    d.push_str("]>");
    let f = d21_failure_fast(
        "d21-1kib-names",
        format!("{d}<r>{}</r>", "<e/>".repeat(20_000)),
        Duration::from_secs(1),
    );
    assert_eq!(f.limit.as_deref(), Some("ExpansionRatio"), "{f:?}");

    let mut d = String::from("<!DOCTYPE r [");
    for i in 0..1024 {
        d.push_str(&format!("<!ATTLIST e attribute_name_{i:04} CDATA \"\">"));
    }
    d.push_str("]>");
    let f = d21_failure_fast(
        "d21-empty-values",
        format!("{d}<r>{}</r>", "<e/>".repeat(50_000)),
        Duration::from_secs(1),
    );
    assert_eq!(f.limit.as_deref(), Some("ExpansionRatio"), "{f:?}");
}

/// 压线（只含空值、长名字的默认属性）：一个 1000 字节名字的空值默认属性。
/// - 65 个 `<e/>` 累计 65 000 字节，刚好在 64 KiB（65 536）下方 → 通过；
/// - 1024 字节名字 × 64 个 `<e/>` = 65 536，正好等于 64 KiB（“≤ 64 KiB 不检查”）→ 通过；
/// - 1000 字节名字 × 66 个 `<e/>` = 66 000 > 64 KiB，输入约 1.3 KB（允许约 13 KB）→ 报 ExpansionRatio。
#[test]
fn d21_name_bytes_ratio_floor_boundary() {
    let doc = |name_len: usize, n: usize| {
        format!(
            "<!DOCTYPE r [<!ATTLIST e n{} CDATA \"\">]><r>{}</r>",
            "x".repeat(name_len - 1),
            "<e/>".repeat(n)
        )
    };
    assert_eq!(
        class(&doc(1000, 65)),
        Ok(()),
        "名字累计 65 000 字节 < 64 KiB，不查比例"
    );
    assert_eq!(
        class(&doc(1024, 64)),
        Ok(()),
        "名字累计 65 536 字节 = 64 KiB，不查比例"
    );
    let f = failure(&doc(1000, 66)).expect("名字累计 66 000 字节 > 64 KiB 且超过 10 倍，应报错");
    assert_eq!(f.class, ErrorClass::SecurityLimit);
    assert_eq!(f.limit.as_deref(), Some("ExpansionRatio"), "{f:?}");
}

/// 下游放大回归（严格口径）：1 MiB 共享默认值 × 1 万个 `<e/>`，输入约 1.09 MB。
/// 解析器本身只共享一份 Arc，但下游对每个属性值 `into_owned`/`to_string` 会分配约 10 GiB，
/// Writer 写回的输出也约 10 GiB。严格口径下第 11 个元素左右分子就超过 10 × 已消费输入，
/// 应当很快（< 1 s）报 SecurityLimit(ExpansionRatio)。793b612 只在第一次注入时计值长，会放行。
#[test]
fn d21_shared_large_default_counts_every_injection() {
    let s = format!(
        "<!DOCTYPE r [<!ATTLIST e a CDATA \"{}\">]><r>{}</r>",
        "v".repeat(1 << 20),
        "<e/>".repeat(10_000)
    );
    assert!(s.len() < 1_100_000, "样本大小 {}", s.len());
    let f = d21_failure_fast("d21-shared-1mib", s, Duration::from_secs(1));
    assert_eq!(f.limit.as_deref(), Some("ExpansionRatio"), "{f:?}");
}

/// 绝对上限不误杀：正常的大文档，大量普通文本配一个 100 字节的默认属性。
/// 2 万个 `<e>` 各带 100 字节文本，注入总量约 2 MB（超过 1 MiB 绝对上限），但比例约 1 倍，必须通过。
#[test]
fn d21_default_injection_not_subject_to_absolute_cap() {
    let s = format!(
        "<!DOCTYPE r [<!ATTLIST e a CDATA \"{}\">]><r>{}</r>",
        "v".repeat(100),
        format!("<e>{}</e>", "t".repeat(100)).repeat(20_000)
    );
    let (r, _) = guarded("d21-abs", 60, s);
    assert_eq!(r, Ok(()), "注入超过 1 MiB 但比例约 1 倍，不应报错");
}

/// 压线：只有注入时，累计 ≤ 64 KiB 不查比例。
/// 100 字节默认值 × 600 个 `<e/>` = 60 000 字节值（加上名字也只有约 6 万），输入约 2.6 KB（比例约 23 倍）→ 必须通过；
/// × 700 个 = 70 000 字节，超过 64 KiB，此时允许量约 10 × 2.9 KB → 报 SecurityLimit(ExpansionRatio)。
#[test]
fn d21_injection_ratio_floor_boundary() {
    let doc = |n: usize| {
        format!(
            "<!DOCTYPE r [<!ATTLIST e a CDATA \"{}\">]><r>{}</r>",
            "v".repeat(100),
            "<e/>".repeat(n)
        )
    };
    assert_eq!(
        class(&doc(600)),
        Ok(()),
        "注入约 60 000 字节 ≤ 64 KiB，不查比例"
    );
    let f = failure(&doc(700)).expect("注入 70 000 字节 > 64 KiB 且超过 10 倍，应报错");
    assert_eq!(f.class, ErrorClass::SecurityLimit);
    assert_eq!(f.limit.as_deref(), Some("ExpansionRatio"), "{f:?}");
}

/// 实体展开与默认值注入共用计数：实体 1000 字节 × 40 次 = 40 000，默认值 100 字节 × 400 个 = 40 000，
/// 各自都 ≤ 64 KiB（单独出现时必须通过），合计 80 000 > 64 KiB，而输入只有约 3.5 KB（允许约 35 KB）→ 报 ExpansionRatio。
#[test]
fn d21_entities_and_defaults_share_budget() {
    let dtd = format!(
        "<!DOCTYPE r [<!ENTITY x \"{}\"><!ATTLIST e a CDATA \"{}\">]>",
        "y".repeat(1000),
        "v".repeat(100)
    );
    let refs = "<t>&x;</t>".repeat(40);
    let es = "<e/>".repeat(400);
    assert_eq!(
        class(&format!("{dtd}<r>{refs}</r>")),
        Ok(()),
        "只有实体展开 40 000 字节，应通过"
    );
    assert_eq!(
        class(&format!("{dtd}<r>{es}</r>")),
        Ok(()),
        "只有默认值注入约 40 000 字节，应通过"
    );
    let f =
        failure(&format!("{dtd}<r>{refs}{es}</r>")).expect("合计 80 000 字节超过比例预算，应报错");
    assert_eq!(f.class, ErrorClass::SecurityLimit);
    assert_eq!(f.limit.as_deref(), Some("ExpansionRatio"), "{f:?}");
}

// ───────────── D16 残留：EndElement 出错时留下 `</`；PUBLIC 缺少系统标识 ─────────────

#[test]
fn d16_end_element_error_leaves_no_partial_output() {
    use purexml::{EndElement, Event, QName, Writer, XmlText};
    let mut w = Writer::new(Vec::new());
    let ev = Event::EndElement(EndElement {
        name: QName {
            prefix: None,
            local: XmlText::borrowed("a b"),
            namespace: None,
        },
    });
    assert!(w.write_event(&ev).is_err());
    assert!(
        w.get_ref().is_empty(),
        "出错后 sink 里留下了 {:?}",
        String::from_utf8_lossy(w.get_ref())
    );
}

/// `<!DOCTYPE r PUBLIC "p">` 不良构：doctypedecl 里的 ExternalID 写 PUBLIC 时必须同时给出 SystemLiteral
#[test]
fn d16_doctype_public_requires_system_id() {
    use purexml::{Doctype, Event, Writer, XmlText};
    let mut w = Writer::new(Vec::new());
    let dt = Doctype {
        name: XmlText::borrowed("r"),
        system_id: None,
        public_id: Some(XmlText::borrowed("p")),
        has_internal_subset: false,
    };
    let r = w.write_event(&Event::Doctype(dt));
    assert!(
        r.is_err(),
        "写出了不良构的 {:?}",
        String::from_utf8_lossy(w.get_ref())
    );
}

// ───────────── D22（新）：Writer 不检查重复属性和保留的命名空间声明 ─────────────

#[test]
fn d22_writer_rejects_duplicate_attributes() {
    use purexml::Writer;
    let mut w = Writer::new(Vec::new());
    assert!(
        w.write_start("a", &[("x", "1"), ("x", "2")]).is_err(),
        "写出了不良构的 {:?}",
        String::from_utf8_lossy(w.get_ref())
    );
    let mut w = Writer::new(Vec::new());
    assert!(w.write_empty("a", &[("p:x", "1"), ("p:x", "2")]).is_err());
}

/// 前缀不同、但展开名相同的属性（Namespaces §6.3 Attributes Unique）：
/// `<a xmlns:p="urn:x" xmlns:q="urn:x" p:x="1" q:x="2">` 不是命名空间良构的，Writer 必须报错。
/// 同一元素上重复声明同一个前缀（两个 `xmlns:p`）同样是重复属性。
#[test]
fn d22_writer_rejects_duplicate_expanded_names() {
    use purexml::{
        Attribute, EmptyElement, Event, Namespace, QName, StartElement, Writer, XmlText,
    };
    use std::sync::Arc;
    let uri: Arc<str> = Arc::from("urn:x");
    let attr = |p: &'static str, v: &'static str| Attribute {
        name: QName {
            prefix: Some(XmlText::borrowed(p)),
            local: XmlText::borrowed("x"),
            namespace: Some(uri.clone()),
        },
        value: XmlText::borrowed(v),
    };
    let ns = |p: &'static str| Namespace {
        prefix: Some(XmlText::borrowed(p)),
        uri: XmlText::borrowed("urn:x"),
    };
    let name = QName {
        prefix: None,
        local: XmlText::borrowed("a"),
        namespace: None,
    };
    // 1) Event 形式：两个前缀绑定同一 URI，p:x 与 q:x 展开名相同
    let start = Event::StartElement(StartElement {
        name: name.clone(),
        attributes: vec![attr("p", "1"), attr("q", "2")],
        namespaces: vec![ns("p"), ns("q")],
    });
    let empty = Event::EmptyElement(EmptyElement {
        name: name.clone(),
        attributes: vec![attr("p", "1"), attr("q", "2")],
        namespaces: vec![ns("p"), ns("q")],
    });
    for ev in [&start, &empty] {
        let mut w = Writer::new(Vec::new());
        assert!(
            w.write_event(ev).is_err(),
            "展开名重复的属性被写出：{:?}",
            String::from_utf8_lossy(w.get_ref())
        );
    }
    // 2) Event 形式：完全相同的属性 QName 出现两次
    let mut w = Writer::new(Vec::new());
    let dup = Event::StartElement(StartElement {
        name: name.clone(),
        attributes: vec![attr("p", "1"), attr("p", "2")],
        namespaces: vec![ns("p")],
    });
    assert!(
        w.write_event(&dup).is_err(),
        "{:?}",
        String::from_utf8_lossy(w.get_ref())
    );
    // 3) 同一前缀声明两次
    let mut w = Writer::new(Vec::new());
    let dup_ns = Event::StartElement(StartElement {
        name,
        attributes: vec![],
        namespaces: vec![ns("p"), ns("p")],
    });
    assert!(
        w.write_event(&dup_ns).is_err(),
        "{:?}",
        String::from_utf8_lossy(w.get_ref())
    );
    // 4) 原始字符串形式：展开名相同
    let mut w = Writer::new(Vec::new());
    assert!(
        w.write_start(
            "a",
            &[
                ("xmlns:p", "urn:x"),
                ("xmlns:q", "urn:x"),
                ("p:x", "1"),
                ("q:x", "2")
            ]
        )
        .is_err(),
        "{:?}",
        String::from_utf8_lossy(w.get_ref())
    );
}

#[test]
fn d22_writer_rejects_reserved_namespace_declarations() {
    use purexml::{Event, Namespace, QName, StartElement, Writer, XmlText};
    for (prefix, uri) in [
        (Some("xmlns"), "urn:x"),
        (Some("p"), ""),
        (Some("xml"), "urn:not-xml"),
        (None, "http://www.w3.org/2000/xmlns/"),
    ] {
        let el = StartElement {
            name: QName {
                prefix: None,
                local: XmlText::borrowed("a"),
                namespace: None,
            },
            attributes: vec![],
            namespaces: vec![Namespace {
                prefix: prefix.map(XmlText::borrowed),
                uri: XmlText::borrowed(uri),
            }],
        };
        let mut w = Writer::new(Vec::new());
        assert!(
            w.write_event(&Event::StartElement(el)).is_err(),
            "命名空间不良构的声明被写出：{:?}",
            String::from_utf8_lossy(w.get_ref())
        );
    }
}

/// 第四轮新增：字符串形式（`write_start` / `write_empty`）同样必须拒绝保留的命名空间声明，
/// 并且出错时 sink 不变。b8c6dc1 的 Event 形式已正确拒绝（见上一个测试），字符串形式仍照样写出。
#[test]
fn d22_string_writer_rejects_reserved_namespace_declarations() {
    use purexml::Writer;
    for attrs in [
        [("xmlns:xmlns", "urn:x")],
        [("xmlns:p", "")],
        [("xmlns:xml", "urn:not-xml")],
        [("xmlns", "http://www.w3.org/2000/xmlns/")],
    ] {
        for empty in [false, true] {
            let mut w = Writer::new(Vec::new());
            w.write_start("root", &[]).unwrap();
            let before = w.get_ref().clone();
            let r = if empty {
                w.write_empty("a", &attrs)
            } else {
                w.write_start("a", &attrs)
            };
            assert!(
                r.is_err(),
                "命名空间不良构的声明被写出：{:?}",
                String::from_utf8_lossy(w.get_ref())
            );
            assert_eq!(*w.get_ref(), before, "出错后 sink 被改动");
        }
    }
}

/// 第四轮新增：字符串和 Event 两种形式的重复属性出错时，sink 都保持不变
#[test]
fn d22_writer_error_leaves_sink_unchanged() {
    use purexml::Writer;
    for attrs in [
        vec![("x", "1"), ("x", "2")],
        vec![
            ("xmlns:p", "urn:x"),
            ("xmlns:q", "urn:x"),
            ("p:x", "1"),
            ("q:x", "2"),
        ],
        vec![("xmlns:p", "u"), ("xmlns:p", "v")],
    ] {
        let mut w = Writer::new(Vec::new());
        w.write_start("root", &[]).unwrap();
        let before = w.get_ref().clone();
        assert!(w.write_start("a", &attrs).is_err());
        assert_eq!(*w.get_ref(), before, "{attrs:?}");
        assert!(w.write_empty("a", &attrs).is_err());
        assert_eq!(*w.get_ref(), before, "{attrs:?}");
    }
}

// ───────────── D23（新）：PI 目标、实体名、记号名里的冒号 ─────────────
//
// Namespaces in XML §7："No entity names, processing instruction targets, or notation names contain any colons."
// 6b29230 和 ddaaacc 全部接受；Writer 又拒绝 `a:` 但接受 `a:b`，所以解析成功的事件会写不出去（writer_roundtrip crash-543336e4）。

#[test]
#[ignore = "M2: D23"]
fn d23_colons_in_pi_entity_notation_names_rejected() {
    for x in [
        "<r><?a:b x?></r>",
        "<r><?a: x?></r>",
        "<?p:q?><r/>",
        "<!DOCTYPE r [<?a:b x?>]><r/>",
        "<!DOCTYPE r [<!ENTITY a:b \"x\">]><r/>",
        "<!DOCTYPE r [<!NOTATION a:b SYSTEM \"x\">]><r/>",
        // DOCTYPE 名是元素类型名，必须是 QName（ddaaacc 上 writer_roundtrip crash-a91aac12）
        "<!DOCTYPE r: SYSTEM \"x.dtd\"><r/>",
        "<!DOCTYPE a:b:c><r/>",
    ] {
        assert_syntax(x);
    }
    assert_wf("<r><?ab x?></r>");
}

// ───────────── D18 边界：实体替换文本内的嵌套计入元素深度 ─────────────

#[test]
fn d18_entity_nesting_depth_boundary() {
    let doc = |n: usize| {
        format!(
            "<!DOCTYPE r [<!ENTITY e \"{}{}\">]><r>&e;</r>",
            "<a>".repeat(n),
            "</a>".repeat(n)
        )
    };
    // <r> 占 1 层，实体内 255 层，合计 256 层，正好达到上限
    assert_eq!(class(&doc(255)), Ok(()));
    let f = failure(&doc(256)).expect("257 层应报错");
    assert_eq!(f.class, ErrorClass::SecurityLimit);
    assert_eq!(f.limit.as_deref(), Some("ElementDepth"));
    // 远超上限：报错且不崩溃（栈上限另见 stack_depth_m1.rs）
    let f = failure(&doc(100_000)).expect("应报错");
    assert_eq!(f.limit.as_deref(), Some("ElementDepth"));
}

// ───────────── D20 边界（默认配置）：内容模型 256 层通过，257 层报 ElementDepth ─────────────
//
// 1024 层上限和小栈余量会让 debug 构建栈溢出，单独放在 stack_depth_r3_m1.rs。

#[test]
fn d20_content_model_default_depth_boundary() {
    let doc = |n: usize| {
        format!(
            "<!DOCTYPE r [<!ELEMENT r {}a{}>]><r/>",
            "(".repeat(n),
            ")".repeat(n)
        )
    };
    let run = |s: String| {
        std::thread::Builder::new()
            .stack_size(2 << 20)
            .spawn(move || common::parse_default(s.as_bytes()).map(|_| ()))
            .unwrap()
            .join()
            .unwrap()
    };
    assert!(run(doc(256)).is_ok());
    let f = run(doc(257)).unwrap_err();
    assert_eq!(f.class, ErrorClass::SecurityLimit);
    assert_eq!(f.limit.as_deref(), Some("ElementDepth"));
    assert_eq!(f.limit_value, Some(256));
}

// ───────────── D15 / D19 第三轮复测：已修复，作为护栏 ─────────────

/// D15 放大版：1024 个默认属性，500 个元素各自显式给全 1024 个属性。带不带 ATTLIST 的耗时比应低于 2 倍
#[test]
fn d15_presence_check_scaled_up() {
    let mut el = String::from("<e");
    for i in 0..1024 {
        el.push_str(&format!(" a{i}=\"\""));
    }
    el.push_str("/>");
    let body = format!("<r>{}</r>", el.repeat(500));
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
    let _ = time(&without);
    let b = time(&without);
    let a = time(&with);
    assert!(a < b * 2 + Duration::from_millis(50), "{a:?} vs {b:?}");
}

/// D19 放大版：16 MiB URI，100 个元素各带 1000 个该前缀的属性，输入约 17.9 MB，应在 3 s 内完成
#[test]
fn d19_sixteen_mib_uri_hundred_elements() {
    let mut el = String::from("<p:e");
    for i in 0..1000 {
        el.push_str(&format!(" p:a{i}=\"1\""));
    }
    el.push_str("/>");
    let s = format!(
        "<r xmlns:p=\"{}\">{}</r>",
        "u".repeat((16 << 20) - 64),
        el.repeat(100)
    );
    let n = s.len();
    let t = Instant::now();
    let (r, peak) = guarded("d19r3", 30, s);
    assert_eq!(r, Ok(()));
    assert!(peak < 3 * n, "峰值 {peak} vs 输入 {n}");
    assert!(t.elapsed() < Duration::from_secs(3), "{:?}", t.elapsed());
}
