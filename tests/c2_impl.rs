//! Implementer checks for DOM mutation and serialization.
//!
//! Tester trees are left untouched. A fuzz target for round-trip and mutation
//! is noted for the Tester; it is not added here.

use purexml::dom::{Document, DomError, DomLimit, Name, NodeId, NodeKind, WriteOptions};
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
            Some(Event::Doctype(dtd)) => {
                let id = doc
                    .create_doctype(
                        dtd.name.as_str(),
                        dtd.system_id.as_deref(),
                        dtd.public_id.as_deref(),
                    )
                    .unwrap();
                doc.append_child(*stack.last().unwrap(), id).unwrap();
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
    let dtd = doc.create_doctype("r", Some("sys"), None).unwrap();
    doc.append_child(root, dtd).unwrap();
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
    let omitted = doc
        .to_string_with(WriteOptions { omit_doctype: true })
        .unwrap();
    assert!(!omitted.contains("<!DOCTYPE"));
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
