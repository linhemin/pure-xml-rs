//! Behavioral tests for the M1 pull parser. Security samples are inline so the
//! suite does not depend on files outside the repository.

use std::io::Cursor;

use purexml_core::{
    Error, ErrorKind, Event, LimitKind, ParserConfig, Reader, Writer, XmlText, DEFAULT_MAX_ATTRS,
    DEFAULT_MAX_DEPTH, DEFAULT_MAX_ENTITY_DEPTH, DEFAULT_MAX_ENTITY_EXPANSIONS,
    DEFAULT_MAX_EXPANDED_BYTES, DEFAULT_MAX_EXPANSION_RATIO, DEFAULT_MAX_NAME_LEN,
    DEFAULT_MAX_TEXT_LEN, DEFAULT_MAX_TOTAL_INPUT, EXPANSION_RATIO_FLOOR_BYTES, XML_NAMESPACE,
};

#[derive(Debug, PartialEq, Eq)]
enum Owned {
    Start {
        local: String,
        ns: Option<String>,
        prefix: Option<String>,
        attrs: Vec<(String, Option<String>, String)>,
        nss: Vec<(Option<String>, String)>,
    },
    End {
        local: String,
        ns: Option<String>,
    },
    Empty {
        local: String,
        ns: Option<String>,
        prefix: Option<String>,
        attrs: Vec<(String, Option<String>, String)>,
        nss: Vec<(Option<String>, String)>,
    },
    Text(String),
    CData(String),
    Comment(String),
    Pi {
        target: String,
        data: Option<String>,
    },
    Decl {
        version: String,
        encoding: Option<String>,
        standalone: Option<bool>,
    },
    Doctype {
        name: String,
        system_id: Option<String>,
        public_id: Option<String>,
        internal: bool,
    },
}

fn own(event: Event<'_>) -> Owned {
    match event {
        Event::StartElement(el) => Owned::Start {
            local: el.name.local.into_owned(),
            ns: el.name.namespace.as_deref().map(str::to_owned),
            attrs: el
                .attributes
                .iter()
                .map(|attr| {
                    (
                        attr.name.local.clone().into_owned(),
                        attr.name.namespace.as_deref().map(str::to_owned),
                        attr.value.as_str().to_owned(),
                    )
                })
                .collect(),
            nss: el
                .namespaces
                .iter()
                .map(|ns| {
                    (
                        ns.prefix.clone().map(XmlText::into_owned),
                        ns.uri.clone().into_owned(),
                    )
                })
                .collect(),
            prefix: el.name.prefix.map(XmlText::into_owned),
        },
        Event::EndElement(el) => Owned::End {
            local: el.name.local.into_owned(),
            ns: el.name.namespace.as_deref().map(str::to_owned),
        },
        Event::EmptyElement(el) => Owned::Empty {
            local: el.name.local.into_owned(),
            ns: el.name.namespace.as_deref().map(str::to_owned),
            attrs: el
                .attributes
                .iter()
                .map(|attr| {
                    (
                        attr.name.local.clone().into_owned(),
                        attr.name.namespace.as_deref().map(str::to_owned),
                        attr.value.as_str().to_owned(),
                    )
                })
                .collect(),
            nss: el
                .namespaces
                .iter()
                .map(|ns| {
                    (
                        ns.prefix.clone().map(XmlText::into_owned),
                        ns.uri.clone().into_owned(),
                    )
                })
                .collect(),
            prefix: el.name.prefix.map(XmlText::into_owned),
        },
        Event::Text(text) => Owned::Text(text.into_owned()),
        Event::CData(text) => Owned::CData(text.into_owned()),
        Event::Comment(text) => Owned::Comment(text.into_owned()),
        Event::PI(pi) => Owned::Pi {
            target: pi.target.into_owned(),
            data: pi.data.map(XmlText::into_owned),
        },
        Event::XmlDecl(decl) => Owned::Decl {
            version: decl.version.into_owned(),
            encoding: decl.encoding.map(XmlText::into_owned),
            standalone: decl.standalone,
        },
        Event::Doctype(dtd) => Owned::Doctype {
            name: dtd.name.into_owned(),
            system_id: dtd.system_id.map(XmlText::into_owned),
            public_id: dtd.public_id.map(XmlText::into_owned),
            internal: dtd.has_internal_subset,
        },
    }
}

