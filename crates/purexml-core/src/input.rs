//! Incremental `BufRead` source and the pre-scan that delimits one token.
//!
//! The engine stays a recursive-descent parser over a complete window. This
//! module never rewinds a token: a retry would count entity expansions twice.
//! The scan cursor only moves forward, including across short reads.

use std::io::{BufRead, ErrorKind as IoErrorKind};

use crate::config::ParserConfig;
use crate::error::{observe_byte, Error, ErrorKind, LimitKind, Position, Result};
use crate::name::{decode_utf8, is_name_char, is_name_start, is_whitespace, utf8_incomplete};

/// Bytes pulled per `read` when the source has them. Not a public knob.
pub(crate) const READ_CHUNK: usize = 8 * 1024;

const COMPACT_MIN: usize = 64 * 1024;
const UTF8_BOM: &[u8] = &[0xEF, 0xBB, 0xBF];

/// Owned window over a [`BufRead`] source.
///
/// [`crate::Reader::from_reader`] stores the source here and pulls only enough
/// bytes for the next top-level token. The buffer stays near
/// [`ParserConfig::max_token_len`] plus [`READ_CHUNK`], independent of the
/// document length.
///
/// `ErrorKind::Interrupted` is retried with no attempt cap, matching
/// `std::io::Read::read_to_end`. `ErrorKind::WouldBlock` is not retried: it
/// becomes [`ErrorKind::Io`] and the reader stays failed. `Ok(0)` is end of
/// input; bytes offered after that are ignored. A `read` that reports more
/// bytes than the destination slice is [`ErrorKind::Io`] and does not panic.
/// Panics from the underlying reader propagate.
///
/// Events borrow this buffer until the next [`crate::Reader::read_event`].
/// The streaming path is not required to be zero-copy.
pub struct IoInput<R> {
    src: R,
    buf: Vec<u8>,
    base: u64,
    eof: bool,
    /// Set when the buffer cannot grow and the framer still wants bytes.
    blocked: bool,
    read_pos: Position,
    read_cr: bool,
    view_end: Option<usize>,
    view_eof: bool,
}

impl<R> IoInput<R> {
    pub(crate) fn base(&self) -> u64 {
        self.base
    }

    pub(crate) fn end_is_eof(&self) -> bool {
        self.view_end.is_none() && self.view_eof
    }

    pub(crate) fn view_end(&self) -> Option<usize> {
        self.view_end
    }

    pub(crate) fn buffered_len(&self) -> usize {
        self.buf.len()
    }

    pub(crate) fn saw_eof(&self) -> bool {
        self.eof
    }

    /// Window the engine is allowed to see. A probe is a prefix.
    pub(crate) fn window(&self) -> &[u8] {
        match self.view_end {
            Some(end) => {
                let end = end.min(self.buf.len());
                &self.buf[..end]
            }
            None => &self.buf,
        }
    }

    /// `Vec` capacity of the streaming window. Hidden test hook.
    pub(crate) fn capacity(&self) -> usize {
        self.buf.capacity()
    }

    fn compact(&mut self, pos: usize) -> usize {
        if pos < COMPACT_MIN || pos < self.buf.len() / 2 || pos > self.buf.len() {
            return pos;
        }
        self.discard_prefix(pos)
    }

    /// Drop bytes before the current token.
    ///
    /// [`compact`] keeps the 64 KiB copy heuristic. The window cap is separate:
    /// a consumed prefix must not count toward `max_token_len`, so a full
    /// window discards it before another read.
    fn discard_prefix(&mut self, pos: usize) -> usize {
        if pos == 0 || pos > self.buf.len() {
            return pos;
        }
        let drop = pos;
        self.buf.copy_within(drop.., 0);
        self.buf.truncate(self.buf.len() - drop);
        self.base = self.base.saturating_add(drop as u64);
        if self.buf.capacity() > self.buf.len().saturating_mul(4)
            && self.buf.capacity() > 1024 * 1024
        {
            self.buf
                .shrink_to(self.buf.len().saturating_mul(2).max(COMPACT_MIN));
        }
        0
    }
}

impl<R: BufRead> IoInput<R> {
    pub(crate) fn new(src: R) -> Self {
        Self {
            src,
            buf: Vec::new(),
            base: 0,
            eof: false,
            blocked: false,
            read_pos: Position::start(),
            read_cr: false,
            view_end: None,
            view_eof: false,
        }
    }

