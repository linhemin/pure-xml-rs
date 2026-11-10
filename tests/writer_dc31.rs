//! D-C3-1 语义锁定（Lead Q5：Arc 地址只能做快速路径，判定结果必须与按内容比较完全一致）。
//!
//! **v10.9：只用 main 上已有的公开 API，main efb8be1 上可编译、可运行（应全部通过）。**
//! 修 D-C3-1 前后都必须通过：修复只能改性能，不能改“重复属性”的判定。
//! 性能门在 complexity_c3.rs（long_uri_joint / long_uri_amp，序列化路径）。
//!
//! 运行：cargo test --test writer_dc31

use std::sync::Arc;

use purexml::{
    Attribute, EmptyElement, EndElement, Event, Namespace, QName, Reader, StartElement, WriteError,
    Writer, XmlText,
};

fn attr<'a>(prefix: &'a str, local: &'a str, ns: &Arc<str>) -> Attribute<'a> {
    Attribute {
        name: QName {
            prefix: Some(XmlText::borrowed(prefix)),
            local: XmlText::borrowed(local),
            namespace: Some(ns.clone()),
        },
        value: XmlText::borrowed(""),
    }
}

fn plain(local: &str) -> Attribute<'_> {
    Attribute {
        name: QName {
            prefix: None,
            local: XmlText::borrowed(local),
            namespace: None,
        },
        value: XmlText::borrowed(""),
    }
}

fn write(attributes: Vec<Attribute<'_>>, decls: &[(&str, &str)]) -> Result<String, WriteError> {
    let mut out = Vec::new();
    {
        let mut w = Writer::new(&mut out);
        w.write_event(&Event::EmptyElement(EmptyElement {
            name: QName {
                prefix: None,
                local: XmlText::borrowed("e"),
                namespace: None,
            },
            attributes,
            namespaces: decls
                .iter()
                .map(|(p, u)| Namespace {
                    prefix: Some(XmlText::borrowed(p)),
                    uri: XmlText::borrowed(u),
                })
                .collect(),
        }))?;
    }
    Ok(String::from_utf8(out).unwrap())
}

fn is_dup(r: &Result<String, WriteError>) -> bool {
    matches!(r, Err(WriteError::Syntax(m)) if m.contains("duplicate attribute"))
}

/// 不同 Arc、同内容 URI、同 local → 重复（不同前缀也一样，展开名相同）。
#[test]
fn distinct_arcs_same_content_are_duplicates() {
    let u1: Arc<str> = Arc::from("urn:x");
    let u2: Arc<str> = Arc::from(String::from("urn:x"));
    assert!(!Arc::ptr_eq(&u1, &u2));
    let r = write(
        vec![attr("p", "a", &u1), attr("q", "a", &u2)],
        &[("p", "urn:x"), ("q", "urn:x")],
    );
    assert!(is_dup(&r), "{r:?}");
    // 同前缀同 Arc 当然也是重复
    let r = write(
        vec![attr("p", "a", &u1), attr("p", "a", &u1)],
        &[("p", "urn:x")],
    );
    assert!(is_dup(&r), "{r:?}");
}

/// 长 URI（512 KiB）：不同 Arc 同内容仍判重复；只差最后一个字节则不重复。
#[test]
fn long_uri_identity_is_by_content() {
    let base = "u".repeat(512 << 10);
    let u1: Arc<str> = Arc::from(base.as_str());
    let u2: Arc<str> = Arc::from(base.clone());
    let mut other = base.clone();
    other.pop();
    other.push('v');
    let u3: Arc<str> = Arc::from(other.as_str());
    let r = write(
        vec![attr("p", "a", &u1), attr("q", "a", &u2)],
        &[("p", &base), ("q", &base)],
    );
    assert!(is_dup(&r), "同内容不同 Arc 必须判重复");
    let r = write(
        vec![attr("p", "a", &u1), attr("q", "a", &u3)],
        &[("p", &base), ("q", &other)],
    );
    assert!(r.is_ok(), "内容不同不应判重复：{:?}", r.err());
}

