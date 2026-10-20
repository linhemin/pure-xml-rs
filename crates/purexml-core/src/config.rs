//! Parser limits. Defaults are safe; every field is tunable, but nothing in
//! this configuration can enable external entity or external DTD loading.

/// Maximum element nesting depth (the 257th open element fails by default).
pub const DEFAULT_MAX_DEPTH: usize = 256;

/// Maximum attributes plus namespace declarations on a single element.
pub const DEFAULT_MAX_ATTRS: usize = 1024;

/// Maximum length, in UTF-8 bytes, of one name or qualified name.
pub const DEFAULT_MAX_NAME_LEN: usize = 1024;

/// Maximum length, in UTF-8 bytes, of one text node and of one normalized
/// attribute value.
pub const DEFAULT_MAX_TEXT_LEN: u64 = 16 * 1024 * 1024;

/// Maximum original input size, in bytes.
pub const DEFAULT_MAX_TOTAL_INPUT: u64 = 1024 * 1024 * 1024;

/// Maximum general-entity nesting depth. A reference in the document is depth 1.
pub const DEFAULT_MAX_ENTITY_DEPTH: usize = 8;

/// Maximum number of general-entity expansions, including repeats.
pub const DEFAULT_MAX_ENTITY_EXPANSIONS: u64 = 10_000;

/// Maximum cumulative replacement-text bytes produced by expansions.
pub const DEFAULT_MAX_EXPANDED_BYTES: u64 = 1024 * 1024;

/// The expansion-ratio numerator must not exceed
/// `bytes_consumed * max_expansion_ratio` once
/// [`EXPANSION_RATIO_FLOOR_BYTES`] is passed.
///
/// The numerator is cumulative general-entity replacement bytes, plus the
/// name bytes of every injected default attribute, plus each shared default
/// value's length the first time that default is injected. Later elements
/// that reuse the same `Arc` do not add the value again. Injected defaults
/// do not count toward [`DEFAULT_MAX_EXPANDED_BYTES`] or
/// [`DEFAULT_MAX_ENTITY_EXPANSIONS`].
pub const DEFAULT_MAX_EXPANSION_RATIO: u64 = 10;

/// The expansion-ratio check is skipped while the ratio numerator is less
/// than or equal to this floor.
pub const EXPANSION_RATIO_FLOOR_BYTES: u64 = 64 * 1024;

/// Tunable limits for [`crate::Reader`].
///
/// External entities and external DTDs cannot be enabled. There is no field
/// for that, on purpose.
///
/// The 0.x configuration shape may still change.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParserConfig {
    /// Element nesting cap. Exceeding yields [`crate::LimitKind::ElementDepth`].
    pub max_depth: usize,
    /// Attributes and namespace declarations allowed on one element.
    pub max_attrs: usize,
    /// Maximum UTF-8 byte length of a name or qualified name.
    pub max_name_len: usize,
    /// Maximum UTF-8 byte length of one text node, and of one attribute value
    /// after normalization and entity expansion.
    pub max_text_len: u64,
    /// Maximum original document size in bytes.
    pub max_total_input: u64,
    /// General-entity nesting cap. Depth 1 is a reference in the document.
    pub max_entity_depth: usize,
    /// How many general-entity expansions are allowed, counting repeats.
    pub max_entity_expansions: u64,
    /// Cumulative general-entity replacement bytes. Injected default
    /// attributes do not count toward this cap.
    pub max_expanded_bytes: u64,
    /// Streaming ratio cap. The numerator includes entity replacement bytes,
    /// every injected default attribute's name, and each shared default
    /// value once. See [`DEFAULT_MAX_EXPANSION_RATIO`].
    pub max_expansion_ratio: u64,
}

impl ParserConfig {
    /// Same as [`Default::default`].
    pub fn new() -> Self {
        Self::default()
    }

    /// Replace the element-depth cap.
    pub fn with_max_depth(mut self, max_depth: usize) -> Self {
        self.max_depth = max_depth;
        self
    }

    /// Replace the per-element attribute cap.
    pub fn with_max_attrs(mut self, max_attrs: usize) -> Self {
        self.max_attrs = max_attrs;
        self
    }

    /// Replace the name-length cap.
    pub fn with_max_name_len(mut self, max_name_len: usize) -> Self {
        self.max_name_len = max_name_len;
        self
    }

    /// Replace the text-node and attribute-value cap.
    pub fn with_max_text_len(mut self, max_text_len: u64) -> Self {
        self.max_text_len = max_text_len;
        self
    }

    /// Replace the total-input cap.
    pub fn with_max_total_input(mut self, max_total_input: u64) -> Self {
        self.max_total_input = max_total_input;
        self
    }

    /// Replace the entity-depth cap.
    pub fn with_max_entity_depth(mut self, max_entity_depth: usize) -> Self {
        self.max_entity_depth = max_entity_depth;
        self
    }

    /// Replace the entity-expansion count cap.
    pub fn with_max_entity_expansions(mut self, max_entity_expansions: u64) -> Self {
        self.max_entity_expansions = max_entity_expansions;
        self
    }

    /// Replace the absolute expanded-bytes cap.
    pub fn with_max_expanded_bytes(mut self, max_expanded_bytes: u64) -> Self {
        self.max_expanded_bytes = max_expanded_bytes;
        self
    }

    /// Replace the expansion-ratio cap.
    pub fn with_max_expansion_ratio(mut self, max_expansion_ratio: u64) -> Self {
        self.max_expansion_ratio = max_expansion_ratio;
        self
    }
}

impl Default for ParserConfig {
    fn default() -> Self {
        Self {
            max_depth: DEFAULT_MAX_DEPTH,
            max_attrs: DEFAULT_MAX_ATTRS,
            max_name_len: DEFAULT_MAX_NAME_LEN,
            max_text_len: DEFAULT_MAX_TEXT_LEN,
            max_total_input: DEFAULT_MAX_TOTAL_INPUT,
            max_entity_depth: DEFAULT_MAX_ENTITY_DEPTH,
            max_entity_expansions: DEFAULT_MAX_ENTITY_EXPANSIONS,
            max_expanded_bytes: DEFAULT_MAX_EXPANDED_BYTES,
            max_expansion_ratio: DEFAULT_MAX_EXPANSION_RATIO,
        }
    }
}