    /// Make `buf` contain one complete top-level token from `pos`, or a probe
    /// prefix. `pos` is updated if the consumed prefix is compacted.
    pub(crate) fn prepare(
        &mut self,
        pos: &mut usize,
        doc_start: bool,
        content: bool,
        config: &ParserConfig,
    ) -> Result<()> {
        *pos = self.compact(*pos);
        self.blocked = false;
        let mut framer = Framer::start(*pos, doc_start, content);
        loop {
            match framer.poll(&self.buf, self.eof, config) {
                Poll::NeedMore => {
                    if self.eof || self.blocked {
                        self.view_end = None;
                        self.view_eof = self.eof;
                        return Ok(());
                    }
                    // `pull` refuses another read once `buf.len()` hits
                    // max_token_len + READ_CHUNK. Bytes already consumed are
                    // not part of this token; drop them and frame it again
                    // before that cap can report a false TokenLength.
                    let cap = (config.max_token_len as usize).saturating_add(READ_CHUNK);
                    if *pos > 0 && *pos <= self.buf.len() && self.buf.len() >= cap {
                        *pos = self.discard_prefix(*pos);
                        framer = Framer::start(*pos, doc_start, content);
                        continue;
                    }
                    self.pull(config)?;
                }
                Poll::Ready { eof } => {
                    self.view_end = None;
                    self.view_eof = eof;
                    return Ok(());
                }
                Poll::Probe { end } => {
                    self.view_end = Some(end.min(self.buf.len()));
                    self.view_eof = false;
                    return Ok(());
                }
            }
        }
    }

    fn pull(&mut self, config: &ParserConfig) -> Result<()> {
        let cap = (config.max_token_len as usize).saturating_add(READ_CHUNK);
        let buf_room = cap.saturating_sub(self.buf.len());
        let max = config.max_total_input;
        let already = self.read_pos.offset;
        if already > max {
            return Err(self.total_input(max));
        }
        let input_room = if max == u64::MAX {
            usize::MAX / 4
        } else {
            ((max + 1) - already) as usize
        };
        let want = READ_CHUNK.min(buf_room).min(input_room);
        if want == 0 {
            if input_room == 0 {
                return Err(self.total_input(max));
            }
            self.blocked = true;
            return Ok(());
        }
        let mut tmp = [0u8; READ_CHUNK];
        let n = loop {
            match self.src.read(&mut tmp[..want]) {
                Ok(0) => {
                    self.eof = true;
                    return Ok(());
                }
                Ok(n) if n > want => {
                    return Err(self.io_error("read claimed more bytes than the buffer"));
                }
                Ok(n) => break n,
                Err(err) if err.kind() == IoErrorKind::Interrupted => continue,
                Err(err) => return Err(self.io_error(&err.to_string())),
            }
        };
        let bytes = &tmp[..n];
        if max != u64::MAX && already.saturating_add(n as u64) > max {
            let pre = (max - already) as usize;
            self.observe(&bytes[..pre]);
            return Err(self.total_input(max));
        }
        self.observe(bytes);
        self.buf.extend_from_slice(bytes);
        Ok(())
    }

    fn total_input(&self, limit: u64) -> Error {
        Error::new(
            ErrorKind::SecurityLimit {
                kind: LimitKind::TotalInput,
                limit,
                actual: limit.saturating_add(1),
            },
            self.read_pos,
            "input exceeds max_total_input",
        )
    }

    fn observe(&mut self, bytes: &[u8]) {
        for &byte in bytes {
            observe_byte(&mut self.read_pos, &mut self.read_cr, byte);
        }
    }

    fn io_error(&self, message: &str) -> Error {
        Error::new(ErrorKind::Io, self.read_pos, message)
    }
}

/// Outcome of one framer step. `Ready` means the buffer already holds the token.
#[derive(Debug)]
pub(crate) enum Poll {
    NeedMore,
    Ready {
        eof: bool,
    },
    /// `end` is exclusive. The byte at `end - 1` is the first past a limit,
    /// or the cut of an unfinished token.
    Probe {
        end: usize,
    },
}

#[derive(Clone, Copy)]
pub(crate) struct Framer {
    anchor: usize,
    i: usize,
    content: bool,
    bom_done: bool,
    state: State,
}

#[derive(Clone, Copy)]
enum State {
    Skip,
    Lt { at: usize },
    Text { start: usize, plain: bool },
    Comment { body: usize, plain: bool },
    CData { body: usize, plain: bool },
    Pi(Pi),
    Tag(Tag),
    Bang,
    Doctype(Doc),
    Lookahead,
}

