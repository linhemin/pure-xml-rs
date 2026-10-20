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

// ═════════════ 第三轮（ddaaacc）新增 ═════════════
//
// 按 ddaaacc 的 XmlText API 编写。各缺陷的实测数据见 M1-test-report.md 第三轮。

fn failure(x: &str) -> Option<common::Failure> {
    common::parse_default(x.as_bytes()).err()
}

// ───────────── D14 残留：字面默认值每个元素都重新做一遍 UTF-8 校验 ─────────────

/// 16 MiB 字面 CDATA 默认值，加 1 万个 `<e/>`（输入约 16.8 MB，其中元素部分只有 40 KB）。
/// 6b29230 把字面默认值保存为输入里的区间，每次物化时都调用 `std::str::from_utf8` 重新校验整段，
/// 实测 6.3 s（约 0.63 ms/元素）。按 Lead 的 O(1) 共享语义，应在 1.5 s 内完成，主要耗时只是读一遍输入。
#[test]
fn d14_literal_default_not_revalidated_per_element() {
    let s = format!(
        "<!DOCTYPE r [<!ATTLIST e a CDATA \"{}\">]><r>{}</r>",
        "v".repeat((16 << 20) - 64),
        "<e/>".repeat(10_000)
    );
    let t = Instant::now();
    let (r, peak) = guarded("d14r3", 60, s);
    let e = t.elapsed();
    assert_eq!(r, Ok(()));
    assert!(peak < 64 << 20, "峰值 {peak}");
    assert!(
        e < Duration::from_millis(1500),
        "用时 {e:?}：默认值随元素数整段重扫"
    );
}

// ───────────── D21（新）：默认属性的数量放大不受任何限额约束 ─────────────

/// 1024 个默认属性，加 5 万个 `<e/>`（输入约 300 KB）。每个 4 字节的空元素要补出 1024 个属性，
/// 而且每次都复制属性名、建 HashSet，实测约 11 s。安全优先的实现要么把补出的默认属性计入限额
/// （展开字节或比例）并报 SecurityLimit，要么做到与默认属性数无关。两种结果都接受，只要求在 2 s 内结束。
#[test]
fn d21_default_attribute_amplification_bounded() {
    let mut d = String::from("<!DOCTYPE r [");
    for i in 0..1024 {
        d.push_str(&format!("<!ATTLIST e attribute_name_{i:04} CDATA \"\">"));
    }
    d.push_str("]>");
    let s = format!("{d}<r>{}</r>", "<e/>".repeat(50_000));
    let t = Instant::now();
    let (r, _) = guarded("d21", 60, s);
    let e = t.elapsed();
    assert!(
        matches!(r, Ok(()) | Err(ErrorClass::SecurityLimit)),
        "{r:?}"
    );
    assert!(
        e < Duration::from_secs(2),
        "用时 {e:?}：默认属性放大没有上界"
    );
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

// ───────────── D23（新）：PI 目标、实体名、记号名里的冒号 ─────────────
//
// Namespaces in XML §7："No entity names, processing instruction targets, or notation names contain any colons."
// 6b29230 和 ddaaacc 全部接受；Writer 又拒绝 `a:` 但接受 `a:b`，所以解析成功的事件会写不出去（writer_roundtrip crash-543336e4）。

#[test]
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
