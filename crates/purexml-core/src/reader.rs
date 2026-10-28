//! Pull parser.
//!
//! [`Reader::read_event`] yields one [`Event`] at a time. [`Reader::from_reader`]
//! pulls a [`BufRead`] source one top-level token at a time; it never opens a
//! file or a socket itself. [`Reader::from_bytes`] and [`Reader::from_vec`]
//! walk a buffer that is already in memory.
//!
//! General-entity limits are enforced incrementally. The expansion-ratio
//! numerator is cumulative replacement-text bytes plus, on every injected
//! default attribute, that attribute's name bytes and its full value bytes.
//! The denominator is original input bytes consumed so far. The ratio is
//! checked on every expansion and every default injection once the numerator
//! exceeds [`EXPANSION_RATIO_FLOOR_BYTES`]. Repeating a large default may
//! require a higher `max_expansion_ratio`.
//!
//! An entity-reference name is an NCName. `&a:b;` is [`ErrorKind::Syntax`]
//! at the colon, not an undefined-entity error.

use std::collections::{HashMap, HashSet};
use std::io::BufRead;
use std::ops::Range;
use std::sync::Arc;

use crate::config::{ParserConfig, EXPANSION_RATIO_FLOOR_BYTES};
use crate::error::{observe_byte, Error, ErrorKind, LimitKind, Position, Result};
use crate::event::{
    Attribute, Doctype, EmptyElement, EndElement, Event, Namespace, ProcessingInstruction, QName,
    StartElement, XmlDecl, XMLNS_NAMESPACE, XML_NAMESPACE,
};
use crate::input::{Framer, IoInput, Poll};
use crate::limit::{decide_limit, LimitFacts, MeasuredLimit};
use crate::name::{
    decode_utf8, is_name_char, is_name_start, is_ncname_char, is_ncname_start, is_pubid_char,
    is_whitespace, is_xml_char, utf8_invalid_index,
};

/// Hard cap on ELEMENT content-model nesting, applied together with `max_depth`.
///
/// The walk is an explicit heap stack, so thread-stack use does not grow with
/// the model. The ceiling still stops a caller who raises `max_depth` from
/// building an arbitrarily deep model.
const MODEL_DEPTH_CEILING: usize = 1024;

/// Streaming XML reader.
///
/// Construct with [`Reader::from_bytes`], [`Reader::from_str`], or
/// [`Reader::from_reader`]. Pull events with [`Reader::read_event`].
///
/// `S` is the input holder: `&[u8]` or `Vec<u8>` for a whole document, or
/// [`IoInput`] for a streaming [`BufRead`]. Events borrow from that input
/// when no normalization or entity expansion was required.
///
/// After any error, later [`Reader::read_event`] calls return the same error.
///
/// Entity-reference names are NCNames. `&a:b;` fails with
/// [`ErrorKind::Syntax`] because of the colon. A well-formed name with no
/// internal declaration, such as `&missing;`, is also [`ErrorKind::Syntax`],
/// and that message says `undefined entity`.
///
/// The 0.x API is unstable.
pub struct Reader<S> {
    input: S,
    engine: Engine,
    poison: Option<Error>,
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
            poison: None,
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
    /// Parse an owned buffer with the same rules as [`Reader::from_bytes`].
    ///
    /// This does not stream. Use [`Reader::from_reader`] to bound memory by
    /// the token cap instead of the document length.
    pub fn from_vec(input: Vec<u8>) -> Self {
        Self {
            input,
            engine: Engine::new(ParserConfig::default()),
            poison: None,
        }
    }
}

impl<R: BufRead> Reader<IoInput<R>> {
    /// Stream `reader` one top-level token at a time.
    ///
    /// The `Result` is always `Ok`. I/O errors and the total-input limit are
    /// reported from [`Reader::read_event`], so existing `?` / `unwrap` call
    /// sites keep compiling. The window stays near `max_token_len` plus one
    /// read chunk, independent of the document length.
    ///
    /// `ErrorKind::Interrupted` is retried. `ErrorKind::WouldBlock` becomes
    /// [`ErrorKind::Io`] and the reader stays failed: every later
    /// `read_event` returns that error. The position of an I/O error is the
    /// number of bytes successfully read so far (line and column match that
    /// offset). `Ok(0)` is end of input.
    ///
    /// `max_total_input` is positional on every entry point. The error is
    /// reported only when parsing needs the byte at that offset, with
    /// `actual = limit + 1` and the position on that byte. Tokens that ended
    /// before it are yielded first. An earlier error wins.
    pub fn from_reader(reader: R) -> Result<Self> {
        Self::from_reader_with_config(reader, ParserConfig::default())
    }

    /// [`Reader::from_reader`] with an explicit limit set.
    pub fn from_reader_with_config(reader: R, config: ParserConfig) -> Result<Self> {
        Ok(Self {
            input: IoInput::new(reader),
            engine: Engine::new(config),
            poison: None,
        })
    }

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
    /// bytes the parser has consumed from the document, not inside a
    /// replacement text.
    pub fn position(&self) -> Position {
        self.engine.pos_now()
    }

    /// Capacity of the streaming window. For tests; not a stable metric.
    #[doc(hidden)]
    pub fn buffered_capacity(&self) -> usize {
        self.input.capacity()
    }

    /// Pull the next event.
    ///
    /// `Ok(None)` is a clean end of input after the root element.
    /// The returned event borrows `self`; drop it before the next call, or
    /// take ownership with [`crate::event::XmlText::into_owned`].
    ///
    /// Text is not split to fit a read chunk: the event sequence matches
    /// [`Reader::from_bytes`] on the same bytes. After an error, this returns
    /// that same error.
    pub fn read_event(&mut self) -> Result<Option<Event<'_>>> {
        if let Some(err) = self.poison.clone() {
            return Err(err);
        }
        let raw = match self.pull_raw() {
            Ok(raw) => raw,
            Err(err) => {
                self.poison = Some(err.clone());
                return Err(err);
            }
        };
        let window = self.input.window();
        Ok(raw.map(|raw| materialize(window, raw)))
    }

    fn pull_raw(&mut self) -> Result<Option<RawEvent>> {
        let mut parked = None;
        loop {
            let probing = if self.engine.frames.is_empty() && self.engine.phase != Phase::Done {
                let content = matches!(self.engine.phase, Phase::Content);
                let doc_start = !self.engine.started;
                let mut pos = self.engine.pos;
                self.input
                    .prepare(&mut pos, doc_start, content, &self.engine.config)?;
                self.engine.pos = pos;
                self.engine.base = self.input.base();
                self.engine.end_is_eof = self.input.end_is_eof();
                self.input.view_end().is_some()
            } else {
                false
            };
            let raw = {
                let window = self.input.window();
                self.engine.next_raw(window)
            };
            if self.engine.need_frame {
                // An empty expansion, or a prolog/epilog whitespace run that
                // filled the window, produced no event. Frame from the cursor
                // and continue. Do not rewind: bytes already read stay read.
                self.engine.need_frame = false;
                let sig = (
                    self.engine.base,
                    self.engine.pos,
                    self.input.buffered_len(),
                    self.input.saw_eof(),
                );
                if parked == Some(sig) {
                    return Err(self.engine.token_limit(self.input.window()));
                }
                parked = Some(sig);
                continue;
            }
            let raw = raw?;
            if probing && self.engine.frames.is_empty() && self.engine.token_started {
                let origin = self
                    .engine
                    .token_origin
                    .offset
                    .saturating_sub(self.engine.base) as usize;
                let consumed = self.engine.pos.saturating_sub(origin) as u64;
                if consumed > self.engine.config.max_token_len {
                    return Err(self.engine.token_limit(self.input.window()));
                }
            }
            return Ok(raw);
        }
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

    /// Byte length of the in-memory document. For tests; streaming readers
    /// report the window capacity instead.
    #[doc(hidden)]
    pub fn buffered_capacity(&self) -> usize {
        self.input.as_ref().len()
    }

    /// Pull the next event.
    ///
    /// `Ok(None)` is a clean end of input after the root element.
    /// The returned event borrows `self`; drop it before the next call, or
    /// take ownership with [`crate::event::XmlText::into_owned`].
    ///
    /// After an error, this returns that same error.
    #[inline]
    pub fn read_event(&mut self) -> Result<Option<Event<'_>>> {
        if self.poison.is_some() {
            return self.poisoned();
        }
        // The buffer is the whole document. There is no framer probe.
        // `max_token_len` and `max_total_input` are the precomputed
        // `main_stop` checked as main-input bytes are consumed.
        let raw = {
            self.engine.end_is_eof = true;
            let input = self.input.as_ref();
            self.engine.next_raw(input)
        };
        if self.engine.need_frame {
            return self.read_slice_after_reframe();
        }
        self.finish_slice(raw)
    }

    #[inline]
    fn finish_slice(&mut self, raw: Result<Option<RawEvent>>) -> Result<Option<Event<'_>>> {
        match raw {
            Ok(raw) => {
                let input = self.input.as_ref();
                Ok(raw.map(|raw| materialize(input, raw)))
            }
            Err(err) => {
                self.poison = Some(err.clone());
                Err(err)
            }
        }
    }

    #[cold]
    #[inline(never)]
    fn poisoned(&mut self) -> Result<Option<Event<'_>>> {
        Err(self.poison.clone().expect("poison"))
    }

    /// The previous `next_raw` finished an empty expansion and asked for a
    /// new token start. Rare; kept out of the slice hot path.
    #[cold]
    #[inline(never)]
    fn read_slice_after_reframe(&mut self) -> Result<Option<Event<'_>>> {
        let mut parked = Some(self.engine.pos);
        loop {
            self.engine.need_frame = false;
            let raw = {
                self.engine.end_is_eof = true;
                let input = self.input.as_ref();
                self.engine.next_raw(input)
            };
            if self.engine.need_frame {
                if parked == Some(self.engine.pos) {
                    let err = {
                        let input = self.input.as_ref();
                        self.engine.token_limit(input)
                    };
                    self.poison = Some(err.clone());
                    return Err(err);
                }
                parked = Some(self.engine.pos);
                continue;
            }
            return self.finish_slice(raw);
        }
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
    namespace: Option<Arc<str>>,
    depth: usize,
}

