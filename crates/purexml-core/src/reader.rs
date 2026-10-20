//! Pull parser.
//!
//! [`Reader::read_event`] yields one [`Event`] at a time. The parser buffers a
//! `BufRead` source up to `max_total_input` and then walks that buffer; it
//! never opens a file or a socket itself.
//!
//! General-entity limits are enforced incrementally:
//! numerator = cumulative replacement-text bytes, denominator = original input
//! bytes consumed so far. The ratio is checked on every expansion once the
//! numerator exceeds [`EXPANSION_RATIO_FLOOR_BYTES`].

use std::borrow::Cow;
use std::collections::{HashMap, HashSet};
use std::io::BufRead;
use std::ops::Range;
use std::rc::Rc;

use crate::config::{ParserConfig, EXPANSION_RATIO_FLOOR_BYTES};
use crate::error::{observe_byte, position_at, Error, ErrorKind, LimitKind, Position, Result};
use crate::event::{
    Attribute, Doctype, EmptyElement, EndElement, Event, Namespace, ProcessingInstruction, QName,
    StartElement, XmlDecl, XMLNS_NAMESPACE, XML_NAMESPACE,
};
use crate::name::{
    decode_utf8, is_name_char, is_name_start, is_ncname_char, is_ncname_start, is_pubid_char,
    is_whitespace, is_xml_char,
};

/// Streaming XML reader.
///
/// Construct with [`Reader::from_bytes`], [`Reader::from_str`], or
/// [`Reader::from_reader`]. Pull events with [`Reader::read_event`].
///
/// `S` is the input holder: `&[u8]` for borrowed bytes, `Vec<u8>` after a
/// buffered read. Events borrow from that input when no normalization or
/// entity expansion was required.
///
/// The 0.x API is unstable.
pub struct Reader<S> {
    input: S,
    engine: Engine,
}

impl<'a> Reader<&'a [u8]> {
    /// Parse `input` as UTF-8 XML. A UTF-8 BOM is skipped.
    ///
    /// The total-input limit is checked on the first [`Reader::read_event`],
    /// so [`Reader::with_config`] can be applied before that call.
    pub fn from_bytes(input: &'a [u8]) -> Self {
        Self {
            input,
            engine: Engine::new(ParserConfig::default()),
        }
    }

    /// [`Reader::from_bytes`] for an already-valid UTF-8 string.
    ///
    /// This is not [`FromStr`]: the reader borrows `input` for `'a`.
    #[allow(clippy::should_implement_trait)]
    pub fn from_str(input: &'a str) -> Self {
        Self::from_bytes(input.as_bytes())
    }
}

impl Reader<Vec<u8>> {
    /// Read `reader` until EOF, refusing input larger than the default
    /// [`crate::DEFAULT_MAX_TOTAL_INPUT`].
    ///
    /// This only pulls bytes the caller already exposed through [`BufRead`].
    /// System and public identifiers are never turned into I/O.
    pub fn from_reader<R: BufRead>(reader: R) -> Result<Self> {
        Self::from_reader_with_config(reader, ParserConfig::default())
    }

    /// [`Reader::from_reader`] with an explicit limit set.
    pub fn from_reader_with_config<R: BufRead>(
        mut reader: R,
        config: ParserConfig,
    ) -> Result<Self> {
        let mut buf = Vec::new();
        let mut tmp = [0u8; 8192];
        let mut line = 1u64;
        let mut column = 1u64;
        let mut offset = 0u64;
        let mut cr_pending = false;
        loop {
            let n = reader.read(&mut tmp).map_err(|err| {
                Error::new(
                    ErrorKind::Io,
                    Position {
                        line,
                        column,
                        offset,
                    },
                    err.to_string(),
                )
            })?;
            if n == 0 {
                break;
            }
            let room = config.max_total_input.saturating_sub(buf.len() as u64) as usize;
            if n > room {
                let mut pos = Position {
                    line,
                    column,
                    offset,
                };
                let mut cr = cr_pending;
                for &b in &tmp[..room] {
                    observe_byte(&mut pos, &mut cr, b);
                }
                return Err(Error::new(
                    ErrorKind::SecurityLimit {
                        kind: LimitKind::TotalInput,
                        limit: config.max_total_input,
                        actual: (buf.len() + n) as u64,
                    },
                    pos,
                    "input exceeds max_total_input",
                ));
            }
            for &b in &tmp[..n] {
                let mut pos = Position {
                    line,
                    column,
                    offset,
                };
                observe_byte(&mut pos, &mut cr_pending, b);
                line = pos.line;
                column = pos.column;
                offset = pos.offset;
            }
            buf.extend_from_slice(&tmp[..n]);
        }
        Ok(Self {
            input: buf,
            engine: Engine::new(config),
        })
    }
}

impl<S: AsRef<[u8]>> Reader<S> {
    /// Replace the configuration. Call this before the first [`Reader::read_event`].
    pub fn with_config(mut self, config: ParserConfig) -> Self {
        if !self.engine.started {
            self.engine.config = config;
        }
        self
    }

    /// Limits currently in force.
    pub fn config(&self) -> &ParserConfig {
        &self.engine.config
    }

    /// Cursor in the original input. During entity expansion this stays on the
    /// reference that is being expanded.
    pub fn position(&self) -> Position {
        self.engine.pos_now()
    }

    /// Pull the next event.
    ///
    /// `Ok(None)` is a clean end of input after the root element.
    /// The returned event borrows `self`; drop it before the next call, or
    /// take ownership with [`Cow::into_owned`].
    pub fn read_event(&mut self) -> Result<Option<Event<'_>>> {
        let raw = self.engine.next_raw(self.input.as_ref())?;
        Ok(raw.map(|raw| materialize(self.input.as_ref(), raw)))
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Phase {
    Prolog,
    Content,
    Epilog,
    Done,
}

struct Frame {
    entity_idx: usize,
    pos: usize,
    origin: Position,
    /// Element stack depth when this entity was pushed.
    element_depth: usize,
    /// Markup-nesting counter when this entity was pushed.
    markup_depth: u32,
}

struct OpenElement {
    prefix: Option<String>,
    local: String,
    namespace: Option<Rc<str>>,
    depth: usize,
}

struct NsDecl {
    depth: usize,
    /// `None` is the default namespace.
    prefix: Option<Rc<str>>,
}

struct AttDef {
    name: String,
    cdata: bool,
    /// `None` for `#REQUIRED` and `#IMPLIED`.
    default_value: Option<String>,
}

#[derive(Clone)]
enum RawStr {
    Range(Range<usize>),
    Owned(String),
}

impl RawStr {
    fn as_str<'a>(&'a self, input: &'a [u8]) -> &'a str {
        match self {
            Self::Range(range) => assume_utf8(&input[range.start..range.end]),
            Self::Owned(text) => text.as_str(),
        }
    }

    fn byte_len(&self) -> usize {
        match self {
            Self::Range(range) => range.end.saturating_sub(range.start),
            Self::Owned(text) => text.len(),
        }
    }

    fn into_string(self, input: &[u8]) -> String {
        match self {
            Self::Range(range) => assume_utf8(&input[range.start..range.end]).to_string(),
            Self::Owned(text) => text,
        }
    }

    fn into_cow(self, input: &[u8]) -> Cow<'_, str> {
        match self {
            Self::Range(range) => Cow::Borrowed(assume_utf8(&input[range.start..range.end])),
            Self::Owned(text) => Cow::Owned(text),
        }
    }
}

struct RawQName {
    prefix: Option<RawStr>,
    local: RawStr,
}

struct ResolvedName {
    prefix: Option<RawStr>,
    local: RawStr,
    namespace: Option<Rc<str>>,
}

struct ResolvedAttr {
    name: ResolvedName,
    value: RawStr,
}

struct RawNs {
    prefix: Option<RawStr>,
    uri: RawStr,
}

struct RawElement {
    name: ResolvedName,
    attrs: Vec<ResolvedAttr>,
    nss: Vec<RawNs>,
}

enum RawEvent {
    Start(RawElement),
    End(ResolvedName),
    Empty(RawElement),
    Text(RawStr),
    CData(RawStr),
    Comment(RawStr),
    PI {
        target: RawStr,
        data: Option<RawStr>,
    },
    XmlDecl {
        version: RawStr,
        encoding: Option<RawStr>,
        standalone: Option<bool>,
    },
    Doctype {
        name: RawStr,
        system_id: Option<RawStr>,
        public_id: Option<RawStr>,
        has_internal_subset: bool,
    },
}

enum RefOut {
    Char(char),
    Pushed,
}

struct Engine {
    config: ParserConfig,
    pos: usize,
    line: u64,
    column: u64,
    cr_pending: bool,
    started: bool,
    at_document_start: bool,
    seen_doctype: bool,
    phase: Phase,
    stack: Vec<OpenElement>,
    default_ns: Vec<Rc<str>>,
    prefix_ns: HashMap<Rc<str>, Vec<Rc<str>>>,
    ns_decls: Vec<NsDecl>,
    interner: HashSet<Rc<str>>,
    attlists: HashMap<String, Vec<AttDef>>,
    markup_depth: u32,
    boundary_error: Option<Error>,
    halt: bool,
    entities: Vec<String>,
    /// Whether `entities[i]` has been checked against the `content` production.
    entity_wf: Vec<bool>,
    entity_index: HashMap<String, usize>,
    /// Internal parameter entities are recorded and never expanded.
    #[allow(dead_code)]
    param_entities: HashMap<String, String>,
    frames: Vec<Frame>,
    entity_depth: usize,
    open_entities: Vec<usize>,
    expansion_count: u64,
    expanded_bytes: u64,
}