#[derive(Clone, Copy)]
struct Pi {
    stage: PiStage,
    target: usize,
    plain: bool,
    body_len: u64,
    quote: Option<u8>,
    decl: bool,
    /// The one whitespace that separates a PI target from its data.
    gap: bool,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum PiStage {
    Target,
    Body,
}

#[derive(Clone, Copy)]
struct Tag {
    quote: Option<u8>,
    in_name: bool,
    name_len: usize,
    value_plain: bool,
    value_len: u64,
}

#[derive(Clone, Copy)]
struct Doc {
    quote: Option<u8>,
    /// `None` before `[`, `Some(count)` inside the subset.
    subset: Option<u64>,
    sub: Sub,
}

#[derive(Clone, Copy)]
struct Mark {
    marker: &'static [u8],
    body: usize,
    plain: bool,
    comment: bool,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Sub {
    Data,
    Comment,
    Pi,
}

impl Framer {
    pub(crate) fn start(pos: usize, doc_start: bool, content: bool) -> Self {
        Self {
            anchor: pos,
            i: pos,
            content,
            bom_done: !doc_start,
            state: State::Skip,
        }
    }

    pub(crate) fn poll(&mut self, buf: &[u8], eof: bool, config: &ParserConfig) -> Poll {
        loop {
            match self.state {
                State::Lookahead => {
                    if self.i < buf.len() || eof {
                        return Poll::Ready { eof };
                    }
                    return Poll::NeedMore;
                }
                State::Skip => {
                    if let Some(done) = self.prepare_document(buf, eof, config) {
                        return done;
                    }
                    if self.content {
                        if self.i >= buf.len() {
                            return if eof {
                                Poll::Ready { eof }
                            } else {
                                Poll::NeedMore
                            };
                        }
                        if buf[self.i] == b'<' {
                            if let Err(done) = self.consume_one(buf, eof, config.max_token_len) {
                                return done;
                            }
                            self.state = State::Lt { at: self.i - 1 };
                        } else {
                            self.state = State::Text {
                                start: self.i,
                                plain: true,
                            };
                        }
                        continue;
                    }
                    if let Err(done) = self.consider(buf, eof, config.max_token_len) {
                        return done;
                    }
                    if is_whitespace(buf[self.i]) {
                        self.i += 1;
                        continue;
                    }
                    if buf[self.i] == b'<' {
                        self.i += 1;
                        self.state = State::Lt { at: self.i - 1 };
                    } else {
                        self.state = State::Text {
                            start: self.i,
                            plain: true,
                        };
                    }
                }
                State::Lt { at } => match classify_lt(buf, at, eof) {
                    LtKind::NeedMore => return Poll::NeedMore,
                    LtKind::Comment => {
                        self.i = at + 4;
                        self.state = State::Comment {
                            body: self.i,
                            plain: true,
                        };
                    }
                    LtKind::CData => {
                        self.i = at + 9;
                        self.state = State::CData {
                            body: self.i,
                            plain: true,
                        };
                    }
                    LtKind::Doctype => {
                        self.i = at + 9;
                        self.state = State::Doctype(Doc {
                            quote: None,
                            subset: None,
                            sub: Sub::Data,
                        });
                    }
                    LtKind::Pi => {
                        self.i = at + 2;
                        self.state = State::Pi(Pi {
                            stage: PiStage::Target,
                            target: self.i,
                            plain: true,
                            body_len: 0,
                            quote: None,
                            decl: false,
                            gap: true,
                        });
                    }
                    LtKind::EndTag => {
                        self.i = at + 2;
                        self.state = State::Tag(Tag::named());
                    }
                    LtKind::StartTag => {
                        self.i = at + 1;
                        self.state = State::Tag(Tag::named());
                    }
                    LtKind::Bang => {
                        self.i = at + 2;
                        self.state = State::Bang;
                    }
                },
                State::Text { start, mut plain } => {
                    if self.i >= buf.len() {
                        self.state = State::Text { start, plain };
                        return if eof {
                            Poll::Ready { eof }
                        } else {
                            Poll::NeedMore
                        };
                    }
                    let byte = buf[self.i];
                    if byte == b'<' {
                        if let Err(done) = self.consume_one(buf, eof, config.max_token_len) {
                            return done;
                        }
                        self.state = State::Lookahead;
                        continue;
                    }
                    if byte == b'&' || byte == b'\r' {
                        plain = false;
                    }
                    if plain {
                        if let Err(done) = self.consider(buf, eof, config.max_token_len) {
                            self.state = State::Text { start, plain };
                            return done;
                        }
                        let width = match self.char_width(buf, eof) {
                            Ok(width) => width,
                            Err(done) => {
                                self.state = State::Text { start, plain };
                                return done;
                            }
                        };
                        let next = (self.i - start) as u64 + width as u64;
                        if next > config.max_text_len {
                            return Poll::Probe {
                                end: self.i + width,
                            };
                        }
                        if let Some(done) =
                            self.advance_width(buf, eof, config.max_token_len, width)
                        {
                            self.state = State::Text { start, plain };
                            return done;
                        }
                        self.state = State::Text { start, plain };
                        continue;
                    }
                    if let Err(done) = self.consume_one(buf, eof, config.max_token_len) {
                        return done;
                    }
                    self.state = State::Text { start, plain };
                }
                State::Comment { body, plain } => {
                    if let Some(done) = self.delimited(
                        buf,
                        eof,
                        config,
                        Mark {
                            marker: b"-->",
                            body,
                            plain,
                            comment: true,
                        },
                    ) {
                        return done;
                    }
                }
                State::CData { body, plain } => {
                    if let Some(done) = self.delimited(
                        buf,
                        eof,
                        config,
                        Mark {
                            marker: b"]]>",
                            body,
                            plain,
                            comment: false,
                        },
                    ) {
                        return done;
                    }
                }
                State::Bang => {
                    if let Some(done) = self.until_lt_or_gt(buf, eof, config.max_token_len) {
                        return done;
                    }
                }
                State::Pi(pi) => {
                    if let Some(done) = self.feed_pi(buf, eof, config, pi) {
                        return done;
                    }
                }
                State::Tag(tag) => {
                    if let Some(done) = self.feed_tag(buf, eof, config, tag) {
                        return done;
                    }
                }
                State::Doctype(doc) => {
                    if let Some(done) = self.feed_doctype(buf, eof, config, doc) {
                        return done;
                    }
                }
            }
        }
    }

