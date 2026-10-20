//! Parse errors. Every error carries a source position.

use std::fmt;

/// 1-based line and column, plus a 0-based byte offset into the original input.
///
/// Column counts Unicode scalar values, not bytes. A UTF-8 BOM contributes to
/// `offset` but not to line or column.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Position {
    /// 1-based line number.
    pub line: u64,
    /// 1-based column in Unicode scalar values.
    pub column: u64,
    /// 0-based byte offset into the original input.
    pub offset: u64,
}

impl Position {
    pub(crate) fn start() -> Self {
        Self {
            line: 1,
            column: 1,
            offset: 0,
        }
    }
}

impl fmt::Display for Position {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "line {}, column {} (byte offset {})",
            self.line, self.column, self.offset
        )
    }
}

/// Which configured limit was exceeded.
///
/// `limit` and `actual` on [`ErrorKind::SecurityLimit`] use the same units for
/// every variant except that callers should treat the numbers as opaque counts
/// of the resource named here (bytes, depth, or repetitions).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum LimitKind {
    /// General-entity nesting depth. A document-level reference is depth 1.
    EntityDepth,
    /// Number of general-entity expansions, including repeated uses.
    EntityExpansions,
    /// Cumulative replacement-text bytes from all expansions.
    ExpandedBytes,
    /// Streaming expansion ratio.
    ///
    /// Checked on every expansion once cumulative replacement bytes exceed
    /// [`crate::EXPANSION_RATIO_FLOOR_BYTES`]. `limit` is
    /// `bytes_consumed_so_far * max_expansion_ratio` and `actual` is the
    /// cumulative replacement-text byte count. The denominator is original
    /// input bytes the parser has consumed, not the full buffer length and not
    /// bytes inside a replacement text.
    ExpansionRatio,
    /// Element nesting depth.
    ElementDepth,
    /// Attributes plus namespace declarations on one element.
    Attributes,
    /// Name length in UTF-8 bytes.
    NameLength,
    /// One text node, in UTF-8 bytes, after end-of-line normalization and
    /// entity expansion.
    TextLength,
    /// Original input size in bytes.
    TotalInput,
    /// One attribute value, in UTF-8 bytes, after attribute-value normalization
    /// and entity expansion. `limit` is `ParserConfig::max_text_len`.
    AttrValueLength,
}

impl fmt::Display for LimitKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::EntityDepth => "entity depth",
            Self::EntityExpansions => "entity expansions",
            Self::ExpandedBytes => "expanded bytes",
            Self::ExpansionRatio => "expansion ratio",
            Self::ElementDepth => "element depth",
            Self::Attributes => "attributes per element",
            Self::NameLength => "name length",
            Self::TextLength => "text length",
            Self::TotalInput => "total input",
            Self::AttrValueLength => "attribute value length",
        })
    }
}

/// Category of a parse failure.
///
/// Human-readable detail lives on [`Error::message`]. Match on this enum in tests.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ErrorKind {
    /// A [`ParserConfig`](crate::ParserConfig) limit was exceeded.
    SecurityLimit {
        /// Which limit fired.
        kind: LimitKind,
        /// Configured maximum, in the units described by `kind`.
        ///
        /// For [`LimitKind::ExpansionRatio`] this is the allowed replacement-byte
        /// budget at the current input cursor (`consumed * max_expansion_ratio`),
        /// not the bare ratio constant.
        /// For [`LimitKind::AttrValueLength`] this is `max_text_len`.
        limit: u64,
        /// The measured value that exceeded `limit`.
        actual: u64,
    },
    /// An entity declaration uses `SYSTEM` or `PUBLIC`, or a parameter-entity
    /// reference (`%name;`) was seen. The parser never loads external data.
    ///
    /// A `DOCTYPE` that merely names an external subset is not this error: the
    /// subset is skipped and the ids are reported on the doctype event.
    /// Declaring an internal parameter entity is also allowed; using one is not.
    ExternalEntityForbidden,
    /// The document is not well-formed, a name is unbound, or a general entity
    /// reference has no internal declaration.
    Syntax,
    /// The bytes are not UTF-8, or the document declares an encoding other than
    /// UTF-8. UTF-16 (BOM or encoding declaration) is rejected here. There is
    /// no transcoding in 0.x.
    Encoding,
    /// The caller-supplied [`std::io::BufRead`] returned an I/O error.
    /// The parser itself never opens files or network sockets.
    Io,
}

/// A parse error with a source position.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Error {
    /// Machine-readable category.
    pub kind: ErrorKind,
    /// Where the failure was detected.
    ///
    /// Inside a general-entity expansion this is the `&` of the outermost
    /// reference in the original document.
    pub position: Position,
    /// Additional detail. Stable matching should use [`Self::kind`].
    pub message: String,
}

impl Error {
    pub(crate) fn new(kind: ErrorKind, position: Position, message: impl Into<String>) -> Self {
        Self {
            kind,
            position,
            message: message.into(),
        }
    }

    /// True when expansion or document limits rejected the input.
    pub fn is_security_limit(&self) -> bool {
        matches!(self.kind, ErrorKind::SecurityLimit { .. })
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.kind {
            ErrorKind::SecurityLimit {
                kind,
                limit,
                actual,
            } => write!(
                f,
                "security limit ({kind}) at {}: limit {limit}, actual {actual}: {}",
                self.position, self.message
            ),
            ErrorKind::ExternalEntityForbidden => {
                write!(
                    f,
                    "external entity forbidden at {}: {}",
                    self.position, self.message
                )
            }
            ErrorKind::Syntax => write!(f, "syntax error at {}: {}", self.position, self.message),
            ErrorKind::Encoding => {
                write!(f, "encoding error at {}: {}", self.position, self.message)
            }
            ErrorKind::Io => write!(f, "i/o error at {}: {}", self.position, self.message),
        }
    }
}

impl std::error::Error for Error {}

/// Result of a parse operation.
pub type Result<T> = std::result::Result<T, Error>;

/// Compute a [`Position`] by scanning `input` up to `offset` bytes.
pub(crate) fn position_at(input: &[u8], offset: usize) -> Position {
    let offset = offset.min(input.len());
    let mut pos = Position::start();
    let mut cr_pending = false;
    for &b in &input[..offset] {
        observe_byte(&mut pos, &mut cr_pending, b);
    }
    pos.offset = offset as u64;
    pos
}

pub(crate) fn observe_byte(pos: &mut Position, cr_pending: &mut bool, b: u8) {
    if *cr_pending {
        *cr_pending = false;
        if b == b'\n' {
            pos.offset = pos.offset.saturating_add(1);
            return;
        }
    }
    if b == b'\r' {
        pos.line = pos.line.saturating_add(1);
        pos.column = 1;
        *cr_pending = true;
    } else if b == b'\n' {
        pos.line = pos.line.saturating_add(1);
        pos.column = 1;
    } else if b < 0x80 || b & 0xC0 != 0x80 {
        pos.column = pos.column.saturating_add(1);
    }
    pos.offset = pos.offset.saturating_add(1);
}