impl Engine {
    fn new(config: ParserConfig) -> Self {
        Self {
            config,
            pos: 0,
            line: 1,
            column: 1,
            cr_pending: false,
            started: false,
            at_document_start: true,
            seen_doctype: false,
            phase: Phase::Prolog,
            stack: Vec::new(),
            default_ns: Vec::new(),
            prefix_ns: HashMap::new(),
            ns_decls: Vec::new(),
            interner: HashSet::new(),
            attlists: HashMap::new(),
            markup_depth: 0,
            boundary_error: None,
            halt: false,
            entities: Vec::new(),
            entity_wf: Vec::new(),
            entity_index: HashMap::new(),
            param_entities: HashMap::new(),
            frames: Vec::new(),
            entity_depth: 0,
            open_entities: Vec::new(),
            expansion_count: 0,
            expanded_bytes: 0,
        }
        .with_builtin_namespaces()
    }

    fn with_builtin_namespaces(mut self) -> Self {
        let prefix = self.intern("xml");
        let uri = self.intern(XML_NAMESPACE);
        self.prefix_ns
            .insert(Rc::clone(&prefix), vec![Rc::clone(&uri)]);
        self.ns_decls.push(NsDecl {
            depth: 0,
            prefix: Some(prefix),
        });
        let _ = self.intern(XMLNS_NAMESPACE);
        self
    }

    fn intern(&mut self, text: &str) -> Rc<str> {
        if let Some(existing) = self.interner.get(text) {
            return Rc::clone(existing);
        }
        let interned: Rc<str> = Rc::from(text);
        self.interner.insert(Rc::clone(&interned));
        interned
    }

    fn pos_now(&self) -> Position {
        Position {
            line: self.line,
            column: self.column,
            offset: self.pos as u64,
        }
    }

    fn err_pos(&self) -> Position {
        self.frames
            .first()
            .map(|frame| frame.origin)
            .unwrap_or_else(|| self.pos_now())
    }

    fn syntax(&self, message: impl Into<String>) -> Error {
        Error::new(ErrorKind::Syntax, self.err_pos(), message)
    }

    fn encoding(&self, message: impl Into<String>) -> Error {
        Error::new(ErrorKind::Encoding, self.err_pos(), message)
    }

    fn external_at(&self, position: Position, message: impl Into<String>) -> Error {
        Error::new(ErrorKind::ExternalEntityForbidden, position, message)
    }

    fn limit(&self, kind: LimitKind, limit: u64, actual: u64) -> Error {
        self.limit_at(kind, limit, actual, self.err_pos())
    }

    fn limit_at(&self, kind: LimitKind, limit: u64, actual: u64, position: Position) -> Error {
        Error::new(
            ErrorKind::SecurityLimit {
                kind,
                limit,
                actual,
            },
            position,
            format!("{kind} exceeded"),
        )
    }

    fn next_raw(&mut self, input: &[u8]) -> Result<Option<RawEvent>> {
        if let Some(err) = self.boundary_error.take() {
            return Err(err);
        }
        self.ensure_started(input)?;
        let result = match self.phase {
            Phase::Done => Ok(None),
            Phase::Prolog => self.next_prolog(input).map(Some),
            Phase::Content => self.next_content(input).map(Some),
            Phase::Epilog => self.next_epilog(input),
        };
        if let Some(err) = self.boundary_error.take() {
            return Err(err);
        }
        result
    }

    fn ensure_started(&mut self, input: &[u8]) -> Result<()> {
        if self.started {
            return Ok(());
        }
        self.started = true;
        if input.len() as u64 > self.config.max_total_input {
            let boundary = self.config.max_total_input as usize;
            return Err(self.limit_at(
                LimitKind::TotalInput,
                self.config.max_total_input,
                input.len() as u64,
                position_at(input, boundary),
            ));
        }
        self.consume_bom(input)
    }

    fn consume_bom(&mut self, input: &[u8]) -> Result<()> {
        if input.starts_with(&[0xEF, 0xBB, 0xBF]) {
            self.pos = 3;
            return Ok(());
        }
        if input.starts_with(&[0xFE, 0xFF])
            || input.starts_with(&[0xFF, 0xFE])
            || input.starts_with(&[0x00, 0x00, 0xFE, 0xFF])
            || input.starts_with(&[0xFF, 0xFE, 0x00, 0x00])
        {
            return Err(self.encoding("UTF-16/UTF-32 BOM is not supported"));
        }
        if input.starts_with(&[0x00, 0x3C, 0x00, 0x3F])
            || input.starts_with(&[0x3C, 0x00, 0x3F, 0x00])
        {
            return Err(self.encoding("UTF-16 encoded XML is not supported"));
        }
        Ok(())
    }

    fn next_prolog(&mut self, input: &[u8]) -> Result<RawEvent> {
        if self.at_document_start {
            self.at_document_start = false;
            if self.looks_like_xml_decl(input) {
                return self.parse_xml_decl(input);
            }
        }
        self.skip_ws(input)?;
        match self.peek_byte(input) {
            None => Err(self.syntax("document has no root element")),
            Some(b'<') => self.parse_prolog_markup(input),
            Some(_) => {
                let _ = self.decode_at_cursor(input)?;
                Err(self.syntax("expected markup in the prolog"))
            }
        }
    }

    fn parse_prolog_markup(&mut self, input: &[u8]) -> Result<RawEvent> {
        if self.starts_with(input, b"<!--") {
            return Ok(RawEvent::Comment(self.parse_comment(input)?));
        }
        if self.starts_with(input, b"<?") {
            return self.parse_pi(input);
        }
        if self.starts_with(input, b"<!DOCTYPE") {
            return self.parse_doctype(input);
        }
        if self.starts_with(input, b"</") {
            return Err(self.syntax("end tag in prolog"));
        }
        if self.starts_with(input, b"<!") {
            return Err(self.syntax("invalid declaration in prolog"));
        }
        self.parse_start_tag(input)
    }

    fn next_epilog(&mut self, input: &[u8]) -> Result<Option<RawEvent>> {
        self.skip_ws(input)?;
        if self.peek_byte(input).is_none() && self.frames.is_empty() {
            self.phase = Phase::Done;
            return Ok(None);
        }
        if self.starts_with(input, b"<!--") {
            return Ok(Some(RawEvent::Comment(self.parse_comment(input)?)));
        }
        if self.starts_with(input, b"<?") {
            return Ok(Some(self.parse_pi(input)?));
        }
        let _ = self.decode_at_cursor(input)?;
        Err(self.syntax("content after the root element"))
    }

    fn next_content(&mut self, input: &[u8]) -> Result<RawEvent> {
        loop {
            self.pop_finished();
            match self.peek_byte(input) {
                None => {
                    if self.halt || self.boundary_error.is_some() {
                        return Err(self.boundary_error.take().unwrap_or_else(|| {
                            self.syntax("entity replacement crosses a markup boundary")
                        }));
                    }
                    if self.frames.is_empty() {
                        return Err(self.syntax("unclosed element"));
                    }
                    self.force_pop_frame();
                }
                Some(b'<') => {
                    let event = self.parse_content_markup(input)?;
                    self.settle_entities()?;
                    return Ok(event);
                }
                Some(_) => {
                    if let Some(text) = self.parse_text(input)? {
                        return Ok(RawEvent::Text(text));
                    }
                }
            }
        }
    }

    fn parse_content_markup(&mut self, input: &[u8]) -> Result<RawEvent> {
        if self.starts_with(input, b"<!--") {
            return Ok(RawEvent::Comment(self.parse_comment(input)?));
        }
        if self.starts_with(input, b"<?") {
            return self.parse_pi(input);
        }
        if self.starts_with(input, b"<![CDATA[") {
            return Ok(RawEvent::CData(self.parse_cdata(input)?));
        }
        if self.starts_with(input, b"</") {
            return self.parse_end_tag(input);
        }
        if self.starts_with(input, b"<!DOCTYPE") {
            return Err(self.syntax("DOCTYPE is only allowed in the prolog"));
        }
        if self.starts_with(input, b"<!") {
            return Err(self.syntax("invalid declaration in element content"));
        }
        self.parse_start_tag(input)
    }

    fn looks_like_xml_decl(&mut self, input: &[u8]) -> bool {
        if !self.starts_with(input, b"<?xml") {
            return false;
        }
        let next = self.byte_at_offset(input, 5);
        match next {
            None => true,
            Some(b) => is_whitespace(b) || b == b'?',
        }
    }

    fn parse_xml_decl(&mut self, input: &[u8]) -> Result<RawEvent> {
        self.expect_bytes(input, b"<?xml")?;
        self.expect_ws(input)?;
        self.expect_token(input, b"version")?;
        self.skip_ws(input)?;
        self.expect_byte(input, b'=')?;
        self.skip_ws(input)?;
        let version = self.read_quoted_plain(input)?;
        let version_text = version.as_str(input).to_string();
        if version_text != "1.0" && version_text != "1.1" {
            return Err(self.syntax(format!("unsupported XML version '{version_text}'")));
        }
        let mut encoding = None;
        let mut standalone = None;
        loop {
            let ws = self.skip_ws(input)?;
            if self.starts_with(input, b"?>") {
                self.advance_n(input, 2)?;
                break;
            }
            if ws == 0 {
                return Err(self.syntax("expected whitespace in the XML declaration"));
            }
            if self.starts_with_token(input, b"encoding") {
                if encoding.is_some() || standalone.is_some() {
                    return Err(self.syntax("encoding declaration is duplicate or out of order"));
                }
                self.advance_n(input, b"encoding".len())?;
                self.skip_ws(input)?;
                self.expect_byte(input, b'=')?;
                self.skip_ws(input)?;
                let enc = self.read_quoted_plain(input)?;
                let enc_text = enc.as_str(input).to_string();
                self.check_encoding(&enc_text)?;
                encoding = Some(enc);
            } else if self.starts_with_token(input, b"standalone") {
                if standalone.is_some() {
                    return Err(self.syntax("duplicate standalone declaration"));
                }
                self.advance_n(input, b"standalone".len())?;
                self.skip_ws(input)?;
                self.expect_byte(input, b'=')?;
                self.skip_ws(input)?;
                let value = self.read_quoted_plain(input)?;
                let value_text = value.as_str(input).to_string();
                standalone = Some(match value_text.as_str() {
                    "yes" => true,
                    "no" => false,
                    other => {
                        return Err(self.syntax(format!("invalid standalone value '{other}'")));
                    }
                });
            } else {
                return Err(self.syntax("unknown XML declaration attribute"));
            }
        }
        Ok(RawEvent::XmlDecl {
            version,
            encoding,
            standalone,
        })
    }