    /// BOM / UTF-16 sniff. `Some` means `poll` should return immediately.
    fn prepare_document(&mut self, buf: &[u8], eof: bool, config: &ParserConfig) -> Option<Poll> {
        if self.bom_done {
            return None;
        }
        let have = buf.len().saturating_sub(self.anchor);
        if have < 4 && !eof {
            return Some(Poll::NeedMore);
        }
        self.bom_done = true;
        let prefix = &buf[self.anchor..];
        if prefix.starts_with(UTF8_BOM) {
            if 3 > config.max_token_len {
                let end = self.anchor + (config.max_token_len as usize) + 1;
                if end <= buf.len() {
                    return Some(Poll::Probe { end });
                }
            }
            self.i = self.anchor + 3;
            return None;
        }
        if rejected_bom(prefix) {
            return Some(Poll::Ready { eof });
        }
        None
    }

    /// Byte length of the UTF-8 character at `i`. Incomplete sequences wait.
    fn char_width(&self, buf: &[u8], eof: bool) -> std::result::Result<usize, Poll> {
        let rest = buf.get(self.i..).unwrap_or(&[]);
        if rest.is_empty() {
            return Err(if eof {
                Poll::Ready { eof: true }
            } else {
                Poll::NeedMore
            });
        }
        let width = match rest[0] {
            0x00..=0x7F => 1,
            0xC0..=0xDF => 2,
            0xE0..=0xEF => 3,
            0xF0..=0xF7 => 4,
            _ => 1,
        };
        if rest.len() < width {
            return Err(if eof {
                Poll::Ready { eof: true }
            } else {
                Poll::NeedMore
            });
        }
        Ok(width)
    }

    fn consider(&self, buf: &[u8], eof: bool, max_token: u64) -> std::result::Result<(), Poll> {
        let next_len = (self.i - self.anchor) as u64 + 1;
        if next_len > max_token {
            if self.i < buf.len() {
                return Err(Poll::Probe { end: self.i + 1 });
            }
            return Err(if eof {
                Poll::Ready { eof: true }
            } else {
                Poll::NeedMore
            });
        }
        if self.i >= buf.len() {
            return Err(if eof {
                Poll::Ready { eof: true }
            } else {
                Poll::NeedMore
            });
        }
        Ok(())
    }

    fn consume_one(
        &mut self,
        buf: &[u8],
        eof: bool,
        max_token: u64,
    ) -> std::result::Result<u8, Poll> {
        self.consider(buf, eof, max_token)?;
        let byte = buf[self.i];
        self.i += 1;
        Ok(byte)
    }