fn parse(xml: &str) -> Result<Vec<Owned>, Error> {
    let mut reader = Reader::from_str(xml);
    let mut out = Vec::new();
    loop {
        match reader.read_event() {
            Ok(Some(event)) => out.push(own(event)),
            Ok(None) => return Ok(out),
            Err(err) => return Err(err),
        }
    }
}

fn parse_with(xml: &str, config: ParserConfig) -> Result<Vec<Owned>, Error> {
    let mut reader = Reader::from_str(xml).with_config(config);
    let mut out = Vec::new();
    loop {
        match reader.read_event() {
            Ok(Some(event)) => out.push(own(event)),
            Ok(None) => return Ok(out),
            Err(err) => return Err(err),
        }
    }
}

fn must(xml: &str) -> Vec<Owned> {
    parse(xml).unwrap_or_else(|err| panic!("{xml}\n{err}"))
}

fn must_err(xml: &str) -> Error {
    parse(xml).expect_err(xml)
}

fn limit_of(err: &Error) -> (LimitKind, u64, u64) {
    match err.kind {
        ErrorKind::SecurityLimit {
            kind,
            limit,
            actual,
        } => (kind, limit, actual),
        _ => panic!("expected SecurityLimit, got {err}"),
    }
}

fn entity_chain(depth: usize) -> String {
    let mut xml = String::from("<!DOCTYPE r [");
    for level in 1..depth {
        xml.push_str(&format!("<!ENTITY e{level} \"&e{};\">", level + 1));
    }
    xml.push_str(&format!("<!ENTITY e{depth} \"OK\">]>"));
    xml.push_str("<r>&e1;</r>");
    xml
}

fn repeated_refs(count: usize) -> String {
    let mut xml = String::from("<!DOCTYPE r [<!ENTITY a \"x\">]><r>");
    for _ in 0..count {
        xml.push_str("&a;");
    }
    xml.push_str("</r>");
    xml
}

fn ratio_doc(reps: usize, pad_before: &str, pad_after: &str) -> String {
    let body = "A".repeat(1024);
    format!(
        "<!DOCTYPE r [<!ENTITY a \"{body}\">]><r>{pad_before}{}{pad_after}</r>",
        "&a;".repeat(reps)
    )
}

#[test]
fn defaults_match_public_constants() {
    let config = ParserConfig::default();
    assert_eq!(config.max_depth, DEFAULT_MAX_DEPTH);
    assert_eq!(DEFAULT_MAX_DEPTH, 256);
    assert_eq!(config.max_attrs, DEFAULT_MAX_ATTRS);
    assert_eq!(DEFAULT_MAX_ATTRS, 1024);
    assert_eq!(config.max_name_len, DEFAULT_MAX_NAME_LEN);
    assert_eq!(DEFAULT_MAX_NAME_LEN, 1024);
    assert_eq!(config.max_text_len, DEFAULT_MAX_TEXT_LEN);
    assert_eq!(DEFAULT_MAX_TEXT_LEN, 16 * 1024 * 1024);
    assert_eq!(config.max_total_input, DEFAULT_MAX_TOTAL_INPUT);
    assert_eq!(DEFAULT_MAX_TOTAL_INPUT, 1024 * 1024 * 1024);
    assert_eq!(config.max_entity_depth, DEFAULT_MAX_ENTITY_DEPTH);
    assert_eq!(DEFAULT_MAX_ENTITY_DEPTH, 8);
    assert_eq!(config.max_entity_expansions, DEFAULT_MAX_ENTITY_EXPANSIONS);
    assert_eq!(DEFAULT_MAX_ENTITY_EXPANSIONS, 10_000);
    assert_eq!(config.max_expanded_bytes, DEFAULT_MAX_EXPANDED_BYTES);
    assert_eq!(DEFAULT_MAX_EXPANDED_BYTES, 1024 * 1024);
    assert_eq!(config.max_expansion_ratio, DEFAULT_MAX_EXPANSION_RATIO);
    assert_eq!(DEFAULT_MAX_EXPANSION_RATIO, 10);
    assert_eq!(EXPANSION_RATIO_FLOOR_BYTES, 64 * 1024);
}