    fn check_encoding(&self, encoding: &str) -> Result<()> {
        if encoding.eq_ignore_ascii_case("UTF-8") {
            return Ok(());
        }
        let lower = encoding.to_ascii_lowercase();
        if lower.contains("utf-16")
            || lower == "utf16"
            || lower.contains("utf-32")
            || lower.contains("ucs-2")
            || lower.contains("ucs-4")
        {
            return Err(self.encoding(format!(
                "encoding '{encoding}' is not supported; 0.x does not transcode"
            )));
        }
        Err(self.encoding(format!(
            "unsupported encoding '{encoding}', only UTF-8 is accepted"
        )))
    }

    fn parse_doctype(&mut self, input: &[u8]) -> Result<RawEvent> {
        if self.seen_doctype {
            return Err(self.syntax("a document may contain only one DOCTYPE"));
        }
        self.seen_doctype = true;
        self.expect_bytes(input, b"<!DOCTYPE")?;
        self.expect_ws(input)?;
        let name = self.read_name(input, false)?;
        self.skip_ws(input)?;
        let mut system_id = None;
        let mut public_id = None;
        if self.starts_with_token(input, b"SYSTEM") {
            self.advance_n(input, 6)?;
            self.expect_ws(input)?;
            system_id = Some(self.read_quoted_plain(input)?);
            self.skip_ws(input)?;
        } else if self.starts_with_token(input, b"PUBLIC") {
            self.advance_n(input, 6)?;
            self.expect_ws(input)?;
            public_id = Some(self.read_pubid_literal(input)?);
            self.expect_ws(input)?;
            system_id = Some(self.read_quoted_plain(input)?);
            self.skip_ws(input)?;
        }
        let mut has_internal_subset = false;
        if self.peek_byte(input) == Some(b'[') {
            has_internal_subset = true;
            self.advance_one(input)?;
            self.parse_internal_subset(input)?;
            self.expect_byte(input, b']')?;
            self.skip_ws(input)?;
        }
        self.expect_byte(input, b'>')?;
        Ok(RawEvent::Doctype {
            name,
            system_id,
            public_id,
            has_internal_subset,
        })
    }

    fn parse_internal_subset(&mut self, input: &[u8]) -> Result<()> {
        loop {
            self.skip_ws(input)?;
            match self.peek_byte(input) {
                Some(b']') => return Ok(()),
                None => return Err(self.syntax("unterminated internal subset")),
                Some(_) => {}
            }
            if self.starts_with(input, b"<!--") {
                let _ = self.parse_comment(input)?;
                continue;
            }
            if self.starts_with(input, b"<?") {
                let _ = self.parse_pi(input)?;
                continue;
            }
            if self.peek_byte(input) == Some(b'%') {
                return self.reject_parameter_reference(input);
            }
            if self.starts_with(input, b"<!ENTITY") {
                self.parse_entity_decl(input)?;
                continue;
            }
            if self.starts_with(input, b"<!ELEMENT") {
                self.parse_element_decl(input)?;
                continue;
            }
            if self.starts_with(input, b"<!ATTLIST") {
                self.parse_attlist_decl(input)?;
                continue;
            }
            if self.starts_with(input, b"<!NOTATION") {
                self.parse_notation_decl(input)?;
                continue;
            }
            if self.starts_with(input, b"<![") {
                return Err(
                    self.syntax("conditional sections are not allowed in the internal subset")
                );
            }
            return Err(self.syntax("invalid markup in the internal subset"));
        }
    }

    fn parse_entity_decl(&mut self, input: &[u8]) -> Result<()> {
        self.expect_bytes(input, b"<!ENTITY")?;
        self.expect_ws(input)?;
        if self.peek_byte(input) == Some(b'%') {
            let at = self.err_pos();
            self.advance_one(input)?;
            self.expect_ws(input)?;
            let name = self.read_name_string(input, false)?;
            self.expect_ws(input)?;
            if self.starts_with_token(input, b"SYSTEM") || self.starts_with_token(input, b"PUBLIC")
            {
                return Err(
                    self.external_at(at, "external parameter-entity declarations are forbidden")
                );
            }
            let value = self.parse_entity_value(input)?;
            self.skip_ws(input)?;
            self.expect_byte(input, b'>')?;
            self.param_entities.entry(name).or_insert(value);
            return Ok(());
        }
        let at = self.err_pos();
        let name = self.read_name_string(input, false)?;
        self.expect_ws(input)?;
        if self.starts_with_token(input, b"SYSTEM") || self.starts_with_token(input, b"PUBLIC") {
            return Err(self.external_at(
                at,
                format!("external entity declaration `&{name};` is forbidden"),
            ));
        }
        let value = self.parse_entity_value(input)?;
        self.skip_ws(input)?;
        self.expect_byte(input, b'>')?;
        if is_predefined_name(&name) || self.entity_index.contains_key(&name) {
            return Ok(());
        }
        // An unused entity may contain text that cannot legally be expanded
        // (for example a bare `<`). That check happens on use.
        let idx = self.entities.len();
        self.entities.push(value);
        self.entity_wf.push(false);
        self.entity_index.insert(name, idx);
        Ok(())
    }

    fn parse_entity_value(&mut self, input: &[u8]) -> Result<String> {
        let quote = self.require_quote(input)?;
        let mut out = String::new();
        loop {
            let Some(b) = self.peek_byte(input) else {
                return Err(self.syntax("unterminated entity value"));
            };
            if b == quote {
                self.advance_one(input)?;
                break;
            }
            if b == b'&' {
                self.advance_one(input)?;
                if self.peek_byte(input) == Some(b'#') {
                    self.advance_one(input)?;
                    out.push(self.parse_char_ref(input)?);
                } else {
                    let name = self.read_name_string(input, false)?;
                    self.expect_byte(input, b';')?;
                    out.push('&');
                    out.push_str(&name);
                    out.push(';');
                }
                continue;
            }
            if b == b'%' && self.percent_starts_name(input)? {
                return Err(
                    self.external_at(self.err_pos(), "parameter-entity references are forbidden")
                );
            }
            out.push(self.consume_content_literal(input)?);
        }
        Ok(out)
    }

    fn reject_parameter_reference(&mut self, input: &[u8]) -> Result<()> {
        let at = self.err_pos();
        if self.percent_starts_name(input)? {
            Err(self.external_at(at, "parameter-entity references are forbidden"))
        } else {
            Err(self.syntax("stray '%' in the internal subset"))
        }
    }

    fn percent_starts_name(&mut self, input: &[u8]) -> Result<bool> {
        let decoded = {
            let rest = self.current_bytes(input);
            if rest.first().copied() != Some(b'%') {
                return Ok(false);
            }
            if rest.len() < 2 {
                return Ok(false);
            }
            decode_utf8(&rest[1..])
        };
        match decoded {
            Ok((ch, _)) => Ok(is_name_start(ch)),
            Err(()) => Err(self.encoding("malformed UTF-8")),
        }
    }

    fn parse_element_decl(&mut self, input: &[u8]) -> Result<()> {
        self.expect_bytes(input, b"<!ELEMENT")?;
        self.expect_ws(input)?;
        self.reject_dtd_pe(input)?;
        let _name = self.read_name(input, false)?;
        self.expect_ws(input)?;
        self.reject_dtd_pe(input)?;
        self.parse_contentspec(input)?;
        self.skip_ws(input)?;
        self.reject_dtd_pe(input)?;
        self.expect_byte(input, b'>')
    }

    fn parse_contentspec(&mut self, input: &[u8]) -> Result<()> {
        if self.starts_with_token(input, b"EMPTY") {
            self.advance_n(input, "EMPTY".len())?;
            return Ok(());
        }
        if self.starts_with_token(input, b"ANY") {
            self.advance_n(input, "ANY".len())?;
            return Ok(());
        }
        self.parse_content_group(input, true)
    }

    fn parse_content_group(&mut self, input: &[u8], allow_mixed: bool) -> Result<()> {
        self.expect_byte(input, b'(')?;
        self.skip_ws(input)?;
        self.reject_dtd_pe(input)?;
        if self.starts_with(input, b"#PCDATA") {
            if !allow_mixed {
                return Err(self.syntax(
                    "mixed content is only allowed as the top-level content specification",
                ));
            }
            return self.parse_mixed(input);
        }
        self.parse_cp(input)?;
        self.skip_ws(input)?;
        self.reject_dtd_pe(input)?;
        if matches!(self.peek_byte(input), Some(b'|' | b',')) {
            let sep = self.peek_byte(input).unwrap_or(b'|');
            let mut separated = false;
            while self.peek_byte(input) == Some(sep) {
                separated = true;
                self.advance_one(input)?;
                self.skip_ws(input)?;
                self.reject_dtd_pe(input)?;
                self.parse_cp(input)?;
                self.skip_ws(input)?;
                self.reject_dtd_pe(input)?;
            }
            if sep == b'|' && !separated {
                return Err(self.syntax("choice list is missing an alternative"));
            }
        }
        self.expect_byte(input, b')')?;
        self.skip_occurrence(input)
    }

    fn parse_cp(&mut self, input: &[u8]) -> Result<()> {
        self.reject_dtd_pe(input)?;
        if self.peek_byte(input) == Some(b'(') {
            self.parse_content_group(input, false)
        } else {
            let _name = self.read_name(input, false)?;
            self.skip_occurrence(input)
        }
    }

