//! Linear DOM construction from parser events.
//!
//! The mutation API walks ancestors, so building a deep tree with it is
//! quadratic. This module links each event in constant time and copies only
//! that event's bytes. Namespace URIs reuse the reader's interned [`Arc`]
//! so one URI is stored once.

use std::collections::HashMap;
use std::io::BufRead;
use std::mem::size_of;
use std::sync::Arc;

use crate::Reader;
use crate::config::ParserConfig;
use crate::dom::document::{
    AttrData, Document, DomError, DomLimit, ElementData, Node, NodeKind, NsDecl, Payload, Range,
    StrSpan,
};
use crate::error::{Error, ErrorKind, LimitKind, Position};
use crate::event::Event;
use crate::input::IoInput;
use crate::limit::{LimitFacts, MeasuredLimit, decide_limit};

/// One pull of events plus the cursor those events were read from.
pub(crate) trait EventSource {
    fn next_event(&mut self) -> crate::Result<Option<Event<'_>>>;
    fn position(&self) -> Position;
    fn last_defaulted(&self) -> usize;
}

impl EventSource for Reader<&[u8]> {
    fn next_event(&mut self) -> crate::Result<Option<Event<'_>>> {
        self.read_event()
    }

    fn position(&self) -> Position {
        self.build_cursor()
    }

    fn last_defaulted(&self) -> usize {
        Reader::last_defaulted(self)
    }
}

impl<R: BufRead> EventSource for Reader<IoInput<R>> {
    fn next_event(&mut self) -> crate::Result<Option<Event<'_>>> {
        self.read_event()
    }

    fn position(&self) -> Position {
        self.build_cursor()
    }

    fn last_defaulted(&self) -> usize {
        Reader::last_defaulted(self)
    }
}

impl Document {
    /// Parse `text` into an owned document.
    ///
    /// This is [`Self::from_bytes`] on the UTF-8 bytes. An XML declaration is
    /// not stored. [`Self::to_string`] always writes UTF-8 and does not emit
    /// a declaration.
    ///
    /// Building is linear in the input. Mutation is O(depth); use this entry
    /// for a large document.
    pub fn parse(text: &str) -> Result<Document, Error> {
        Self::from_bytes(text.as_bytes())
    }

    /// [`Self::parse`] with an explicit limit set. `config` is borrowed, like
    /// [`Self::with_config`].
    pub fn parse_with_config(text: &str, config: &ParserConfig) -> Result<Document, Error> {
        Self::from_bytes_with_config(text.as_bytes(), config)
    }

    /// Parse UTF-8 bytes into an owned document.
    ///
    /// An XML declaration is not stored. Serialization is always UTF-8.
    pub fn from_bytes(bytes: &[u8]) -> Result<Document, Error> {
        Self::from_bytes_with_config(bytes, &ParserConfig::default())
    }

    /// [`Self::from_bytes`] with an explicit limit set.
    pub fn from_bytes_with_config(bytes: &[u8], config: &ParserConfig) -> Result<Document, Error> {
        let mut reader = Reader::from_bytes(bytes).with_config(config.clone());
        build_from(&mut reader, config)
    }

    /// Parse a buffered reader into an owned document.
    ///
    /// The event sequence matches [`Self::from_bytes`] on the same bytes,
    /// including a reader that yields one byte at a time.
    pub fn from_reader<R: BufRead>(reader: R) -> Result<Document, Error> {
        Self::from_reader_with_config(reader, &ParserConfig::default())
    }

    /// [`Self::from_reader`] with an explicit limit set.
    pub fn from_reader_with_config<R: BufRead>(
        reader: R,
        config: &ParserConfig,
    ) -> Result<Document, Error> {
        let mut reader = Reader::from_reader_with_config(reader, config.clone())?;
        build_from(&mut reader, config)
    }
}

pub(crate) fn build_from(
    source: &mut impl EventSource,
    config: &ParserConfig,
) -> Result<Document, Error> {
    let mut doc =
        Document::with_config(config).map_err(|err| dom_error(err, Position::start(), config))?;
    let mut open = vec![0u32];
    let mut uris: HashMap<usize, (Arc<str>, StrSpan)> = HashMap::new();
    while let Some(event) = source.next_event()? {
        let owned = OwnedEvent::from_event(event);
        let position = source.position();
        let defaulted = source.last_defaulted();
        apply(&mut doc, &mut open, &mut uris, owned, defaulted)
            .map_err(|err| dom_error(err, position, config))?;
    }
    Ok(doc)
}

