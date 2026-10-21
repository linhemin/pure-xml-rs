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
//!
//! An entity-reference name is an NCName. `&a:b;` is [`ErrorKind::Syntax`]
//! because the name contains a colon. It is not reported as an undefined entity.

#![forbid(unsafe_code)]

mod config;
mod error;
mod event;
mod name;
mod reader;
mod rules;
mod wf;
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
    StartElement, XmlDecl, XmlText, XMLNS_NAMESPACE, XML_NAMESPACE,
};
pub use reader::Reader;
pub use writer::{escape_attribute, escape_text, WriteError, Writer};

/// Public event and error types are shareable across threads.
const _: fn() = || {
    fn assert<T: Send + Sync>() {}
    assert::<Event<'static>>();
    assert::<XmlText<'static>>();
    assert::<QName<'static>>();
    assert::<Attribute<'static>>();
    assert::<Error>();
    assert::<StartElement<'static>>();
    assert::<EndElement<'static>>();
    assert::<EmptyElement<'static>>();
    assert::<Namespace<'static>>();
    assert::<ProcessingInstruction<'static>>();
    assert::<XmlDecl<'static>>();
    assert::<Doctype<'static>>();
};