    /// Comment, CDATA: body text limit, then a fixed ASCII terminator.
    fn delimited(
        &mut self,
        buf: &[u8],
        eof: bool,
        config: &ParserConfig,
        mut mark: Mark,
    ) -> Option<Poll> {
        if starts_at(buf, self.i, mark.marker) {
            for _ in 0..mark.marker.len() {
                if let Err(done) = self.consume_one(buf, eof, config.max_token_len) {
                    self.remember_delimited(mark);
                    return Some(done);
                }
            }
            self.state = State::Lookahead;
            return None;
        }
        if !eof && proper_prefix_at(buf, self.i, mark.marker) {
            self.remember_delimited(mark);
            return Some(Poll::NeedMore);
        }
        if let Err(done) = self.consider(buf, eof, config.max_token_len) {
            self.remember_delimited(mark);
            return Some(done);
        }
        let byte = buf[self.i];
        if byte == b'\r' {
            mark.plain = false;
        }
        if mark.plain {
            let width = match self.char_width(buf, eof) {
                Ok(width) => width,
                Err(done) => {
                    self.remember_delimited(mark);
                    return Some(done);
                }
            };
            let next = (self.i - mark.body) as u64 + width as u64;
            if next > config.max_text_len {
                self.remember_delimited(mark);
                return Some(Poll::Probe {
                    end: self.i + width,
                });
            }
            if let Some(done) = self.advance_width(buf, eof, config.max_token_len, width) {
                self.remember_delimited(mark);
                return Some(done);
            }
            self.remember_delimited(mark);
            return None;
        }
        self.i += 1;
        self.remember_delimited(mark);
        None
    }

    fn remember_delimited(&mut self, mark: Mark) {
        self.state = if mark.comment {
            State::Comment {
                body: mark.body,
                plain: mark.plain,
            }
        } else {
            State::CData {
                body: mark.body,
                plain: mark.plain,
            }
        };
    }

    fn until_lt_or_gt(&mut self, buf: &[u8], eof: bool, max_token: u64) -> Option<Poll> {
        if let Err(done) = self.consider(buf, eof, max_token) {
            return Some(done);
        }
        let byte = buf[self.i];
        self.i += 1;
        if byte == b'>' || byte == b'<' {
            self.state = State::Lookahead;
        }
        None
    }

    fn feed_pi(
        &mut self,
        buf: &[u8],
        eof: bool,
        config: &ParserConfig,
        mut pi: Pi,
    ) -> Option<Poll> {
        if pi.stage == PiStage::Body && pi.quote.is_none() && starts_at(buf, self.i, b"?>") {
            for _ in 0..2 {
                if let Err(done) = self.consume_one(buf, eof, config.max_token_len) {
                    self.state = State::Pi(pi);
                    return Some(done);
                }
            }
            self.state = State::Lookahead;
            return None;
        }
        if pi.stage == PiStage::Body
            && pi.quote.is_none()
            && !eof
            && proper_prefix_at(buf, self.i, b"?>")
        {
            self.state = State::Pi(pi);
            return Some(Poll::NeedMore);
        }
        if let Err(done) = self.consider(buf, eof, config.max_token_len) {
            self.state = State::Pi(pi);
            return Some(done);
        }
        let byte = buf[self.i];
        match pi.stage {
            PiStage::Target => {
                let rest = &buf[self.i..];
                if eof && utf8_incomplete(rest) {
                    self.state = State::Pi(pi);
                    return Some(Poll::Ready { eof: true });
                }
                match decode_utf8(rest) {
                    Ok((ch, width)) => {
                        if is_name_char(ch) && !(pi.target == self.i && !is_name_start(ch)) {
                            let next = pi_target_len(buf, pi.target, self.i, width);
                            if next > config.max_name_len {
                                let end = self.i + width;
                                if end > buf.len() {
                                    self.state = State::Pi(pi);
                                    return Some(if eof {
                                        Poll::Ready { eof: true }
                                    } else {
                                        Poll::NeedMore
                                    });
                                }
                                return Some(Poll::Probe { end });
                            }
                            if let Some(done) =
                                self.advance_width(buf, eof, config.max_token_len, width)
                            {
                                self.state = State::Pi(pi);
                                return Some(done);
                            }
                            self.state = State::Pi(pi);
                            return None;
                        }
                        // Target ended. `<?xml` + whitespace or `?` is the declaration.
                        let target = &buf[pi.target..self.i];
                        let decl = target == b"xml" && (is_whitespace(byte) || byte == b'?');
                        pi.decl = decl;
                        pi.stage = PiStage::Body;
                        pi.plain = true;
                        pi.body_len = 0;
                        pi.gap = true;
                        self.state = State::Pi(pi);
                    }
                    Err(()) if utf8_incomplete(rest) => {
                        self.state = State::Pi(pi);
                        return Some(Poll::NeedMore);
                    }
                    Err(()) => {
                        pi.stage = PiStage::Body;
                        pi.decl = false;
                        pi.gap = true;
                        self.state = State::Pi(pi);
                    }
                }
            }
            PiStage::Body => {
                if let Some(quote) = pi.quote {
                    if byte == quote {
                        pi.quote = None;
                    }
                    self.i += 1;
                    self.state = State::Pi(pi);
                    return None;
                }
                if pi.decl && (byte == b'"' || byte == b'\'') {
                    pi.quote = Some(byte);
                    self.i += 1;
                    self.state = State::Pi(pi);
                    return None;
                }
                if !pi.decl && pi.gap && is_whitespace(byte) {
                    // One separator, matching `consume_one_space` (CR swallows LF).
                    pi.gap = false;
                    self.i += 1;
                    if byte == b'\r' && self.i < buf.len() && buf[self.i] == b'\n' {
                        self.i += 1;
                    } else if byte == b'\r' && self.i >= buf.len() && !eof {
                        self.i -= 1;
                        pi.gap = true;
                        self.state = State::Pi(pi);
                        return Some(Poll::NeedMore);
                    }
                    self.state = State::Pi(pi);
                    return None;
                }
                pi.gap = false;
                if !pi.decl {
                    if byte == b'\r' {
                        pi.plain = false;
                    }
                    if pi.plain && byte != b'?' {
                        let width = match self.char_width(buf, eof) {
                            Ok(width) => width,
                            Err(done) => {
                                self.state = State::Pi(pi);
                                return Some(done);
                            }
                        };
                        let next = pi.body_len.saturating_add(width as u64);
                        if next > config.max_text_len {
                            return Some(Poll::Probe {
                                end: self.i + width,
                            });
                        }
                        if let Some(done) =
                            self.advance_width(buf, eof, config.max_token_len, width)
                        {
                            self.state = State::Pi(pi);
                            return Some(done);
                        }
                        pi.body_len = next;
                        self.state = State::Pi(pi);
                        return None;
                    }
                }
                self.i += 1;
                self.state = State::Pi(pi);
            }
        }
        None
    }

