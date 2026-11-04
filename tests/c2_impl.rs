//! Implementer checks for DOM mutation and serialization.
//!
//! Tester trees are left untouched. A fuzz target for round-trip and mutation
//! is noted for the Tester; it is not added here.

use purexml::dom::{Document, DomError, DomLimit, Name, NodeId, NodeKind};
use purexml::{Event, Reader};

fn root_id(doc: &Document) -> NodeId {
    doc.root().expect("document node").id()
}

fn append_local(doc: &mut Document, name: &str) -> NodeId {
    let parent = root_id(doc);
    append_new(doc, parent, name)
}

fn append_new(doc: &mut Document, parent: NodeId, name: &str) -> NodeId {
    let id = doc.create_element(Name::local(name)).unwrap();
    doc.append_child(parent, id).unwrap();
    id
}

fn fingerprint(doc: &Document, id: NodeId) -> String {
    let Some(node) = doc.get(id) else {
        return "gone".to_string();
    };
    let mut out = String::new();
    match node.kind() {
        Some(NodeKind::Document) => out.push_str("document"),
        Some(NodeKind::Element) => {
            let name = node.name().unwrap();
            out.push_str(&format!(
                "elem {}{}{}",
                name.prefix.map(|p| format!("{p}:")).unwrap_or_default(),
                name.local,
                name.namespace
                    .map(|ns| format!("@{ns}"))
                    .unwrap_or_default()
            ));
        }
        Some(NodeKind::Text) => out.push_str(&format!("text {}", node.text().unwrap_or(""))),
        Some(NodeKind::CData) => out.push_str(&format!("cdata {}", node.text().unwrap_or(""))),
        Some(NodeKind::Comment) => out.push_str(&format!("comment {}", node.text().unwrap_or(""))),
        Some(NodeKind::Pi) => {
            let (target, data) = node.pi().unwrap();
            out.push_str(&format!("pi {target} {:?}", data));
        }
        Some(NodeKind::Doctype) => {
            let dtd = node.doctype().unwrap();
            out.push_str(&format!(
                "dtd {} {:?} {:?}",
                dtd.name, dtd.system_id, dtd.public_id
            ));
        }
        None => out.push_str("none"),
    }
    for child in node.children() {
        out.push('{');
        out.push_str(&fingerprint(doc, child.id()));
        out.push('}');
    }
    if node.kind() == Some(NodeKind::Element) {
        for attr in node.attributes() {
            out.push_str(&format!(
                "[attr {}{}={}]",
                attr.prefix().map(|p| format!("{p}:")).unwrap_or_default(),
                attr.local(),
                attr.value()
            ));
        }
    }
    out
}

fn rebuild(xml: &str) -> Document {
    let mut reader = Reader::from_str(xml);
    let mut doc = Document::new();
    let mut stack = vec![root_id(&doc)];
    loop {
        match reader.read_event().expect("parse") {
            None => break,
            Some(Event::XmlDecl(_)) => {}
            Some(Event::StartElement(el)) => {
                let id = element_from_event(&mut doc, &el.name, &el.namespaces, &el.attributes);
                doc.append_child(*stack.last().unwrap(), id).unwrap();
                stack.push(id);
            }
            Some(Event::EmptyElement(el)) => {
                let id = element_from_event(&mut doc, &el.name, &el.namespaces, &el.attributes);
                doc.append_child(*stack.last().unwrap(), id).unwrap();
            }
            Some(Event::EndElement(_)) => {
                stack.pop();
            }
            Some(Event::Text(text)) => {
                let id = doc.create_text(text.as_str()).unwrap();
                doc.append_child(*stack.last().unwrap(), id).unwrap();
            }
            Some(Event::CData(text)) => {
                let id = doc.create_cdata(text.as_str()).unwrap();
                doc.append_child(*stack.last().unwrap(), id).unwrap();
            }
            Some(Event::Comment(text)) => {
                let id = doc.create_comment(text.as_str()).unwrap();
                doc.append_child(*stack.last().unwrap(), id).unwrap();
            }
            Some(Event::PI(pi)) => {
                let id = doc
                    .create_pi(pi.target.as_str(), pi.data.as_deref())
                    .unwrap();
                doc.append_child(*stack.last().unwrap(), id).unwrap();
            }
            Some(Event::Doctype(_)) => {
                panic!("integration tests do not rebuild a document type");
            }
        }
    }
    doc
}

