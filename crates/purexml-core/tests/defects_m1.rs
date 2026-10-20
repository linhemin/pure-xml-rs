//! Regressions for the M1 defects that block release.
//!
//! D4, D6, D8, D9, and D11 stay open for M2 and are not covered here.

use std::rc::Rc;

use purexml_core::{ErrorKind, Event, Reader};

fn parse(xml: &str) -> Result<Vec<EventOwned>, String> {
    let mut reader = Reader::from_str(xml);
    let mut out = Vec::new();
    loop {
        match reader.read_event() {
            Ok(None) => return Ok(out),
            Ok(Some(event)) => out.push(EventOwned::from_event(event)),
            Err(err) => return Err(err.to_string()),
        }
    }
}

fn must(xml: &str) -> Vec<EventOwned> {
    parse(xml).unwrap_or_else(|err| panic!("{xml}\n{err}"))
}

fn must_err(xml: &str) -> String {
    parse(xml)
        .err()
        .unwrap_or_else(|| panic!("expected an error for {xml}"))
}

#[derive(Debug)]
enum EventOwned {
    Start {
        local: String,
        ns: Option<Rc<str>>,
    },
    Empty {
        local: String,
        ns: Option<Rc<str>>,
        attrs: Vec<(String, String)>,
    },
    Text(String),
    Other,
}

impl EventOwned {
    fn from_event(event: Event<'_>) -> Self {
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
                    .map(|attr| {
                        (
                            attr.name.local.clone().into_owned(),
                            attr.value.clone().into_owned(),
                        )
                    })
                    .collect(),
            },
            Event::Text(text) => Self::Text(text.into_owned()),
            _ => Self::Other,
        }
    }
}

#[test]
fn d1_replacement_text_must_be_well_formed() {
    let err = must_err(r#"<!DOCTYPE r [<!ENTITY e "<a>">]><r>&e;</a></r>"#);
    assert!(err.contains("syntax"), "{err}");

    let err = must_err(r#"<!DOCTYPE r [<!ENTITY e "<!--">]><r>&e;</r>"#);
    assert!(err.contains("syntax"), "{err}");
    assert!(
        parse(r#"<!DOCTYPE r [<!ENTITY e "&#60;">]><r></r>"#).is_ok(),
        "an unexpanded entity may contain a bare '<'"
    );

    let err = must_err(
        r#"<!DOCTYPE r [<!ENTITY open "<a>"><!ENTITY close "</a>">]><r>&open;x&close;</r>"#,
    );
    assert!(err.contains("syntax"), "{err}");

    let events = must(r#"<!DOCTYPE r [<!ENTITY e "<a>t</a>">]><r>&e;</r>"#);
    assert!(events
        .iter()
        .any(|ev| matches!(ev, EventOwned::Start { local, .. } if local == "a")));
    assert!(events
        .iter()
        .any(|ev| matches!(ev, EventOwned::Text(text) if text == "t")));
}

#[test]
fn d2_lt_from_an_entity_is_rejected_in_attributes() {
    let err = must_err(r#"<!DOCTYPE r [<!ENTITY e "<a/>">]><r b="&e;"/>"#);
    assert!(err.contains("syntax"), "{err}");

    let events = must(r#"<!DOCTYPE r [<!ENTITY e "&lt;">]><r b="x&e;y"/>"#);
    match &events[1] {
        EventOwned::Empty { attrs, .. } => assert_eq!(attrs[0].1, "x<y"),
        other => panic!("{other:?}"),
    }
}

#[test]
fn d5_declarations_are_syntax_checked() {
    assert!(must_err(r#"<!DOCTYPE r [<!ELEMENT r (a|)>]><r/>"#).contains("syntax"));
    assert!(must_err(r#"<!DOCTYPE r [<!ATTLIST r a (red|) "red">]><r/>"#).contains("syntax"));
    assert!(must_err(r#"<!DOCTYPE r [<!NOTATION n FOO "x">]><r/>"#).contains("syntax"));
    assert!(must(r#"<!DOCTYPE r [<!ELEMENT r (#PCDATA|a)*>]><r>t</r>"#).len() > 1);
    assert!(must(r#"<!DOCTYPE r [<!NOTATION n PUBLIC "pub" "sys">]><r/>"#).len() > 1);
}

#[test]
fn d10_prefix_bindings_shadow_and_restore() {
    let events = must(r#"<a xmlns:p="urn:1"><p:b xmlns:p="urn:2"><p:c/></p:b><p:d/></a>"#);
    let nss: Vec<_> = events
        .iter()
        .filter_map(|ev| match ev {
            EventOwned::Empty { local, ns, .. } | EventOwned::Start { local, ns } => {
                Some((local.as_str(), ns.as_deref().map(str::to_owned)))
            }
            _ => None,
        })
        .collect();
    assert_eq!(nss[0], ("a", None));
    assert_eq!(nss[1].0, "b");
    assert_eq!(nss[1].1.as_deref(), Some("urn:2"));
    assert_eq!(nss[2].0, "c");
    assert_eq!(nss[2].1.as_deref(), Some("urn:2"));
    assert_eq!(nss[3].0, "d");
    assert_eq!(nss[3].1.as_deref(), Some("urn:1"));
}

#[test]
fn d12_defaults_and_token_normalization() {
    let events = must(r#"<!DOCTYPE r [<!ATTLIST r a NMTOKEN "  x   y  ">]><r/>"#);
    match &events[1] {
        EventOwned::Empty { attrs, .. } => assert_eq!(attrs, &[("a".into(), "x y".into())]),
        other => panic!("{other:?}"),
    }

    let events = must(r#"<!DOCTYPE r [<!ATTLIST r a NMTOKEN #IMPLIED>]><r a="  x   y  "/>"#);
    match &events[1] {
        EventOwned::Empty { attrs, .. } => assert_eq!(attrs[0].1, "x y"),
        other => panic!("{other:?}"),
    }

    let events = must(r#"<!DOCTYPE r [<!ATTLIST r a CDATA #IMPLIED>]><r a="  x   y  "/>"#);
    match &events[1] {
        EventOwned::Empty { attrs, .. } => assert_eq!(attrs[0].1, "  x   y  "),
        other => panic!("{other:?}"),
    }

    let events = must(
        r#"<!DOCTYPE a [<!ATTLIST a xmlns CDATA "urn:def" xmlns:p CDATA "urn:p">]><a><p:b/></a>"#,
    );
    assert!(events.iter().any(|ev| matches!(
        ev,
        EventOwned::Start { local, ns } if local == "a" && ns.as_deref() == Some("urn:def")
    )));
    assert!(events.iter().any(|ev| matches!(
        ev,
        EventOwned::Empty { local, ns, .. } if local == "b" && ns.as_deref() == Some("urn:p")
    )));
}

#[test]
fn d13_namespace_uris_are_interned() {
    let xml = r#"<a xmlns:p="urn:example"><p:b/><p:c/></a>"#;
    let mut reader = Reader::from_str(xml);
    let mut uris = Vec::new();
    loop {
        match reader.read_event().unwrap() {
            None => break,
            Some(Event::EmptyElement(el)) => uris.push(el.name.namespace.expect("namespace")),
            Some(_) => {}
        }
    }
    assert_eq!(uris.len(), 2);
    assert!(Rc::ptr_eq(&uris[0], &uris[1]));
    assert_eq!(&*uris[0], "urn:example");
}

#[test]
fn syntax_errors_are_syntax() {
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