    fn feed_tag(
        &mut self,
        buf: &[u8],
        eof: bool,
        config: &ParserConfig,
        mut tag: Tag,
    ) -> Option<Poll> {
        if let Err(done) = self.consider(buf, eof, config.max_token_len) {
            self.state = State::Tag(tag);
            return Some(done);
        }
        let byte = buf[self.i];
        if let Some(quote) = tag.quote {
            if byte == quote {
                tag.quote = None;
                self.i += 1;
                self.state = State::Tag(tag);
                return None;
            }
            if byte == b'<' {
                self.i += 1;
                self.state = State::Lookahead;
                return None;
            }
            if tag.value_plain {
                if byte == b'&' || !(0x20..0x7F).contains(&byte) {
                    tag.value_plain = false;
                } else {
                    tag.value_len = tag.value_len.saturating_add(1);
                    if tag.value_len > config.max_text_len {
                        return Some(Poll::Probe { end: self.i + 1 });
                    }
                }
            }
            self.i += 1;
            self.state = State::Tag(tag);
            return None;
        }
        if byte == b'"' || byte == b'\'' {
            tag.quote = Some(byte);
            tag.in_name = false;
            tag.value_plain = true;
            tag.value_len = 0;
            self.i += 1;
            self.state = State::Tag(tag);
            return None;
        }
        if byte == b'>' || byte == b'<' {
            self.i += 1;
            self.state = State::Lookahead;
            return None;
        }
        if tag.in_name {
            let rest = &buf[self.i..];
            if eof && utf8_incomplete(rest) {
                self.state = State::Tag(tag);
                return Some(Poll::Ready { eof: true });
            }
            match decode_utf8(rest) {
                Ok((ch, width)) => {
                    let starts = tag.name_len == 0;
                    let ok = if starts {
                        is_name_start(ch)
                    } else {
                        is_name_char(ch)
                    };
                    if ok {
                        let next = tag.name_len.saturating_add(width);
                        if next > config.max_name_len {
                            let end = self.i + width;
                            if end > buf.len() {
                                self.state = State::Tag(tag);
                                return Some(if eof {
                                    Poll::Ready { eof: true }
                                } else {
                                    Poll::NeedMore
                                });
                            }
                            return Some(Poll::Probe { end });
                        }
                        if let Some(done) =
                            self.advance_width(buf, eof, config.max_token_len, width)
                        {
                            self.state = State::Tag(tag);
                            return Some(done);
                        }
                        tag.name_len = next;
                        self.state = State::Tag(tag);
                        return None;
                    }
                    tag.in_name = false;
                }
                Err(()) if utf8_incomplete(rest) => {
                    self.state = State::Tag(tag);
                    return Some(Poll::NeedMore);
                }
                Err(()) => tag.in_name = false,
            }
        } else if is_ascii_name_start(byte) {
            tag.in_name = true;
            tag.name_len = 0;
            self.state = State::Tag(tag);
            return None;
        }
        self.i += 1;
        self.state = State::Tag(tag);
        None
    }