    fn parse_mixed(&mut self, input: &[u8]) -> Result<()> {
        self.expect_bytes(input, b"#PCDATA")?;
        self.skip_ws(input)?;
        self.reject_dtd_pe(input)?;
        if self.peek_byte(input) == Some(b')') {
            self.advance_one(input)?;
            return self.expect_mixed_closer(input, false);
        }
        loop {
            self.expect_byte(input, b'|')?;
            self.skip_ws(input)?;
            self.reject_dtd_pe(input)?;
            let _name = self.read_name(input, false)?;
            self.skip_ws(input)?;
            self.reject_dtd_pe(input)?;
            if self.peek_byte(input) == Some(b')') {
                self.advance_one(input)?;
                return self.expect_mixed_closer(input, true);
            }
        }
    }

    fn expect_mixed_closer(&mut self, input: &[u8], names: bool) -> Result<()> {
        match self.peek_byte(input) {
            Some(b'*') => {
                self.advance_one(input)?;
                Ok(())
            }
            Some(b'?' | b'+') => Err(self.syntax("mixed content must be closed by `)` or `)*`")),
            _ if names => Err(self.syntax("mixed content with element names must end in `)*`")),
            _ => Ok(()),
        }
    }

    fn skip_occurrence(&mut self, input: &[u8]) -> Result<()> {
        if matches!(self.peek_byte(input), Some(b'?' | b'*' | b'+')) {
            self.advance_one(input)?;
        }
        Ok(())
    }

    fn parse_attlist_decl(&mut self, input: &[u8]) -> Result<()> {
        self.expect_bytes(input, b"<!ATTLIST")?;
        self.expect_ws(input)?;
        self.reject_dtd_pe(input)?;
        let element = self.read_name_string(input, false)?;
        loop {
            let ws = self.skip_ws(input)?;
            self.reject_dtd_pe(input)?;
            match self.peek_byte(input) {
                Some(b'>') => {
                    self.advance_one(input)?;
                    return Ok(());
                }
                None => return Err(self.syntax("unterminated attribute-list declaration")),
                Some(_) => {
                    if ws == 0 {
                        return Err(
                            self.syntax("expected whitespace before an attribute definition")
                        );
                    }
                    self.parse_att_def(input, &element)?;
                }
            }
        }
    }

    fn parse_att_def(&mut self, input: &[u8], element: &str) -> Result<()> {
        let name = self.read_name_string(input, false)?;
        self.expect_ws(input)?;
        self.reject_dtd_pe(input)?;
        let cdata = self.parse_att_type(input)?;
        self.expect_ws(input)?;
        self.reject_dtd_pe(input)?;
        let default_value = self.parse_default_decl(input, cdata)?;
        let defs = self.attlists.entry(element.to_string()).or_default();
        if defs.iter().any(|def| def.name == name) {
            return Ok(());
        }
        defs.push(AttDef {
            name,
            cdata,
            default_value,
        });
        Ok(())
    }

    fn parse_att_type(&mut self, input: &[u8]) -> Result<bool> {
        if self.starts_with_token(input, b"CDATA") {
            self.advance_n(input, "CDATA".len())?;
            return Ok(true);
        }
        for token in [
            "ID", "IDREF", "IDREFS", "ENTITY", "ENTITIES", "NMTOKEN", "NMTOKENS",
        ] {
            if self.starts_with_token(input, token.as_bytes()) {
                self.advance_n(input, token.len())?;
                return Ok(false);
            }
        }
        if self.starts_with_token(input, b"NOTATION") {
            self.advance_n(input, "NOTATION".len())?;
            self.expect_ws(input)?;
            self.reject_dtd_pe(input)?;
            self.expect_byte(input, b'(')?;
            self.skip_ws(input)?;
            self.reject_dtd_pe(input)?;
            let _name = self.read_name(input, false)?;
            loop {
                self.skip_ws(input)?;
                self.reject_dtd_pe(input)?;
                if self.peek_byte(input) != Some(b'|') {
                    break;
                }
                self.advance_one(input)?;
                self.skip_ws(input)?;
                self.reject_dtd_pe(input)?;
                let _name = self.read_name(input, false)?;
            }
            self.expect_byte(input, b')')?;
            return Ok(false);
        }
        if self.peek_byte(input) == Some(b'(') {
            self.advance_one(input)?;
            self.skip_ws(input)?;
            self.reject_dtd_pe(input)?;
            let _token = self.read_nmtoken(input)?;
            loop {
                self.skip_ws(input)?;
                self.reject_dtd_pe(input)?;
                if self.peek_byte(input) != Some(b'|') {
                    break;
                }
                self.advance_one(input)?;
                self.skip_ws(input)?;
                self.reject_dtd_pe(input)?;
                let _token = self.read_nmtoken(input)?;
            }
            self.expect_byte(input, b')')?;
            return Ok(false);
        }
        Err(self.syntax("invalid attribute type"))
    }

    fn parse_default_decl(&mut self, input: &[u8], cdata: bool) -> Result<Option<String>> {
        if self.starts_with_token(input, b"#REQUIRED") {
            self.advance_n(input, "#REQUIRED".len())?;
            return Ok(None);
        }
        if self.starts_with_token(input, b"#IMPLIED") {
            self.advance_n(input, "#IMPLIED".len())?;
            return Ok(None);
        }
        if self.starts_with_token(input, b"#FIXED") {
            self.advance_n(input, "#FIXED".len())?;
            self.expect_ws(input)?;
            self.reject_dtd_pe(input)?;
        }
        let raw = self.parse_attr_value(input)?;
        let mut text = raw.into_string(input);
        if !cdata {
            text = crate::wf::collapse_spaces(&text);
        }
        Ok(Some(text))
    }

    fn parse_notation_decl(&mut self, input: &[u8]) -> Result<()> {
        self.expect_bytes(input, b"<!NOTATION")?;
        self.expect_ws(input)?;
        self.reject_dtd_pe(input)?;
        let _name = self.read_name(input, false)?;
        self.expect_ws(input)?;
        self.reject_dtd_pe(input)?;
        if self.starts_with_token(input, b"SYSTEM") {
            self.advance_n(input, "SYSTEM".len())?;
            self.expect_ws(input)?;
            self.reject_dtd_pe(input)?;
            let _system = self.read_quoted_plain(input)?;
        } else if self.starts_with_token(input, b"PUBLIC") {
            self.advance_n(input, "PUBLIC".len())?;
            self.expect_ws(input)?;
            self.reject_dtd_pe(input)?;
            let _public = self.read_pubid_literal(input)?;
            self.skip_ws(input)?;
            self.reject_dtd_pe(input)?;
            if matches!(self.peek_byte(input), Some(b'"' | b'\'')) {
                let _system = self.read_quoted_plain(input)?;
            }
        } else {
            return Err(self.syntax("notation declaration requires SYSTEM or PUBLIC"));
        }
        self.skip_ws(input)?;
        self.reject_dtd_pe(input)?;
        self.expect_byte(input, b'>')
    }

    fn reject_dtd_pe(&mut self, input: &[u8]) -> Result<()> {
        if self.peek_byte(input) == Some(b'%') {
            self.reject_parameter_reference(input)?;
        }
        Ok(())
    }

    fn read_nmtoken(&mut self, input: &[u8]) -> Result<String> {
        let Some(first) = self.peek_char(input)? else {
            return Err(self.syntax("expected an nmtoken"));
        };
        if !is_name_char(first) {
            return Err(self.syntax(format!("invalid nmtoken start U+{:04X}", u32::from(first))));
        }
        let mut token = String::new();
        loop {
            let Some(ch) = self.peek_char(input)? else {
                break;
            };
            if !is_name_char(ch) {
                break;
            }
            self.consume_char(input)?;
            token.push(ch);
        }
        Ok(token)
    }

    fn add_default_attributes(
        &mut self,
        input: &[u8],
        element: &str,
        attrs: &mut Vec<(RawQName, RawStr)>,
    ) {
        let Some(defs) = self.attlists.get(element) else {
            return;
        };
        let defaults: Vec<(String, String)> = defs
            .iter()
            .filter_map(|def| {
                def.default_value
                    .as_ref()
                    .map(|value| (def.name.clone(), value.clone()))
            })
            .collect();
        for (name, value) in defaults {
            let present = attrs
                .iter()
                .any(|(qname, _)| raw_qname_string(input, qname) == name);
            if present {
                continue;
            }
            attrs.push((owned_qname(&name), RawStr::Owned(value)));
        }
    }

    fn normalize_typed_attr(
        &self,
        element: &str,
        attribute: &str,
        value: RawStr,
        input: &[u8],
    ) -> RawStr {
        let cdata = self
            .attlists
            .get(element)
            .and_then(|defs| defs.iter().find(|def| def.name == attribute))
            .map(|def| def.cdata)
            .unwrap_or(true);
        if cdata {
            return value;
        }
        let text = value.as_str(input);
        let collapsed = crate::wf::collapse_spaces(text);
        if collapsed == text {
            value
        } else {
            RawStr::Owned(collapsed)
        }
    }

    fn parse_start_tag(&mut self, input: &[u8]) -> Result<RawEvent> {
        self.in_markup(|this| this.parse_start_tag_inner(input))
    }

