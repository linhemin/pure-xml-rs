//! M2 PR-A cases: incremental attribute-value limits (D11 / D-Q4), the
//! shared NCName / QName rules (D23), and per-character comment, CDATA, and
//! PI lengths (D26).

use purexml::{
    Doctype, Error, ErrorKind, Event, LimitKind, ParserConfig, ProcessingInstruction, Reader,
    Writer, XmlText,
};

fn config(max_text_len: u64) -> ParserConfig {
    ParserConfig {
        max_text_len,
        ..ParserConfig::default()
    }
}

fn pull(xml: &str, cfg: ParserConfig) -> Result<Vec<EventOwned>, Error> {
    let mut reader = Reader::from_str(xml).with_config(cfg);
    let mut out = Vec::new();
    loop {
        match reader.read_event() {
            Ok(None) => return Ok(out),
            Ok(Some(Event::Doctype(dtd))) => out.push(EventOwned::Doctype(dtd.name.into_owned())),
            Ok(Some(Event::PI(pi))) => out.push(EventOwned::Pi(pi.target.into_owned())),
            Ok(Some(_)) => out.push(EventOwned::Other),
            Err(err) => return Err(err),
        }
    }
}

fn must_ok(xml: &str) -> Vec<EventOwned> {
    pull(xml, ParserConfig::default()).unwrap_or_else(|err| panic!("{xml}\n{err}"))
}

fn must_syntax(xml: &str) -> Error {
    let err = pull(xml, ParserConfig::default()).expect_err(xml);
    assert!(
        matches!(err.kind, ErrorKind::Syntax),
        "{xml}\nexpected Syntax, got {err}"
    );
    err
}

fn must_limit(xml: &str, max_text_len: u64) -> Error {
    let err = pull(xml, config(max_text_len)).expect_err(xml);
    match err.kind {
        ErrorKind::SecurityLimit {
            kind: LimitKind::AttrValueLength,
            ..
        } => err,
        _ => panic!("{xml}\nexpected AttrValueLength, got {err}"),
    }
}

fn attr_actual(err: &Error) -> (u64, u64, u64) {
    match err.kind {
        ErrorKind::SecurityLimit {
            kind: LimitKind::AttrValueLength,
            limit,
            actual,
        } => (limit, actual, err.position.offset),
        _ => panic!("{err}"),
    }
}

#[derive(Debug)]
enum EventOwned {
    Doctype(String),
    Pi(String),
    Other,
}

#[test]
fn d11_plain_attr_stops_at_the_first_overflow() {
    let xml = "<r a=\"0123456789\"/>";
    let err = must_limit(xml, 4);
    let (limit, actual, offset) = attr_actual(&err);
    assert_eq!(limit, 4);
    assert_eq!(actual, 5);
    let value_at = xml.find('"').unwrap() + 1;
    assert_eq!(offset, (value_at + 5) as u64);
    assert!(offset < xml.rfind('"').unwrap() as u64);

    assert!(pull("<r a=\"0123\"/>", config(4)).is_ok());
    assert!(pull("<r a=\"\"/>", config(0)).is_ok());
    let err = must_limit("<r a=\"x\"/>", 0);
    assert_eq!(attr_actual(&err).1, 1);
}

#[test]
fn d11_slow_attr_does_not_buffer_the_tail() {
    let head = "<r a=\"&amp;";
    let mut xml = String::from(head);
    xml.push_str(&"x".repeat(4));
    xml.push_str(&"y".repeat(1 << 20));
    xml.push_str("\"/>");
    let err = must_limit(&xml, 4);
    let (limit, actual, offset) = attr_actual(&err);
    assert_eq!(limit, 4);
    // `&amp;` contributes one byte, then four `x`s make length 5.
    assert_eq!(actual, 5);
    assert_eq!(offset, (head.len() + 4) as u64);
    assert!(offset + 1_000 < xml.len() as u64);

    assert!(pull("<r a=\"&amp;xxx\"/>", config(4)).is_ok());
    let tab = "<r a=\"\txxx\"/>";
    assert!(pull(tab, config(4)).is_ok());
    let err = must_limit("<r a=\"\txxxx\"/>", 4);
    assert_eq!(attr_actual(&err).1, 5);
}

#[test]
fn d11_multibyte_char_reports_its_utf8_length() {
    // U+4E2D is three UTF-8 bytes. 2 + 3 crosses a cap of 4 at length 5.
    let xml = "<r a=\"ab&#x4E2D;xxxx\"/>";
    let err = must_limit(xml, 4);
    let (limit, actual, offset) = attr_actual(&err);
    assert_eq!(limit, 4);
    assert_eq!(actual, 5);
    assert!(offset < xml.len() as u64);
}

