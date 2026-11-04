//! Writer（M1 只要求转义）测试：输出必须良构，且经 Reader 重新解析后内容不变。
//! d7_* 为缺陷用例（M1-test-report.md D7），保持开启。

use purexml::{escape_attribute, escape_text, Event, Reader, Writer};

/// 解析 `out`，返回 (根元素属性, 根元素内文本)。
fn reparse(out: &[u8]) -> Result<(Vec<(String, String)>, String), String> {
    let mut r = Reader::from_bytes(out);
    let mut attrs = Vec::new();
    let mut text = String::new();
    loop {
        match r.read_event() {
            Ok(Some(Event::StartElement(e))) if attrs.is_empty() => {
                for a in &e.attributes {
                    attrs.push((a.name.local.to_string(), a.value.to_string()));
                }
            }
            Ok(Some(Event::EmptyElement(e))) if attrs.is_empty() => {
                for a in &e.attributes {
                    attrs.push((a.name.local.to_string(), a.value.to_string()));
                }
            }
            Ok(Some(Event::Text(t))) | Ok(Some(Event::CData(t))) => text.push_str(&t),
            Ok(Some(_)) => {}
            Ok(None) => return Ok((attrs, text)),
            Err(e) => return Err(format!("{:?}: {}", e.kind, e.message)),
        }
    }
}

fn doc(attr: &str, text: &str) -> Vec<u8> {
    let mut w = Writer::new(Vec::new());
    w.write_start("r", &[("a", attr)]).unwrap();
    w.write_text(text).unwrap();
    w.write_end("r").unwrap();
    w.into_inner()
}

#[test]
fn basic_escaping_round_trips() {
    let out = doc(r#"<&>"'"#, "<&>\"'");
    let (attrs, text) = reparse(&out).expect("Writer 输出必须良构");
    assert_eq!(attrs[0].1, r#"<&>"'"#);
    assert_eq!(text, "<&>\"'");
    assert_eq!(escape_text("a<b&c"), "a&lt;b&amp;c");
    assert!(escape_attribute("\"<&").contains("&quot;"));
}

#[test]
fn cdata_comment_pi_round_trip() {
    let mut w = Writer::new(Vec::new());
    w.write_start("r", &[]).unwrap();
    w.write_cdata("x<y").unwrap();
    w.write_comment("c").unwrap();
    w.write_pi_str("p", Some("d")).unwrap();
    w.write_end("r").unwrap();
    let (_, text) = reparse(&w.into_inner()).unwrap();
    assert_eq!(text, "x<y");
}

/// 文本里的 "]]>" 不能原样写出（§2.4），至少要把 '>' 转义
#[test]
fn d7_text_cdata_end_escaped() {
    let out = doc("", "a]]>b");
    let (_, text) = reparse(&out).expect("Writer 输出必须良构");
    assert_eq!(text, "a]]>b");
}

/// 属性值里的 \t \n \r 必须写成字符引用，否则重新解析时会按 §3.3.3 规范化为空格
#[test]
fn d7_attribute_whitespace_round_trips() {
    let v = "x\ty\nz\r!";
    let (attrs, _) = reparse(&doc(v, "")).unwrap();
    assert_eq!(attrs[0].1, v, "属性值空白往返失真");
}

/// 文本里的 \r 必须写成 &#13;，否则重新解析时按 §2.11 规范化为 \n
#[test]
fn d7_text_cr_round_trips() {
    let t = "a\r\nb\rc";
    let (_, text) = reparse(&doc("", t)).unwrap();
    assert_eq!(text, t, "文本 CR 往返失真");
}

/// XML 1.0 不允许 U+0001 等控制字符（也不能用字符引用表示），Writer 必须返回 Err
#[test]
fn d7_illegal_char_rejected() {
    let mut w = Writer::new(Vec::new());
    w.write_start("r", &[]).unwrap();
    assert!(
        w.write_text("bad\u{1}char").is_err(),
        "文本中的 U+0001 应报错"
    );
    let mut w = Writer::new(Vec::new());
    assert!(
        w.write_start("r", &[("a", "\u{1}")]).is_err(),
        "属性中的 U+0001 应报错"
    );
    let mut w = Writer::new(Vec::new());
    assert!(w.write_cdata("\u{0}").is_err(), "CDATA 中的 NUL 应报错");
}

/// 名字不校验会导致标记注入
#[test]
fn d7_invalid_names_rejected() {
    let mut w = Writer::new(Vec::new());
    assert!(w.write_start("a b=\"x\"", &[]).is_err(), "非法元素名应报错");
    let mut w = Writer::new(Vec::new());
    assert!(
        w.write_start("r", &[("c d", "1")]).is_err(),
        "非法属性名应报错"
    );
    let mut w = Writer::new(Vec::new());
    assert!(
        w.write_pi_str("a b", Some("x")).is_err(),
        "非法 PI 目标应报错"
    );
}
