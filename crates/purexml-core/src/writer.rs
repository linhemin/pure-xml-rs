//! A small XML writer. Text and attribute values are escaped differently.
//!
//! This is not a full serializer: namespace declarations already present on an
//! event are written back, and the writer does not invent them from resolved
//! URIs. Round-tripping a DOM is out of scope for 0.x.

use std::borrow::Cow;
use std::collections::HashSet;
use std::io::Write;

use crate::event::{
    Attribute, Doctype, EmptyElement, EndElement, Event, Namespace, ProcessingInstruction, QName,
    StartElement, XmlDecl, XMLNS_NAMESPACE, XML_NAMESPACE,
};
use crate::name::{is_ncname_char, is_ncname_start, is_pubid_char, is_xml_char};

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

/// Escape `&`, `<`, `>`, and CR for character data.
///
/// CR is written as `&#13;` so a later parse does not turn it into a line feed.
/// Every `>` is escaped, so `]]>` cannot close character data early.
pub fn escape_text(input: &str) -> Cow<'_, str> {
    if !needs_text_escape(input) {
        Cow::Borrowed(input)
    } else {
        Cow::Owned(escape_into(input, false))
    }
}

/// Escape `&`, `<`, `>`, `"`, tab, LF, and CR for a double-quoted attribute.
///
/// Tab, LF, and CR become character references. A reparse then keeps them
/// instead of normalizing them to spaces.
pub fn escape_attribute(input: &str) -> Cow<'_, str> {
    if !needs_attr_escape(input) {
        Cow::Borrowed(input)
    } else {
        Cow::Owned(escape_into(input, true))
    }
}

fn needs_text_escape(input: &str) -> bool {
    input
        .bytes()
        .any(|b| matches!(b, b'&' | b'<' | b'>' | b'\r'))
}

fn needs_attr_escape(input: &str) -> bool {
    input
        .bytes()
        .any(|b| matches!(b, b'&' | b'<' | b'>' | b'"' | b'\t' | b'\n' | b'\r'))
}

fn escape_into(input: &str, attribute: bool) -> String {
    let mut out = String::with_capacity(input.len());
    for ch in input.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' if attribute => out.push_str("&quot;"),
            '\r' => out.push_str("&#13;"),
            '\t' if attribute => out.push_str("&#9;"),
            '\n' if attribute => out.push_str("&#10;"),
            other => out.push(other),
        }
    }
    out
}

fn check_chars(text: &str, what: &str) -> Result<(), WriteError> {
    for ch in text.chars() {
        if !is_xml_char(ch) {
            return Err(WriteError::Syntax(format!(
                "illegal XML character U+{:04X} in {what}",
                u32::from(ch)
            )));
        }
    }
    Ok(())
}

fn check_qname(name: &crate::event::QName<'_>) -> Result<(), WriteError> {
    if let Some(prefix) = &name.prefix {
        check_ncname(prefix)?;
    }
    check_ncname(&name.local)
}

fn check_version(version: &str) -> Result<(), WriteError> {
    let Some(digits) = version.strip_prefix("1.") else {
        return Err(WriteError::Syntax(format!(
            "invalid XML version `{version}`"
        )));
    };
    if digits.is_empty() || !digits.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(WriteError::Syntax(format!(
            "invalid XML version `{version}`"
        )));
    }
    Ok(())
}

fn check_encoding(encoding: &str) -> Result<(), WriteError> {
    let mut chars = encoding.chars();
    let Some(first) = chars.next() else {
        return Err(WriteError::Syntax("empty encoding name".to_string()));
    };
    if !first.is_ascii_alphabetic()
        || !chars.all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '.' | '_' | '-'))
    {
        return Err(WriteError::Syntax(format!(
            "invalid encoding name `{encoding}`"
        )));
    }
    Ok(())
}

fn check_pubid(value: &str) -> Result<(), WriteError> {
    if value.chars().all(is_pubid_char) {
        Ok(())
    } else {
        Err(WriteError::Syntax(
            "public id contains a character outside PubidChar".to_string(),
        ))
    }
}

fn check_system_literal(value: &str) -> Result<(), WriteError> {
    check_chars(value, "system identifier")?;
    if value.contains('"') && value.contains('\'') {
        return Err(WriteError::Syntax(
            "system identifier contains both quote characters".to_string(),
        ));
    }
    Ok(())
}