#[test]
fn d23_pi_entity_and_notation_names_are_ncnames() {
    for xml in [
        "<r><?a:b x?></r>",
        "<r><?a: x?></r>",
        "<?p:q?><r/>",
        "<?:a ?><r/>",
        "<?xmlns: ?><r/>",
        "<?:ml ?><r/>",
        "<!DOCTYPE r [<?a:b x?>]><r/>",
        "<!DOCTYPE r [<!ENTITY a:b \"x\">]><r/>",
        "<!DOCTYPE r [<!ENTITY % a:b \"x\">]><r/>",
        "<!DOCTYPE r [<!NOTATION a:b SYSTEM \"x\">]><r/>",
        "<!DOCTYPE r [<!ATTLIST r a NOTATION (a:b) #IMPLIED>]><r/>",
        "<!DOCTYPE r [<!ENTITY e SYSTEM \"u\" NDATA a:b>]><r/>",
        "<r>&a:b;</r>",
        "<!DOCTYPE r [<!ENTITY e \"&a:b;\">]><r/>",
        "<!DOCTYPE r [<!ENTITY e \"<?a:b x?>\">]><r>&e;</r>",
    ] {
        let err = must_syntax(xml);
        assert!(
            !err.to_string().contains("undefined entity"),
            "{xml}\n{err}"
        );
    }

    let err = must_syntax("<r>&missing;</r>");
    assert!(err.to_string().contains("undefined entity"), "{err}");

    let events = must_ok("<r><?ab x?></r>");
    assert!(
        events
            .iter()
            .any(|ev| matches!(ev, EventOwned::Pi(t) if t == "ab"))
    );
    let events = must_ok("<?xml-stylesheet type=\"text/xsl\" href=\"a\"?><r/>");
    assert!(
        events
            .iter()
            .any(|ev| matches!(ev, EventOwned::Pi(t) if t == "xml-stylesheet"))
    );
}

#[test]
fn d23_doctype_name_is_a_qname() {
    for xml in [
        "<!DOCTYPE r: SYSTEM \"x.dtd\"><r/>",
        "<!DOCTYPE :r><r/>",
        "<!DOCTYPE a:b:c><r/>",
        "<!DOCTYPE r:><r/>",
    ] {
        must_syntax(xml);
    }
    let events = must_ok("<!DOCTYPE a:b><r/>");
    assert!(
        events
            .iter()
            .any(|ev| matches!(ev, EventOwned::Doctype(name) if name == "a:b"))
    );
}

#[test]
fn d23_writer_uses_the_same_name_rules() {
    for target in [":a", "a:", "xml:x", "xmlns:", ":ml", "a:b", "p:q"] {
        let mut writer = Writer::new(Vec::new());
        assert!(writer.write_pi_str(target, Some("x")).is_err(), "{target}");
        assert!(writer.get_ref().is_empty(), "{target}");
        let event = Event::PI(ProcessingInstruction {
            target: XmlText::borrowed(target),
            data: None,
        });
        let mut writer = Writer::new(Vec::new());
        assert!(writer.write_event(&event).is_err(), "{target}");
        assert!(writer.get_ref().is_empty(), "{target}");
    }

    let mut writer = Writer::new(Vec::new());
    writer.write_pi_str("ab", Some("x")).unwrap();
    assert_eq!(writer.get_ref(), b"<?ab x?>");

    for name in ["r:", ":r", "a:b:c", "a::b"] {
        let mut writer = Writer::new(Vec::new());
        let event = doctype(name);
        assert!(writer.write_event(&event).is_err(), "{name}");
        assert!(writer.get_ref().is_empty(), "{name}");
    }
    let mut writer = Writer::new(Vec::new());
    writer.write_event(&doctype("a:b")).unwrap();
    assert_eq!(writer.get_ref(), b"<!DOCTYPE a:b>");

    let events = must_ok("<?ab data?><r/>");
    let mut reader = Reader::from_str("<?ab data?><r/>");
    let pi = reader.read_event().unwrap().unwrap();
    let mut writer = Writer::new(Vec::new());
    writer.write_event(&pi).unwrap();
    assert!(events.iter().any(|ev| matches!(ev, EventOwned::Pi(_))));
    assert_eq!(&writer.get_ref()[..], b"<?ab data?>");
}

fn doctype(name: &str) -> Event<'_> {
    Event::Doctype(Doctype {
        name: XmlText::borrowed(name),
        system_id: None,
        public_id: None,
        has_internal_subset: false,
    })
}