fn apply(
    doc: &mut Document,
    open: &mut Vec<u32>,
    uris: &mut HashMap<usize, (Arc<str>, StrSpan)>,
    event: OwnedEvent,
    defaulted: usize,
) -> Result<(), DomError> {
    let parent = *open.last().expect("document node stays open");
    match event {
        OwnedEvent::Start(element) => {
            let index = append_element(doc, parent, &element, defaulted, uris)?;
            open.push(index);
        }
        OwnedEvent::Empty(element) => {
            append_element(doc, parent, &element, defaulted, uris)?;
        }
        OwnedEvent::End => {
            if open.len() <= 1 {
                return Err(DomError::Hierarchy);
            }
            open.pop();
        }
        OwnedEvent::Text(text) => {
            if open.len() == 1 {
                if is_xml_whitespace(&text) {
                    return Ok(());
                }
                debug_assert!(false, "non-whitespace text outside the root element");
                return Err(DomError::Hierarchy);
            }
            append_chars(doc, parent, NodeKind::Text, &text)?;
        }
        OwnedEvent::CData(text) => append_chars(doc, parent, NodeKind::CData, &text)?,
        OwnedEvent::Comment(text) => append_chars(doc, parent, NodeKind::Comment, &text)?,
        OwnedEvent::Pi { target, data } => append_pi(doc, parent, &target, data.as_deref())?,
        OwnedEvent::Doctype {
            name,
            system,
            public,
        } => {
            let id = doc.create_doctype(&name, system.as_deref(), public.as_deref())?;
            doc.link_last(parent, id.index());
        }
        OwnedEvent::XmlDecl => {}
    }
    Ok(())
}

struct OwnedName {
    prefix: Option<String>,
    local: String,
    namespace: Option<Arc<str>>,
}

struct OwnedAttr {
    name: OwnedName,
    value: String,
}

struct OwnedNs {
    prefix: Option<String>,
    uri: String,
}

struct OwnedElement {
    name: OwnedName,
    attributes: Vec<OwnedAttr>,
    namespaces: Vec<OwnedNs>,
}

enum OwnedEvent {
    Start(OwnedElement),
    Empty(OwnedElement),
    End,
    Text(String),
    CData(String),
    Comment(String),
    Pi {
        target: String,
        data: Option<String>,
    },
    Doctype {
        name: String,
        system: Option<String>,
        public: Option<String>,
    },
    XmlDecl,
}

impl OwnedEvent {
    fn from_event(event: Event<'_>) -> Self {
        match event {
            Event::StartElement(element) => Self::Start(OwnedElement::from_parts(
                &element.name,
                &element.attributes,
                &element.namespaces,
            )),
            Event::EmptyElement(element) => Self::Empty(OwnedElement::from_parts(
                &element.name,
                &element.attributes,
                &element.namespaces,
            )),
            Event::EndElement(_) => Self::End,
            Event::Text(text) => Self::Text(text.as_str().to_string()),
            Event::CData(text) => Self::CData(text.as_str().to_string()),
            Event::Comment(text) => Self::Comment(text.as_str().to_string()),
            Event::PI(pi) => Self::Pi {
                target: pi.target.as_str().to_string(),
                data: pi.data.as_ref().map(|text| text.as_str().to_string()),
            },
            Event::Doctype(doctype) => Self::Doctype {
                name: doctype.name.as_str().to_string(),
                system: doctype
                    .system_id
                    .as_ref()
                    .map(|text| text.as_str().to_string()),
                public: doctype
                    .public_id
                    .as_ref()
                    .map(|text| text.as_str().to_string()),
            },
            Event::XmlDecl(_) => Self::XmlDecl,
        }
    }
}