    fn parse_start_tag_inner(&mut self, input: &[u8]) -> Result<RawEvent> {
        if self.phase == Phase::Epilog {
            return Err(self.syntax("content after the root element"));
        }
        self.expect_byte(input, b'<')?;
        let qname = self.read_qname(input)?;
        let mut attrs: Vec<(RawQName, RawStr)> = Vec::new();
        let mut seen_raw: HashSet<(Option<String>, String)> = HashSet::new();
        loop {
            let ws = self.skip_ws(input)?;
            match self.peek_byte(input) {
                Some(b'>') => {
                    self.advance_one(input)?;
                    return self.finish_element(input, qname, attrs, false);
                }
                Some(b'/') => {
                    self.advance_one(input)?;
                    self.expect_byte(input, b'>')?;
                    return self.finish_element(input, qname, attrs, true);
                }
                Some(_) => {
                    if ws == 0 {
                        return Err(self.syntax("expected whitespace before attribute"));
                    }
                    let count = attrs.len() as u64 + 1;
                    if count > self.config.max_attrs as u64 {
                        return Err(self.limit(
                            LimitKind::Attributes,
                            self.config.max_attrs as u64,
                            count,
                        ));
                    }
                    let name = self.read_qname(input)?;
                    let raw_key = (
                        name.prefix
                            .as_ref()
                            .map(|prefix| prefix.as_str(input).to_string()),
                        name.local.as_str(input).to_string(),
                    );
                    if !seen_raw.insert(raw_key) {
                        return Err(self.syntax("duplicate attribute"));
                    }
                    self.skip_ws(input)?;
                    self.expect_byte(input, b'=')?;
                    self.skip_ws(input)?;
                    let value = self.parse_attr_value(input)?;
                    let attr_key = raw_qname_string(input, &name);
                    let elem_key = raw_qname_string(input, &qname);
                    let value = self.normalize_typed_attr(&elem_key, &attr_key, value, input);
                    attrs.push((name, value));
                }
                None => return Err(self.syntax("unterminated start tag")),
            }
        }
    }

    fn parse_end_tag(&mut self, input: &[u8]) -> Result<RawEvent> {
        self.in_markup(|this| this.parse_end_tag_inner(input))
    }

    fn parse_end_tag_inner(&mut self, input: &[u8]) -> Result<RawEvent> {
        let at = self.err_pos();
        self.expect_bytes(input, b"</")?;
        let qname = self.read_qname(input)?;
        self.skip_ws(input)?;
        self.expect_byte(input, b'>')?;
        let Some(open) = self.stack.pop() else {
            return Err(Error::new(ErrorKind::Syntax, at, "unexpected end tag"));
        };
        let prefix = qname
            .prefix
            .as_ref()
            .map(|value| value.as_str(input).to_string());
        let local = qname.local.as_str(input).to_string();
        if prefix != open.prefix || local != open.local {
            let expected = qualified(&open.prefix, &open.local);
            let found = qualified(&prefix, &local);
            return Err(Error::new(
                ErrorKind::Syntax,
                at,
                format!("mismatched end tag: expected `{expected}`, found `{found}`"),
            ));
        }
        self.pop_ns(open.depth);
        if self.stack.is_empty() {
            self.phase = Phase::Epilog;
        }
        Ok(RawEvent::End(ResolvedName {
            prefix: qname.prefix,
            local: qname.local,
            namespace: open.namespace,
        }))
    }

    fn finish_element(
        &mut self,
        input: &[u8],
        qname: RawQName,
        attrs: Vec<(RawQName, RawStr)>,
        empty: bool,
    ) -> Result<RawEvent> {
        let new_depth = self.stack.len() + 1;
        if new_depth > self.config.max_depth {
            return Err(self.limit(
                LimitKind::ElementDepth,
                self.config.max_depth as u64,
                new_depth as u64,
            ));
        }

        let elem_key = raw_qname_string(input, &qname);
        let mut attrs = attrs;
        self.add_default_attributes(input, &elem_key, &mut attrs);
        if attrs.len() as u64 > self.config.max_attrs as u64 {
            return Err(self.limit(
                LimitKind::Attributes,
                self.config.max_attrs as u64,
                attrs.len() as u64,
            ));
        }

        let mut nss = Vec::new();
        let mut regular = Vec::new();
        for (name, value) in attrs {
            let prefix = name
                .prefix
                .as_ref()
                .map(|value| value.as_str(input).to_string());
            let local = name.local.as_str(input).to_string();
            if prefix.as_deref() == Some("xmlns") || (prefix.is_none() && local == "xmlns") {
                let decl_prefix = if prefix.is_some() { Some(local) } else { None };
                nss.push((decl_prefix, value));
            } else {
                regular.push((name, value));
            }
        }

        let mut prepared = Vec::with_capacity(nss.len());
        let mut raw_nss = Vec::with_capacity(nss.len());
        for (decl_prefix, value) in nss {
            let uri = value.as_str(input).to_string();
            self.validate_xmlns(decl_prefix.as_deref(), &uri)?;
            raw_nss.push(RawNs {
                prefix: decl_prefix.clone().map(RawStr::Owned),
                uri: value,
            });
            prepared.push((decl_prefix, uri));
        }
        for (prefix, uri) in prepared {
            let prefix = prefix.map(|prefix| self.intern(&prefix));
            let uri = self.intern(&uri);
            self.push_ns(prefix, uri, new_depth);
        }

        let resolved_name = self.resolve_name(input, qname, true)?;
        let mut seen_exp: HashSet<(Option<Rc<str>>, String)> = HashSet::new();
        let mut resolved_attrs = Vec::with_capacity(regular.len());
        for (name, value) in regular {
            let resolved = self.resolve_name(input, name, false)?;
            let key = (
                resolved.namespace.clone(),
                resolved.local.as_str(input).to_string(),
            );
            if !seen_exp.insert(key) {
                self.pop_ns(new_depth);
                return Err(self.syntax("duplicate expanded attribute name"));
            }
            resolved_attrs.push(ResolvedAttr {
                name: resolved,
                value,
            });
        }

        let open = OpenElement {
            prefix: resolved_name
                .prefix
                .as_ref()
                .map(|value| value.as_str(input).to_string()),
            local: resolved_name.local.as_str(input).to_string(),
            namespace: resolved_name.namespace.clone(),
            depth: new_depth,
        };
        let element = RawElement {
            name: resolved_name,
            attrs: resolved_attrs,
            nss: raw_nss,
        };
        if empty {
            self.pop_ns(new_depth);
            if self.stack.is_empty() {
                self.phase = Phase::Epilog;
            } else {
                self.phase = Phase::Content;
            }
            Ok(RawEvent::Empty(element))
        } else {
            self.stack.push(open);
            self.phase = Phase::Content;
            Ok(RawEvent::Start(element))
        }
    }

    fn validate_xmlns(&self, prefix: Option<&str>, uri: &str) -> Result<()> {
        match prefix {
            None => {
                if uri == XML_NAMESPACE || uri == XMLNS_NAMESPACE {
                    return Err(
                        self.syntax("the default namespace must not be the xml or xmlns namespace")
                    );
                }
            }
            Some("xmlns") => {
                return Err(self.syntax("the xmlns prefix must not be declared"));
            }
            Some("xml") => {
                if uri != XML_NAMESPACE {
                    return Err(self.syntax("the xml prefix must be bound to the XML namespace"));
                }
            }
            Some(_) => {
                if uri.is_empty() {
                    return Err(
                        self.syntax("cannot undeclare a namespace prefix in XML namespaces 1.0")
                    );
                }
                if uri == XML_NAMESPACE || uri == XMLNS_NAMESPACE {
                    return Err(
                        self.syntax("this prefix must not be bound to a reserved namespace")
                    );
                }
            }
        }
        Ok(())
    }

    fn resolve_name(&self, input: &[u8], name: RawQName, element: bool) -> Result<ResolvedName> {
        let namespace = if let Some(prefix) = &name.prefix {
            let prefix_text = prefix.as_str(input);
            if prefix_text == "xmlns" {
                return Err(self.syntax("the xmlns prefix is reserved for namespace declarations"));
            }
            match self.lookup_ns(Some(prefix_text)) {
                Some(uri) => Some(uri),
                None => {
                    return Err(self.syntax(format!("unbound namespace prefix `{prefix_text}`")));
                }
            }
        } else if element {
            self.lookup_ns(None)
        } else {
            None
        };
        Ok(ResolvedName {
            prefix: name.prefix,
            local: name.local,
            namespace,
        })
    }

    fn lookup_ns(&self, prefix: Option<&str>) -> Option<Rc<str>> {
        let uri = match prefix {
            None => self.default_ns.last().cloned(),
            Some(prefix) => self
                .prefix_ns
                .get(prefix)
                .and_then(|stack| stack.last().cloned()),
        };
        match uri {
            Some(uri) if uri.is_empty() => None,
            other => other,
        }
    }

    fn push_ns(&mut self, prefix: Option<Rc<str>>, uri: Rc<str>, depth: usize) {
        match &prefix {
            None => self.default_ns.push(uri),
            Some(prefix) => {
                self.prefix_ns
                    .entry(Rc::clone(prefix))
                    .or_default()
                    .push(uri);
            }
        }
        self.ns_decls.push(NsDecl { depth, prefix });
    }

    fn pop_ns(&mut self, depth: usize) {
        while self.ns_decls.last().is_some_and(|decl| decl.depth == depth) {
            let decl = self.ns_decls.pop().expect("namespace declaration");
            match decl.prefix {
                None => {
                    self.default_ns.pop();
                }
                Some(prefix) => {
                    let empty = self
                        .prefix_ns
                        .get_mut(&prefix)
                        .map(|stack| {
                            stack.pop();
                            stack.is_empty()
                        })
                        .unwrap_or(false);
                    if empty {
                        self.prefix_ns.remove(&prefix);
                    }
                }
            }
        }
    }

    fn in_markup<T>(&mut self, body: impl FnOnce(&mut Self) -> Result<T>) -> Result<T> {
        self.markup_depth += 1;
        let result = body(self);
        self.markup_depth -= 1;
        result
    }

    fn settle_entities(&mut self) -> Result<()> {
        self.pop_finished();
        if let Some(err) = self.boundary_error.take() {
            Err(err)
        } else {
            Ok(())
        }
    }

