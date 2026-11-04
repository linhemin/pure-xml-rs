//! M1 测试轮发现的缺陷回归用例（内存样本）。全部按 XML 1.0 规范 / Lead 已定决策断言，
//! 实现修复前这些用例会失败——按约定保持开启，不加 #[ignore]。
//! 缺陷编号见 M1-test-report.md。

mod common;

use common::{run_guarded, ErrorClass, Guarded, Node};
use std::time::Duration;

fn class(x: &str) -> Result<(), ErrorClass> {
    common::parse_default(x.as_bytes())
        .map(|_| ())
        .map_err(|f| f.class)
}

#[track_caller]
fn assert_syntax(x: &str) {
    assert_eq!(class(x), Err(ErrorClass::Syntax), "输入应报 Syntax：{x}");
}

#[track_caller]
fn assert_wf(x: &str) {
    assert_eq!(class(x), Ok(()), "输入应良构：{x}");
}

// ── D1 实体替换文本必须自身匹配 content（XML 1.0 §4.3.2 WFC: Parsed Entity）──

#[test]
fn d1_start_tag_split_across_entity() {
    assert_syntax(r#"<!DOCTYPE r [<!ENTITY e "<a">]><r>&e;/></r>"#);
}

#[test]
fn d1_start_tag_attrs_split_across_entity() {
    assert_syntax(r#"<!DOCTYPE r [<!ENTITY e "<a x='1'">]><r>&e; y="2"/></r>"#);
}

#[test]
fn d1_comment_split_across_entity() {
    assert_syntax(r#"<!DOCTYPE r [<!ENTITY e "<!--">]><r>&e; x --></r>"#);
}

#[test]
fn d1_cdata_split_across_entity() {
    assert_syntax(r#"<!DOCTYPE r [<!ENTITY e "<![CDATA[">]><r>&e; x ]]></r>"#);
}

#[test]
fn d1_pi_split_across_entity() {
    assert_syntax(r#"<!DOCTYPE r [<!ENTITY e "<?p ">]><r>&e; x ?></r>"#);
}

#[test]
fn d1_end_tag_inside_entity() {
    assert_syntax(r#"<!DOCTYPE r [<!ENTITY e "</a>">]><r><a>&e;</r>"#);
}

// ── D2 属性值中直接或间接引用的实体替换文本不得含 '<'（WFC: No < in Attribute Values）──

#[test]
fn d2_lt_via_entity_in_attribute() {
    assert_syntax(r#"<!DOCTYPE r [<!ENTITY e "a<b">]><r x="&e;"/>"#);
}

/// `&#60;` 在声明时就展开为 '<'，替换文本同样含 '<'（XML 1.0 附录 D）
#[test]
fn d2_lt_charref_via_entity_in_attribute() {
    assert_syntax(r#"<!DOCTYPE r [<!ENTITY e "&#60;">]><r x="&e;"/>"#);
}

// ── D4 ATTLIST 默认值字面量中的 '%' 不是参数实体引用（W3C valid-sa-094）──

#[test]
fn d4_percent_in_attlist_default_is_not_pe() {
    assert_wf(r#"<!DOCTYPE r [<!ATTLIST r a CDATA "%p;">]><r/>"#);
}

// ── D5 DTD 声明语法未校验（W3C not-wf-sa 共 30 例）──

#[test]
fn d5_element_decl_garbage() {
    assert_syntax(r#"<!DOCTYPE r [<!ELEMENT r (#PCDATA)garbage!!>]><r/>"#);
}

#[test]
fn d5_attlist_decl_garbage() {
    assert_syntax(r#"<!DOCTYPE r [<!ATTLIST r a NOTATYPE #IMPLIED>]><r/>"#);
}

// ── D6 ATTLIST 默认值中的实体引用必须已声明（WFC: Entity Declared，W3C not-wf-sa-078/079/080/180）──

#[test]
fn d6_undeclared_entity_in_attlist_default() {
    assert_syntax(r#"<!DOCTYPE r [<!ATTLIST r a CDATA "&undef;">]><r/>"#);
}

// ── D8 字符引用的 'x' 必须小写（W3C not-wf-sa-093）──

#[test]
fn d8_uppercase_hex_charref() {
    assert_syntax("<r>&#X58;</r>");
}

// ── D9 字符引用允许任意多前导零（W3C valid-sa-042/056）──

#[test]
fn d9_charref_long_leading_zeros() {
    assert_wf("<r>&#00000000000000000000000065;&#x0000000000000000000041;</r>");
}

/// 但数值本身溢出 / 超出 Char 范围仍须拒绝
#[test]
fn d9_charref_overflow_still_rejected() {
    assert_syntax("<r>&#99999999999999999999999999;</r>");
    assert_syntax("<r>&#x110000;</r>");
}

// ── D12 内部子集 ATTLIST 的默认值与属性值规范化（XML 1.0 §3.3.2、§3.3.3，W3C valid-sa-044…111）──

#[test]
fn d12_attlist_default_applied() {
    let got = common::structure(br#"<!DOCTYPE r [<!ATTLIST r a CDATA "v">]><r/>"#).unwrap();
    assert_eq!(
        got.first(),
        Some(&Node::Start {
            name: (None, "r".into()),
            attrs: vec![((None, "a".into()), "v".into())],
        }),
        "非验证处理器也必须应用内部子集里的属性默认值"
    );
}

#[test]
fn d12_nmtokens_normalized() {
    let got =
        common::structure(b"<!DOCTYPE r [<!ATTLIST r a NMTOKENS #IMPLIED>]><r a=\" 1  \t2 \t\"/>")
            .unwrap();
    assert_eq!(
        got.first(),
        Some(&Node::Start {
            name: (None, "r".into()),
            attrs: vec![((None, "a".into()), "1 2".into())],
        })
    );
}

// ── D10 命名空间前缀查找 O(深度×声明数) → CPU DoS ──
//
// 255 层 × 每层 1023 个 xmlns 声明（均在默认限额内）+ 10 万个带前缀子元素，
// 输入约 5.5 MB。线性实现应远低于 10 秒；当前实现约每元素 0.46 ms（约 46 秒）。

#[test]
fn d10_namespace_lookup_not_quadratic() {
    let levels = 255;
    let decls = 1023;
    let kids = 100_000;
    let mut s = String::with_capacity(6 << 20);
    for l in 0..levels {
        s.push_str("<e");
        for d in 0..decls {
            s.push_str(&format!(" xmlns:p{l}_{d}=\"u\""));
        }
        s.push('>');
    }
    for _ in 0..kids {
        // 前缀绑定在最外层且是第一个声明：线性扫描栈顶向下时最坏
        s.push_str("<p0_0:k/>");
    }
    for _ in 0..levels {
        s.push_str("</e>");
    }
    let bytes = s.into_bytes();
    match run_guarded("d10", Duration::from_secs(10), move || {
        common::parse_default(&bytes)
            .map(|_| ())
            .map_err(|f| f.class)
    }) {
        Guarded::Done { value, elapsed, .. } => {
            assert_eq!(value, Ok(()));
            eprintln!("d10 耗时 {elapsed:?}");
        }
        Guarded::TimedOut => panic!("命名空间解析超过 10 秒：前缀查找是二次复杂度（CPU DoS）"),
        Guarded::Panicked(m) => panic!("panic: {m}"),
    }
}

// ── D13 命名空间 URI 每次解析都整串复制 → 内存 / CPU 放大 ──
//
// resolve_name 对每个元素名、每个带前缀属性都 `uri.to_string()`，属性去重键再复制一次。
// 默认限额下 URI 最长 16 MiB、属性 1024 个：单个开始标签可放大到约 32 GiB。

/// 64 KiB URI × 1000 个带前缀属性，输入约 74 KB；当前峰值约 125 MiB（放大约 1700 倍）
#[test]
fn d13_ns_uri_attr_memory_amplification() {
    let uri = "u".repeat(64 << 10);
    let mut s = format!("<r xmlns:p=\"{uri}\"><e");
    for i in 0..1000 {
        s.push_str(&format!(" p:a{i}=\"\""));
    }
    s.push_str("/></r>");
    let n = s.len();
    match run_guarded("d13a", Duration::from_secs(30), move || {
        common::parse_default(s.as_bytes())
            .map(|_| ())
            .map_err(|f| f.class)
    }) {
        Guarded::Done {
            value, peak_bytes, ..
        } => {
            assert_eq!(value, Ok(()));
            assert!(
                peak_bytes < 16 * n,
                "峰值内存 {peak_bytes} 字节 > 输入 {n} 字节的 16 倍：命名空间 URI 被逐属性复制"
            );
        }
        other => panic!("{other:?}"),
    }
}

/// 4 MiB URI + 25 000 个带前缀空元素，输入约 4.3 MB；当前每个元素复制 URI，约需数十 GB memcpy
#[test]
fn d13_ns_uri_cpu_amplification() {
    let uri = "u".repeat(4 << 20);
    let mut s = format!("<r xmlns:p=\"{uri}\">");
    for _ in 0..25_000 {
        s.push_str("<p:a/>");
    }
    s.push_str("</r>");
    match run_guarded("d13b", Duration::from_secs(5), move || {
        common::parse_default(s.as_bytes())
            .map(|_| ())
            .map_err(|f| f.class)
    }) {
        Guarded::Done { value, .. } => assert_eq!(value, Ok(())),
        Guarded::TimedOut => panic!("超过 5 秒：命名空间 URI 每个元素复制一次（CPU 放大）"),
        Guarded::Panicked(m) => panic!("panic: {m}"),
    }
}