fn check_name(name: &str) -> Result<(), WriteError> {
    let mut parts = name.split(':');
    let Some(first) = parts.next().filter(|part| !part.is_empty()) else {
        return Err(WriteError::Syntax("empty name".to_string()));
    };
    check_ncname(first)?;
    if let Some(second) = parts.next() {
        check_ncname(second)?;
        if parts.next().is_some() {
            return Err(WriteError::Syntax(format!("invalid name `{name}`")));
        }
    }
    Ok(())
}

fn split_qname(name: &str) -> (Option<&str>, &str) {
    match name.split_once(':') {
        Some((prefix, local)) => (Some(prefix), local),
        None => (None, name),
    }
}

fn check_ncname(name: &str) -> Result<(), WriteError> {
    let mut chars = name.chars();
    let Some(first) = chars.next() else {
        return Err(WriteError::Syntax("empty name".to_string()));
    };
    if !is_ncname_start(first) || !chars.all(is_ncname_char) {
        return Err(WriteError::Syntax(format!("invalid name `{name}`")));
    }
    Ok(())
}

/// Expanded name when a namespace URI is present, otherwise the lexical QName.
fn attribute_identity<'a>(name: &'a QName<'a>) -> (Option<&'a str>, Option<&'a str>, &'a str) {
    if name.namespace.is_some() {
        (name.namespace.as_deref(), None, name.local.as_str())
    } else {
        (None, name.prefix.as_deref(), name.local.as_str())
    }
}