    fn feed_doctype(
        &mut self,
        buf: &[u8],
        eof: bool,
        config: &ParserConfig,
        mut doc: Doc,
    ) -> Option<Poll> {
        if doc.subset.is_some() {
            if let Some(done) = self.feed_subset(buf, eof, config, doc) {
                return Some(done);
            }
            return None;
        }
        if let Err(done) = self.consider(buf, eof, config.max_token_len) {
            self.state = State::Doctype(doc);
            return Some(done);
        }
        let byte = buf[self.i];
        if let Some(quote) = doc.quote {
            if byte == quote {
                doc.quote = None;
            }
            self.i += 1;
            self.state = State::Doctype(doc);
            return None;
        }
        if byte == b'"' || byte == b'\'' {
            doc.quote = Some(byte);
            self.i += 1;
            self.state = State::Doctype(doc);
            return None;
        }
        if byte == b'[' {
            self.i += 1;
            doc.subset = Some(0);
            doc.sub = Sub::Data;
            self.state = State::Doctype(doc);
            return None;
        }
        if byte == b'>' {
            self.i += 1;
            self.state = State::Lookahead;
            return None;
        }
        self.i += 1;
        self.state = State::Doctype(doc);
        None
    }

    fn feed_subset(
        &mut self,
        buf: &[u8],
        eof: bool,
        config: &ParserConfig,
        mut doc: Doc,
    ) -> Option<Poll> {
        match doc.sub {
            Sub::Comment => {
                if starts_at(buf, self.i, b"-->") {
                    for _ in 0..3 {
                        if let Some(done) = self.bump_subset(buf, eof, config, &mut doc) {
                            return Some(done);
                        }
                    }
                    doc.sub = Sub::Data;
                    self.state = State::Doctype(doc);
                    return None;
                }
                if !eof && proper_prefix_at(buf, self.i, b"-->") {
                    self.state = State::Doctype(doc);
                    return Some(Poll::NeedMore);
                }
            }
            Sub::Pi => {
                if starts_at(buf, self.i, b"?>") {
                    for _ in 0..2 {
                        if let Some(done) = self.bump_subset(buf, eof, config, &mut doc) {
                            return Some(done);
                        }
                    }
                    doc.sub = Sub::Data;
                    self.state = State::Doctype(doc);
                    return None;
                }
                if !eof && proper_prefix_at(buf, self.i, b"?>") {
                    self.state = State::Doctype(doc);
                    return Some(Poll::NeedMore);
                }
            }
            Sub::Data => {
                if doc.quote.is_none() {
                    // Consume the whole opener before looking for the closer.
                    // One byte would let `-->` match inside `<!-->` (`--` plus
                    // the following `>`), and the same overlap hits `<?` / `?>`.
                    if starts_at(buf, self.i, b"<!--") {
                        doc.sub = Sub::Comment;
                        for _ in 0..4 {
                            if let Some(done) = self.bump_subset(buf, eof, config, &mut doc) {
                                return Some(done);
                            }
                        }
                        self.state = State::Doctype(doc);
                        return None;
                    } else if starts_at(buf, self.i, b"<?") {
                        doc.sub = Sub::Pi;
                        for _ in 0..2 {
                            if let Some(done) = self.bump_subset(buf, eof, config, &mut doc) {
                                return Some(done);
                            }
                        }
                        self.state = State::Doctype(doc);
                        return None;
                    } else if !eof
                        && (proper_prefix_at(buf, self.i, b"<!--")
                            || proper_prefix_at(buf, self.i, b"<?"))
                    {
                        self.state = State::Doctype(doc);
                        return Some(Poll::NeedMore);
                    }
                }
                if doc.sub == Sub::Data && doc.quote.is_none() && buf.get(self.i) == Some(&b']') {
                    // `]` closes the subset and is not part of its length.
                    if let Err(done) = self.consume_one(buf, eof, config.max_token_len) {
                        self.state = State::Doctype(doc);
                        return Some(done);
                    }
                    doc.subset = None;
                    self.state = State::Doctype(doc);
                    return None;
                }
            }
        }
        self.bump_subset(buf, eof, config, &mut doc)
    }

