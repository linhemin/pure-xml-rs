//! A small XML writer. Text and attribute values are escaped differently.
//!
//! This is not a full serializer: namespace declarations already present on an
//! event are written back, and the writer does not invent them from resolved
//! URIs. Round-tripping a DOM is out of scope for 0.x.

use std::borrow::Cow;
use std::io::Write;

use crate::event::{
    Attribute, Doctype, EmptyElement, EndElement, Event, Namespace, ProcessingInstruction,
    StartElement, XmlDecl,
};

/// Failure while writing XML.
#[derive(Debug)]
pub enum WriteError {
    /// The underlying sink failed.
    Io(std::io::Error),
    /// The event cannot be written as well-formed XML.
    Syntax(String),
}

impl std::fmt::Display for WriteError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(err) => write!(f, "write error: {err}"),
            Self::Syntax(msg) => write!(f, "write error: {msg}"),
        }
    }
}

impl std::error::Error for WriteError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(err) => Some(err),
            Self::Syntax(_) => None,
        }
    }
}

impl From<std::io::Error> for WriteError {
    fn from(value: std::io::Error) -> Self {
        Self::Io(value)
    }
}

/// Escape `&`, `<`, and `>` for character data.
pub fn escape_text(input: &str) -> Cow<'_, str> {
    if !input.bytes().any(|b| matches!(b, b'&' | b'<' | b'>')) {
        Cow::Borrowed(input)
    } else {
        Cow::Owned(escape_into(input, false))
    }
}

/// Escape `&`, `<`, `>`, and `"` for a double-quoted attribute value.
pub fn escape_attribute(input: &str) -> Cow<'_, str> {
    if !input
        .bytes()
        .any(|b| matches!(b, b'&' | b'<' | b'>' | b'"'))
    {
        Cow::Borrowed(input)
    } else {
        Cow::Owned(escape_into(input, true))
    }
}

fn escape_into(input: &str, attribute: bool) -> String {
    let mut out = String::with_capacity(input.len());
    for ch in input.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' if attribute => out.push_str("&quot;"),
            other => out.push(other),
        }
    }
    out
}

/// Writes events or individual nodes to any [`Write`] sink.
pub struct Writer<W> {
    sink: W,
}

impl<W: Write> Writer<W> {
    /// Wrap a sink. Nothing is written until a `write_*` method is called.
    pub fn new(sink: W) -> Self {
        Self { sink }
    }

    /// Write one event.
    ///
    /// A doctype is written with its name and external ids only. The internal
    /// subset is not retained on the event, so it is not reproduced.
    pub fn write_event(&mut self, event: &Event<'_>) -> Result<(), WriteError> {
        match event {
            Event::StartElement(el) => self.write_start_element(el),
            Event::EndElement(el) => self.write_end_element(el),
            Event::EmptyElement(el) => self.write_empty_element(el),
            Event::Text(text) => self.write_text(text),
            Event::CData(text) => self.write_cdata(text),
            Event::Comment(text) => self.write_comment(text),
            Event::PI(pi) => self.write_pi(pi),
            Event::XmlDecl(decl) => self.write_xml_decl(decl),
            Event::Doctype(dtd) => self.write_doctype(dtd),
        }
    }

    /// Write `<name attr="value">`. `name` is used literally and is not checked
    /// beyond being non-empty.
    pub fn write_start(&mut self, name: &str, attrs: &[(&str, &str)]) -> Result<(), WriteError> {
        self.write_start_raw(name, attrs, false)
    }

    /// Write `<name attr="value"/>`.
    pub fn write_empty(&mut self, name: &str, attrs: &[(&str, &str)]) -> Result<(), WriteError> {
        self.write_start_raw(name, attrs, true)
    }

    /// Write `</name>`.
    pub fn write_end(&mut self, name: &str) -> Result<(), WriteError> {
        if name.is_empty() {
            return Err(WriteError::Syntax("empty end tag name".to_string()));
        }
        self.sink.write_all(b"</")?;
        self.sink.write_all(name.as_bytes())?;
        self.sink.write_all(b">")?;
        Ok(())
    }

    /// Write escaped character data.
    pub fn write_text(&mut self, text: &str) -> Result<(), WriteError> {
        let escaped = escape_text(text);
        self.sink.write_all(escaped.as_bytes())?;
        Ok(())
    }