fn element_from_event(
    doc: &mut Document,
    name: &purexml::QName<'_>,
    namespaces: &[purexml::Namespace<'_>],
    attributes: &[purexml::Attribute<'_>],
) -> NodeId {
    let built = match name.namespace.as_deref() {
        Some(ns) => Name::qualified(ns, name.prefix.as_deref(), name.local.as_str()),
        None => Name::local(name.local.as_str()),
    };
    let id = doc.create_element(built).unwrap();
    for ns in namespaces {
        doc.declare_namespace(id, ns.prefix.as_deref(), ns.uri.as_str())
            .unwrap();
    }
    for attr in attributes {
        let built = match attr.name.namespace.as_deref() {
            Some(ns) => Name::qualified(ns, attr.name.prefix.as_deref(), attr.name.local.as_str()),
            None => Name::local(attr.name.local.as_str()),
        };
        doc.set_attribute(id, built, attr.value.as_str()).unwrap();
    }
    id
}

#[test]
fn round_trip_keeps_structure() {
    let mut doc = Document::new();
    let root = root_id(&doc);
    let element = append_new(&mut doc, root, "r");
    doc.declare_namespace(element, Some("p"), "urn:p").unwrap();
    let child = doc
        .create_element(Name::qualified("urn:p", Some("p"), "c"))
        .unwrap();
    doc.append_child(element, child).unwrap();
    doc.set_attribute(child, Name::local("id"), "1").unwrap();
    let text = doc.create_text("a<b").unwrap();
    doc.append_child(child, text).unwrap();
    let cdata = doc.create_cdata("CD").unwrap();
    doc.append_child(element, cdata).unwrap();
    let comment = doc.create_comment("note").unwrap();
    doc.append_child(element, comment).unwrap();
    let pi = doc.create_pi("target", Some("data")).unwrap();
    doc.append_child(root, pi).unwrap();

    let xml = doc.to_string().unwrap();
    let again = rebuild(&xml);
    assert_eq!(
        fingerprint(&doc, root_id(&doc)),
        fingerprint(&again, root_id(&again))
    );
}

#[test]
fn rejected_edits_do_not_change_the_document() {
    let mut doc = Document::new();
    let root = root_id(&doc);
    let element = append_new(&mut doc, root, "r");
    let child = append_new(&mut doc, element, "c");
    let before_nodes = doc.node_count();
    let before_bytes = doc.approx_heap_bytes();
    assert_eq!(doc.create_cdata("]]>").unwrap_err(), DomError::InvalidChar);
    assert_eq!(doc.create_comment("--").unwrap_err(), DomError::InvalidChar);
    assert_eq!(doc.create_comment("x-").unwrap_err(), DomError::InvalidChar);
    assert_eq!(
        doc.create_pi("xml", None).unwrap_err(),
        DomError::InvalidName
    );
    assert_eq!(
        doc.create_pi("ok", Some("?>")).unwrap_err(),
        DomError::InvalidChar
    );
    assert_eq!(
        doc.set_attribute(element, Name::local("xmlns"), "urn")
            .unwrap_err(),
        DomError::Namespace
    );
    assert_eq!(
        doc.append_child(child, element).unwrap_err(),
        DomError::Hierarchy
    );
    assert_eq!(doc.node_count(), before_nodes);
    assert_eq!(doc.approx_heap_bytes(), before_bytes);
}

#[test]
fn limits_fire_and_leave_the_tree() {
    let mut named =
        Document::with_config(&purexml::ParserConfig::default().with_max_name_len(1)).unwrap();
    let err = named.create_element(Name::local("ab")).unwrap_err();
    assert!(matches!(
        err,
        DomError::Limit {
            kind: DomLimit::NameLength,
            ..
        }
    ));
    assert_eq!(named.node_count(), 1);

    let mut text =
        Document::with_config(&purexml::ParserConfig::default().with_max_text_len(2)).unwrap();
    let err = text.create_text("abc").unwrap_err();
    assert!(matches!(
        err,
        DomError::Limit {
            kind: DomLimit::TextLength,
            ..
        }
    ));
    text.create_text("ab").unwrap();

    let mut counted =
        Document::with_config(&purexml::ParserConfig::default().with_max_nodes(2)).unwrap();
    let element = counted.create_element(Name::local("a")).unwrap();
    counted.append_child(root_id(&counted), element).unwrap();
    let err = counted.create_element(Name::local("b")).unwrap_err();
    assert!(matches!(
        err,
        DomError::Limit {
            kind: DomLimit::NodeCount,
            ..
        }
    ));
    assert_eq!(counted.node_count(), 2);

    let mut deep =
        Document::with_config(&purexml::ParserConfig::default().with_max_depth(1)).unwrap();
    let top = append_local(&mut deep, "a");
    let nested = deep.create_element(Name::local("b")).unwrap();
    let err = deep.append_child(top, nested).unwrap_err();
    assert!(matches!(
        err,
        DomError::Limit {
            kind: DomLimit::ElementDepth,
            ..
        }
    ));
    assert!(deep.get(nested).unwrap().parent().is_none());
}

#[test]
fn attribute_cap_and_namespace_rules() {
    let mut doc =
        Document::with_config(&purexml::ParserConfig::default().with_max_attrs(1)).unwrap();
    let element = append_local(&mut doc, "a");
    doc.set_attribute(element, Name::local("id"), "1").unwrap();
    let err = doc
        .set_attribute(element, Name::local("x"), "2")
        .unwrap_err();
    assert!(matches!(
        err,
        DomError::Limit {
            kind: DomLimit::Attributes,
            ..
        }
    ));
    doc.set_attribute(element, Name::local("id"), "9").unwrap();
    assert_eq!(doc.get(element).unwrap().attribute("id"), Some("9"));
    doc.remove_attribute(element, Name::local("id")).unwrap();
    assert_eq!(doc.get(element).unwrap().attribute("id"), None);

    doc.declare_namespace(element, Some("p"), "urn:p").unwrap();
    let child = doc
        .create_element(Name::qualified("urn:p", Some("p"), "c"))
        .unwrap();
    doc.append_child(element, child).unwrap();
    assert_eq!(
        doc.remove_namespace(element, Some("p")).unwrap_err(),
        DomError::Namespace
    );
    doc.detach(child).unwrap();
    let host = doc.create_element(Name::local("host")).unwrap();
    doc.append_child(element, host).unwrap();
    doc.append_child(host, child).unwrap();
    let xml = doc.to_string().unwrap();
    assert!(xml.contains("xmlns:p=\"urn:p\""));
    let again = rebuild(&xml);
    assert_eq!(
        fingerprint(&doc, root_id(&doc)),
        fingerprint(&again, root_id(&again))
    );
}

#[test]
fn order_cache_clears_when_the_shape_changes() {
    let mut doc = Document::new();
    let root = root_id(&doc);
    let element = append_new(&mut doc, root, "a");
    doc.cmp_document_order(root, element).unwrap();
    assert!(doc.order_cache_len().is_some());
    let extra = doc.create_element(Name::local("b")).unwrap();
    doc.append_child(element, extra).unwrap();
    assert!(doc.order_cache_len().is_none());
    doc.remove(extra).unwrap();
    assert!(doc.get(extra).is_none());
    let fresh = doc.create_element(Name::local("c")).unwrap();
    assert_ne!(fresh, extra);
}

#[test]
fn same_expanded_name_replaces_and_round_trips() {
    let mut doc = Document::new();
    let element = append_local(&mut doc, "r");
    doc.declare_namespace(element, Some("p"), "urn:a").unwrap();
    doc.declare_namespace(element, Some("q"), "urn:a").unwrap();
    doc.set_attribute(element, Name::qualified("urn:a", Some("p"), "id"), "1")
        .unwrap();
    doc.set_attribute(element, Name::qualified("urn:a", Some("q"), "id"), "2")
        .unwrap();
    let attrs: Vec<_> = doc.get(element).unwrap().attributes().collect();
    assert_eq!(attrs.len(), 1);
    assert_eq!(attrs[0].prefix(), Some("q"));
    assert_eq!(attrs[0].local(), "id");
    assert_eq!(attrs[0].value(), "2");
    assert_eq!(
        doc.get(element).unwrap().attribute_ns(Some("urn:a"), "id"),
        Some("2")
    );
    let before = doc.node_count();
    assert_eq!(
        doc.set_attribute(element, Name::qualified("urn:other", Some("q"), "id"), "3")
            .unwrap_err(),
        DomError::Namespace
    );
    assert_eq!(doc.get(element).unwrap().attribute("id"), None);
    assert_eq!(
        doc.get(element).unwrap().attribute_ns(Some("urn:a"), "id"),
        Some("2")
    );
    assert_eq!(doc.node_count(), before);
    let xml = doc.to_string().unwrap();
    assert!(xml.contains("xmlns:q=\"urn:a\""));
    assert!(xml.contains("q:id=\"2\""));
    assert!(!xml.contains("p:id="));
    let again = rebuild(&xml);
    assert_eq!(
        fingerprint(&doc, root_id(&doc)),
        fingerprint(&again, root_id(&again))
    );

    let child = doc
        .create_element(Name::qualified("urn:a", Some("p"), "c"))
        .unwrap();
    doc.append_child(element, child).unwrap();
    let mut buf = Vec::new();
    {
        let mut writer = purexml::Writer::new(&mut buf);
        doc.get(child).unwrap().write_to(&mut writer).unwrap();
    }
    let subtree = String::from_utf8(buf).unwrap();
    assert!(subtree.contains("xmlns:p=\"urn:a\""));
    let parsed = rebuild(&subtree);
    assert_eq!(
        fingerprint(&doc, child),
        fingerprint(&parsed, parsed.root_element().unwrap().id())
    );
}

#[test]
fn escapes_round_trip_and_illegal_forms_are_rejected() {
    let mut doc = Document::new();
    let element = append_local(&mut doc, "r");
    let text = doc.create_text("a&b<c]]>d").unwrap();
    doc.append_child(element, text).unwrap();
    let cdata = doc.create_cdata("x<y&z").unwrap();
    doc.append_child(element, cdata).unwrap();
    doc.set_attribute(element, Name::local("q"), "\"a&b<c\nb\tc\rd")
        .unwrap();
    let comment = doc.create_comment("ok ]]> note").unwrap();
    doc.append_child(element, comment).unwrap();
    let pi = doc.create_pi("tgt", Some("keep >")).unwrap();
    doc.append_child(element, pi).unwrap();

    let xml = doc.to_string().unwrap();
    assert!(xml.contains("a&amp;b&lt;c]]&gt;d"));
    assert!(xml.contains("<![CDATA[x<y&z]]>"));
    assert!(xml.contains("&#10;"));
    assert!(xml.contains("&#9;"));
    assert!(xml.contains("&#13;"));
    assert!(xml.contains("&quot;"));
    let again = rebuild(&xml);
    assert_eq!(
        fingerprint(&doc, root_id(&doc)),
        fingerprint(&again, root_id(&again))
    );
    let mut reader = Reader::from_str(&xml);
    let mut saw_text = false;
    let mut saw_attr = false;
    loop {
        match reader.read_event().unwrap() {
            None => break,
            Some(Event::Text(text)) => {
                assert_eq!(text.as_str(), "a&b<c]]>d");
                saw_text = true;
            }
            Some(Event::EmptyElement(_)) => {}
            Some(Event::StartElement(el)) => {
                let value = el.attribute("q").unwrap().value.as_str();
                assert_eq!(value, "\"a&b<c\nb\tc\rd");
                saw_attr = true;
            }
            _ => {}
        }
    }
    assert!(saw_text && saw_attr);

    let before = doc.approx_heap_bytes();
    assert_eq!(
        doc.create_comment("a--b").unwrap_err(),
        DomError::InvalidChar
    );
    assert_eq!(
        doc.create_comment("tail-").unwrap_err(),
        DomError::InvalidChar
    );
    assert_eq!(
        doc.set_text(comment, "a--b").unwrap_err(),
        DomError::InvalidChar
    );
    assert_eq!(
        doc.create_pi("tgt", Some("bad?>")).unwrap_err(),
        DomError::InvalidChar
    );
    assert_eq!(
        doc.set_pi_data(pi, Some("bad?>")).unwrap_err(),
        DomError::InvalidChar
    );
    assert_eq!(
        doc.create_cdata("has]]>end").unwrap_err(),
        DomError::InvalidChar
    );
    assert_eq!(
        doc.create_text("\u{0001}").unwrap_err(),
        DomError::InvalidChar
    );
    assert_eq!(
        doc.set_attribute(element, Name::local("z"), "\u{FFFE}")
            .unwrap_err(),
        DomError::InvalidChar
    );
    assert_eq!(doc.approx_heap_bytes(), before);
    assert_eq!(doc.get(comment).unwrap().text(), Some("ok ]]> note"));
}

#[test]
fn byte_cap_rejects_without_growing_the_arena() {
    let mut doc =
        Document::with_config(&purexml::ParserConfig::default().with_max_dom_bytes(120)).unwrap();
    let before = doc.approx_heap_bytes();
    let err = doc.create_text(&"x".repeat(80)).unwrap_err();
    assert!(matches!(
        err,
        DomError::Limit {
            kind: DomLimit::DomBytes,
            ..
        }
    ));
    assert_eq!(doc.approx_heap_bytes(), before);
}
