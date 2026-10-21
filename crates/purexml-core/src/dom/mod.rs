//! Owned arena DOM.
//!
//! This module is the read-only skeleton. It stores nodes and answers
//! traversal queries. It does not parse, mutate through a public API, or
//! serialize. Those come later.
//!
//! [`NodeId`] carries a document id and a generation. Slots are never reused.
//! A removed node stays in the arena as a tombstone; [`Document::get`] returns
//! [`None`] for it, and for an id from another document.

mod document;

pub use document::{
    AttrRef, Attrs, DoctypeName, Document, DomError, DomLimit, ExpandedName, NodeId, NodeKind,
    NodeRef, Nodes,
};