    /// Write a CDATA section, splitting it if the body contains `]]>`.
    pub fn write_cdata(&mut self, text: &str) -> Result<(), WriteError> {
        self.sink.write_all(b"<![CDATA[")?;
        let mut rest = text;
        while let Some(idx) = rest.find("]]>") {
            self.sink.write_all(rest[..idx].as_bytes())?;
            // `]]>` cannot appear raw inside CDATA. Close and reopen around `>`.
            self.sink.write_all(b"]]]]><![CDATA[>")?;
            rest = &rest[idx + 3..];
        }
        self.sink.write_all(rest.as_bytes())?;
        self.sink.write_all(b"]]>")?;
        Ok(())
    }

    /// Write a comment. `--` is rejected.
    pub fn write_comment(&mut self, text: &str) -> Result<(), WriteError> {
        if text.contains("--") || text.ends_with('-') {
            return Err(WriteError::Syntax(
                "comment must not contain `--` or end with `-`".to_string(),
            ));
        }
        self.sink.write_all(b"<!--")?;
        self.sink.write_all(text.as_bytes())?;
        self.sink.write_all(b"-->")?;
        Ok(())
    }

    /// Write a processing instruction.
    pub fn write_pi_str(&mut self, target: &str, data: Option<&str>) -> Result<(), WriteError> {
        if target.is_empty() || target.eq_ignore_ascii_case("xml") {
            return Err(WriteError::Syntax(
                "processing instruction target is reserved or empty".to_string(),
            ));
        }
        if data.is_some_and(|d| d.contains("?>")) {
            return Err(WriteError::Syntax(
                "processing instruction data must not contain `?>`".to_string(),
            ));
        }
        self.sink.write_all(b"<?")?;
        self.sink.write_all(target.as_bytes())?;
        if let Some(data) = data {
            self.sink.write_all(b" ")?;
            self.sink.write_all(data.as_bytes())?;
        }
        self.sink.write_all(b"?>")?;
        Ok(())
    }

    /// Recover the sink.
    pub fn into_inner(self) -> W {
        self.sink
    }

    /// Borrow the sink.
    pub fn get_ref(&self) -> &W {
        &self.sink
    }

    fn write_start_element(&mut self, el: &StartElement<'_>) -> Result<(), WriteError> {
        self.write_element_open(&el.name, &el.namespaces, &el.attributes, false)
    }

    fn write_empty_element(&mut self, el: &EmptyElement<'_>) -> Result<(), WriteError> {
        self.write_element_open(&el.name, &el.namespaces, &el.attributes, true)
    }

    fn write_end_element(&mut self, el: &EndElement<'_>) -> Result<(), WriteError> {
        self.sink.write_all(b"</")?;
        self.write_qname(&el.name)?;
        self.sink.write_all(b">")?;
        Ok(())
    }

    fn write_pi(&mut self, pi: &ProcessingInstruction<'_>) -> Result<(), WriteError> {
        self.write_pi_str(&pi.target, pi.data.as_deref())
    }

    fn write_xml_decl(&mut self, decl: &XmlDecl<'_>) -> Result<(), WriteError> {
        self.sink.write_all(b"<?xml version=\"")?;
        self.sink
            .write_all(escape_attribute(&decl.version).as_bytes())?;
        self.sink.write_all(b"\"")?;
        if let Some(enc) = &decl.encoding {
            self.sink.write_all(b" encoding=\"")?;
            self.sink.write_all(escape_attribute(enc).as_bytes())?;
            self.sink.write_all(b"\"")?;
        }
        if let Some(standalone) = decl.standalone {
            let value = if standalone { "yes" } else { "no" };
            self.sink.write_all(b" standalone=\"")?;
            self.sink.write_all(value.as_bytes())?;
            self.sink.write_all(b"\"")?;
        }
        self.sink.write_all(b"?>")?;
        Ok(())
    }

    fn write_doctype(&mut self, dtd: &Doctype<'_>) -> Result<(), WriteError> {
        self.sink.write_all(b"<!DOCTYPE ")?;
        self.sink.write_all(dtd.name.as_bytes())?;
        if let Some(public_id) = &dtd.public_id {
            self.sink.write_all(b" PUBLIC ")?;
            self.write_quoted(public_id)?;
            if let Some(system_id) = &dtd.system_id {
                self.sink.write_all(b" ")?;
                self.write_quoted(system_id)?;
            }
        } else if let Some(system_id) = &dtd.system_id {
            self.sink.write_all(b" SYSTEM ")?;
            self.write_quoted(system_id)?;
        }
        self.sink.write_all(b">")?;
        Ok(())
    }

