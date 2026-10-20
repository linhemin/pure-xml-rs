//! Extra M1 cases that are not duplicated by the tester suite.
//!
//! The tester files under `tests/` stay unchanged. These cover the positive
//! sides of the same rules: a balanced entity, a predefined `&lt;` inside an
//! attribute, namespace restore, interned URIs, and literal `]]>` versus
//! `]]&gt;`.

use std::sync::Arc;

use purexml::{ErrorKind, Event, Reader};

fn events(xml: &str) -> Result<Vec<Owned>, String> {
    let mut reader = Reader::from_str(xml);
    let mut out = Vec::new();
    loop {
        match reader.read_event() {
            Ok(None) => return Ok(out),
            Ok(Some(event)) => out.push(Owned::from(event)),
            Err(err) => return Err(err.to_string()),
        }
    }
}

fn must(xml: &str) -> Vec<Owned> {
    events(xml).unwrap_or_else(|err| panic!("{xml}\n{err}"))
}

fn must_err(xml: &str) -> String {
    events(xml)
        .err()
        .unwrap_or_else(|| panic!("expected an error for {xml}"))
}

#[derive(Debug)]
enum Owned {
    Start {
        local: String,
        ns: Option<Arc<str>>,
    },
    Empty {
        local: String,
        ns: Option<Arc<str>>,
        attrs: Vec<(String, String)>,
    },
    Text(String),
    Other,
}

impl Owned {
    fn from(event: Event<'_>) -> Self {
        match event {
            Event::StartElement(el) => Self::Start {
                local: el.name.local.into_owned(),
                ns: el.name.namespace,
            },
            Event::EmptyElement(el) => Self::Empty {
                local: el.name.local.into_owned(),
                ns: el.name.namespace,
                attrs: el
                    .attributes
                    .iter()
                    .map(|attr| (attr.name.local.to_string(), attr.value.to_string()))
                    .collect(),
            },
            Event::Text(text) => Self::Text(text.into_owned()),
            _ => Self::Other,
        }
    }
}

#[test]
fn balanced_entity_is_content_and_an_unused_bare_lt_is_allowed() {
    let parsed = must(r#"<!DOCTYPE r [<!ENTITY e "<a>t</a>">]><r>&e;</r>"#);
    assert!(parsed
        .iter()
        .any(|ev| matches!(ev, Owned::Start { local, .. } if local == "a")));
    assert!(parsed
        .iter()
        .any(|ev| matches!(ev, Owned::Text(text) if text == "t")));
    assert!(
        events(r#"<!DOCTYPE r [<!ENTITY e "&#60;">]><r></r>"#).is_ok(),
        "an entity that is never used may contain a bare '<'"
    );
    let err = must_err(
        r#"<!DOCTYPE r [<!ENTITY open "<a>"><!ENTITY close "</a>">]><r>&open;x&close;</r>"#,
    );
    assert!(err.contains("syntax"), "{err}");
}

#[test]
fn predefined_lt_in_an_attribute_stays_a_character() {
    let parsed = must(r#"<!DOCTYPE r [<!ENTITY e "&lt;">]><r b="x&e;y"/>"#);
    match &parsed[1] {
        Owned::Empty { attrs, .. } => assert_eq!(attrs[0].1, "x<y"),
        other => panic!("{other:?}"),
    }
}

#[test]
fn escaped_cdata_end_is_character_data() {
    let parsed = must("<r>]]&gt;</r>");
    assert!(parsed
        .iter()
        .any(|ev| matches!(ev, Owned::Text(text) if text == "]]>")));
    let err = must_err("<r>]]></r>");
    assert!(err.contains("syntax"), "{err}");
}

#[test]
fn prefix_bindings_restore_after_shadowing() {
    let parsed = must(r#"<a xmlns:p="urn:1"><p:b xmlns:p="urn:2"><p:c/></p:b><p:d/></a>"#);
    let nss: Vec<_> = parsed
        .iter()
        .filter_map(|ev| match ev {
            Owned::Empty { local, ns, .. } | Owned::Start { local, ns } => {
                Some((local.as_str(), ns.as_deref().map(str::to_owned)))
            }
            _ => None,
        })
        .collect();
    assert_eq!(nss[1].1.as_deref(), Some("urn:2"));
    assert_eq!(nss[2].1.as_deref(), Some("urn:2"));
    assert_eq!(nss[3].1.as_deref(), Some("urn:1"));
}

#[test]
fn namespace_uris_are_interned() {
    let mut reader = Reader::from_str(r#"<a xmlns:p="urn:example"><p:b/><p:c/></a>"#);
    let mut uris = Vec::new();
    loop {
        match reader.read_event().unwrap() {
            None => break,
            Some(Event::EmptyElement(el)) => uris.push(el.name.namespace.expect("namespace")),
            Some(_) => {}
        }
    }
    assert_eq!(uris.len(), 2);
    assert!(Arc::ptr_eq(&uris[0], &uris[1]));
}

#[test]
fn defaults_cover_tokens_cdata_and_xmlns() {
    match &must(r#"<!DOCTYPE r [<!ATTLIST r a NMTOKEN "  x   y  ">]><r/>"#)[1] {
        Owned::Empty { attrs, .. } => assert_eq!(attrs, &[("a".into(), "x y".into())]),
        other => panic!("{other:?}"),
    }
    match &must(r#"<!DOCTYPE r [<!ATTLIST r a CDATA #IMPLIED>]><r a="  x   y  "/>"#)[1] {
        Owned::Empty { attrs, .. } => assert_eq!(attrs[0].1, "  x   y  "),
        other => panic!("{other:?}"),
    }
    let parsed = must(
        r#"<!DOCTYPE a [<!ATTLIST a xmlns CDATA "urn:def" xmlns:p CDATA "urn:p">]><a><p:b/></a>"#,
    );
    assert!(parsed.iter().any(|ev| matches!(
        ev,
        Owned::Start { local, ns } if local == "a" && ns.as_deref() == Some("urn:def")
    )));
    assert!(parsed.iter().any(|ev| matches!(
        ev,
        Owned::Empty { local, ns, .. } if local == "b" && ns.as_deref() == Some("urn:p")
    )));
}

#[test]
fn syntax_error_kind_is_syntax() {
    let mut reader = Reader::from_str("<a></b>");
    let err = loop {
        match reader.read_event() {
            Ok(Some(_)) => {}
            Ok(None) => panic!("expected an error"),
            Err(err) => break err,
        }
    };
    assert!(matches!(err.kind, ErrorKind::Syntax));
}