struct NsDecl {
    depth: usize,
    /// `None` is the default namespace.
    prefix: Option<Arc<str>>,
}

struct AttDef {
    name: String,
    cdata: bool,
    /// `None` for `#REQUIRED` and `#IMPLIED`.
    ///
    /// Validated once when the declaration is parsed, then shared. Applying
    /// the default clones the [`Arc`] and does not scan the bytes again.
    /// Each injection still adds the full value length to the expansion-ratio
    /// numerator.
    default_value: Option<Arc<str>>,
}

#[derive(Clone, Copy)]
struct ContentGroup {
    allow_mixed: bool,
    sep: Option<u8>,
    saw_cp: bool,
}

#[derive(Default)]
struct AttList {
    /// Declaration order. The first declaration of a name wins.
    order: Vec<AttDef>,
    index: HashMap<String, usize>,
}

#[derive(Clone)]
enum RawStr {
    Range(Range<usize>),
    Owned(String),
    /// DTD default shared across every element that inherits it.
    Shared(Arc<str>),
}

impl RawStr {
    fn as_str<'a>(&'a self, input: &'a [u8]) -> &'a str {
        match self {
            Self::Range(range) => assume_utf8(&input[range.start..range.end]),
            Self::Owned(text) => text.as_str(),
            Self::Shared(text) => text.as_ref(),
        }
    }

    fn byte_len(&self) -> usize {
        match self {
            Self::Range(range) => range.end.saturating_sub(range.start),
            Self::Owned(text) => text.len(),
            Self::Shared(text) => text.len(),
        }
    }

    fn into_string(self, input: &[u8]) -> String {
        match self {
            Self::Range(range) => assume_utf8(&input[range.start..range.end]).to_string(),
            Self::Owned(text) => text,
            Self::Shared(text) => text.to_string(),
        }
    }

    fn into_text(self, input: &[u8]) -> crate::event::XmlText<'_> {
        match self {
            Self::Range(range) => {
                crate::event::XmlText::borrowed(assume_utf8(&input[range.start..range.end]))
            }
            Self::Owned(text) => crate::event::XmlText::shared(Arc::from(text)),
            Self::Shared(text) => crate::event::XmlText::shared(text),
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
    namespace: Option<Arc<str>>,
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
    /// Absolute offset of `input[0]`. Always 0 for a slice.
    base: u64,
    pos: usize,
    /// When false, the end of the window is not the end of the document.
    end_is_eof: bool,
    /// Window index of the first internal-subset byte, after `[`.
    subset_start: Option<usize>,
    /// Position of the top-level token currently being parsed.
    ///
    /// Set in [`Self::begin_token`] on the first byte of the token. A UTF-8
    /// BOM and prolog or epilog whitespace are consumed first and are not
    /// part of this offset. Content whitespace is the text token, so it is.
    token_origin: Position,
    /// `false` while a BOM or prolog/epilog whitespace is still ahead of the
    /// token. Total input still applies; the token cap does not.
    token_started: bool,
    /// `cr_pending` at `token_origin`, so a later byte position can be scanned.
    token_cr: bool,
    /// Main-input index where the current `next_raw` started parsing.
    frame_anchor: usize,
    /// The engine finished one main-input token and the next one is not framed.
    need_frame: bool,
    /// First main-input index that must not be consumed for this token.
    /// The minimum of the token cap, `max_total_input`, and, while a subset
    /// is open, the subset cap. The hot path compares against this once.
    main_stop: usize,
    /// First main-input index at absolute offset `max_total_input`.
    total_stop: usize,
    /// First main-input index past `max_token_len` from `token_origin`.
    token_stop: usize,
    /// Length of the window passed to the current `next_raw`.
    ///
    /// Lets [`Self::syntax`] see whether the cursor is a buffered cap byte
    /// without taking the slice again.
    window_len: usize,
    line: u64,
    column: u64,
    cr_pending: bool,
    started: bool,
    at_document_start: bool,
    seen_doctype: bool,
    phase: Phase,
    stack: Vec<OpenElement>,
    default_ns: Vec<Arc<str>>,
    prefix_ns: HashMap<Arc<str>, Vec<Arc<str>>>,
    ns_decls: Vec<NsDecl>,
    interner: HashSet<Arc<str>>,
    attlists: HashMap<String, AttList>,
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
    /// Entity replacement bytes. This is the absolute [`LimitKind::ExpandedBytes`] cap.
    expanded_bytes: u64,
    /// Expansion-ratio numerator: entity replacement bytes, plus the name
    /// bytes and the full value bytes of every injected default attribute.
    /// The value is one shared [`Arc`], and each injection still adds its
    /// full length. Defaults do not move `expanded_bytes` or
    /// `expansion_count`.
    ratio_bytes: u64,
}

impl Engine {
    fn new(config: ParserConfig) -> Self {
        Self {
            config,
            base: 0,
            pos: 0,
            end_is_eof: true,
            subset_start: None,
            token_origin: Position::start(),
            token_started: false,
            token_cr: false,
            frame_anchor: 0,
            need_frame: false,
            main_stop: usize::MAX,
            total_stop: usize::MAX,
            token_stop: usize::MAX,
            window_len: 0,
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
            ratio_bytes: 0,
        }
        .with_builtin_namespaces()
    }

    fn with_builtin_namespaces(mut self) -> Self {
        let prefix = self.intern("xml");
        let uri = self.intern(XML_NAMESPACE);
        self.prefix_ns
            .insert(Arc::clone(&prefix), vec![Arc::clone(&uri)]);
        self.ns_decls.push(NsDecl {
            depth: 0,
            prefix: Some(prefix),
        });
        let _ = self.intern(XMLNS_NAMESPACE);
        self
    }

    fn intern(&mut self, text: &str) -> Arc<str> {
        if let Some(existing) = self.interner.get(text) {
            return Arc::clone(existing);
        }
        let interned: Arc<str> = Arc::from(text);
        self.interner.insert(Arc::clone(&interned));
        interned
    }

    fn pos_now(&self) -> Position {
        Position {
            line: self.line,
            column: self.column,
            offset: self.base + self.pos as u64,
        }
    }

    #[cold]
    #[inline(never)]
    fn token_limit(&self, input: &[u8]) -> Error {
        self.breach_at_cursor(input)
    }

    /// The framer, started at `token_origin`, cuts this window on `max_token_len`.
    ///
    /// A `TokenLength` report whose cut is somewhere else means the scanner
    /// and the engine do not agree on where the token ends.
    fn token_limit_matches_scanner(&self, input: &[u8]) -> bool {
        let start = self.token_origin.offset.saturating_sub(self.base) as usize;
        if start > input.len() {
            return false;
        }
        let content = matches!(self.phase, Phase::Content);
        let doc_start = start == 0 && input.starts_with(&[0xEF, 0xBB, 0xBF]);
        let mut framer = Framer::start(start, doc_start, content);
        let cap_end = start
            .saturating_add(self.config.max_token_len as usize)
            .saturating_add(1);
        match framer.poll(input, self.end_is_eof, &self.config) {
            Poll::Probe { end } => end == cap_end,
            Poll::NeedMore | Poll::Ready { .. } => false,
        }
    }

    fn scanner_poll_label(&self, input: &[u8]) -> String {
        let start = self.token_origin.offset.saturating_sub(self.base) as usize;
        if start > input.len() {
            return format!("start {start} past window");
        }
        let content = matches!(self.phase, Phase::Content);
        let doc_start = start == 0 && input.starts_with(&[0xEF, 0xBB, 0xBF]);
        let mut framer = Framer::start(start, doc_start, content);
        let shown: String = input.iter().take(16).map(|b| format!("{b:02x}")).collect();
        match framer.poll(input, self.end_is_eof, &self.config) {
            Poll::Probe { end } => format!("Probe end={end} content={content} [{shown}]"),
            Poll::NeedMore => format!("NeedMore content={content} [{shown}]"),
            Poll::Ready { eof } => format!("Ready eof={eof} content={content} [{shown}]"),
        }
    }

    /// Cursor sitting on the byte `index` bytes after `token_origin`.
    ///
    /// Index 0 is the first byte of the token. Scanning `index` bytes leaves
    /// the position on the next byte, which is the one that makes a raw-byte
    /// cap `index` overflow.
    fn position_on_token_byte(&self, input: &[u8], index: u64) -> Position {
        let start = self.token_origin.offset.saturating_sub(self.base) as usize;
        if start > input.len() {
            return self.token_origin;
        }
        let mut pos = self.token_origin;
        let mut cr = self.token_cr;
        let take = (index as usize).min(input.len().saturating_sub(start));
        for &byte in &input[start..start + take] {
            observe_byte(&mut pos, &mut cr, byte);
        }
        pos
    }

    /// The cursor is on a main-input byte that a byte-counted cap forbids.
    fn cursor_limited(&self, input: &[u8]) -> bool {
        self.frames.is_empty() && self.pos >= self.main_stop && self.pos < input.len()
    }