    fn write_element_open(
        &mut self,
        name: &crate::event::QName<'_>,
        namespaces: &[Namespace<'_>],
        attributes: &[Attribute<'_>],
        empty: bool,
    ) -> Result<(), WriteError> {
        self.sink.write_all(b"<")?;
        self.write_qname(name)?;
        for ns in namespaces {
            self.sink.write_all(b" ")?;
            match &ns.prefix {
                Some(prefix) => {
                    self.sink.write_all(b"xmlns:")?;
                    self.sink.write_all(prefix.as_bytes())?;
                }
                None => self.sink.write_all(b"xmlns")?,
            }
            self.sink.write_all(b"=\"")?;
            self.sink.write_all(escape_attribute(&ns.uri).as_bytes())?;
            self.sink.write_all(b"\"")?;
        }
        for attr in attributes {
            self.sink.write_all(b" ")?;
            self.write_qname(&attr.name)?;
            self.sink.write_all(b"=\"")?;
            self.sink
                .write_all(escape_attribute(&attr.value).as_bytes())?;
            self.sink.write_all(b"\"")?;
        }
        if empty {
            self.sink.write_all(b"/>")?;
        } else {
            self.sink.write_all(b">")?;
        }
        Ok(())
    }

    fn write_qname(&mut self, name: &crate::event::QName<'_>) -> Result<(), WriteError> {
        if let Some(prefix) = &name.prefix {
            self.sink.write_all(prefix.as_bytes())?;
            self.sink.write_all(b":")?;
        }
        self.sink.write_all(name.local.as_bytes())?;
        Ok(())
    }

    fn write_start_raw(
        &mut self,
        name: &str,
        attrs: &[(&str, &str)],
        empty: bool,
    ) -> Result<(), WriteError> {
        if name.is_empty() {
            return Err(WriteError::Syntax("empty element name".to_string()));
        }
        self.sink.write_all(b"<")?;
        self.sink.write_all(name.as_bytes())?;
        for (attr, value) in attrs {
            self.sink.write_all(b" ")?;
            self.sink.write_all(attr.as_bytes())?;
            self.sink.write_all(b"=\"")?;
            self.sink.write_all(escape_attribute(value).as_bytes())?;
            self.sink.write_all(b"\"")?;
        }
        if empty {
            self.sink.write_all(b"/>")?;
        } else {
            self.sink.write_all(b">")?;
        }
        Ok(())
    }

    fn write_quoted(&mut self, value: &str) -> Result<(), WriteError> {
        if value.contains('"') && value.contains('\'') {
            return Err(WriteError::Syntax(
                "identifier contains both quote characters".to_string(),
            ));
        }
        if value.contains('"') {
            self.sink.write_all(b"'")?;
            self.sink.write_all(value.as_bytes())?;
            self.sink.write_all(b"'")?;
        } else {
            self.sink.write_all(b"\"")?;
            self.sink.write_all(value.as_bytes())?;
            self.sink.write_all(b"\"")?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_and_attribute_escaping_differ() {
        assert_eq!(escape_text("a<b&c>d").as_ref(), "a&lt;b&amp;c&gt;d");
        assert_eq!(
            escape_attribute("a<b&c>\"d").as_ref(),
            "a&lt;b&amp;c&gt;&quot;d"
        );
        assert!(matches!(escape_text("plain"), Cow::Borrowed("plain")));
        assert!(matches!(escape_attribute("a'b"), Cow::Borrowed("a'b")));
    }

    #[test]
    fn writes_a_small_document() {
        let mut writer = Writer::new(Vec::new());
        writer.write_start("root", &[("a", "1&2<3\"4")]).unwrap();
        writer.write_text("x<y&z>").unwrap();
        writer.write_empty("e", &[]).unwrap();
        writer.write_comment("hi").unwrap();
        writer.write_cdata("a]]>b").unwrap();
        writer.write_pi_str("target", Some("data")).unwrap();
        writer.write_end("root").unwrap();
        let xml = String::from_utf8(writer.into_inner()).unwrap();
        assert_eq!(
            xml,
            "<root a=\"1&amp;2&lt;3&quot;4\">x&lt;y&amp;z&gt;<e/><!--hi--><![CDATA[a]]]]><![CDATA[>b]]><?target data?></root>"
        );
    }
}