fn text_actual(err: &Error) -> (u64, u64, u64) {
    match err.kind {
        ErrorKind::SecurityLimit {
            kind: LimitKind::TextLength,
            limit,
            actual,
        } => (limit, actual, err.position.offset),
        _ => panic!("expected TextLength, got {err}"),
    }
}

/// `from_bytes` and `from_reader` report the same TextLength, including offset.
fn assert_text_limit(xml: &str, max: u64, actual: u64, body_at: usize) {
    let slice = pull(xml, config(max)).expect_err(xml);
    let (limit, got, offset) = text_actual(&slice);
    assert_eq!(limit, max, "{xml}");
    assert_eq!(got, actual, "{xml}");
    assert_eq!(offset, (body_at + actual as usize) as u64, "{xml}");
    assert!(offset < xml.len() as u64, "{xml}");

    let streamed = read_reader(xml, max).expect_err(xml);
    assert_eq!(slice.kind, streamed.kind, "{xml}");
    assert_eq!(slice.position, streamed.position, "{xml}");
}

fn read_reader(xml: &str, max: u64) -> Result<Vec<EventOwned>, Error> {
    let mut streamed = Reader::from_reader(std::io::Cursor::new(xml.as_bytes()))
        .unwrap()
        .with_config(config(max));
    let mut out = Vec::new();
    loop {
        match streamed.read_event() {
            Ok(None) => return Ok(out),
            Ok(Some(Event::Doctype(dtd))) => out.push(EventOwned::Doctype(dtd.name.into_owned())),
            Ok(Some(Event::PI(pi))) => out.push(EventOwned::Pi(pi.target.into_owned())),
            Ok(Some(_)) => out.push(EventOwned::Other),
            Err(err) => return Err(err),
        }
    }
}

#[test]
fn d26_comment_cdata_and_pi_stop_at_the_first_overflow() {
    let comment = format!("<!--{}--><r/>", "0123456789");
    assert_text_limit(&comment, 4, 5, comment.find('0').unwrap());
    let long = format!("<!--{}--><r/>", "x".repeat(100_000));
    assert_text_limit(&long, 4, 5, long.find('x').unwrap());
    assert!(pull("<!--0123--><r/>", config(4)).is_ok());
    assert!(pull("<!----><r/>", config(0)).is_ok());
    let one = "<!--x--><r/>";
    assert_text_limit(one, 0, 1, one.find('x').unwrap());

    let cdata = format!("<r><![CDATA[{}]]></r>", "0123456789");
    assert_text_limit(&cdata, 4, 5, cdata.find('0').unwrap());
    assert!(pull("<r><![CDATA[0123]]></r>", config(4)).is_ok());

    let pi = format!("<?t {}?><r/>", "0123456789");
    assert_text_limit(&pi, 4, 5, pi.find('0').unwrap());
    assert!(pull("<?t 0123?><r/>", config(4)).is_ok());
    assert!(pull("<?t?><r/>", config(0)).is_ok());
    assert!(pull("<?t ?><r/>", config(0)).is_ok());
}

#[test]
fn d26_multibyte_crlf_and_unterminated_comment() {
    // U+00E9 is two bytes. Four ASCII bytes sit on the cap, so actual is 6.
    let comment = "<!--xxxxéyyy--><r/>";
    assert_text_limit(comment, 4, 6, comment.find('x').unwrap());
    // Three ASCII bytes, then a 4-byte character, cap 4: actual is 7.
    let crossed = "<!--xxx😀--><r/>";
    assert_text_limit(crossed, 4, 7, crossed.find('x').unwrap());
    let cdata = "<r><![CDATA[xxxx€]]></r>";
    assert_text_limit(cdata, 4, 7, cdata.find('x').unwrap());
    let pi = "<?t xxxx😀?><r/>";
    assert_text_limit(pi, 4, 8, pi.find('x').unwrap());

    // CRLF is one content character. Four `x`s plus that newline is length 5,
    // and the cursor sits after both raw bytes.
    let crlf = "<!--xxxx\r\nyyy--><r/>";
    let err = pull(crlf, config(4)).expect_err(crlf);
    let (limit, actual, offset) = text_actual(&err);
    assert_eq!((limit, actual), (4, 5));
    assert_eq!(offset, (crlf.find('x').unwrap() + 6) as u64);
    let streamed = read_reader(crlf, 4).expect_err(crlf);
    assert_eq!(err.kind, streamed.kind);
    assert_eq!(err.position, streamed.position);
    assert!(pull("<!--xxx\r\n--><r/>", config(4)).is_ok());

    let mut open = String::from("<!--");
    open.push_str(&"x".repeat(100_000));
    assert_text_limit(&open, 4, 5, open.find('x').unwrap());
}