impl OwnedElement {
    fn from_parts(
        name: &crate::event::QName<'_>,
        attributes: &[crate::event::Attribute<'_>],
        namespaces: &[crate::event::Namespace<'_>],
    ) -> Self {
        Self {
            name: OwnedName::from_qname(name),
            attributes: attributes
                .iter()
                .map(|attr| OwnedAttr {
                    name: OwnedName::from_qname(&attr.name),
                    value: attr.value.as_str().to_string(),
                })
                .collect(),
            namespaces: namespaces
                .iter()
                .map(|ns| OwnedNs {
                    prefix: ns.prefix.as_ref().map(|text| text.as_str().to_string()),
                    uri: ns.uri.as_str().to_string(),
                })
                .collect(),
        }
    }
}

impl OwnedName {
    fn from_qname(name: &crate::event::QName<'_>) -> Self {
        Self {
            prefix: name.prefix.as_ref().map(|text| text.as_str().to_string()),
            local: name.local.as_str().to_string(),
            namespace: name.namespace.clone(),
        }
    }
}

fn append_element(
    doc: &mut Document,
    parent: u32,
    element: &OwnedElement,
    defaulted: usize,
    uris: &mut HashMap<usize, (Arc<str>, StrSpan)>,
) -> Result<u32, DomError> {
    let attributes = &element.attributes;
    let namespaces = &element.namespaces;
    let name = &element.name;
    let defaulted = defaulted.min(attributes.len());
    let first_default = attributes.len() - defaulted;
    let mut seen_uris = Vec::new();
    let mut string_bytes = name.local.len() + name.prefix.as_ref().map(String::len).unwrap_or(0);
    if let Some(uri) = &name.namespace {
        string_bytes += fresh_uri_len(uris, uri, &mut seen_uris);
    }
    for attr in attributes {
        string_bytes += attr.name.local.len()
            + attr.value.len()
            + attr.name.prefix.as_ref().map(String::len).unwrap_or(0);
        if let Some(uri) = &attr.name.namespace {
            string_bytes += fresh_uri_len(uris, uri, &mut seen_uris);
        }
    }
    for ns in namespaces {
        string_bytes += ns.uri.len() + ns.prefix.as_ref().map(String::len).unwrap_or(0);
    }
    let extra = size_of::<Node>()
        + size_of::<ElementData>()
        + attributes.len() * size_of::<AttrData>()
        + namespaces.len() * size_of::<NsDecl>()
        + string_bytes;
    doc.reserve(1, extra as u64)?;

    let namespace = match &name.namespace {
        Some(uri) => Some(intern_uri(doc, uris, uri)?),
        None => None,
    };
    let prefix = match &name.prefix {
        Some(prefix) => Some(doc.intern_span(prefix)?),
        None => None,
    };
    let local = doc.intern_span(&name.local)?;
    let attr_start = doc.attrs.len() as u32;
    for (index, attr) in attributes.iter().enumerate() {
        let namespace = match &attr.name.namespace {
            Some(uri) => Some(intern_uri(doc, uris, uri)?),
            None => None,
        };
        let prefix = match &attr.name.prefix {
            Some(prefix) => Some(doc.intern_span(prefix)?),
            None => None,
        };
        let local = doc.intern_span(&attr.name.local)?;
        let value = doc.intern_span(&attr.value)?;
        doc.attrs.push(AttrData {
            namespace,
            prefix,
            local,
            value,
            specified: index < first_default,
        });
    }
    let ns_start = doc.ns_decls.len() as u32;
    for ns in namespaces {
        let prefix = match &ns.prefix {
            Some(prefix) => Some(doc.intern_span(prefix)?),
            None => None,
        };
        let uri = doc.intern_span(&ns.uri)?;
        doc.ns_decls.push(NsDecl { prefix, uri });
    }
    let slot = doc.elements.len() as u32;
    doc.elements.push(ElementData {
        namespace,
        prefix,
        local,
        attrs: Range {
            start: attr_start,
            len: attributes.len() as u32,
        },
        ns_decls: Range {
            start: ns_start,
            len: namespaces.len() as u32,
        },
    });
    let index = push_node(doc, NodeKind::Element, Payload::Element(slot));
    doc.link_last(parent, index);
    Ok(index)
}

fn append_chars(
    doc: &mut Document,
    parent: u32,
    kind: NodeKind,
    text: &str,
) -> Result<(), DomError> {
    doc.reserve(1, (size_of::<Node>() + text.len()) as u64)?;
    let span = doc.intern_span(text)?;
    let index = push_node(doc, kind, Payload::Span(span));
    doc.link_last(parent, index);
    Ok(())
}

fn append_pi(
    doc: &mut Document,
    parent: u32,
    target: &str,
    data: Option<&str>,
) -> Result<(), DomError> {
    let bytes = target.len() + data.map(str::len).unwrap_or(0);
    doc.reserve(1, (size_of::<Node>() + bytes) as u64)?;
    let target = doc.intern_span(target)?;
    let data = match data {
        Some(data) => Some(doc.intern_span(data)?),
        None => None,
    };
    let index = push_node(doc, NodeKind::Pi, Payload::Pi { target, data });
    doc.link_last(parent, index);
    Ok(())
}

fn push_node(doc: &mut Document, kind: NodeKind, payload: Payload) -> u32 {
    let index = doc.nodes.len() as u32;
    doc.nodes.push(Node {
        kind,
        alive: true,
        generation: 1,
        parent: None,
        prev: None,
        next: None,
        first_child: None,
        last_child: None,
        payload,
    });
    index
}

fn uri_key(uri: &Arc<str>) -> usize {
    Arc::as_ptr(uri) as *const u8 as usize
}

fn fresh_uri_len(
    uris: &HashMap<usize, (Arc<str>, StrSpan)>,
    uri: &Arc<str>,
    seen: &mut Vec<usize>,
) -> usize {
    let key = uri_key(uri);
    if uris.contains_key(&key) || seen.contains(&key) {
        return 0;
    }
    seen.push(key);
    uri.len()
}

fn intern_uri(
    doc: &mut Document,
    uris: &mut HashMap<usize, (Arc<str>, StrSpan)>,
    uri: &Arc<str>,
) -> Result<StrSpan, DomError> {
    let key = uri_key(uri);
    if let Some((_, span)) = uris.get(&key) {
        return Ok(*span);
    }
    let span = doc.intern_span(uri)?;
    uris.insert(key, (Arc::clone(uri), span));
    Ok(span)
}

fn is_xml_whitespace(text: &str) -> bool {
    text.bytes()
        .all(|byte| matches!(byte, b' ' | b'\t' | b'\n' | b'\r'))
}

fn dom_error(err: DomError, position: Position, config: &ParserConfig) -> Error {
    let DomError::Limit {
        kind,
        limit,
        actual,
    } = err
    else {
        return Error::new(ErrorKind::Syntax, position, format!("dom build: {err:?}"));
    };
    let kind = match kind {
        DomLimit::NodeCount => LimitKind::NodeCount,
        DomLimit::DomBytes => LimitKind::DomBytes,
        other => {
            return Error::new(
                ErrorKind::Syntax,
                position,
                format!("dom build hit {other:?}"),
            );
        }
    };
    let facts = LimitFacts {
        config,
        cursor: position.offset,
        token_origin: None,
        subset_start: None,
        measured: Some(MeasuredLimit {
            kind,
            limit,
            actual,
            offset: position.offset,
            needed: position.offset,
        }),
        encoding_needed: None,
    };
    let decision = decide_limit(&facts).expect("a measured limit is a decision");
    Error::new(
        ErrorKind::SecurityLimit {
            kind: decision.kind,
            limit: decision.limit,
            actual: decision.actual,
        },
        Position {
            line: position.line,
            column: position.column,
            offset: decision.offset,
        },
        format!("security limit ({})", decision.kind),
    )
}

#[cfg(test)]
mod tests {
    use std::io::{BufRead, BufReader, Read};

    use super::Document;
    use crate::config::ParserConfig;
    use crate::error::{ErrorKind, LimitKind};

    struct OneByte<R> {
        inner: R,
    }

    impl<R: BufRead> Read for OneByte<R> {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            if buf.is_empty() {
                return Ok(0);
            }
            self.inner.read(&mut buf[..1])
        }
    }

    impl<R: BufRead> BufRead for OneByte<R> {
        fn fill_buf(&mut self) -> std::io::Result<&[u8]> {
            self.inner
                .fill_buf()
                .map(|buf| if buf.is_empty() { buf } else { &buf[..1] })
        }

        fn consume(&mut self, amt: usize) {
            self.inner.consume(amt);
        }
    }

    #[test]
    fn four_paths_agree_on_a_small_document() {
        let xml = r#"<r a="1"><t>hi</t></r>"#;
        let bytes = xml.as_bytes();
        let from_parse = Document::parse(xml).unwrap().to_string().unwrap();
        let from_bytes = Document::from_bytes(bytes).unwrap().to_string().unwrap();
        let from_buf = Document::from_reader(BufReader::new(bytes))
            .unwrap()
            .to_string()
            .unwrap();
        let from_one = Document::from_reader(BufReader::new(OneByte {
            inner: std::io::Cursor::new(bytes),
        }))
        .unwrap()
        .to_string()
        .unwrap();
        assert_eq!(from_parse, "<r a=\"1\"><t>hi</t></r>");
        assert_eq!(from_parse, from_bytes);
        assert_eq!(from_parse, from_buf);
        assert_eq!(from_parse, from_one);
    }

    #[test]
    fn node_cap_accepts_equality_and_rejects_one_past() {
        let xml = "<r/>";
        let exact = ParserConfig::default().with_max_nodes(2);
        assert!(Document::parse_with_config(xml, &exact).is_ok());
        let short = ParserConfig::default().with_max_nodes(1);
        let err = Document::parse_with_config(xml, &short).unwrap_err();
        assert!(matches!(
            err.kind,
            ErrorKind::SecurityLimit {
                kind: LimitKind::NodeCount,
                ..
            }
        ));
        assert!(err.position.offset > 0);
        let empty = ParserConfig::default().with_max_nodes(0);
        let err = Document::parse_with_config(xml, &empty).unwrap_err();
        match err.kind {
            ErrorKind::SecurityLimit {
                kind: LimitKind::NodeCount,
                ..
            } => {}
            other => panic!("{other:?}"),
        }
        assert_eq!(err.position.offset, 0);
    }

    #[test]
    fn default_attribute_is_not_specified() {
        let xml = r#"<!DOCTYPE r [<!ATTLIST r a CDATA "1">]><r b="2"/>"#;
        let doc = Document::parse(xml).unwrap();
        let element = doc.root_element().unwrap();
        let attrs: Vec<_> = element
            .attributes()
            .map(|attr| (attr.local().to_string(), attr.specified()))
            .collect();
        assert_eq!(attrs, [("b".to_string(), true), ("a".to_string(), false)]);
    }
}