/// 同 Arc、不同 local → 不重复；无命名空间 `a` 与 `{urn:x}a` → 不重复。
#[test]
fn same_arc_distinct_names_ok() {
    let u: Arc<str> = Arc::from("urn:x");
    let r = write(
        vec![attr("p", "a", &u), attr("p", "b", &u), plain("a")],
        &[("p", "urn:x")],
    );
    assert_eq!(r.unwrap(), r#"<e xmlns:p="urn:x" p:a="" p:b="" a=""/>"#);
}

/// 跨事件：Writer 若引入“跨调用缓存”（设计 §8.1），前一个元素的 Arc 释放后地址可能被复用，
/// 不能因此把后一个元素里不同内容的 URI 当成同一个。这里让 Arc 逐个元素创建、释放。
#[test]
fn cross_event_cache_no_address_reuse_confusion() {
    let root = || QName {
        prefix: None,
        local: XmlText::borrowed("r"),
        namespace: None,
    };
    let mut out = Vec::new();
    {
        let mut w = Writer::new(&mut out);
        w.write_event(&Event::StartElement(StartElement {
            name: root(),
            attributes: vec![],
            namespaces: vec![],
        }))
        .unwrap();
        for i in 0..2000 {
            let ua: Arc<str> = Arc::from(format!("urn:a{i}"));
            let ub: Arc<str> = Arc::from(format!("urn:b{i}"));
            let pa = format!("urn:a{i}");
            let pb = format!("urn:b{i}");
            let ev = Event::EmptyElement(EmptyElement {
                name: QName {
                    prefix: None,
                    local: XmlText::borrowed("e"),
                    namespace: None,
                },
                attributes: vec![attr("p", "a", &ua), attr("q", "a", &ub)],
                namespaces: vec![
                    Namespace {
                        prefix: Some(XmlText::borrowed("p")),
                        uri: XmlText::borrowed(&pa),
                    },
                    Namespace {
                        prefix: Some(XmlText::borrowed("q")),
                        uri: XmlText::borrowed(&pb),
                    },
                ],
            });
            w.write_event(&ev).unwrap_or_else(|e| panic!("i={i}: {e}"));
            drop(ev);
        }
        w.write_event(&Event::EndElement(EndElement { name: root() }))
            .unwrap();
    }
    // 写出的结果能被 Reader 读回（每个元素 2 个属性、展开名不同）
    let mut r = Reader::from_bytes(&out);
    let mut n = 0;
    while let Some(ev) = r.read_event().unwrap_or_else(|e| panic!("{e:?}")) {
        if let Event::EmptyElement(e) = ev {
            assert_eq!(e.attributes.len(), 2);
            n += 1;
        }
    }
    assert_eq!(n, 2000);
}

/// PR #8 89f6e78 实测缺陷（Tester 11-10）：UriIdentity 的 by_ptr 以“非 canonical 的 Arc”地址为键，
/// 却只保留 canonical Arc；键对应的分配释放后，地址被另一个内容不同的 URI 复用，就会命中旧条目。
/// 结果：既有误报（{urn:y}a 与 {urn:x}a 被判重复），也有漏报（两个 {urn:y}a 被放行，写出
/// 命名空间不良构的标签）。修复必须同时保留作为键的那个 Arc（或不以它的地址为键）。
#[test]
fn non_canonical_arc_address_reuse() {
    let mut out = Vec::new();
    let mut w = Writer::new(&mut out);
    let x0: Arc<str> = Arc::from("urn:x");
    let one = |a: Vec<Attribute<'static>>, d: &[(&'static str, &'static str)]| {
        Event::EmptyElement(EmptyElement {
            name: QName {
                prefix: None,
                local: XmlText::borrowed("e"),
                namespace: None,
            },
            attributes: a,
            namespaces: d
                .iter()
                .map(|(p, u)| Namespace {
                    prefix: Some(XmlText::borrowed(p)),
                    uri: XmlText::borrowed(u),
                })
                .collect(),
        })
    };
    w.write_event(&one(vec![attr("p", "a", &x0)], &[("p", "urn:x")]))
        .unwrap();
    let mut false_dup = 0;
    let mut missed_dup = 0;
    for _ in 0..200 {
        let b: Arc<str> = Arc::from(String::from("urn:x"));
        w.write_event(&one(vec![attr("p", "a", &b)], &[("p", "urn:x")]))
            .unwrap();
        drop(b);
        let c: Arc<str> = Arc::from(String::from("urn:y"));
        let r = w.write_event(&one(
            vec![attr("p", "a", &c), attr("q", "a", &x0)],
            &[("p", "urn:y"), ("q", "urn:x")],
        ));
        if r.is_err() {
            false_dup += 1;
        }
        let c2: Arc<str> = Arc::from("urn:y");
        let r = w.write_event(&one(
            vec![attr("p", "a", &c), attr("q", "a", &c2)],
            &[("p", "urn:y"), ("q", "urn:y")],
        ));
        if r.is_ok() {
            missed_dup += 1;
        }
    }
    assert_eq!(
        (false_dup, missed_dup),
        (0, 0),
        "地址复用导致误报 {false_dup} 次、漏报 {missed_dup} 次（共 200 轮）"
    );
}