    #[cold]
    #[inline(never)]
    fn end_input(&self, input: &[u8], message: &str) -> Error {
        // A name or text probe also ends the window. That is not a token cap.
        if self.cursor_limited(input) {
            return self.breach_at_cursor(input);
        }
        self.syntax(message)
    }

    fn cut_utf8(&self, rest: &[u8]) -> bool {
        self.frames.is_empty() && !self.end_is_eof && crate::name::utf8_incomplete(rest)
    }

    fn err_pos(&self) -> Position {
        self.frames
            .first()
            .map(|frame| frame.origin)
            .unwrap_or_else(|| self.pos_now())
    }

    /// True when the next main-input byte is already inside an armed cap.
    /// A Syntax or Encoding decision would have to read that byte.
    fn on_armed_byte(&self) -> bool {
        self.frames.is_empty() && self.pos >= self.main_stop && self.pos < self.window_len
    }

    #[cold]
    #[inline(never)]
    fn syntax(&self, message: impl Into<String>) -> Error {
        if self.on_armed_byte() {
            return self.breach_without_window();
        }
        Error::new(ErrorKind::Syntax, self.err_pos(), message)
    }

    #[cold]
    #[inline(never)]
    fn encoding(&self, message: impl Into<String>) -> Error {
        if self.on_armed_byte() {
            return self.breach_without_window();
        }
        Error::new(ErrorKind::Encoding, self.err_pos(), message)
    }

    /// Malformed UTF-8 at `lead`. `proven_index` is the byte, relative to the
    /// lead, that shows the sequence is illegal.
    ///
    /// [`decide_limit`] sees that proving offset. A byte cap at or before it
    /// wins and is reported on the cap byte. Otherwise the error stays
    /// [`ErrorKind::Encoding`] on the lead.
    #[cold]
    #[inline(never)]
    fn utf8_against_limit(&self, input: &[u8], lead: u64, proven_index: usize) -> Error {
        let needed = lead.saturating_add(proven_index as u64);
        let mut facts = self.limit_facts(None);
        facts.cursor = lead;
        facts.encoding_needed = Some(needed);
        if let Some(decision) = decide_limit(&facts) {
            return self.error_from_decision(input, decision, self.pos_now());
        }
        let at = if self.base + self.pos as u64 == lead {
            self.pos_now()
        } else {
            let index = lead.saturating_sub(self.token_origin.offset);
            self.position_on_token_byte(input, index)
        };
        Error::new(ErrorKind::Encoding, at, "malformed UTF-8")
    }

    /// [`decide_limit`] at the cursor. Used when the window slice is not in hand.
    /// The cursor is the offending byte, so the reported position is `pos_now`.
    #[cold]
    #[inline(never)]
    fn breach_without_window(&self) -> Error {
        let facts = self.limit_facts(None);
        let here = self.pos_now();
        let decision = decide_limit(&facts).unwrap_or(crate::limit::LimitDecision {
            kind: LimitKind::TokenLength,
            limit: self.config.max_token_len,
            actual: self.config.max_token_len.saturating_add(1),
            offset: here.offset,
        });
        let position = if decision.offset == here.offset {
            here
        } else {
            Position {
                line: here.line,
                column: here.column,
                offset: decision.offset,
            }
        };
        self.limit_at(decision.kind, decision.limit, decision.actual, position)
    }

    fn external_at(&self, position: Position, message: impl Into<String>) -> Error {
        Error::new(ErrorKind::ExternalEntityForbidden, position, message)
    }

    #[cold]
    #[inline(never)]
    fn limit(&self, input: &[u8], kind: LimitKind, limit: u64, actual: u64) -> Error {
        let pos = self.err_pos();
        // Char-counted limits report the cursor after the character. The
        // byte that was read is the one before that cursor.
        let needed = match kind {
            LimitKind::NameLength | LimitKind::TextLength | LimitKind::AttrValueLength => {
                pos.offset.saturating_sub(1)
            }
            _ => pos.offset,
        };
        self.measured_limit(input, kind, limit, actual, pos, needed)
    }

    /// Every security-limit error goes through [`decide_limit`].
    ///
    /// `hint` is the position the caller would report for `kind`. A byte cap
    /// whose offending byte is earlier, or tied and higher priority, replaces
    /// both the kind and the position.
    #[cold]
    #[inline(never)]
    fn measured_limit(
        &self,
        input: &[u8],
        kind: LimitKind,
        limit: u64,
        actual: u64,
        hint: Position,
        needed: u64,
    ) -> Error {
        let measured = MeasuredLimit {
            kind,
            limit,
            actual,
            offset: hint.offset,
            needed,
        };
        let facts = self.limit_facts(Some(measured));
        let decision = decide_limit(&facts).unwrap_or(crate::limit::LimitDecision {
            kind,
            limit,
            actual,
            offset: hint.offset,
        });
        self.error_from_decision(input, decision, hint)
    }

    fn limit_facts(&self, measured: Option<MeasuredLimit>) -> LimitFacts<'_> {
        let on_main = self.frames.is_empty();
        // Before the token starts, keep a token origin so TotalInput is still
        // decided here, but put the token cap out of reach. The real origin is
        // the first token byte, after the BOM and prolog/epilog whitespace.
        let token_origin = if !on_main {
            None
        } else if self.token_started {
            Some(self.token_origin.offset)
        } else {
            Some(u64::MAX)
        };
        LimitFacts {
            config: &self.config,
            cursor: self.base + self.pos as u64,
            token_origin,
            subset_start: if on_main {
                self.subset_start.map(|start| self.base + start as u64)
            } else {
                None
            },
            measured,
            encoding_needed: None,
        }
    }

    /// The cursor is on the byte a byte-counted cap forbids.
    #[cold]
    #[inline(never)]
    fn breach_at_cursor(&self, input: &[u8]) -> Error {
        let facts = self.limit_facts(None);
        let Some(decision) = decide_limit(&facts) else {
            debug_assert!(
                false,
                "breach_at_cursor with no exceeded cap (pos {}, stop {})",
                self.pos, self.main_stop
            );
            return Error::new(ErrorKind::Syntax, self.pos_now(), "internal limit dispatch");
        };
        if decision.kind == LimitKind::TokenLength {
            debug_assert!(
                self.token_limit_matches_scanner(input),
                "TokenLength: scanner and engine disagree (origin {:?}, pos {}, window {}, max {}, {})",
                self.token_origin,
                self.pos,
                input.len(),
                self.config.max_token_len,
                self.scanner_poll_label(input)
            );
        }
        self.error_from_decision(input, decision, self.pos_now())
    }

    fn error_from_decision(
        &self,
        input: &[u8],
        decision: crate::limit::LimitDecision,
        hint: Position,
    ) -> Error {
        let position = if hint.offset == decision.offset {
            hint
        } else if self.base + self.pos as u64 == decision.offset {
            self.pos_now()
        } else {
            let index = decision.offset.saturating_sub(self.token_origin.offset);
            self.position_on_token_byte(input, index)
        };
        self.limit_at(decision.kind, decision.limit, decision.actual, position)
    }

    #[cold]
    #[inline(never)]
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
        self.window_len = input.len();
        if self.frames.is_empty() {
            // The token cap starts at the first token byte, not at a BOM or
            // at prolog/epilog whitespace. TotalInput stays armed.
            self.suspend_token_limit();
        }
        if let Some(err) = self.boundary_error.take() {
            return Err(err);
        }
        self.ensure_started(input)?;
        if self.frames.is_empty() {
            self.frame_anchor = self.pos;
        }
        let result = match self.phase {
            Phase::Done => Ok(None),
            Phase::Prolog => self.next_prolog(input).map(Some),
            Phase::Content => {
                if self.frames.is_empty() {
                    self.begin_token(input)?;
                }
                self.next_content(input).map(Some)
            }
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
        self.consume_bom(input)
    }