    fn parse_attr_value(&mut self, input: &[u8]) -> Result<RawStr> {
        let quote = self.require_quote(input)?;
        let base_depth = self.frames.len();
        let start = self.pos;
        let mut buf = String::new();
        let mut plain = base_depth == 0;
        loop {
            self.pop_finished();
            if self.frames.len() < base_depth {
                return Err(self.syntax("unterminated attribute value"));
            }
            let at_base = self.frames.len() == base_depth;
            let Some(b) = self.peek_byte(input) else {
                if at_base {
                    return Err(self.syntax("unterminated attribute value"));
                }
                self.force_pop_frame();
                continue;
            };
            if at_base && b == quote {
                self.advance_one(input)?;
                break;
            }
            if b == b'<' {
                return Err(self.syntax("`<` is not allowed in an attribute value"));
            }
            if b == b'&' {
                if plain {
                    buf.push_str(assume_utf8(&input[start..self.pos]));
                    plain = false;
                }
                match self.expand_ref(input)? {
                    RefOut::Char(ch) => buf.push(ch),
                    RefOut::Pushed => {}
                }
                continue;
            }
            let mark = self.pos;
            let raw_special = matches!(b, b'\t' | b'\n' | b'\r');
            let ch = self.consume_attr_literal(input)?;
            if plain && raw_special {
                buf.push_str(assume_utf8(&input[start..mark]));
                plain = false;
                buf.push(ch);
            } else if !plain {
                buf.push(ch);
            }
        }
        // Length is the value after normalization and entity expansion.
        if plain {
            self.check_len(self.pos - start - 1, LimitKind::AttrValueLength)?;
            Ok(RawStr::Range(start..self.pos - 1))
        } else {
            self.check_len(buf.len(), LimitKind::AttrValueLength)?;
            Ok(RawStr::Owned(buf))
        }
    }

    fn parse_text(&mut self, input: &[u8]) -> Result<Option<RawStr>> {
        if let Some(text) = self.try_plain_text(input)? {
            return Ok(Some(text));
        }
        let mut buf = String::new();
        let mut brackets = 0u8;
        loop {
            self.pop_finished();
            let Some(b) = self.peek_byte(input) else {
                if self.frames.is_empty() {
                    break;
                }
                self.force_pop_frame();
                continue;
            };
            if b == b'<' {
                break;
            }
            if b == b'&' {
                match self.expand_ref(input)? {
                    RefOut::Char(ch) => self.push_text(&mut buf, &mut brackets, ch)?,
                    RefOut::Pushed => {
                        self.pop_finished();
                        if self.peek_byte(input) == Some(b'<') {
                            break;
                        }
                    }
                }
                continue;
            }
            let ch = self.consume_content_literal(input)?;
            self.push_text(&mut buf, &mut brackets, ch)?;
        }
        if buf.is_empty() {
            Ok(None)
        } else {
            Ok(Some(RawStr::Owned(buf)))
        }
    }

    fn try_plain_text(&mut self, input: &[u8]) -> Result<Option<RawStr>> {
        if !self.frames.is_empty() {
            return Ok(None);
        }
        let start = self.pos;
        let bytes = &input[start..];
        let mut i = 0usize;
        let mut brackets = 0u8;
        while i < bytes.len() {
            let b = bytes[i];
            if b == b'<' {
                break;
            }
            if b == b'&' || b == b'\r' {
                return Ok(None);
            }
            if b == b']' {
                brackets = (brackets + 1).min(2);
                i += 1;
                if (i as u64) > self.config.max_text_len {
                    self.advance_n(input, i)?;
                    return Err(self.limit(
                        LimitKind::TextLength,
                        self.config.max_text_len,
                        i as u64,
                    ));
                }
                continue;
            }
            if b == b'>' {
                if brackets >= 2 {
                    self.advance_n(input, i)?;
                    return Err(self.syntax("`]]>` is not allowed in content"));
                }
                brackets = 0;
                i += 1;
                continue;
            }
            brackets = 0;
            if b < 0x80 {
                if !is_xml_char(b as char) {
                    self.advance_n(input, i)?;
                    return Err(self.syntax(format!("illegal XML character U+{b:04X}")));
                }
                i += 1;
            } else {
                match decode_utf8(&bytes[i..]) {
                    Ok((ch, n)) => {
                        if !is_xml_char(ch) {
                            self.advance_n(input, i)?;
                            return Err(self
                                .syntax(format!("illegal XML character U+{:04X}", u32::from(ch))));
                        }
                        i += n;
                    }
                    Err(()) => {
                        self.advance_n(input, i)?;
                        return Err(self.encoding("malformed UTF-8"));
                    }
                }
            }
            if (i as u64) > self.config.max_text_len {
                self.advance_n(input, i.min(bytes.len()))?;
                return Err(self.limit(LimitKind::TextLength, self.config.max_text_len, i as u64));
            }
        }
        if i == 0 {
            return Ok(None);
        }
        self.advance_n(input, i)?;
        Ok(Some(RawStr::Range(start..start + i)))
    }

    fn push_text(&mut self, buf: &mut String, brackets: &mut u8, ch: char) -> Result<()> {
        if ch == ']' {
            *brackets = brackets.saturating_add(1).min(2);
        } else if ch == '>' && *brackets >= 2 {
            return Err(self.syntax("`]]>` is not allowed in content"));
        } else {
            *brackets = 0;
        }
        self.push_limited(buf, ch, LimitKind::TextLength)
    }

    fn parse_comment(&mut self, input: &[u8]) -> Result<RawStr> {
        self.in_markup(|this| this.parse_comment_inner(input))
    }

    fn parse_comment_inner(&mut self, input: &[u8]) -> Result<RawStr> {
        self.expect_bytes(input, b"<!--")?;
        let body = self.read_terminated(input, b"-->", true, "unterminated comment")?;
        self.expect_bytes(input, b"-->")?;
        Ok(body)
    }

    fn parse_cdata(&mut self, input: &[u8]) -> Result<RawStr> {
        self.in_markup(|this| this.parse_cdata_inner(input))
    }

    fn parse_cdata_inner(&mut self, input: &[u8]) -> Result<RawStr> {
        self.expect_bytes(input, b"<![CDATA[")?;
        let body = self.read_terminated(input, b"]]>", false, "unterminated CDATA section")?;
        self.expect_bytes(input, b"]]>")?;
        Ok(body)
    }

    fn parse_pi(&mut self, input: &[u8]) -> Result<RawEvent> {
        self.in_markup(|this| this.parse_pi_inner(input))
    }

    fn parse_pi_inner(&mut self, input: &[u8]) -> Result<RawEvent> {
        self.expect_bytes(input, b"<?")?;
        let target = self.read_name(input, false)?;
        if target.as_str(input).eq_ignore_ascii_case("xml") {
            return Err(self.syntax("processing instruction target must not be 'xml'"));
        }
        let data = match self.peek_byte(input) {
            Some(b'?') if self.starts_with(input, b"?>") => None,
            Some(b) if is_whitespace(b) => {
                self.consume_one_space(input)?;
                if self.starts_with(input, b"?>") {
                    Some(RawStr::Owned(String::new()))
                } else {
                    Some(self.read_terminated(
                        input,
                        b"?>",
                        false,
                        "unterminated processing instruction",
                    )?)
                }
            }
            Some(_) => {
                return Err(self.syntax("expected whitespace after processing instruction target"));
            }
            None => return Err(self.syntax("unterminated processing instruction")),
        };
        self.expect_bytes(input, b"?>")?;
        if let Some(RawStr::Owned(text)) = &data {
            self.check_len(text.len(), LimitKind::TextLength)?;
        }
        Ok(RawEvent::PI { target, data })
    }

    fn read_terminated(
        &mut self,
        input: &[u8],
        marker: &[u8],
        comment: bool,
        eof_message: &str,
    ) -> Result<RawStr> {
        let start = self.pos;
        let on_main = self.frames.is_empty();
        let mut buf = String::new();
        let mut plain = on_main;
        loop {
            if self.starts_with(input, marker) {
                let end = if self.frames.is_empty() { self.pos } else { 0 };
                if plain && on_main {
                    self.check_len(end.saturating_sub(start), LimitKind::TextLength)?;
                    return Ok(RawStr::Range(start..end));
                }
                self.check_len(buf.len(), LimitKind::TextLength)?;
                return Ok(RawStr::Owned(buf));
            }
            if comment && self.starts_with(input, b"--") {
                return Err(self.syntax("`--` is not allowed in a comment"));
            }
            if self.peek_byte(input).is_none() {
                return Err(self.syntax(eof_message));
            }
            let mark = self.pos;
            let raw_cr = self.peek_byte(input) == Some(b'\r') && self.frames.is_empty();
            let ch = self.consume_content_literal(input)?;
            if plain && raw_cr {
                buf.push_str(assume_utf8(&input[start..mark]));
                plain = false;
            }
            if !plain {
                let new_len = buf.len() + ch.len_utf8();
                self.check_len(new_len, LimitKind::TextLength)?;
                buf.push(ch);
            }
        }
    }

    fn consume_one_space(&mut self, input: &[u8]) -> Result<()> {
        let b = self.peek_byte(input).unwrap_or(0);
        self.advance_one(input)?;
        if b == b'\r' && self.peek_byte(input) == Some(b'\n') {
            self.advance_one(input)?;
        }
        Ok(())
    }

    fn read_pubid_literal(&mut self, input: &[u8]) -> Result<RawStr> {
        let raw = self.read_quoted_plain(input)?;
        let text = raw.as_str(input).to_string();
        if let Some(ch) = text.chars().find(|ch| !is_pubid_char(*ch)) {
            return Err(self.syntax(format!(
                "illegal public identifier character U+{:04X}",
                u32::from(ch)
            )));
        }
        Ok(raw)
    }

    fn read_quoted_plain(&mut self, input: &[u8]) -> Result<RawStr> {
        let quote = self.require_quote(input)?;
        let on_main = self.frames.is_empty();
        let start = self.pos;
        let mut buf = String::new();
        let mut plain = on_main;
        loop {
            let Some(b) = self.peek_byte(input) else {
                return Err(self.syntax("unterminated quoted literal"));
            };
            if b == quote {
                self.advance_one(input)?;
                break;
            }
            let mark = self.pos;
            let raw_cr = b == b'\r' && self.frames.is_empty();
            let ch = self.consume_content_literal(input)?;
            if plain && raw_cr {
                buf.push_str(assume_utf8(&input[start..mark]));
                plain = false;
            }
            if !plain {
                buf.push(ch);
            }
        }
        if plain && on_main {
            Ok(RawStr::Range(start..self.pos - 1))
        } else {
            Ok(RawStr::Owned(buf))
        }
    }

