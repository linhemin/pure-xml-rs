//! Security-first streaming XML parser and writer.
//!
//! This is the 0.x core of `purexml`. The public API is unstable until 1.0.
//!
//! The parser is a pull reader ([`Reader::read_event`]) with safe defaults:
//! external entities and external DTDs are never loaded, parameter-entity
//! references are rejected, and entity expansion is capped by depth, count,
//! absolute size, and a streaming expansion ratio. There is no configuration
//! switch that turns those protections off.
//!
//! UTF-8 is the only accepted encoding. A UTF-16 BOM or a UTF-16 encoding
//! declaration is an [`ErrorKind::Encoding`] error. This crate does not transcode.

#![forbid(unsafe_code)]

mod config;
mod error;
mod event;
mod name;
mod reader;
mod writer;

pub use config::{
    ParserConfig, DEFAULT_MAX_ATTRS, DEFAULT_MAX_DEPTH, DEFAULT_MAX_ENTITY_DEPTH,
    DEFAULT_MAX_ENTITY_EXPANSIONS, DEFAULT_MAX_EXPANDED_BYTES, DEFAULT_MAX_EXPANSION_RATIO,
    DEFAULT_MAX_NAME_LEN, DEFAULT_MAX_TEXT_LEN, DEFAULT_MAX_TOTAL_INPUT,
    EXPANSION_RATIO_FLOOR_BYTES,
};
pub use error::{Error, ErrorKind, LimitKind, Position, Result};
pub use event::{
    Attribute, Doctype, EmptyElement, EndElement, Event, Namespace, ProcessingInstruction, QName,
    StartElement, XmlDecl, XMLNS_NAMESPACE, XML_NAMESPACE,
};
pub use reader::Reader;
pub use writer::{escape_attribute, escape_text, WriteError, Writer};