#[test]
fn parses_elements_text_and_borrows_plain_text() {
    let xml = "<root id=\"1\">hello</root>";
    let mut reader = Reader::from_str(xml);
    match reader.read_event().unwrap().unwrap() {
        Event::StartElement(el) => {
            assert!(el.name.local.is_borrowed());
            assert_eq!(el.name.local.as_str(), "root");
            assert!(el.name.namespace.is_none());
            let attr = el.attributes().next().unwrap();
            assert_eq!(attr.name.local.as_ref(), "id");
            assert!(attr.value.is_borrowed());
            assert_eq!(attr.value.as_str(), "1");
            assert!(el.attribute("id").is_some());
        }
        other => panic!("{other:?}"),
    }
    match reader.read_event().unwrap().unwrap() {
        Event::Text(text) => {
            assert!(text.is_borrowed());
            assert_eq!(text.as_str(), "hello");
        }
        other => panic!("{other:?}"),
    }
    assert!(matches!(
        reader.read_event().unwrap().unwrap(),
        Event::EndElement(_)
    ));
    assert!(reader.read_event().unwrap().is_none());
}

#[test]
fn empty_element_cdata_comment_pi_and_declaration() {
    let events = must(
        r#"<?xml version="1.0" encoding="utf-8" standalone="yes"?>
        <!--note-->
        <!DOCTYPE r>
        <?style data?>
        <r><![CDATA[a&b<c]]></r>
        <!--after-->"#,
    );
    assert!(matches!(
        &events[0],
        Owned::Decl {
            version,
            encoding: Some(enc),
            standalone: Some(true),
        } if version == "1.0" && enc.eq_ignore_ascii_case("utf-8")
    ));
    assert_eq!(events[1], Owned::Comment("note".into()));
    assert_eq!(
        events[2],
        Owned::Doctype {
            name: "r".into(),
            system_id: None,
            public_id: None,
            internal: false,
        }
    );
    assert_eq!(
        events[3],
        Owned::Pi {
            target: "style".into(),
            data: Some("data".into()),
        }
    );
    assert!(matches!(&events[4], Owned::Start { local, .. } if local == "r"));
    assert_eq!(events[5], Owned::CData("a&b<c".into()));
    assert!(matches!(&events[6], Owned::End { local, .. } if local == "r"));
    assert_eq!(events[7], Owned::Comment("after".into()));
}

#[test]
fn pi_without_data_and_with_only_separator() {
    let events = must("<?target?><r/><?target ?>");
    assert_eq!(
        events[0],
        Owned::Pi {
            target: "target".into(),
            data: None,
        }
    );
    assert_eq!(
        events[2],
        Owned::Pi {
            target: "target".into(),
            data: Some(String::new()),
        }
    );
}

#[test]
fn predefined_and_numeric_character_references() {
    assert_eq!(
        must("<a>&lt;&gt;&amp;&apos;&quot;&#65;&#x42;</a>")
            .into_iter()
            .find_map(|ev| match ev {
                Owned::Text(text) => Some(text),
                _ => None,
            }),
        Some("<>&'\"AB".into())
    );
}

#[test]
fn character_reference_to_cr_is_preserved_in_text() {
    let events = must("<a>A&#13;B</a>");
    assert!(events
        .iter()
        .any(|ev| matches!(ev, Owned::Text(text) if text.as_str() == "A\rB")));
}

#[test]
fn crlf_in_text_becomes_lf_and_is_owned() {
    let xml = "<a>A\r\nB</a>";
    let mut reader = Reader::from_str(xml);
    let _ = reader.read_event().unwrap().unwrap();
    match reader.read_event().unwrap().unwrap() {
        Event::Text(text) if !text.is_borrowed() && text.as_str() == "A\nB" => {}
        other => panic!("{other:?}"),
    }
}

#[test]
fn literal_newline_in_attribute_becomes_space_but_char_ref_does_not() {
    let literal = must("<a b=\"X\nY\"/>");
    match &literal[0] {
        Owned::Empty { attrs, .. } => assert_eq!(attrs[0].2, "X Y"),
        other => panic!("{other:?}"),
    }
    let referenced = must("<a b=\"X&#10;Y\"/>");
    match &referenced[0] {
        Owned::Empty { attrs, .. } => assert_eq!(attrs[0].2, "X\nY"),
        other => panic!("{other:?}"),
    }
}