    fn read_qname(&mut self, input: &[u8]) -> Result<RawQName> {
        let first = self.read_name(input, true)?;
        if self.peek_byte(input) == Some(b':') {
            let prefix_len = first.byte_len();
            self.advance_one(input)?;
            let local = self.read_name(input, true)?;
            let total = prefix_len + 1 + local.byte_len();
            if total > self.config.max_name_len {
                return Err(self.limit(
                    LimitKind::NameLength,
                    self.config.max_name_len as u64,
                    total as u64,
                ));
            }
            if self.peek_byte(input) == Some(b':') {
                return Err(self.syntax("invalid qualified name"));
            }
            Ok(RawQName {
                prefix: Some(first),
                local,
            })
        } else {
            Ok(RawQName {
                prefix: None,
                local: first,
            })
        }
    }

    fn read_name_string(&mut self, input: &[u8], ncname: bool) -> Result<String> {
        Ok(self.read_name(input, ncname)?.into_string(input))
    }

    fn read_name(&mut self, input: &[u8], ncname: bool) -> Result<RawStr> {
        let on_main = self.frames.is_empty();
        let start = self.pos;
        let max = self.config.max_name_len;
        let Some(first) = self.peek_char(input)? else {
            return Err(self.syntax("unexpected end of input in name"));
        };
        let ok_start = if ncname {
            is_ncname_start(first)
        } else {
            is_name_start(first)
        };
        if !ok_start {
            return Err(self.syntax(format!("invalid name start U+{:04X}", u32::from(first))));
        }
        let mut total = first.len_utf8();
        if total > max {
            return Err(self.limit(LimitKind::NameLength, max as u64, total as u64));
        }
        let mut owned = String::new();
        self.consume_char(input)?;
        owned.push(first);
        loop {
            let Some(ch) = self.peek_char(input)? else {
                break;
            };
            let ok = if ncname {
                is_ncname_char(ch)
            } else {
                is_name_char(ch)
            };
            if !ok {
                break;
            }
            let next = total + ch.len_utf8();
            if next > max {
                return Err(self.limit(LimitKind::NameLength, max as u64, next as u64));
            }
            self.consume_char(input)?;
            owned.push(ch);
            total = next;
        }
        if on_main && self.frames.is_empty() {
            Ok(RawStr::Range(start..self.pos))
        } else {
            Ok(RawStr::Owned(owned))
        }
    }

    fn expand_ref(&mut self, input: &[u8]) -> Result<RefOut> {
        let origin = self.err_pos();
        self.advance_one(input)?;
        if self.peek_byte(input) == Some(b'#') {
            self.advance_one(input)?;
            return Ok(RefOut::Char(self.parse_char_ref(input)?));
        }
        let name = self.read_name_string(input, false)?;
        self.expect_byte(input, b';')?;
        if let Some(ch) = predefined(&name) {
            return Ok(RefOut::Char(ch));
        }
        self.push_entity(&name, origin)?;
        Ok(RefOut::Pushed)
    }

    fn parse_char_ref(&mut self, input: &[u8]) -> Result<char> {
        let hex = match self.peek_byte(input) {
            Some(b'x') => {
                self.advance_one(input)?;
                true
            }
            Some(b'X') => {
                return Err(
                    self.syntax("hexadecimal character references must use a lowercase 'x'")
                );
            }
            _ => false,
        };
        let mut value: u32 = 0;
        let mut digits = 0u32;
        let base: u32 = if hex { 16 } else { 10 };
        loop {
            let Some(b) = self.peek_byte(input) else {
                return Err(self.syntax("truncated character reference"));
            };
            let Some(digit) = hex_or_dec(b, hex) else {
                break;
            };
            digits += 1;
            if value > (0x10_FFFF - digit) / base {
                return Err(self.syntax("invalid code point in character reference"));
            }
            value = value * base + digit;
            self.advance_one(input)?;
        }
        if digits == 0 {
            return Err(self.syntax("empty character reference"));
        }
        self.expect_byte(input, b';')?;
        let Some(ch) = char::from_u32(value) else {
            return Err(self.syntax("invalid code point in character reference"));
        };
        if !is_xml_char(ch) {
            return Err(self.syntax(format!("illegal XML character U+{value:04X}")));
        }
        Ok(ch)
    }

    fn push_entity(&mut self, name: &str, origin: Position) -> Result<()> {
        let Some(&idx) = self.entity_index.get(name) else {
            return Err(Error::new(
                ErrorKind::Syntax,
                origin,
                format!("undefined entity `&{name};`"),
            ));
        };
        if self.open_entities.contains(&idx) {
            return Err(Error::new(
                ErrorKind::Syntax,
                origin,
                format!("recursive entity reference `&{name};`"),
            ));
        }
        let new_depth = self.entity_depth + 1;
        if new_depth > self.config.max_entity_depth {
            return Err(Error::new(
                ErrorKind::SecurityLimit {
                    kind: LimitKind::EntityDepth,
                    limit: self.config.max_entity_depth as u64,
                    actual: new_depth as u64,
                },
                origin,
                "entity nesting depth exceeded",
            ));
        }
        self.expansion_count += 1;
        if self.expansion_count > self.config.max_entity_expansions {
            return Err(Error::new(
                ErrorKind::SecurityLimit {
                    kind: LimitKind::EntityExpansions,
                    limit: self.config.max_entity_expansions,
                    actual: self.expansion_count,
                },
                origin,
                "entity expansion count exceeded",
            ));
        }
        if !self.entity_wf[idx] {
            let replacement = self.entities[idx].clone();
            if let Err(message) = crate::wf::check_replacement_content(&replacement) {
                return Err(Error::new(ErrorKind::Syntax, origin, message));
            }
            self.entity_wf[idx] = true;
        }
        let repl_len = self.entities[idx].len() as u64;
        self.expanded_bytes += repl_len;
        if self.expanded_bytes > self.config.max_expanded_bytes {
            return Err(Error::new(
                ErrorKind::SecurityLimit {
                    kind: LimitKind::ExpandedBytes,
                    limit: self.config.max_expanded_bytes,
                    actual: self.expanded_bytes,
                },
                origin,
                "expanded byte cap exceeded",
            ));
        }
        if self.expanded_bytes > EXPANSION_RATIO_FLOOR_BYTES {
            let consumed = self.pos as u64;
            let allowed = consumed.saturating_mul(self.config.max_expansion_ratio);
            if self.expanded_bytes > allowed {
                return Err(Error::new(
                    ErrorKind::SecurityLimit {
                        kind: LimitKind::ExpansionRatio,
                        limit: allowed,
                        actual: self.expanded_bytes,
                    },
                    origin,
                    format!(
                        "expansion ratio exceeded after consuming {consumed} input bytes (cap {})",
                        self.config.max_expansion_ratio
                    ),
                ));
            }
        }
        self.entity_depth = new_depth;
        self.open_entities.push(idx);
        self.frames.push(Frame {
            entity_idx: idx,
            pos: 0,
            origin,
            element_depth: self.stack.len(),
            markup_depth: self.markup_depth,
        });
        Ok(())
    }

    fn push_limited(&mut self, buf: &mut String, ch: char, kind: LimitKind) -> Result<()> {
        let new_len = buf.len() + ch.len_utf8();
        self.check_len(new_len, kind)?;
        buf.push(ch);
        Ok(())
    }

    fn check_len(&self, len: usize, kind: LimitKind) -> Result<()> {
        let actual = len as u64;
        if actual > self.config.max_text_len {
            Err(self.limit(kind, self.config.max_text_len, actual))
        } else {
            Ok(())
        }
    }

    fn consume_content_literal(&mut self, input: &[u8]) -> Result<char> {
        let on_main = self.frames.is_empty();
        let ch = self.consume_char(input)?;
        if on_main && ch == '\r' {
            if self.peek_byte(input) == Some(b'\n') {
                self.advance_one(input)?;
            }
            Ok('\n')
        } else {
            Ok(ch)
        }
    }

    fn consume_attr_literal(&mut self, input: &[u8]) -> Result<char> {
        let on_main = self.frames.is_empty();
        let mut ch = self.consume_char(input)?;
        if on_main && ch == '\r' {
            if self.peek_byte(input) == Some(b'\n') {
                self.advance_one(input)?;
            }
            ch = '\n';
        }
        if matches!(ch, ' ' | '\t' | '\n' | '\r') {
            Ok(' ')
        } else {
            Ok(ch)
        }
    }

    fn consume_char(&mut self, input: &[u8]) -> Result<char> {
        let (ch, width) = self.decode_at_cursor(input)?;
        if !is_xml_char(ch) {
            return Err(self.syntax(format!("illegal XML character U+{:04X}", u32::from(ch))));
        }
        self.advance_n(input, width)?;
        Ok(ch)
    }

    fn peek_char(&mut self, input: &[u8]) -> Result<Option<char>> {
        self.pop_finished();
        let decoded = {
            let slice = self.current_bytes(input);
            if slice.is_empty() {
                None
            } else {
                Some(decode_utf8(slice).map(|(ch, _)| ch))
            }
        };
        match decoded {
            None => Ok(None),
            Some(Ok(ch)) => Ok(Some(ch)),
            Some(Err(())) => Err(self.encoding("malformed UTF-8")),
        }
    }

    fn decode_at_cursor(&mut self, input: &[u8]) -> Result<(char, usize)> {
        self.pop_finished();
        let decoded = {
            let slice = self.current_bytes(input);
            if slice.is_empty() {
                None
            } else {
                Some(decode_utf8(slice))
            }
        };
        match decoded {
            None => Err(self.syntax("unexpected end of input")),
            Some(Ok(value)) => Ok(value),
            Some(Err(())) => Err(self.encoding("malformed UTF-8")),
        }
    }

