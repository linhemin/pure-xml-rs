//! Owned arena DOM.
//!
//! Nodes live in one arena. Handles are [`NodeId`]s and are not reused.
//! Mutation checks the same name, character, and namespace rules as the
//! reader and the writer, and serialization emits events for that writer.
//!
//! [`NodeId`] carries a document id and a generation. Slots are never reused.
//! A removed node stays in the arena as a tombstone; [`Document::get`] returns
//! [`None`] for it, and for an id from another document.

mod document;
mod edit;
mod ser;

pub use document::{
    AttrRef, Attrs, DoctypeName, Document, DomError, DomLimit, ExpandedName, NodeId, NodeKind,
    NodeRef, Nodes,
};
pub use edit::{Name, WriteOptions};