    fn consume_bom(&mut self, input: &[u8]) -> Result<()> {
        if input.starts_with(&[0xEF, 0xBB, 0xBF]) {
            // The BOM is not a token byte. A cap that falls inside it is
            // TotalInput, at that raw offset. Column stays put.
            if self.total_stop < 3 && self.total_stop < input.len() {
                self.pos = self.total_stop;
                return Err(self.breach_at_cursor(input));
            }
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
                self.begin_token(input)?;
                return self.parse_xml_decl(input);
            }
        }
        self.skip_misc(input)?;
        self.begin_token(input)?;
        match self.peek_byte(input) {
            None => Err(self.end_input(input, "document has no root element")),
            Some(b'<') => self.parse_prolog_markup(input),
            Some(_) => {
                let _ = self.decode_at_cursor(input)?;
                Err(self.syntax("expected markup in the prolog"))
            }
        }
    }

    fn parse_prolog_markup(&mut self, input: &[u8]) -> Result<RawEvent> {
        self.breach_if_cut(input, &[b"<!--", b"<?", b"<!DOCTYPE", b"</", b"<!"])?;
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
        self.skip_misc(input)?;
        if self.peek_byte(input).is_none() && self.frames.is_empty() {
            if !self.end_is_eof {
                return Err(self.end_input(input, "unexpected end of input in the epilog"));
            }
            self.phase = Phase::Done;
            return Ok(None);
        }
        self.begin_token(input)?;
        self.breach_if_cut(input, &[b"<!--", b"<?"])?;
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
            // Empty text (an entity that expands to nothing) does not produce
            // an event, so this loop steps onto the next main-input token.
            // Token start is recorded in `begin_token` on the next `next_raw`.
            // Ask for a new frame so the next token is not counted as part of
            // this one, and so a streaming window is not read as a one-byte
            // lookahead.
            if self.frames.is_empty() && self.pos != self.frame_anchor {
                self.need_frame = true;
                return Err(self.syntax("reframing"));
            }
            self.pop_finished();
            match self.peek_byte(input) {
                None => {
                    if self.halt || self.boundary_error.is_some() {
                        return Err(self.boundary_error.take().unwrap_or_else(|| {
                            self.syntax("entity replacement crosses a markup boundary")
                        }));
                    }
                    if self.frames.is_empty() {
                        return Err(self.end_input(input, "unclosed element"));
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
        self.breach_if_cut(
            input,
            &[b"<!--", b"<?", b"<![CDATA[", b"</", b"<!DOCTYPE", b"<!"],
        )?;
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
            if self.prefix_cut(input, b"?>")
                || self.prefix_cut(input, b"encoding")
                || self.prefix_cut(input, b"standalone")
            {
                return Err(self.land_and_breach(input));
            }
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
                if self.cursor_limited(input) {
                    return Err(self.breach_at_cursor(input));
                }
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
            } else if self.prefix_cut(input, b"encoding")
                || self.prefix_cut(input, b"standalone")
                || self.prefix_cut(input, b"?>")
            {
                return Err(self.land_and_breach(input));
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
        // A document-type name is a QName: zero or one colon, both sides NCName.
        let name = flatten_qname(input, self.read_qname(input)?);
        self.skip_ws(input)?;
        let mut system_id = None;
        let mut public_id = None;
        self.breach_if_cut(input, &[b"SYSTEM", b"PUBLIC"])?;
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
            self.subset_start = Some(self.pos);
            self.rearm_main_stop();
            let subset = self.parse_internal_subset(input);
            self.subset_start = None;
            self.rearm_main_stop();
            subset?;
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
                None => return Err(self.end_input(input, "unterminated internal subset")),
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
            // A probe can end on the byte that crosses `max_internal_subset_len`
            // before a declaration keyword is complete (`<!ENT` is not `<!ENTITY`).
            // Count those bytes so the subset cap wins over "invalid markup".
            if self.subset_introducer_incomplete(input) {
                while self.peek_byte(input).is_some() {
                    self.advance_one(input)?;
                }
                return Err(self.end_input(input, "unterminated internal subset"));
            }
            return Err(self.syntax("invalid markup in the internal subset"));
        }
    }

    /// True when the bytes at the cursor are a proper prefix of subset markup.
    /// The window ended before the keyword could be recognized.
    fn subset_introducer_incomplete(&mut self, input: &[u8]) -> bool {
        const INTROS: &[&[u8]] = &[
            b"<!--",
            b"<?",
            b"<!ENTITY",
            b"<!ELEMENT",
            b"<!ATTLIST",
            b"<!NOTATION",
            b"<![",
        ];
        let rest = self.current_bytes(input);
        !rest.is_empty()
            && INTROS
                .iter()
                .any(|intro| intro.starts_with(rest) && rest.len() < intro.len())
    }

    fn parse_entity_decl(&mut self, input: &[u8]) -> Result<()> {
        self.expect_bytes(input, b"<!ENTITY")?;
        self.expect_ws(input)?;
        if self.peek_byte(input) == Some(b'%') {
            let at = self.err_pos();
            self.advance_one(input)?;
            self.expect_ws(input)?;
            let name = self.read_ncname_string(input, "entity name")?;
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
        let name = self.read_ncname_string(input, "entity name")?;
        self.expect_ws(input)?;
        if self.starts_with_token(input, b"SYSTEM") || self.starts_with_token(input, b"PUBLIC") {
            // A bad NDATA notation name is Syntax. A well-formed external
            // declaration is still forbidden and is never loaded.
            self.scan_external_notation(input)?;
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
                return Err(self.end_input(input, "unterminated entity value"));
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
                    let name = self.read_name_string(input, true)?;
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
            Err(()) => {
                let rest = self.current_bytes(input);
                if let Some(proven) = utf8_invalid_index(&rest[1..]) {
                    let lead = self.base + self.pos as u64 + 1;
                    Err(self.utf8_against_limit(input, lead, proven))
                } else {
                    Err(self.encoding("malformed UTF-8"))
                }
            }
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
        self.breach_if_cut(input, &[b"EMPTY", b"ANY"])?;
        if self.starts_with_token(input, b"EMPTY") {
            self.advance_n(input, "EMPTY".len())?;
            return Ok(());
        }
        if self.starts_with_token(input, b"ANY") {
            self.advance_n(input, "ANY".len())?;
            return Ok(());
        }
        self.parse_content_group(input)
    }

    /// Content-model parentheses are attacker-controlled. Nesting is an
    /// explicit heap stack, capped by `max_depth` and [`MODEL_DEPTH_CEILING`],
    /// so thread-stack use stays flat in every build profile.
    fn parse_content_group(&mut self, input: &[u8]) -> Result<()> {
        let limit = self.config.max_depth.min(MODEL_DEPTH_CEILING);
        let mut stack = Vec::new();
        self.open_content_group(input, &mut stack, 1, true, limit)?;
        while let Some(frame) = stack.last().copied() {
            if !frame.saw_cp {
                self.skip_ws(input)?;
                self.reject_dtd_pe(input)?;
                if self.starts_with(input, b"#PCDATA") {
                    if !frame.allow_mixed {
                        return Err(self.syntax(
                            "mixed content is only allowed as the top-level content specification",
                        ));
                    }
                    self.parse_mixed(input)?;
                    stack.pop();
                    if let Some(parent) = stack.last_mut() {
                        parent.saw_cp = true;
                    }
                    continue;
                }
                self.push_content_particle(input, &mut stack, limit)?;
                continue;
            }
            self.skip_ws(input)?;
            self.reject_dtd_pe(input)?;
            if let Some(sep) = self
                .peek_byte(input)
                .filter(|byte| matches!(byte, b'|' | b','))
            {
                if frame.sep.is_none() || frame.sep == Some(sep) {
                    stack.last_mut().expect("open content group").sep = Some(sep);
                    self.advance_one(input)?;
                    self.skip_ws(input)?;
                    self.reject_dtd_pe(input)?;
                    self.push_content_particle(input, &mut stack, limit)?;
                    continue;
                }
            }
            self.expect_byte(input, b')')?;
            self.skip_occurrence(input)?;
            stack.pop();
            if let Some(parent) = stack.last_mut() {
                parent.saw_cp = true;
            }
        }
        Ok(())
    }

    fn open_content_group(
        &mut self,
        input: &[u8],
        stack: &mut Vec<ContentGroup>,
        depth: usize,
        allow_mixed: bool,
        limit: usize,
    ) -> Result<()> {
        if depth > limit {
            return Err(self.limit(input, LimitKind::ElementDepth, limit as u64, depth as u64));
        }
        self.expect_byte(input, b'(')?;
        stack.push(ContentGroup {
            allow_mixed,
            sep: None,
            saw_cp: false,
        });
        Ok(())
    }

    /// One `cp`: a name, or a nested group pushed onto `stack`.
    fn push_content_particle(
        &mut self,
        input: &[u8],
        stack: &mut Vec<ContentGroup>,
        limit: usize,
    ) -> Result<()> {
        self.reject_dtd_pe(input)?;
        if self.peek_byte(input) == Some(b'(') {
            let depth = stack.len() + 1;
            self.open_content_group(input, stack, depth, false, limit)?;
            return Ok(());
        }
        let _name = self.read_name(input, false)?;
        self.skip_occurrence(input)?;
        stack.last_mut().expect("open content group").saw_cp = true;
        Ok(())
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
                None => {
                    return Err(self.end_input(input, "unterminated attribute-list declaration"))
                }
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
        // Attribute names in the document are QNames. A default such as `a:`
        // must fail here, not only when someone writes the attribute explicitly.
        let qname = self.read_qname(input)?;
        let name = raw_qname_string(input, &qname);
        self.expect_ws(input)?;
        self.reject_dtd_pe(input)?;
        let cdata = self.parse_att_type(input)?;
        self.expect_ws(input)?;
        self.reject_dtd_pe(input)?;
        let default_value = self.parse_default_decl(input, cdata)?;
        let list = self.attlists.entry(element.to_string()).or_default();
        if list.index.contains_key(&name) {
            return Ok(());
        }
        list.index.insert(name.clone(), list.order.len());
        list.order.push(AttDef {
            name,
            cdata,
            default_value,
        });
        Ok(())
    }

    fn parse_att_type(&mut self, input: &[u8]) -> Result<bool> {
        // A byte cap inside the type keyword is that cap, before "invalid type".
        for token in [
            b"CDATA".as_slice(),
            b"IDREFS",
            b"IDREF",
            b"ID",
            b"ENTITY",
            b"ENTITIES",
            b"NMTOKENS",
            b"NMTOKEN",
            b"NOTATION",
        ] {
            if self.prefix_cut(input, token) {
                return Err(self.land_and_breach(input));
            }
        }
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
            let _name = self.read_ncname_decl(input, "notation name")?;
            loop {
                self.skip_ws(input)?;
                self.reject_dtd_pe(input)?;
                if self.peek_byte(input) != Some(b'|') {
                    break;
                }
                self.advance_one(input)?;
                self.skip_ws(input)?;
                self.reject_dtd_pe(input)?;
                let _name = self.read_ncname_decl(input, "notation name")?;
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

    fn parse_default_decl(&mut self, input: &[u8], cdata: bool) -> Result<Option<Arc<str>>> {
        self.breach_if_cut(input, &[b"#REQUIRED", b"#IMPLIED", b"#FIXED"])?;
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
        // One UTF-8 check at the declaration. Later elements clone the `Arc`.
        if cdata {
            if let RawStr::Range(range) = raw {
                let bytes = &input[range.start..range.end];
                let text = std::str::from_utf8(bytes)
                    .map_err(|_| self.encoding("malformed UTF-8 in attribute default"))?;
                return Ok(Some(Arc::from(text)));
            }
        }
        let text = raw.into_string(input);
        let text = if cdata {
            text
        } else {
            crate::wf::collapse_spaces(&text)
        };
        Ok(Some(Arc::from(text)))
    }

    fn parse_notation_decl(&mut self, input: &[u8]) -> Result<()> {
        self.expect_bytes(input, b"<!NOTATION")?;
        self.expect_ws(input)?;
        self.reject_dtd_pe(input)?;
        let _name = self.read_ncname_decl(input, "notation name")?;
        self.expect_ws(input)?;
        self.reject_dtd_pe(input)?;
        self.breach_if_cut(input, &[b"SYSTEM", b"PUBLIC"])?;
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
        while let Some(ch) = self.peek_char(input)? {
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
    ) -> Result<()> {
        let pending = {
            let Some(list) = self.attlists.get(element) else {
                return Ok(());
            };
            let mut explicit = HashSet::with_capacity(attrs.len());
            for (qname, _) in attrs.iter() {
                explicit.insert(raw_qname_string(input, qname));
            }
            list.order
                .iter()
                .enumerate()
                .filter(|(_, def)| def.default_value.is_some() && !explicit.contains(&def.name))
                .map(|(index, _)| index)
                .collect::<Vec<_>>()
        };
        for index in pending {
            let (name, value) = {
                let def = &self.attlists.get(element).expect("attribute list").order[index];
                let value = Arc::clone(def.default_value.as_ref().expect("default value"));
                (def.name.clone(), value)
            };
            // Same rule as entity expansion: the full value counts on every
            // injection, even though the bytes live in one shared `Arc`.
            // Saturating addition never panics if a raised input cap lets the
            // numerator reach `u64::MAX`.
            let charge = (name.len() as u64).saturating_add(value.len() as u64);
            self.ratio_bytes = self.ratio_bytes.saturating_add(charge);
            let position = self.err_pos();
            self.check_expansion_ratio(input, position)?;
            attrs.push((owned_qname(&name), RawStr::Shared(value)));
        }
        Ok(())
    }

    fn normalize_typed_attr(
        &self,
        element: &str,
        attribute: &str,
        value: RawStr,
        input: &[u8],
    ) -> RawStr {
        let cdata = if let Some(list) = self.attlists.get(element) {
            list.index
                .get(attribute)
                .map(|&index| list.order[index].cdata)
                .unwrap_or(true)
        } else {
            true
        };
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
        let elem_key = raw_qname_string(input, &qname);
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
                            input,
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
                    let value = self.normalize_typed_attr(&elem_key, &attr_key, value, input);
                    attrs.push((name, value));
                }
                None => return Err(self.end_input(input, "unterminated start tag")),
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
                input,
                LimitKind::ElementDepth,
                self.config.max_depth as u64,
                new_depth as u64,
            ));
        }

        let elem_key = raw_qname_string(input, &qname);
        let mut attrs = attrs;
        self.add_default_attributes(input, &elem_key, &mut attrs)?;
        if attrs.len() as u64 > self.config.max_attrs as u64 {
            return Err(self.limit(
                input,
                LimitKind::Attributes,
                self.config.max_attrs as u64,
                attrs.len() as u64,
            ));
        }

        let mut nss = Vec::new();
        let mut regular = Vec::new();
        for (name, value) in attrs {
            match name.prefix {
                Some(prefix) if prefix.as_str(input) == "xmlns" => {
                    // Keep the local-name slice. A literal `xmlns:p` stays a
                    // borrow of the input on `Namespace.prefix`.
                    nss.push((Some(name.local), value));
                }
                None if name.local.as_str(input) == "xmlns" => {
                    nss.push((None, value));
                }
                prefix => {
                    regular.push((
                        RawQName {
                            prefix,
                            local: name.local,
                        },
                        value,
                    ));
                }
            }
        }

        let mut raw_nss = Vec::with_capacity(nss.len());
        for (decl_prefix, value) in nss {
            let prefix_text = decl_prefix.as_ref().map(|prefix| prefix.as_str(input));
            let uri_text = value.as_str(input);
            self.validate_xmlns(prefix_text, uri_text)?;
            // The intern pool is internal. The event keeps `decl_prefix` and
            // `value`, so a declaration written in the input stays borrowed.
            let interned_prefix = prefix_text.map(|prefix| self.intern(prefix));
            let interned_uri = self.intern(uri_text);
            raw_nss.push(RawNs {
                prefix: decl_prefix,
                uri: value,
            });
            self.push_ns(interned_prefix, interned_uri, new_depth);
        }

        let resolved_name = self.resolve_name(input, qname, true)?;
        // Hash the interned allocation's address, not the URI bytes. `Arc<str>`
        // hashes by content, which makes a long namespace URI cost O(URI) per attribute.
        let mut seen_exp: HashSet<(usize, String)> = HashSet::new();
        let mut resolved_attrs = Vec::with_capacity(regular.len());
        for (name, value) in regular {
            let resolved = self.resolve_name(input, name, false)?;
            let key = (
                ns_ptr(&resolved.namespace),
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
        let message = match crate::rules::namespace_binding(prefix, uri) {
            Ok(()) => return Ok(()),
            Err(crate::rules::BindingFault::XmlnsPrefix) => "the xmlns prefix must not be declared",
            Err(crate::rules::BindingFault::XmlPrefix) => {
                "the xml prefix must be bound to the XML namespace"
            }
            Err(crate::rules::BindingFault::EmptyPrefixed) => {
                "cannot undeclare a namespace prefix in XML namespaces 1.0"
            }
            Err(
                crate::rules::BindingFault::XmlNamespace
                | crate::rules::BindingFault::XmlnsNamespace,
            ) if prefix.is_none() => "the default namespace must not be the xml or xmlns namespace",
            Err(
                crate::rules::BindingFault::XmlNamespace
                | crate::rules::BindingFault::XmlnsNamespace,
            ) => "this prefix must not be bound to a reserved namespace",
        };
        Err(self.syntax(message))
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

    fn lookup_ns(&self, prefix: Option<&str>) -> Option<Arc<str>> {
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

    fn push_ns(&mut self, prefix: Option<Arc<str>>, uri: Arc<str>, depth: usize) {
        match &prefix {
            None => self.default_ns.push(uri),
            Some(prefix) => {
                self.prefix_ns
                    .entry(Arc::clone(prefix))
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
        if self.frames.is_empty() {
            if let Some(raw) = self.try_plain_attr(input, quote)? {
                return Ok(raw);
            }
        }
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
                    return Err(self.end_input(input, "unterminated attribute value"));
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
                    self.append_attr_prefix(&mut buf, input, start, self.pos)?;
                    plain = false;
                }
                match self.expand_ref(input)? {
                    RefOut::Char(ch) => {
                        self.push_limited(input, &mut buf, ch, LimitKind::AttrValueLength)?
                    }
                    RefOut::Pushed => {}
                }
                continue;
            }
            let mark = self.pos;
            let raw_special = matches!(b, b'\t' | b'\n' | b'\r');
            let ch = self.consume_attr_literal(input)?;
            if plain && raw_special {
                self.append_attr_prefix(&mut buf, input, start, mark)?;
                plain = false;
                self.push_limited(input, &mut buf, ch, LimitKind::AttrValueLength)?;
            } else if plain {
                // Still a borrow of the input. Stop at this character instead
                // of waiting until the closing quote.
                self.check_len(input, self.pos - start, LimitKind::AttrValueLength)?;
            } else {
                self.push_limited(input, &mut buf, ch, LimitKind::AttrValueLength)?;
            }
        }
        if plain {
            self.check_len(input, self.pos - start - 1, LimitKind::AttrValueLength)?;
            Ok(RawStr::Range(start..self.pos - 1))
        } else {
            Ok(RawStr::Owned(buf))
        }
    }

    /// Copy a plain prefix that is already known to fit, or refuse the tail.
    ///
    /// The per-character check in [`Self::parse_attr_value`] rejects an
    /// over-long prefix before this runs. The walk is the backstop so a
    /// prefix is never copied past the cap.
    fn append_attr_prefix(
        &mut self,
        buf: &mut String,
        input: &[u8],
        start: usize,
        end: usize,
    ) -> Result<()> {
        let len = end.saturating_sub(start);
        if (len as u64) > self.config.max_text_len {
            let mut accepted = 0usize;
            for ch in assume_utf8(&input[start..end]).chars() {
                accepted += ch.len_utf8();
                if (accepted as u64) > self.config.max_text_len {
                    return Err(self.limit(
                        input,
                        LimitKind::AttrValueLength,
                        self.config.max_text_len,
                        accepted as u64,
                    ));
                }
            }
        }
        if len > 0 {
            buf.push_str(assume_utf8(&input[start..end]));
        }
        Ok(())
    }

    /// ASCII attribute value with no references and no whitespace that needs
    /// normalizing.
    ///
    /// Returns `None` without consuming when the value needs the normalizing
    /// path. A value that crosses `max_text_len` fails at that byte: `actual`
    /// is the length then, and the rest of the value is not scanned.
    #[inline]
    fn try_plain_attr(&mut self, input: &[u8], quote: u8) -> Result<Option<RawStr>> {
        if !self.frames.is_empty() || self.cr_pending {
            return Ok(None);
        }
        let start = self.pos;
        let bytes = &input[start..];
        let max = self.config.max_text_len;
        let hard = self.main_stop.saturating_sub(start);
        let mut i = 0usize;
        while i < bytes.len() {
            if i >= hard {
                return self.stop_plain_scan(input, i);
            }
            let byte = bytes[i];
            if byte == quote {
                let len = i as u64;
                self.column = self.column.saturating_add(len);
                self.pos = start + i;
                self.advance_one(input)?;
                self.check_len(input, i, LimitKind::AttrValueLength)?;
                return Ok(Some(RawStr::Range(start..start + i)));
            }
            // XML 1.0 Char excludes C0 controls and U+007F. Those, references,
            // and '<' take the normalizing path (or its error). A prefix that
            // is already over the cap fails here, before that byte.
            if !(0x20..0x7F).contains(&byte) || byte == b'&' || byte == b'<' {
                return Ok(None);
            }
            i += 1;
            if (i as u64) > max {
                self.advance_n(input, i)?;
                return Err(self.limit(input, LimitKind::AttrValueLength, max, i as u64));
            }
        }
        Ok(None)
    }

    fn parse_text(&mut self, input: &[u8]) -> Result<Option<RawStr>> {
        if let Some(text) = self.try_plain_text(input)? {
            return Ok(Some(text));
        }
        let mut buf = String::new();
        let mut brackets = 0u8;
        loop {
            let depth = self.frames.len();
            self.pop_finished();
            // `]]>` is forbidden only as three literal CharData characters.
            // A reference ends that run, so `]]&gt;` is the well-formed spelling
            // of the characters `]]>` and must not be rejected.
            if self.frames.len() != depth {
                brackets = 0;
            }
            let Some(b) = self.peek_byte(input) else {
                if self.frames.is_empty() {
                    if self.cursor_limited(input) || !self.end_is_eof {
                        return Err(self.end_input(input, "unexpected end of input in text"));
                    }
                    break;
                }
                self.force_pop_frame();
                brackets = 0;
                continue;
            };
            if b == b'<' {
                break;
            }
            if b == b'&' {
                brackets = 0;
                match self.expand_ref(input)? {
                    RefOut::Char(ch) => {
                        self.push_limited(input, &mut buf, ch, LimitKind::TextLength)?
                    }
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
            self.push_text(input, &mut buf, &mut brackets, ch)?;
        }
        if buf.is_empty() {
            Ok(None)
        } else {
            Ok(Some(RawStr::Owned(buf)))
        }
    }

    #[inline]
    fn try_plain_text(&mut self, input: &[u8]) -> Result<Option<RawStr>> {
        if !self.frames.is_empty() {
            return Ok(None);
        }
        let start = self.pos;
        let bytes = &input[start..];
        let hard = self.main_stop.saturating_sub(start);
        let mut i = 0usize;
        let mut brackets = 0u8;
        while i < bytes.len() {
            if i >= hard {
                return self.stop_plain_scan(input, i);
            }
            let b = bytes[i];
            if b == b'<' {
                let at = start + i;
                // `<` starts the next token, unless this byte itself is a
                // total-input or subset cap.
                if at >= self.total_stop || (self.subset_start.is_some() && at >= self.main_stop) {
                    return self.stop_plain_scan(input, i);
                }
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
                        input,
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
                if (i as u64) > self.config.max_text_len {
                    self.advance_n(input, i)?;
                    return Err(self.limit(
                        input,
                        LimitKind::TextLength,
                        self.config.max_text_len,
                        i as u64,
                    ));
                }
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
                        if let Some(proven) = utf8_invalid_index(&bytes[i..]) {
                            let lead = self.base + (start + i) as u64;
                            return Err(self.utf8_against_limit(input, lead, proven));
                        }
                        let at = start + i;
                        let width = match bytes.get(i).copied() {
                            Some(0xC0..=0xDF) => 2,
                            Some(0xE0..=0xEF) => 3,
                            Some(0xF0..=0xF7) => 4,
                            _ => 1,
                        };
                        if at >= self.main_stop {
                            return self.stop_plain_scan(input, i);
                        }
                        if self.main_stop < at + width && self.main_stop < input.len() {
                            return self.stop_plain_scan(input, self.main_stop - start);
                        }
                        // A short non-eof window is not the token cap. The
                        // slow path, or a later byte, decides.
                        if self.cut_utf8(&bytes[i..]) {
                            return Ok(None);
                        }
                        self.advance_n(input, i)?;
                        return Err(self.encoding("malformed UTF-8"));
                    }
                }
            }
            if (i as u64) > self.config.max_text_len {
                self.advance_n(input, i.min(bytes.len()))?;
                return Err(self.limit(
                    input,
                    LimitKind::TextLength,
                    self.config.max_text_len,
                    i as u64,
                ));
            }
        }
        let ended_on_markup = i < bytes.len() && bytes[i] == b'<';
        if !ended_on_markup && !self.end_is_eof && self.frames.is_empty() {
            if i == 0 {
                return Ok(None);
            }
            if start + i >= self.main_stop {
                return self.stop_plain_scan(input, i);
            }
            // The window ended before the armed cap. The slow path decides
            // Syntax versus a real limit; this is not TokenLength by itself.
            return Ok(None);
        }
        if i == 0 {
            return Ok(None);
        }
        self.advance_n(input, i)?;
        Ok(Some(RawStr::Range(start..start + i)))
    }

    fn push_text(
        &mut self,
        input: &[u8],
        buf: &mut String,
        brackets: &mut u8,
        ch: char,
    ) -> Result<()> {
        if ch == ']' {
            *brackets = brackets.saturating_add(1).min(2);
        } else if ch == '>' && *brackets >= 2 {
            return Err(self.syntax("`]]>` is not allowed in content"));
        } else {
            *brackets = 0;
        }
        self.push_limited(input, buf, ch, LimitKind::TextLength)
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
        let target = self.read_ncname_decl(input, "processing instruction target")?;
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
            None => return Err(self.end_input(input, "unterminated processing instruction")),
        };
        self.expect_bytes(input, b"?>")?;
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
            if self.cursor_limited(input) || self.prefix_cut(input, marker) {
                return Err(self.land_and_breach(input));
            }
            if comment && self.prefix_cut(input, b"--") {
                return Err(self.land_and_breach(input));
            }
            if self.starts_with(input, marker) {
                let end = if self.frames.is_empty() { self.pos } else { 0 };
                if plain && on_main {
                    self.check_len(input, end.saturating_sub(start), LimitKind::TextLength)?;
                    return Ok(RawStr::Range(start..end));
                }
                self.check_len(input, buf.len(), LimitKind::TextLength)?;
                return Ok(RawStr::Owned(buf));
            }
            if comment && self.starts_with(input, b"--") {
                return Err(self.syntax("`--` is not allowed in a comment"));
            }
            if self.peek_byte(input).is_none() {
                return Err(self.end_input(input, eof_message));
            }
            let mark = self.pos;
            let raw_cr = self.peek_byte(input) == Some(b'\r') && self.frames.is_empty();
            let ch = self.consume_content_literal(input)?;
            if plain && raw_cr {
                buf.push_str(assume_utf8(&input[start..mark]));
                plain = false;
            }
            // Count this character before looking for the terminator. A comment
            // that never closes still reports TextLength at the first overflow,
            // with `actual` equal to the length including that character.
            if plain {
                self.check_len(input, self.pos - start, LimitKind::TextLength)?;
            } else {
                let new_len = buf.len() + ch.len_utf8();
                self.check_len(input, new_len, LimitKind::TextLength)?;
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
                return Err(self.end_input(input, "unterminated quoted literal"));
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

    /// NCName, rejecting a colon that would make the token a QName.
    fn read_ncname_decl(&mut self, input: &[u8], what: &str) -> Result<RawStr> {
        let name = self.read_name(input, true)?;
        if self.peek_byte(input) == Some(b':') {
            return Err(self.syntax(format!("{what} must be an NCName")));
        }
        Ok(name)
    }

    fn read_ncname_string(&mut self, input: &[u8], what: &str) -> Result<String> {
        Ok(self.read_ncname_decl(input, what)?.into_string(input))
    }

    /// Parse `ExternalID NDataDecl?` far enough to check the notation name.
    ///
    /// The caller still reports [`ErrorKind::ExternalEntityForbidden`]. A
    /// notation name that is not an NCName is [`ErrorKind::Syntax`] instead.
    fn scan_external_notation(&mut self, input: &[u8]) -> Result<()> {
        self.breach_if_cut(input, &[b"SYSTEM", b"PUBLIC"])?;
        if self.starts_with_token(input, b"SYSTEM") {
            self.advance_n(input, 6)?;
            self.expect_ws(input)?;
            let _system = self.read_quoted_plain(input)?;
        } else {
            self.advance_n(input, 6)?;
            self.expect_ws(input)?;
            let _public = self.read_pubid_literal(input)?;
            self.expect_ws(input)?;
            let _system = self.read_quoted_plain(input)?;
        }
        self.skip_ws(input)?;
        if self.starts_with_token(input, b"NDATA") {
            self.advance_n(input, 5)?;
            self.expect_ws(input)?;
            let _name = self.read_ncname_decl(input, "notation name")?;
        }
        Ok(())
    }

    fn read_qname(&mut self, input: &[u8]) -> Result<RawQName> {
        // One running total across prefix, colon, and local name (L7).
        let first = self.read_name_budget(input, true, 0)?;
        if self.cursor_limited(input) {
            return Err(self.breach_at_cursor(input));
        }
        if self.peek_byte(input) == Some(b':') {
            let with_colon = first.byte_len() + 1;
            if with_colon > self.config.max_name_len {
                self.advance_one(input)?;
                return Err(self.limit(
                    input,
                    LimitKind::NameLength,
                    self.config.max_name_len as u64,
                    with_colon as u64,
                ));
            }
            self.advance_one(input)?;
            let local = self.read_name_budget(input, true, with_colon)?;
            if self.cursor_limited(input) {
                return Err(self.breach_at_cursor(input));
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
        self.read_name_budget(input, ncname, 0)
    }

    /// `prior` bytes of this QName are already accepted. They count toward
    /// `max_name_len` and are not part of the returned slice.
    fn read_name_budget(&mut self, input: &[u8], ncname: bool, prior: usize) -> Result<RawStr> {
        let on_main = self.frames.is_empty();
        let start = self.pos;
        let max = self.config.max_name_len;
        let Some(first) = self.peek_char(input)? else {
            return Err(self.end_input(input, "unexpected end of input in name"));
        };
        let ok_start = if ncname {
            is_ncname_start(first)
        } else {
            is_name_start(first)
        };
        if !ok_start {
            return Err(self.syntax(format!("invalid name start U+{:04X}", u32::from(first))));
        }
        let mut total = prior + first.len_utf8();
        self.consume_char(input)?;
        if total > max {
            return Err(self.limit(input, LimitKind::NameLength, max as u64, total as u64));
        }
        let mut owned = String::new();
        owned.push(first);
        while let Some(ch) = self.peek_char(input)? {
            let ok = if ncname {
                is_ncname_char(ch)
            } else {
                is_name_char(ch)
            };
            if !ok {
                break;
            }
            let next = total + ch.len_utf8();
            self.consume_char(input)?;
            if next > max {
                return Err(self.limit(input, LimitKind::NameLength, max as u64, next as u64));
            }
            owned.push(ch);
            total = next;
        }
        if on_main && self.frames.is_empty() {
            Ok(RawStr::Range(start..self.pos))
        } else {
            Ok(RawStr::Owned(owned))
        }
    }

    /// Read one general-entity or character reference.
    ///
    /// The entity name is an NCName, so `&a:b;` fails at the colon with
    /// [`ErrorKind::Syntax`]. The name is not looked up, and the error is not
    /// an undefined entity.
    fn expand_ref(&mut self, input: &[u8]) -> Result<RefOut> {
        let origin = self.err_pos();
        self.advance_one(input)?;
        if self.peek_byte(input) == Some(b'#') {
            self.advance_one(input)?;
            return Ok(RefOut::Char(self.parse_char_ref(input)?));
        }
        let name = self.read_name_string(input, true)?;
        self.expect_byte(input, b';')?;
        if let Some(ch) = predefined(&name) {
            return Ok(RefOut::Char(ch));
        }
        self.push_entity(input, &name, origin)?;
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

    fn push_entity(&mut self, input: &[u8], name: &str, origin: Position) -> Result<()> {
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
        let needed = (self.base + self.pos as u64).saturating_sub(1);
        if new_depth > self.config.max_entity_depth {
            return Err(self.measured_limit(
                input,
                LimitKind::EntityDepth,
                self.config.max_entity_depth as u64,
                new_depth as u64,
                origin,
                needed,
            ));
        }
        self.expansion_count += 1;
        if self.expansion_count > self.config.max_entity_expansions {
            return Err(self.measured_limit(
                input,
                LimitKind::EntityExpansions,
                self.config.max_entity_expansions,
                self.expansion_count,
                origin,
                needed,
            ));
        }
        if !self.entity_wf[idx] {
            let replacement = self.entities[idx].clone();
            match crate::wf::check_replacement_content(&replacement, self.config.max_depth) {
                Ok(()) => self.entity_wf[idx] = true,
                Err(crate::wf::WfFail::Syntax(message)) => {
                    return Err(Error::new(ErrorKind::Syntax, origin, message));
                }
                Err(crate::wf::WfFail::TooDeep { limit, actual }) => {
                    return Err(self.measured_limit(
                        input,
                        LimitKind::ElementDepth,
                        limit,
                        actual,
                        origin,
                        needed,
                    ));
                }
            }
        }
        let repl_len = self.entities[idx].len() as u64;
        self.expanded_bytes += repl_len;
        self.ratio_bytes = self.ratio_bytes.saturating_add(repl_len);
        if self.expanded_bytes > self.config.max_expanded_bytes {
            return Err(self.measured_limit(
                input,
                LimitKind::ExpandedBytes,
                self.config.max_expanded_bytes,
                self.expanded_bytes,
                origin,
                needed,
            ));
        }
        self.check_expansion_ratio(input, origin)?;
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

    fn check_expansion_ratio(&self, input: &[u8], position: Position) -> Result<()> {
        if self.ratio_bytes > EXPANSION_RATIO_FLOOR_BYTES {
            let consumed = self.base + self.pos as u64;
            // `u64::MAX` disables the check even when nothing has been
            // consumed yet: `0 * u64::MAX` would otherwise stay 0.
            let allowed = if self.config.max_expansion_ratio == u64::MAX {
                u64::MAX
            } else {
                consumed.saturating_mul(self.config.max_expansion_ratio)
            };
            if self.ratio_bytes > allowed {
                return Err(self.measured_limit(
                    input,
                    LimitKind::ExpansionRatio,
                    allowed,
                    self.ratio_bytes,
                    position,
                    position.offset,
                ));
            }
        }
        Ok(())
    }

    fn push_limited(
        &mut self,
        input: &[u8],
        buf: &mut String,
        ch: char,
        kind: LimitKind,
    ) -> Result<()> {
        let new_len = buf.len() + ch.len_utf8();
        self.check_len(input, new_len, kind)?;
        buf.push(ch);
        Ok(())
    }

    fn check_len(&self, input: &[u8], len: usize, kind: LimitKind) -> Result<()> {
        let actual = len as u64;
        if actual > self.config.max_text_len {
            Err(self.limit(input, kind, self.config.max_text_len, actual))
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
        if self.cursor_limited(input) {
            return Err(self.breach_at_cursor(input));
        }
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
            Some(Err(())) => {
                let slice = self.current_bytes(input);
                if self.cursor_limited(input) {
                    Err(self.breach_at_cursor(input))
                } else if self.cut_utf8(slice) {
                    if self.char_reaches_cap(input) {
                        Err(self.land_and_breach(input))
                    } else {
                        Err(self.end_input(input, "unexpected end of input"))
                    }
                } else if let Some(proven) = utf8_invalid_index(slice) {
                    let lead = self.base + self.pos as u64;
                    Err(self.utf8_against_limit(input, lead, proven))
                } else {
                    Err(self.encoding("malformed UTF-8"))
                }
            }
        }
    }

    fn decode_at_cursor(&mut self, input: &[u8]) -> Result<(char, usize)> {
        self.pop_finished();
        if self.cursor_limited(input) {
            return Err(self.breach_at_cursor(input));
        }
        let decoded = {
            let slice = self.current_bytes(input);
            if slice.is_empty() {
                None
            } else {
                Some(decode_utf8(slice))
            }
        };
        match decoded {
            None => Err(self.end_input(input, "unexpected end of input")),
            Some(Ok(value)) => Ok(value),
            Some(Err(())) => {
                let slice = self.current_bytes(input);
                if self.cursor_limited(input) {
                    Err(self.breach_at_cursor(input))
                } else if self.cut_utf8(slice) {
                    if self.char_reaches_cap(input) {
                        Err(self.land_and_breach(input))
                    } else {
                        Err(self.end_input(input, "unexpected end of input"))
                    }
                } else if let Some(proven) = utf8_invalid_index(slice) {
                    let lead = self.base + self.pos as u64;
                    Err(self.utf8_against_limit(input, lead, proven))
                } else {
                    Err(self.encoding("malformed UTF-8"))
                }
            }
        }
    }

    fn require_quote(&mut self, input: &[u8]) -> Result<u8> {
        match self.peek_byte(input) {
            Some(b @ (b'"' | b'\'')) => {
                self.advance_one(input)?;
                Ok(b)
            }
            Some(_) => Err(self.syntax("expected a quoted literal")),
            None => Err(self.end_input(input, "unexpected end of input")),
        }
    }

    fn expect_byte(&mut self, input: &[u8], expected: u8) -> Result<()> {
        match self.peek_byte(input) {
            Some(b) if b == expected => self.advance_one(input),
            Some(b) => {
                Err(self.syntax(format!("expected '{}', found U+{b:02X}", expected as char)))
            }
            None => Err(self.end_input(input, "unexpected end of input")),
        }
    }

    fn expect_bytes(&mut self, input: &[u8], pattern: &[u8]) -> Result<()> {
        if self.prefix_cut(input, pattern) {
            return Err(self.land_and_breach(input));
        }
        if self.starts_with(input, pattern) {
            self.advance_n(input, pattern.len())
        } else {
            Err(self.syntax(format!("expected {}", String::from_utf8_lossy(pattern))))
        }
    }

    fn expect_token(&mut self, input: &[u8], token: &[u8]) -> Result<()> {
        if self.prefix_cut(input, token) {
            return Err(self.land_and_breach(input));
        }
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

    #[inline]
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

    #[inline]
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

    #[inline]
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

    #[inline]
    fn advance_n(&mut self, input: &[u8], n: usize) -> Result<()> {
        for _ in 0..n {
            self.advance_one(input)?;
        }
        Ok(())
    }

    /// Drop the token cap while a BOM or prolog/epilog whitespace is ahead.
    ///
    /// [`Self::main_stop`] stays on the total-input cap, so those bytes are
    /// still one compare on the hot path. [`Self::begin_token`] puts the
    /// token cap back on the first byte of the token.
    fn suspend_token_limit(&mut self) {
        self.token_started = false;
        self.token_stop = usize::MAX;
        self.total_stop = self.total_stop_index();
        self.rearm_main_stop();
    }

    /// The next byte is the first byte of a top-level token.
    ///
    /// A token that starts on the total-input cap reports that cap before
    /// any markup is peeked.
    fn begin_token(&mut self, input: &[u8]) -> Result<()> {
        self.token_started = true;
        self.token_origin = self.pos_now();
        self.token_cr = self.cr_pending;
        self.arm_main_limits();
        if self.frames.is_empty() && self.pos >= self.total_stop && self.pos < input.len() {
            return Err(self.breach_at_cursor(input));
        }
        Ok(())
    }

    /// Prolog/epilog `S`. Not part of the following token.
    ///
    /// A non-eof window that ends inside the run asks for another frame so
    /// the streaming reader can discard the prefix and keep reading.
    fn skip_misc(&mut self, input: &[u8]) -> Result<()> {
        self.skip_ws(input)?;
        if self.peek_byte(input).is_none() && !self.end_is_eof && self.frames.is_empty() {
            self.need_frame = true;
            return Err(self.syntax("reframing"));
        }
        Ok(())
    }

    fn total_stop_index(&self) -> usize {
        if self.config.max_total_input <= self.base {
            0
        } else {
            u64_as_index(self.config.max_total_input - self.base)
        }
    }

    /// Recompute the main-input cursor ceiling from `token_origin`.
    ///
    /// One compare against [`Self::main_stop`] replaces a per-byte
    /// `max_token_len` / `max_total_input` calculation. The ceiling is an
    /// index into the current buffer; `prepare` updates `base` before the
    /// next token arms this again.
    fn arm_main_limits(&mut self) {
        let origin = self.token_origin.offset.saturating_sub(self.base);
        let token_room = u64_as_index(self.config.max_token_len);
        let token_stop = u64_as_index(origin).saturating_add(token_room);
        self.token_stop = token_stop;
        self.total_stop = self.total_stop_index();
        self.rearm_main_stop();
    }

    /// `main_stop` is the first index a byte-counted cap forbids.
    fn rearm_main_stop(&mut self) {
        let mut stop = self.token_stop.min(self.total_stop);
        if let Some(start) = self.subset_start {
            let subset = start.saturating_add(u64_as_index(self.config.max_internal_subset_len));
            stop = stop.min(subset);
        }
        self.main_stop = stop;
    }

    #[cold]
    #[inline(never)]
    fn main_limit_error(&self, input: &[u8]) -> Error {
        self.breach_at_cursor(input)
    }

    /// `scanned` bytes of a plain run fit. The next byte is past `main_stop`.
    #[cold]
    #[inline(never)]
    fn stop_plain_scan(&mut self, input: &[u8], scanned: usize) -> Result<Option<RawStr>> {
        self.advance_n(input, scanned)?;
        Err(self.breach_at_cursor(input))
    }

    fn char_reaches_cap(&self, input: &[u8]) -> bool {
        if !self.frames.is_empty() {
            return false;
        }
        let rest = self.current_bytes(input);
        let Some(&lead) = rest.first() else {
            return false;
        };
        let width = match lead {
            0x00..=0x7F => 1,
            0xC0..=0xDF => 2,
            0xE0..=0xEF => 3,
            0xF0..=0xF7 => 4,
            _ => 1,
        };
        let cap = self.main_stop;
        cap >= self.pos && cap < self.pos.saturating_add(width) && cap < input.len()
    }

    fn breach_if_cut(&mut self, input: &[u8], tokens: &[&[u8]]) -> Result<()> {
        if tokens.iter().any(|token| self.prefix_cut(input, token)) {
            Err(self.land_and_breach(input))
        } else {
            Ok(())
        }
    }

    /// Bytes strictly before the armed cap are a proper prefix of `pattern`,
    /// so deciding the keyword would read the forbidden byte.
    fn prefix_cut(&self, input: &[u8], pattern: &[u8]) -> bool {
        if !self.frames.is_empty() {
            return false;
        }
        let cap = self.main_stop;
        if cap < self.pos || cap >= input.len() {
            return false;
        }
        let have = cap - self.pos;
        if have >= pattern.len() {
            return false;
        }
        pattern.starts_with(&input[self.pos..cap])
    }

    /// Move onto the armed byte and report whichever cap it belongs to.
    #[cold]
    #[inline(never)]
    fn land_and_breach(&mut self, input: &[u8]) -> Error {
        if self.pos < self.main_stop {
            let skip = self.main_stop - self.pos;
            if self.advance_n(input, skip).is_err() {
                return self.breach_at_cursor(input);
            }
        }
        self.breach_at_cursor(input)
    }

    #[inline]
    fn advance_one(&mut self, input: &[u8]) -> Result<()> {
        self.pop_finished();
        if let Some(frame) = self.frames.last_mut() {
            frame.pos += 1;
            return Ok(());
        }
        // One predicted compare. The default caps sit far past a normal
        // document, so this branch is not taken on the slice hot path.
        if self.pos >= self.main_stop {
            return self.advance_at_stop(input);
        }
        if self.subset_start.is_some() {
            return self.advance_main_subset(input);
        }
        let b = input
            .get(self.pos)
            .copied()
            .ok_or_else(|| self.end_input(input, "unexpected end of input"))?;
        self.observe(b);
        self.pos += 1;
        Ok(())
    }

    #[cold]
    #[inline(never)]
    fn advance_at_stop(&mut self, input: &[u8]) -> Result<()> {
        if self.pos >= input.len() {
            return Err(self.end_input(input, "unexpected end of input"));
        }
        // `]` closes an exactly-full subset and is not subset content.
        if self.subset_closer(input) {
            let byte = input[self.pos];
            self.observe(byte);
            self.pos += 1;
            self.subset_start = None;
            self.rearm_main_stop();
            return Ok(());
        }
        Err(self.main_limit_error(input))
    }

    fn subset_closer(&self, input: &[u8]) -> bool {
        let Some(start) = self.subset_start else {
            return false;
        };
        let subset_stop = start.saturating_add(u64_as_index(self.config.max_internal_subset_len));
        self.pos == subset_stop
            && self.pos < self.token_stop
            && self.pos < self.total_stop
            && input.get(self.pos) == Some(&b']')
    }

    /// Byte inside an internal subset. The length cap is not on the content path.
    #[inline(never)]
    fn advance_main_subset(&mut self, input: &[u8]) -> Result<()> {
        let b = input
            .get(self.pos)
            .copied()
            .ok_or_else(|| self.end_input(input, "unexpected end of input"))?;
        let at_byte = self.pos_now();
        self.observe(b);
        self.pos += 1;
        if let Some(start) = self.subset_start {
            let len = (self.pos - start) as u64;
            if len > self.config.max_internal_subset_len {
                return Err(self.measured_limit(
                    input,
                    LimitKind::InternalSubsetLength,
                    self.config.max_internal_subset_len,
                    len,
                    at_byte,
                    at_byte.offset,
                ));
            }
        }
        Ok(())
    }

    #[inline]
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
        while let Some(frame) = self.frames.last() {
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

fn u64_as_index(value: u64) -> usize {
    usize::try_from(value).unwrap_or(usize::MAX)
}

fn flatten_qname(input: &[u8], name: RawQName) -> RawStr {
    let Some(prefix) = name.prefix else {
        return name.local;
    };
    match (prefix, name.local) {
        (RawStr::Range(prefix), RawStr::Range(local)) if prefix.end + 1 == local.start => {
            RawStr::Range(prefix.start..local.end)
        }
        (prefix, local) => {
            RawStr::Owned(format!("{}:{}", prefix.as_str(input), local.as_str(input)))
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
        RawEvent::Text(text) => Event::Text(text.into_text(input)),
        RawEvent::CData(text) => Event::CData(text.into_text(input)),
        RawEvent::Comment(text) => Event::Comment(text.into_text(input)),
        RawEvent::PI { target, data } => Event::PI(ProcessingInstruction {
            target: target.into_text(input),
            data: data.map(|data| data.into_text(input)),
        }),
        RawEvent::XmlDecl {
            version,
            encoding,
            standalone,
        } => Event::XmlDecl(XmlDecl {
            version: version.into_text(input),
            encoding: encoding.map(|encoding| encoding.into_text(input)),
            standalone,
        }),
        RawEvent::Doctype {
            name,
            system_id,
            public_id,
            has_internal_subset,
        } => Event::Doctype(Doctype {
            name: name.into_text(input),
            system_id: system_id.map(|value| value.into_text(input)),
            public_id: public_id.map(|value| value.into_text(input)),
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
        prefix: name.prefix.map(|prefix| prefix.into_text(input)),
        local: name.local.into_text(input),
        namespace: name.namespace,
    }
}

fn materialize_attr(input: &[u8], attr: ResolvedAttr) -> Attribute<'_> {
    Attribute {
        name: materialize_name(input, attr.name),
        value: attr.value.into_text(input),
    }
}

fn ns_ptr(namespace: &Option<Arc<str>>) -> usize {
    namespace
        .as_ref()
        .map(|uri| Arc::as_ptr(uri) as *const u8 as usize)
        .unwrap_or(0)
}

fn materialize_ns(input: &[u8], ns: RawNs) -> Namespace<'_> {
    Namespace {
        prefix: ns.prefix.map(|prefix| prefix.into_text(input)),
        uri: ns.uri.into_text(input),
    }
}