    fn require_quote(&mut self, input: &[u8]) -> Result<u8> {
        match self.peek_byte(input) {
            Some(b @ (b'"' | b'\'')) => {
                self.advance_one(input)?;
                Ok(b)
            }
            Some(_) => Err(self.syntax("expected a quoted literal")),
            None => Err(self.syntax("unexpected end of input")),
        }
    }

    fn expect_byte(&mut self, input: &[u8], expected: u8) -> Result<()> {
        match self.peek_byte(input) {
            Some(b) if b == expected => self.advance_one(input),
            Some(b) => {
                Err(self.syntax(format!("expected '{}', found U+{b:02X}", expected as char)))
            }
            None => Err(self.syntax("unexpected end of input")),
        }
    }

    fn expect_bytes(&mut self, input: &[u8], pattern: &[u8]) -> Result<()> {
        if self.starts_with(input, pattern) {
            self.advance_n(input, pattern.len())
        } else {
            Err(self.syntax(format!("expected {}", String::from_utf8_lossy(pattern))))
        }
    }

    fn expect_token(&mut self, input: &[u8], token: &[u8]) -> Result<()> {
        if self.starts_with_token(input, token) {
            self.advance_n(input, token.len())
        } else {
            Err(self.syntax(format!("expected {}", String::from_utf8_lossy(token))))
        }
    }

    fn expect_ws(&mut self, input: &[u8]) -> Result<()> {
        if self.skip_ws(input)? == 0 {
            Err(self.syntax("expected whitespace"))
        } else {
            Ok(())
        }
    }

    fn skip_ws(&mut self, input: &[u8]) -> Result<usize> {
        let mut count = 0usize;
        while let Some(b) = self.peek_byte(input) {
            if !is_whitespace(b) {
                break;
            }
            self.advance_one(input)?;
            count += 1;
        }
        Ok(count)
    }

    fn starts_with_token(&mut self, input: &[u8], token: &[u8]) -> bool {
        if !self.starts_with(input, token) {
            return false;
        }
        match self.byte_at_offset(input, token.len()) {
            None => true,
            Some(b) => !is_ascii_name_byte(b),
        }
    }

    fn starts_with(&mut self, input: &[u8], pattern: &[u8]) -> bool {
        self.pop_finished();
        self.current_bytes(input).starts_with(pattern)
    }

    fn byte_at_offset(&mut self, input: &[u8], offset: usize) -> Option<u8> {
        self.pop_finished();
        self.current_bytes(input).get(offset).copied()
    }

    fn peek_byte(&mut self, input: &[u8]) -> Option<u8> {
        if self.halt {
            return None;
        }
        self.pop_finished();
        if self.halt {
            return None;
        }
        self.current_bytes(input).first().copied()
    }

    fn current_bytes<'a>(&'a self, input: &'a [u8]) -> &'a [u8] {
        if let Some(frame) = self.frames.last() {
            let bytes = self.entities[frame.entity_idx].as_bytes();
            let pos = frame.pos.min(bytes.len());
            &bytes[pos..]
        } else {
            let pos = self.pos.min(input.len());
            &input[pos..]
        }
    }

    fn advance_n(&mut self, input: &[u8], n: usize) -> Result<()> {
        for _ in 0..n {
            self.advance_one(input)?;
        }
        Ok(())
    }

    fn advance_one(&mut self, input: &[u8]) -> Result<()> {
        self.pop_finished();
        if self.frames.is_empty() {
            let b = input
                .get(self.pos)
                .copied()
                .ok_or_else(|| self.syntax("unexpected end of input"))?;
            self.observe(b);
            self.pos += 1;
        } else if let Some(frame) = self.frames.last_mut() {
            frame.pos += 1;
        }
        Ok(())
    }

    fn observe(&mut self, byte: u8) {
        let mut position = self.pos_now();
        observe_byte(&mut position, &mut self.cr_pending, byte);
        self.line = position.line;
        self.column = position.column;
    }

    fn pop_finished(&mut self) {
        if self.halt {
            return;
        }
        loop {
            let Some(frame) = self.frames.last() else {
                break;
            };
            let len = self
                .entities
                .get(frame.entity_idx)
                .map(String::len)
                .unwrap_or(0);
            if frame.pos < len {
                break;
            }
            let element_depth = frame.element_depth;
            let markup_depth = frame.markup_depth;
            if self.markup_depth > markup_depth || self.stack.len() != element_depth {
                let position = frame.origin;
                self.halt = true;
                self.boundary_error = Some(Error::new(
                    ErrorKind::Syntax,
                    position,
                    "entity replacement crosses a markup boundary",
                ));
                return;
            }
            self.frames.pop();
            self.entity_depth = self.entity_depth.saturating_sub(1);
            self.open_entities.pop();
        }
    }

    fn force_pop_frame(&mut self) {
        if self.frames.pop().is_some() {
            self.entity_depth = self.entity_depth.saturating_sub(1);
            self.open_entities.pop();
        }
    }
}

fn raw_qname_string(input: &[u8], name: &RawQName) -> String {
    match &name.prefix {
        Some(prefix) => format!("{}:{}", prefix.as_str(input), name.local.as_str(input)),
        None => name.local.as_str(input).to_string(),
    }
}

fn owned_qname(name: &str) -> RawQName {
    if let Some((prefix, local)) = name.split_once(':') {
        if !prefix.is_empty() && !local.is_empty() && !local.contains(':') {
            return RawQName {
                prefix: Some(RawStr::Owned(prefix.to_string())),
                local: RawStr::Owned(local.to_string()),
            };
        }
    }
    RawQName {
        prefix: None,
        local: RawStr::Owned(name.to_string()),
    }
}

fn is_ascii_name_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b':' | b'-' | b'.')
}

fn is_predefined_name(name: &str) -> bool {
    matches!(name, "lt" | "gt" | "amp" | "apos" | "quot")
}

fn predefined(name: &str) -> Option<char> {
    match name {
        "lt" => Some('<'),
        "gt" => Some('>'),
        "amp" => Some('&'),
        "apos" => Some('\''),
        "quot" => Some('"'),
        _ => None,
    }
}

fn hex_or_dec(byte: u8, hex: bool) -> Option<u32> {
    if byte.is_ascii_digit() {
        return Some(u32::from(byte - b'0'));
    }
    if hex {
        match byte {
            b'a'..=b'f' => Some(u32::from(byte - b'a' + 10)),
            b'A'..=b'F' => Some(u32::from(byte - b'A' + 10)),
            _ => None,
        }
    } else {
        None
    }
}

fn qualified(prefix: &Option<String>, local: &str) -> String {
    match prefix {
        Some(prefix) => format!("{prefix}:{local}"),
        None => local.to_string(),
    }
}

fn assume_utf8(bytes: &[u8]) -> &str {
    std::str::from_utf8(bytes).expect("internal: XML text was validated as UTF-8")
}

fn materialize(input: &[u8], raw: RawEvent) -> Event<'_> {
    match raw {
        RawEvent::Start(element) => Event::StartElement(materialize_start(input, element)),
        RawEvent::Empty(element) => Event::EmptyElement(materialize_empty(input, element)),
        RawEvent::End(name) => Event::EndElement(EndElement {
            name: materialize_name(input, name),
        }),
        RawEvent::Text(text) => Event::Text(text.into_cow(input)),
        RawEvent::CData(text) => Event::CData(text.into_cow(input)),
        RawEvent::Comment(text) => Event::Comment(text.into_cow(input)),
        RawEvent::PI { target, data } => Event::PI(ProcessingInstruction {
            target: target.into_cow(input),
            data: data.map(|data| data.into_cow(input)),
        }),
        RawEvent::XmlDecl {
            version,
            encoding,
            standalone,
        } => Event::XmlDecl(XmlDecl {
            version: version.into_cow(input),
            encoding: encoding.map(|encoding| encoding.into_cow(input)),
            standalone,
        }),
        RawEvent::Doctype {
            name,
            system_id,
            public_id,
            has_internal_subset,
        } => Event::Doctype(Doctype {
            name: name.into_cow(input),
            system_id: system_id.map(|value| value.into_cow(input)),
            public_id: public_id.map(|value| value.into_cow(input)),
            has_internal_subset,
        }),
    }
}

fn materialize_start(input: &[u8], element: RawElement) -> StartElement<'_> {
    StartElement {
        name: materialize_name(input, element.name),
        attributes: element
            .attrs
            .into_iter()
            .map(|attr| materialize_attr(input, attr))
            .collect(),
        namespaces: element
            .nss
            .into_iter()
            .map(|ns| materialize_ns(input, ns))
            .collect(),
    }
}

fn materialize_empty(input: &[u8], element: RawElement) -> EmptyElement<'_> {
    EmptyElement {
        name: materialize_name(input, element.name),
        attributes: element
            .attrs
            .into_iter()
            .map(|attr| materialize_attr(input, attr))
            .collect(),
        namespaces: element
            .nss
            .into_iter()
            .map(|ns| materialize_ns(input, ns))
            .collect(),
    }
}

fn materialize_name(input: &[u8], name: ResolvedName) -> QName<'_> {
    QName {
        prefix: name.prefix.map(|prefix| prefix.into_cow(input)),
        local: name.local.into_cow(input),
        namespace: name.namespace,
    }
}

fn materialize_attr(input: &[u8], attr: ResolvedAttr) -> Attribute<'_> {
    Attribute {
        name: materialize_name(input, attr.name),
        value: attr.value.into_cow(input),
    }
}

fn materialize_ns(input: &[u8], ns: RawNs) -> Namespace<'_> {
    Namespace {
        prefix: ns.prefix.map(|prefix| prefix.into_cow(input)),
        uri: ns.uri.into_cow(input),
    }
}