    fn bump_subset(
        &mut self,
        buf: &[u8],
        eof: bool,
        config: &ParserConfig,
        doc: &mut Doc,
    ) -> Option<Poll> {
        if let Err(done) = self.consider(buf, eof, config.max_token_len) {
            self.state = State::Doctype(*doc);
            return Some(done);
        }
        let count = doc.subset.unwrap_or(0);
        let next = count.saturating_add(1);
        if next > config.max_internal_subset_len {
            return Some(Poll::Probe { end: self.i + 1 });
        }
        let byte = buf[self.i];
        if doc.sub == Sub::Data {
            if doc.quote.is_none() && (byte == b'"' || byte == b'\'') {
                doc.quote = Some(byte);
            } else if doc.quote == Some(byte) {
                doc.quote = None;
            }
        }
        doc.subset = Some(next);
        self.i += 1;
        self.state = State::Doctype(*doc);
        None
    }

    /// Advance `width` bytes, each subject to the token cap.
    /// The first byte was already approved by `consider`.
    fn advance_width(
        &mut self,
        buf: &[u8],
        eof: bool,
        max_token: u64,
        width: usize,
    ) -> Option<Poll> {
        for step in 0..width {
            if step > 0 {
                if let Err(done) = self.consider(buf, eof, max_token) {
                    return Some(done);
                }
            }
            if self.i >= buf.len() {
                return Some(if eof {
                    Poll::Ready { eof: true }
                } else {
                    Poll::NeedMore
                });
            }
            self.i += 1;
        }
        None
    }
}

impl Tag {
    fn named() -> Self {
        Self {
            quote: None,
            in_name: true,
            name_len: 0,
            value_plain: true,
            value_len: 0,
        }
    }
}

fn pi_target_len(buf: &[u8], target: usize, i: usize, width: usize) -> usize {
    buf[target..i].len().saturating_add(width)
}

fn is_ascii_name_start(byte: u8) -> bool {
    byte.is_ascii_alphabetic() || byte == b'_' || byte == b':'
}

fn starts_at(buf: &[u8], i: usize, pattern: &[u8]) -> bool {
    buf.get(i..).is_some_and(|rest| rest.starts_with(pattern))
}

fn proper_prefix_at(buf: &[u8], i: usize, pattern: &[u8]) -> bool {
    match buf.get(i..) {
        Some(rest) if !rest.is_empty() && rest.len() < pattern.len() => pattern.starts_with(rest),
        _ => false,
    }
}

fn rejected_bom(prefix: &[u8]) -> bool {
    prefix.starts_with(&[0xFE, 0xFF])
        || prefix.starts_with(&[0xFF, 0xFE])
        || prefix.starts_with(&[0x00, 0x00, 0xFE, 0xFF])
        || prefix.starts_with(&[0xFF, 0xFE, 0x00, 0x00])
        || prefix.starts_with(&[0x00, 0x3C, 0x00, 0x3F])
        || prefix.starts_with(&[0x3C, 0x00, 0x3F, 0x00])
}

enum LtKind {
    NeedMore,
    Comment,
    CData,
    Doctype,
    Pi,
    EndTag,
    StartTag,
    Bang,
}

fn classify_lt(buf: &[u8], at: usize, eof: bool) -> LtKind {
    let rest = &buf[at..];
    if rest.starts_with(b"<!--") {
        return LtKind::Comment;
    }
    if rest.starts_with(b"<![CDATA[") {
        return LtKind::CData;
    }
    if rest.starts_with(b"<!DOCTYPE") {
        return LtKind::Doctype;
    }
    if rest.starts_with(b"<?") {
        return LtKind::Pi;
    }
    if rest.starts_with(b"</") {
        return LtKind::EndTag;
    }
    let patterns: [&[u8]; 5] = [b"<!--", b"<![CDATA[", b"<!DOCTYPE", b"<?", b"</"];
    let waiting = patterns
        .iter()
        .any(|pattern| pattern.starts_with(rest) && rest.len() < pattern.len());
    if waiting && !eof {
        return LtKind::NeedMore;
    }
    if rest.len() >= 2 && rest[1] == b'!' {
        return LtKind::Bang;
    }
    if rest.len() >= 2 || eof {
        return LtKind::StartTag;
    }
    LtKind::NeedMore
}