#[test]
fn internal_entities_expand_including_markup_and_predefined() {
    let events = must(
        r#"<!DOCTYPE r [<!ENTITY e "<a b='v'>t</a>"><!ENTITY ampish "&amp;">]><r>&e;&ampish;</r>"#,
    );
    assert!(matches!(&events[0], Owned::Doctype { name, internal: true, .. } if name == "r"));
    assert!(matches!(&events[1], Owned::Start { local, .. } if local == "r"));
    assert!(
        matches!(&events[2], Owned::Start { local, attrs, .. } if local == "a" && attrs[0].2 == "v")
    );
    assert_eq!(events[3], Owned::Text("t".into()));
    assert!(matches!(&events[4], Owned::End { local, .. } if local == "a"));
    assert_eq!(events[5], Owned::Text("&".into()));
}

#[test]
fn char_ref_is_expanded_when_the_entity_is_declared() {
    let events = must(r#"<!DOCTYPE r [<!ENTITY e "&#65;">]><r>&e;</r>"#);
    assert!(events
        .iter()
        .any(|ev| matches!(ev, Owned::Text(text) if text == "A")));
}

#[test]
fn entity_quote_and_lt_are_literal_inside_attributes() {
    let events =
        must(r#"<!DOCTYPE a [<!ENTITY q "&#34;"><!ENTITY lt "&#60;">]><a b="x&q;y" c="&lt;"/>"#);
    match &events[1] {
        Owned::Empty { attrs, .. } => {
            assert_eq!(attrs[0].2, "x\"y");
            assert_eq!(attrs[1].2, "<");
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn namespaces_resolve_and_do_not_apply_default_to_attributes() {
    let events = must(
        r#"<a xmlns="urn:def" xmlns:p="urn:p" p:x="1" y="2" xml:lang="en"><p:b/><c xmlns=""/></a>"#,
    );
    match &events[0] {
        Owned::Start {
            local,
            ns,
            attrs,
            nss,
            ..
        } => {
            assert_eq!(local, "a");
            assert_eq!(ns.as_deref(), Some("urn:def"));
            assert_eq!(nss.len(), 2);
            assert_eq!(attrs.len(), 3);
            assert_eq!(attrs[0].0, "x");
            assert_eq!(attrs[0].1.as_deref(), Some("urn:p"));
            assert_eq!(attrs[1].0, "y");
            assert_eq!(attrs[1].1, None);
            assert_eq!(attrs[2].0, "lang");
            assert_eq!(attrs[2].1.as_deref(), Some(XML_NAMESPACE));
        }
        other => panic!("{other:?}"),
    }
    match &events[1] {
        Owned::Empty {
            local, ns, prefix, ..
        } => {
            assert_eq!(local, "b");
            assert_eq!(prefix.as_deref(), Some("p"));
            assert_eq!(ns.as_deref(), Some("urn:p"));
        }
        other => panic!("{other:?}"),
    }
    match &events[2] {
        Owned::Empty { local, ns, .. } => {
            assert_eq!(local, "c");
            assert!(ns.is_none());
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn child_inherits_default_namespace() {
    let events = must(r#"<a xmlns="urn:def"><b/></a>"#);
    match &events[1] {
        Owned::Empty { local, ns, .. } => {
            assert_eq!(local, "b");
            assert_eq!(ns.as_deref(), Some("urn:def"));
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn unbound_prefix_is_syntax() {
    let err = must_err("<p:child/>");
    assert!(matches!(err.kind, ErrorKind::Syntax));
    assert_eq!(err.position.line, 1);
}

#[test]
fn duplicate_expanded_attributes_are_syntax() {
    let err = must_err(r#"<a xmlns:p="urn" xmlns:q="urn" p:x="1" q:x="2"/>"#);
    assert!(matches!(err.kind, ErrorKind::Syntax), "{err}");
}

#[test]
fn mismatched_end_tag_reports_position() {
    let err = must_err("<a></b>");
    assert!(matches!(err.kind, ErrorKind::Syntax));
    assert_eq!(err.position.line, 1);
    assert_eq!(err.position.column, 4);
    assert_eq!(err.position.offset, 3);

    let err = must_err("<a>\n</b>");
    assert_eq!(err.position.line, 2);
    assert_eq!(err.position.column, 1);
    assert_eq!(err.position.offset, 4);
}

#[test]
fn crlf_before_mismatched_end_tag() {
    let err = must_err("<a>\r\n</b>");
    assert_eq!(err.position.line, 2);
    assert_eq!(err.position.column, 1);
    assert_eq!(err.position.offset, 5);
}

#[test]
fn rejects_illegal_characters_and_bad_names() {
    assert!(matches!(must_err("<a>&#0;</a>").kind, ErrorKind::Syntax));
    assert!(matches!(
        must_err("<a>&#xD800;</a>").kind,
        ErrorKind::Syntax
    ));
    assert!(matches!(
        must_err("<a>&#xFFFE;</a>").kind,
        ErrorKind::Syntax
    ));
    assert!(matches!(must_err("<a>]]></a>").kind, ErrorKind::Syntax));
    assert!(matches!(
        must_err("<a><!-- bad -- comment --></a>").kind,
        ErrorKind::Syntax
    ));
    assert!(matches!(must_err("<1a/>").kind, ErrorKind::Syntax));
    assert!(matches!(must_err("<a></a><b/>").kind, ErrorKind::Syntax));
    assert!(matches!(must_err("<a>").kind, ErrorKind::Syntax));
}

#[test]
fn utf8_bom_is_skipped_and_bad_encodings_fail() {
    let mut bytes = vec![0xEF, 0xBB, 0xBF];
    bytes.extend_from_slice(b"<a/>");
    let mut reader = Reader::from_bytes(&bytes);
    assert!(matches!(
        reader.read_event().unwrap().unwrap(),
        Event::EmptyElement(_)
    ));

    let err = Reader::from_bytes(&[0xFF, 0xFE, b'<', b'a', b'/', b'>'])
        .read_event()
        .unwrap_err();
    assert!(matches!(err.kind, ErrorKind::Encoding), "{err}");

    let err = Reader::from_bytes(b"\xFF<a/>").read_event().unwrap_err();
    assert!(matches!(err.kind, ErrorKind::Encoding), "{err}");

    let err = must_err(r#"<?xml version="1.0" encoding="UTF-16"?><a/>"#);
    assert!(matches!(err.kind, ErrorKind::Encoding), "{err}");
    let err = must_err(r#"<?xml version="1.0" encoding="ISO-8859-1"?><a/>"#);
    assert!(matches!(err.kind, ErrorKind::Encoding), "{err}");
}

#[test]
fn external_dtd_is_skipped_and_ids_are_exposed() {
    let events = must(r#"<!DOCTYPE foo SYSTEM "http://evil.example/x.dtd"><foo/>"#);
    assert_eq!(
        events[0],
        Owned::Doctype {
            name: "foo".into(),
            system_id: Some("http://evil.example/x.dtd".into()),
            public_id: None,
            internal: false,
        }
    );
    assert!(matches!(events[1], Owned::Empty { .. }));

    let events = must(r#"<!DOCTYPE foo PUBLIC "-//Example//EN" "file:///etc/passwd"><foo/>"#);
    match &events[0] {
        Owned::Doctype {
            public_id,
            system_id,
            internal,
            ..
        } => {
            assert_eq!(public_id.as_deref(), Some("-//Example//EN"));
            assert_eq!(system_id.as_deref(), Some("file:///etc/passwd"));
            assert!(!internal);
        }
        other => panic!("{other:?}"),
    }

    let events = must(r#"<?xml version="1.0" standalone="no"?><!DOCTYPE a SYSTEM "x"><a/>"#);
    assert!(events.len() >= 2);
}

#[test]
fn undefined_entity_is_syntax_even_if_an_external_dtd_was_named() {
    let err = must_err("<foo>&nope;</foo>");
    assert!(matches!(err.kind, ErrorKind::Syntax), "{err}");
    let err = must_err(r#"<!DOCTYPE foo SYSTEM "x.dtd"><foo>&nope;</foo>"#);
    assert!(matches!(err.kind, ErrorKind::Syntax), "{err}");
    assert!(!matches!(err.kind, ErrorKind::ExternalEntityForbidden));
}

#[test]
fn external_entity_declarations_are_forbidden() {
    for xml in [
        r#"<!DOCTYPE foo [<!ENTITY xxe SYSTEM "file:///etc/passwd">]><foo/>"#,
        r#"<!DOCTYPE foo [<!ENTITY xxe PUBLIC "-//OASIS//X" "http://evil.example/x">]><foo/>"#,
        r#"<!DOCTYPE foo [<!ENTITY % remote SYSTEM "http://evil.example/x.dtd">]><foo/>"#,
        r#"<!DOCTYPE foo [<!ENTITY % remote PUBLIC "pub" "http://evil.example/x.dtd">]><foo/>"#,
    ] {
        let err = must_err(xml);
        assert!(
            matches!(err.kind, ErrorKind::ExternalEntityForbidden),
            "{xml} -> {err}"
        );
        assert!(err.position.line >= 1);
    }
}

#[test]
fn internal_parameter_entity_may_be_declared_but_not_referenced() {
    let events = must(r#"<!DOCTYPE foo [<!ENTITY % p "bar">]><foo/>"#);
    assert!(matches!(&events[0], Owned::Doctype { internal: true, .. }));
    assert!(matches!(events[1], Owned::Empty { .. }));

    let err = must_err(r#"<!DOCTYPE foo [<!ENTITY % p "bar"> %p;]><foo/>"#);
    assert!(
        matches!(err.kind, ErrorKind::ExternalEntityForbidden),
        "{err}"
    );

    let err = must_err(r#"<!DOCTYPE foo [<!ENTITY % p "bar"><!ENTITY e "a%p;b">]><foo/>"#);
    assert!(
        matches!(err.kind, ErrorKind::ExternalEntityForbidden),
        "{err}"
    );
}

#[test]
fn element_and_notation_declarations_are_skipped() {
    let events = must(
        r#"<!DOCTYPE r [
            <!ELEMENT r (#PCDATA)>
            <!ATTLIST r a CDATA "x">
            <!NOTATION n SYSTEM "notation.file">
            <!ENTITY e "z">
        ]><r>&e;</r>"#,
    );
    assert!(events
        .iter()
        .any(|ev| matches!(ev, Owned::Text(text) if text == "z")));
    match events.iter().find(|ev| matches!(ev, Owned::Start { .. })) {
        Some(Owned::Start { attrs, .. }) => {
            assert_eq!(attrs.len(), 1, "ATTLIST default is applied");
            assert_eq!(attrs[0].0, "a");
            assert_eq!(attrs[0].2, "x");
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn entity_depth_eight_ok_nine_fails() {
    let events = must(&entity_chain(8));
    assert!(events
        .iter()
        .any(|ev| matches!(ev, Owned::Text(text) if text == "OK")));

    let err = must_err(&entity_chain(9));
    let (kind, limit, actual) = limit_of(&err);
    assert_eq!(kind, LimitKind::EntityDepth);
    assert_eq!(limit, 8);
    assert_eq!(actual, 9);
    assert_eq!(err.position.line, 1);
}

#[test]
fn ten_thousand_expansions_ok_one_more_fails() {
    assert!(parse(&repeated_refs(10_000)).is_ok());
    let err = must_err(&repeated_refs(10_001));
    let (kind, limit, actual) = limit_of(&err);
    assert_eq!(kind, LimitKind::EntityExpansions);
    assert_eq!(limit, 10_000);
    assert_eq!(actual, 10_001);
}

#[test]
fn expansion_ratio_is_streaming_and_respects_the_floor() {
    assert!(
        parse(&ratio_doc(64, "", "")).is_ok(),
        "64 KiB exactly stays under the floor"
    );

    let err = must_err(&ratio_doc(65, "", ""));
    let (kind, limit, actual) = limit_of(&err);
    assert_eq!(kind, LimitKind::ExpansionRatio);
    assert!(actual > EXPANSION_RATIO_FLOOR_BYTES);
    assert!(actual > limit, "actual {actual} limit {limit}");

    let err = must_err(&ratio_doc(65, "", &"Z".repeat(8_000)));
    assert_eq!(limit_of(&err).0, LimitKind::ExpansionRatio);

    assert!(
        parse(&ratio_doc(65, &"Z".repeat(8_000), "")).is_ok(),
        "padding already consumed raises the denominator"
    );
}

#[test]
fn expansion_ratio_zero_rejects_past_the_floor_and_max_disables_the_check() {
    let under = ratio_doc(64, "", "");
    assert!(
        parse_with(
            &under,
            ParserConfig {
                max_expansion_ratio: 0,
                ..ParserConfig::default()
            },
        )
        .is_ok(),
        "at the 64 KiB floor a zero ratio still parses"
    );

    let err = parse_with(
        &ratio_doc(65, "", ""),
        ParserConfig {
            max_expansion_ratio: 0,
            ..ParserConfig::default()
        },
    )
    .unwrap_err();
    let (kind, limit, actual) = limit_of(&err);
    assert_eq!(kind, LimitKind::ExpansionRatio);
    assert_eq!(limit, 0);
    assert!(actual > EXPANSION_RATIO_FLOOR_BYTES);

    assert!(
        parse_with(
            &ratio_doc(65, "", ""),
            ParserConfig {
                max_expansion_ratio: u64::MAX,
                ..ParserConfig::default()
            },
        )
        .is_ok(),
        "u64::MAX disables the ratio check"
    );
}

#[test]
fn injected_defaults_use_the_expansion_ratio_not_the_byte_cap() {
    let once = format!(
        "<!DOCTYPE r [<!ATTLIST e a CDATA \"{}\">]><r><e/></r>",
        "v".repeat(1_500_000)
    );
    assert!(
        parse(&once).is_ok(),
        "one injected default stays under the ratio and does not use the 1 MiB entity cap"
    );

    let repeated = format!(
        "<!DOCTYPE r [<!ATTLIST e a CDATA \"{}\">]><r>{}</r>",
        "v".repeat(80 * 1024),
        "<e/>".repeat(20)
    );
    let err = must_err(&repeated);
    let (kind, _, _) = limit_of(&err);
    assert_eq!(
        kind,
        LimitKind::ExpansionRatio,
        "twenty injections of one 80 KiB default exceed the expansion ratio"
    );

    let name = "n".repeat(400);
    let mut decls = String::from("<!DOCTYPE r [");
    for i in 0..200 {
        decls.push_str(&format!("<!ATTLIST e {name}{i:04} CDATA \"\">"));
    }
    decls.push_str("]>");
    let many = format!("{decls}<r>{}</r>", "<e/>".repeat(30));
    let err = must_err(&many);
    let (kind, _, _) = limit_of(&err);
    assert_eq!(kind, LimitKind::ExpansionRatio);
}

#[test]
fn expanded_bytes_attribute_text_name_depth_and_input_limits() {
    let err = parse_with(
        r#"<!DOCTYPE r [<!ENTITY a "0123456789ABCDEFGHIJ">]><r>&a;</r>"#,
        ParserConfig {
            max_expanded_bytes: 10,
            ..ParserConfig::default()
        },
    )
    .unwrap_err();
    let (kind, limit, actual) = limit_of(&err);
    assert_eq!(kind, LimitKind::ExpandedBytes);
    assert_eq!(limit, 10);
    assert_eq!(actual, 20);

    let err = parse_with(
        "<a>0123456789</a>",
        ParserConfig {
            max_text_len: 4,
            ..ParserConfig::default()
        },
    )
    .unwrap_err();
    let (kind, limit, actual) = limit_of(&err);
    assert_eq!(kind, LimitKind::TextLength);
    assert_eq!(limit, 4);
    assert!(actual > limit);

    let err = parse_with(
        r#"<a b="0123456789"/>"#,
        ParserConfig {
            max_text_len: 4,
            ..ParserConfig::default()
        },
    )
    .unwrap_err();
    let (kind, limit, actual) = limit_of(&err);
    assert_eq!(kind, LimitKind::AttrValueLength);
    assert_eq!(limit, 4);
    // First character past the cap, on the plain scan and after expansion.
    assert_eq!(actual, 5);

    let err = parse_with(
        r#"<!DOCTYPE a [<!ENTITY e "0123456789">]><a b="&e;"/>"#,
        ParserConfig {
            max_text_len: 4,
            ..ParserConfig::default()
        },
    )
    .unwrap_err();
    let (kind, limit, actual) = limit_of(&err);
    assert_eq!(kind, LimitKind::AttrValueLength);
    assert_eq!(limit, 4);
    assert_eq!(actual, 5);

    let err = parse_with(
        "<a>ab&#10;c</a>",
        ParserConfig {
            max_text_len: 3,
            ..ParserConfig::default()
        },
    )
    .unwrap_err();
    assert_eq!(limit_of(&err).0, LimitKind::TextLength);
    assert!(parse_with(
        "<a>a&#10;b</a>",
        ParserConfig {
            max_text_len: 3,
            ..ParserConfig::default()
        }
    )
    .is_ok());

    let mut name = String::from("<");
    name.push_str(&"a".repeat(1025));
    name.push_str("/>");
    let err = must_err(&name);
    let (kind, limit, actual) = limit_of(&err);
    assert_eq!(kind, LimitKind::NameLength);
    assert_eq!(limit, 1024);
    assert_eq!(actual, 1025);

    let err = parse_with(
        "<abcdefgh/>",
        ParserConfig {
            max_total_input: 4,
            ..ParserConfig::default()
        },
    )
    .unwrap_err();
    let (kind, limit, actual) = limit_of(&err);
    assert_eq!(kind, LimitKind::TotalInput);
    assert_eq!(limit, 4);
    assert!(actual > limit);

    let mut attrs = String::from("<a");
    for index in 0..3 {
        attrs.push_str(&format!(" a{index}=\"1\""));
    }
    attrs.push_str("/>");
    let err = parse_with(
        &attrs,
        ParserConfig {
            max_attrs: 2,
            ..ParserConfig::default()
        },
    )
    .unwrap_err();
    let (kind, limit, actual) = limit_of(&err);
    assert_eq!(kind, LimitKind::Attributes);
    assert_eq!(limit, 2);
    assert_eq!(actual, 3);
}

#[test]
fn default_element_depth_and_attribute_caps() {
    let mut ok = String::new();
    let mut deep = String::new();
    for _ in 0..256 {
        ok.push_str("<a>");
        deep.push_str("<a>");
    }
    deep.push_str("<a>");
    for _ in 0..256 {
        ok.push_str("</a>");
        deep.push_str("</a>");
    }
    deep.push_str("</a>");
    assert!(parse(&ok).is_ok());
    let err = must_err(&deep);
    let (kind, limit, actual) = limit_of(&err);
    assert_eq!(kind, LimitKind::ElementDepth);
    assert_eq!(limit, 256);
    assert_eq!(actual, 257);

    let mut element = String::from("<a");
    for index in 0..1024 {
        element.push_str(&format!(" a{index}=\"1\""));
    }
    element.push_str("/>");
    assert!(parse(&element).is_ok());
    element.insert_str(2, " extra=\"1\"");
    // The string above is no longer a clean 1025-attribute document; build it explicitly.
    let mut too_many = String::from("<a");
    for index in 0..1025 {
        too_many.push_str(&format!(" a{index}=\"1\""));
    }
    too_many.push_str("/>");
    let err = must_err(&too_many);
    let (kind, limit, actual) = limit_of(&err);
    assert_eq!(kind, LimitKind::Attributes);
    assert_eq!(limit, 1024);
    assert_eq!(actual, 1025);
}

#[test]
fn recursive_entity_is_syntax_and_nested_bomb_hits_a_limit() {
    let err = must_err(r#"<!DOCTYPE r [<!ENTITY a "&a;">]><r>&a;</r>"#);
    assert!(matches!(err.kind, ErrorKind::Syntax), "{err}");

    let err = must_err(
        r#"<!DOCTYPE r [
            <!ENTITY a "aaaaaaaaaa">
            <!ENTITY b "&a;&a;&a;&a;&a;&a;&a;&a;&a;&a;">
            <!ENTITY c "&b;&b;&b;&b;&b;&b;&b;&b;&b;&b;">
            <!ENTITY d "&c;&c;&c;&c;&c;&c;&c;&c;&c;&c;">
            <!ENTITY e "&d;&d;&d;&d;&d;&d;&d;&d;&d;&d;">
            <!ENTITY f "&e;&e;&e;&e;&e;&e;&e;&e;&e;&e;">
        ]><r>&f;</r>"#,
    );
    assert!(
        matches!(
            err.kind,
            ErrorKind::SecurityLimit {
                kind: LimitKind::EntityExpansions
                    | LimitKind::ExpandedBytes
                    | LimitKind::ExpansionRatio
                    | LimitKind::EntityDepth,
                ..
            }
        ),
        "{err}"
    );
}

#[test]
fn from_reader_and_writer_round_trip() {
    let xml = "<root id=\"1\">hello</root>";
    let mut reader = Reader::from_reader(Cursor::new(xml.as_bytes())).unwrap();
    let mut writer = Writer::new(Vec::new());
    while let Some(event) = reader.read_event().unwrap() {
        writer.write_event(&event).unwrap();
    }
    let written = String::from_utf8(writer.into_inner()).unwrap();
    let again = must(&written);
    assert!(matches!(&again[0], Owned::Start { local, .. } if local == "root"));
    assert_eq!(again[1], Owned::Text("hello".into()));
}

#[test]
fn unicode_name_and_text() {
    let events = must("<é>é</é>");
    assert!(matches!(&events[0], Owned::Start { local, .. } if local == "é"));
    assert_eq!(events[1], Owned::Text("é".into()));
}
