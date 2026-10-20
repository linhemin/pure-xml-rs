//! Writer regressions, including the D7 escaping and validation cases.

use purexml_core::{Reader, Writer};

fn written(write: impl FnOnce(&mut Writer<Vec<u8>>)) -> String {
    let mut writer = Writer::new(Vec::new());
    write(&mut writer);
    String::from_utf8(writer.into_inner()).unwrap()
}

#[test]
fn d7_text_escapes_cdata_terminator_and_cr() {
    let xml = written(|writer| {
        writer.write_text("]]>\r").unwrap();
    });
    assert_eq!(xml, "]]&gt;&#13;");
    assert!(!xml.contains("]]>"));
    assert!(!xml.contains('\r'));
}

#[test]
fn d7_attributes_escape_whitespace_as_character_references() {
    let xml = written(|writer| {
        writer.write_empty("a", &[("b", "x\ty\nz\rw")]).unwrap();
    });
    assert_eq!(xml, "<a b=\"x&#9;y&#10;z&#13;w\"/>");
    let mut reader = Reader::from_str(&xml);
    match reader.read_event().unwrap().unwrap() {
        purexml_core::Event::EmptyElement(el) => {
            assert_eq!(el.attributes[0].value.as_ref(), "x\ty\nz\rw");
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn d7_illegal_characters_and_invalid_names_are_rejected() {
    let mut writer = Writer::new(Vec::new());
    assert!(writer.write_text("\u{0}").is_err());
    assert!(writer.write_cdata("\u{1}").is_err());
    assert!(writer.write_start("1abc", &[]).is_err());
    assert!(writer.write_start("a b", &[]).is_err());
    assert!(writer.write_start("a<b", &[]).is_err());
    assert!(writer.write_empty("ok", &[("b\"c", "d")]).is_err());
    assert!(writer.write_end("a:b:c").is_err());
    assert!(writer.write_comment("bad\u{0}").is_err());
    assert!(writer.write_pi_str("123", None).is_err());
}

#[test]
fn d7_names_cannot_inject_markup() {
    let mut writer = Writer::new(Vec::new());
    assert!(writer.write_start("a><injected", &[]).is_err());
    assert!(writer.write_empty("a", &[("b><c", "d")]).is_err());
    let xml = written(|writer| {
        writer.write_start("note", &[("a", "x<y&\"z")]).unwrap();
        writer.write_text("]]>").unwrap();
        writer.write_end("note").unwrap();
    });
    assert_eq!(xml, "<note a=\"x&lt;y&amp;&quot;z\">]]&gt;</note>");
    assert!(Reader::from_str(&xml).read_event().is_ok());
}