fn check_distinct_attributes(attributes: &[Attribute<'_>]) -> Result<(), WriteError> {
    let mut seen = HashSet::with_capacity(attributes.len());
    for attr in attributes {
        if !seen.insert(attribute_identity(&attr.name)) {
            return Err(WriteError::Syntax(format!(
                "duplicate attribute `{}`",
                attr.name.local
            )));
        }
    }
    Ok(())
}

fn check_namespace_decls(namespaces: &[Namespace<'_>]) -> Result<(), WriteError> {
    let mut seen_prefix: HashSet<Option<&str>> = HashSet::with_capacity(namespaces.len());
    for ns in namespaces {
        let prefix = ns.prefix.as_deref();
        let uri = ns.uri.as_str();
        if !seen_prefix.insert(prefix) {
            return Err(WriteError::Syntax(
                "duplicate namespace declaration".to_string(),
            ));
        }
        if prefix == Some("xmlns") {
            return Err(WriteError::Syntax(
                "the xmlns prefix must not be declared".to_string(),
            ));
        }
        if prefix == Some("xml") && uri != XML_NAMESPACE {
            return Err(WriteError::Syntax(
                "the xml prefix is bound to the XML namespace".to_string(),
            ));
        }
        if prefix.is_some() && uri.is_empty() {
            return Err(WriteError::Syntax(
                "a prefixed namespace declaration must not have an empty URI".to_string(),
            ));
        }
        if uri == XML_NAMESPACE && prefix != Some("xml") {
            return Err(WriteError::Syntax(
                "the XML namespace must not be bound to another prefix".to_string(),
            ));
        }
        if uri == XMLNS_NAMESPACE {
            return Err(WriteError::Syntax(
                "the xmlns namespace must not be declared".to_string(),
            ));
        }
    }
    Ok(())
}

/// Writes events or individual nodes to any [`Write`] sink.
///
/// Open elements keep their namespace declarations. The string API resolves
/// attribute prefixes against that stack and against `xmlns` declarations in
/// the same call, then rejects duplicate expanded names before writing.
pub struct Writer<W> {
    sink: W,
    /// Declarations of elements opened and not yet closed, outermost first.
    /// `None` is the default namespace, which does not apply to attributes.
    ns_stack: Vec<Vec<(Option<String>, String)>>,
}

impl<W: Write> Writer<W> {
    /// Wrap a sink. Nothing is written until a `write_*` method is called.
    pub fn new(sink: W) -> Self {
        Self {
            sink,
            ns_stack: Vec::new(),
        }
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

    /// Write `<name attr="value">`.
    ///
    /// Prefixes on attributes are resolved against `xmlns` declarations in
    /// `attrs` and against declarations still in scope from elements this
    /// writer has opened. Duplicate expanded names are rejected before any
    /// byte of this call is written.
    pub fn write_start(&mut self, name: &str, attrs: &[(&str, &str)]) -> Result<(), WriteError> {
        self.write_start_raw(name, attrs, false)
    }

    /// Write `<name attr="value"/>`.
    pub fn write_empty(&mut self, name: &str, attrs: &[(&str, &str)]) -> Result<(), WriteError> {
        self.write_start_raw(name, attrs, true)
    }

    /// Write `</name>`.
    pub fn write_end(&mut self, name: &str) -> Result<(), WriteError> {
        check_name(name)?;
        self.sink.write_all(b"</")?;
        self.sink.write_all(name.as_bytes())?;
        self.sink.write_all(b">")?;
        self.ns_stack.pop();
        Ok(())
    }

    /// Write escaped character data.
    pub fn write_text(&mut self, text: &str) -> Result<(), WriteError> {
        check_chars(text, "text")?;
        let escaped = escape_text(text);
        self.sink.write_all(escaped.as_bytes())?;
        Ok(())
    }

    /// Write a CDATA section, splitting it if the body contains `]]>`.
    pub fn write_cdata(&mut self, text: &str) -> Result<(), WriteError> {
        check_chars(text, "CDATA")?;
        self.sink.write_all(b"<![CDATA[")?;
        let bytes = text.as_bytes();
        let mut start = 0usize;
        while let Some(rel) = text[start..].find("]]>") {
            let idx = start + rel;
            self.sink.write_all(&bytes[start..idx])?;
            // `]]>` cannot appear raw inside CDATA. Close and reopen around `>`.
            self.sink.write_all(b"]]]]><![CDATA[>")?;
            start = idx + 3;
        }
        self.sink.write_all(&bytes[start..])?;
        self.sink.write_all(b"]]>")?;
        Ok(())
    }

    /// Write a comment. `--` is rejected.
    pub fn write_comment(&mut self, text: &str) -> Result<(), WriteError> {
        check_chars(text, "comment")?;
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
        check_name(target)?;
        if target.eq_ignore_ascii_case("xml") {
            return Err(WriteError::Syntax(
                "processing instruction target is reserved or empty".to_string(),
            ));
        }
        if let Some(data) = data {
            check_chars(data, "processing instruction")?;
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
        check_qname(&el.name)?;
        self.sink.write_all(b"</")?;
        self.write_qname(&el.name)?;
        self.sink.write_all(b">")?;
        self.ns_stack.pop();
        Ok(())
    }

    fn write_pi(&mut self, pi: &ProcessingInstruction<'_>) -> Result<(), WriteError> {
        self.write_pi_str(&pi.target, pi.data.as_deref())
    }

    fn write_xml_decl(&mut self, decl: &XmlDecl<'_>) -> Result<(), WriteError> {
        check_version(&decl.version)?;
        if let Some(enc) = &decl.encoding {
            check_encoding(enc)?;
        }
        self.sink.write_all(b"<?xml version=\"")?;
        self.sink.write_all(decl.version.as_bytes())?;
        self.sink.write_all(b"\"")?;
        if let Some(enc) = &decl.encoding {
            self.sink.write_all(b" encoding=\"")?;
            self.sink.write_all(enc.as_bytes())?;
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
        check_name(&dtd.name)?;
        if dtd.public_id.is_some() && dtd.system_id.is_none() {
            return Err(WriteError::Syntax(
                "PUBLIC identifier requires a system identifier".to_string(),
            ));
        }
        if let Some(public_id) = &dtd.public_id {
            check_pubid(public_id)?;
        }
        if let Some(system_id) = &dtd.system_id {
            check_system_literal(system_id)?;
        }
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
        check_chars(name.local.as_ref(), "name")?;
        if let Some(prefix) = &name.prefix {
            check_chars(prefix.as_ref(), "name")?;
        }
        check_qname(name)?;
        for ns in namespaces {
            if let Some(prefix) = &ns.prefix {
                check_ncname(prefix)?;
            }
            check_chars(&ns.uri, "namespace URI")?;
        }
        for attr in attributes {
            check_qname(&attr.name)?;
            check_chars(&attr.value, "attribute value")?;
        }
        check_distinct_attributes(attributes)?;
        check_namespace_decls(namespaces)?;
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
            self.ns_stack.push(
                namespaces
                    .iter()
                    .map(|ns| {
                        (
                            ns.prefix.as_ref().map(|prefix| prefix.as_str().to_string()),
                            ns.uri.as_str().to_string(),
                        )
                    })
                    .collect(),
            );
        }
        Ok(())
    }

    fn write_qname(&mut self, name: &crate::event::QName<'_>) -> Result<(), WriteError> {
        if let Some(prefix) = &name.prefix {
            check_ncname(prefix)?;
            self.sink.write_all(prefix.as_bytes())?;
            self.sink.write_all(b":")?;
        }
        check_ncname(&name.local)?;
        self.sink.write_all(name.local.as_bytes())?;
        Ok(())
    }

    fn write_start_raw(
        &mut self,
        name: &str,
        attrs: &[(&str, &str)],
        empty: bool,
    ) -> Result<(), WriteError> {
        check_name(name)?;
        let decls = self.check_raw_attributes(attrs)?;
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
            self.ns_stack.push(
                decls
                    .into_iter()
                    .map(|(prefix, uri)| (prefix.map(str::to_string), uri.to_string()))
                    .collect(),
            );
        }
        Ok(())
    }

    /// Namespace declarations in this attribute list, after lexical and
    /// expanded-name checks. Nothing is written on error.
    fn check_raw_attributes<'a>(
        &self,
        attrs: &'a [(&'a str, &'a str)],
    ) -> Result<Vec<(Option<&'a str>, &'a str)>, WriteError> {
        let mut lexical = HashSet::with_capacity(attrs.len());
        let mut decls = Vec::new();
        let mut regular = Vec::new();
        for &(attr, value) in attrs {
            check_name(attr)?;
            check_chars(value, "attribute value")?;
            if !lexical.insert(attr) {
                return Err(WriteError::Syntax(format!("duplicate attribute `{attr}`")));
            }
            let (prefix, local) = split_qname(attr);
            if prefix == Some("xmlns") {
                decls.push((Some(local), value));
            } else if prefix.is_none() && local == "xmlns" {
                decls.push((None, value));
            } else {
                regular.push((prefix, local, attr));
            }
        }
        let mut expanded: HashSet<(String, String)> = HashSet::new();
        for (prefix, _) in &decls {
            let local = prefix.unwrap_or("xmlns");
            if !expanded.insert((XMLNS_NAMESPACE.to_string(), local.to_string())) {
                return Err(WriteError::Syntax(format!(
                    "duplicate attribute `{}`",
                    match prefix {
                        Some(prefix) => format!("xmlns:{prefix}"),
                        None => "xmlns".to_string(),
                    }
                )));
            }
        }
        for (prefix, local, raw) in regular {
            let Some(prefix) = prefix else {
                continue;
            };
            let Some(uri) = self.lookup_prefix(&decls, prefix) else {
                continue;
            };
            if uri.is_empty() {
                continue;
            }
            if !expanded.insert((uri.to_string(), local.to_string())) {
                return Err(WriteError::Syntax(format!("duplicate attribute `{raw}`")));
            }
        }
        Ok(decls)
    }

    fn lookup_prefix<'a>(
        &'a self,
        local_decls: &'a [(Option<&str>, &str)],
        prefix: &str,
    ) -> Option<&'a str> {
        if prefix == "xml" {
            return Some(XML_NAMESPACE);
        }
        if let Some((_, uri)) = local_decls
            .iter()
            .rev()
            .find(|(declared, _)| *declared == Some(prefix))
        {
            return Some(*uri);
        }
        for frame in self.ns_stack.iter().rev() {
            if let Some((_, uri)) = frame
                .iter()
                .rev()
                .find(|(declared, _)| declared.as_deref() == Some(prefix))
            {
                return Some(uri.as_str());
            }
        }
        None
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
    fn string_api_rejects_duplicate_expanded_names() {
        let mut writer = Writer::new(Vec::new());
        let err = writer.write_start(
            "a",
            &[
                ("xmlns:p", "urn:x"),
                ("xmlns:q", "urn:x"),
                ("p:x", "1"),
                ("q:x", "2"),
            ],
        );
        assert!(err.is_err());
        assert!(writer.get_ref().is_empty());

        let mut writer = Writer::new(Vec::new());
        writer.write_start("a", &[("xmlns:p", "urn:x")]).unwrap();
        let err = writer.write_empty("b", &[("xmlns:q", "urn:x"), ("p:x", "1"), ("q:x", "2")]);
        assert!(err.is_err());
        assert_eq!(
            String::from_utf8(writer.into_inner()).unwrap(),
            "<a xmlns:p=\"urn:x\">"
        );
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
