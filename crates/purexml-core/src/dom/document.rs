//! Arena storage and the read-only node API.

use std::mem::size_of;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::config::ParserConfig;
use crate::event::{XML_NAMESPACE, XMLNS_NAMESPACE};

static NEXT_DOCUMENT_ID: AtomicU64 = AtomicU64::new(1);

fn next_document_id() -> u64 {
    loop {
        let id = NEXT_DOCUMENT_ID.fetch_add(1, Ordering::Relaxed);
        if id != 0 {
            return id;
        }
    }
}

/// Handle of one node.
///
/// `document` selects the arena. `generation` changes when the node becomes a
/// tombstone, so a saved handle cannot observe a later occupant. The slot
/// index is never handed out again.
#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash)]
pub struct NodeId {
    document: u64,
    index: u32,
    generation: u32,
}

impl NodeId {
    /// Arena that minted this handle.
    pub fn document_id(self) -> u64 {
        self.document
    }

    /// Generation captured when the handle was minted.
    pub fn generation(self) -> u32 {
        self.generation
    }

    /// Slot in the arena. Tombstones keep the slot; it is not issued again.
    #[allow(dead_code)]
    pub(crate) fn index(self) -> u32 {
        self.index
    }
}

/// What a live node is.
#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash)]
pub enum NodeKind {
    Document,
    Element,
    Text,
    /// CDATA is its own node. It is not merged into adjacent text.
    CData,
    Comment,
    Pi,
    /// Name, system id, and public id only. The internal subset is not stored.
    Doctype,
}

/// Why building a document node was refused.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DomError {
    /// `actual` is strictly greater than `limit`.
    Limit {
        kind: DomLimit,
        limit: u64,
        actual: u64,
    },
}

/// DOM caps. Exceeding either one is [`DomError::Limit`].
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum DomLimit {
    /// Created nodes, including the document node and tombstones.
    NodeCount,
    /// Logical record and arena bytes, not allocator capacity.
    DomBytes,
}

#[derive(Copy, Clone)]
struct StrSpan {
    start: u32,
    len: u32,
}

#[derive(Copy, Clone)]
struct ElementData {
    namespace: Option<StrSpan>,
    prefix: Option<StrSpan>,
    local: StrSpan,
    attrs: Range,
    ns_decls: Range,
}

#[derive(Copy, Clone)]
struct Range {
    start: u32,
    len: u32,
}

#[derive(Copy, Clone)]
struct AttrData {
    namespace: Option<StrSpan>,
    prefix: Option<StrSpan>,
    local: StrSpan,
    value: StrSpan,
    specified: bool,
}

#[derive(Copy, Clone)]
struct NsDecl {
    /// `None` is the default namespace (`xmlns`).
    prefix: Option<StrSpan>,
    uri: StrSpan,
}

/// Filled by the skeleton tests. Parsing does not build nodes yet.
#[allow(dead_code)]
#[derive(Copy, Clone)]
enum Payload {
    None,
    Element(u32),
    Span(StrSpan),
    Pi {
        target: StrSpan,
        data: Option<StrSpan>,
    },
    Doctype {
        name: StrSpan,
        system: Option<StrSpan>,
        public: Option<StrSpan>,
    },
}

#[derive(Copy, Clone)]
struct Node {
    kind: NodeKind,
    alive: bool,
    generation: u32,
    parent: Option<u32>,
    prev: Option<u32>,
    next: Option<u32>,
    first_child: Option<u32>,
    last_child: Option<u32>,
    payload: Payload,
}

impl Node {
    fn root() -> Self {
        Self {
            kind: NodeKind::Document,
            alive: true,
            generation: 1,
            parent: None,
            prev: None,
            next: None,
            first_child: None,
            last_child: None,
            payload: Payload::None,
        }
    }
}

/// One XML document, owned outright.
///
/// The read API returns [`Option`] or [`Result`] and does not panic when a
/// handle is stale, foreign, or points past the arena.
pub struct Document {
    id: u64,
    nodes: Vec<Node>,
    elements: Vec<ElementData>,
    attrs: Vec<AttrData>,
    ns_decls: Vec<NsDecl>,
    arena: String,
    max_nodes: u64,
    max_dom_bytes: u64,
}

impl Document {
    /// Empty document: one live document node, default limits.
    pub fn new() -> Self {
        Self::with_config(&ParserConfig::default())
            .expect("default DOM limits accept the document node")
    }

    /// Empty document under `config`'s node and byte caps.
    ///
    /// Fails when the single document node would already exceed a cap.
    /// "Exceed" is strict: a total equal to the cap is kept.
    pub fn with_config(config: &ParserConfig) -> Result<Self, DomError> {
        let mut doc = Self {
            id: next_document_id(),
            nodes: Vec::new(),
            elements: Vec::new(),
            attrs: Vec::new(),
            ns_decls: Vec::new(),
            arena: String::new(),
            max_nodes: config.max_nodes,
            max_dom_bytes: config.max_dom_bytes,
        };
        doc.reserve(1, size_of::<Node>() as u64)?;
        doc.nodes.push(Node::root());
        Ok(doc)
    }

    /// Id shared by every handle this arena mints.
    pub fn document_id(&self) -> u64 {
        self.id
    }

    /// The document node, when it is still live.
    pub fn root(&self) -> Option<NodeRef<'_>> {
        self.node_ref(0)
    }

    /// First live element child of the document node.
    pub fn root_element(&self) -> Option<NodeRef<'_>> {
        let mut child = self.root()?.first_child();
        while let Some(node) = child {
            if node.kind() == Some(NodeKind::Element) {
                return Some(node);
            }
            child = node.next_sibling();
        }
        None
    }

    /// Live node for `id`.
    ///
    /// `None` when the handle is from another document, the generation does
    /// not match, the slot is a tombstone, or the index is out of range.
    pub fn get(&self, id: NodeId) -> Option<NodeRef<'_>> {
        if id.document != self.id {
            return None;
        }
        let node = self.nodes.get(id.index as usize)?;
        if !node.alive || node.generation != id.generation {
            return None;
        }
        Some(NodeRef { doc: self, id })
    }

    /// Nodes created in this arena, including tombstones and the document node.
    ///
    /// Attributes are not included. This is not a stable public counter.
    #[doc(hidden)]
    pub fn node_count(&self) -> u64 {
        self.nodes.len() as u64
    }

    /// Logical heap bytes. Allocator capacity is not included.
    ///
    /// Tombstones and attribute records that were copied aside still count.
    #[doc(hidden)]
    pub fn approx_heap_bytes(&self) -> u64 {
        (self.arena.len() as u64)
            .saturating_add(self.nodes.len() as u64 * size_of::<Node>() as u64)
            .saturating_add(self.elements.len() as u64 * size_of::<ElementData>() as u64)
            .saturating_add(self.attrs.len() as u64 * size_of::<AttrData>() as u64)
            .saturating_add(self.ns_decls.len() as u64 * size_of::<NsDecl>() as u64)
    }

    fn node_cap(&self) -> u64 {
        self.max_nodes.min(u32::MAX as u64)
    }

    fn byte_cap(&self) -> u64 {
        self.max_dom_bytes.min(u32::MAX as u64)
    }

    fn reserve(&self, extra_nodes: u64, extra_bytes: u64) -> Result<(), DomError> {
        if extra_nodes > 0 {
            let cap = self.node_cap();
            let next = (self.nodes.len() as u64).saturating_add(extra_nodes);
            if self.nodes.len() as u64 >= cap || next > cap {
                return Err(DomError::Limit {
                    kind: DomLimit::NodeCount,
                    limit: cap,
                    actual: next,
                });
            }
        }
        let cap = self.byte_cap();
        let next = self.approx_heap_bytes().saturating_add(extra_bytes);
        if next > cap {
            return Err(DomError::Limit {
                kind: DomLimit::DomBytes,
                limit: cap,
                actual: next,
            });
        }
        Ok(())
    }

    fn node_ref(&self, index: u32) -> Option<NodeRef<'_>> {
        let node = self.nodes.get(index as usize)?;
        if !node.alive {
            return None;
        }
        Some(NodeRef {
            doc: self,
            id: NodeId {
                document: self.id,
                index,
                generation: node.generation,
            },
        })
    }

    fn str_at(&self, span: StrSpan) -> Option<&str> {
        let start = span.start as usize;
        let end = start.checked_add(span.len as usize)?;
        self.arena.get(start..end)
    }

    fn live(&self, index: u32) -> Option<&Node> {
        let node = self.nodes.get(index as usize)?;
        if node.alive { Some(node) } else { None }
    }
}

impl Default for Document {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Debug for Document {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Document")
            .field("id", &self.id)
            .field("nodes", &self.nodes.len())
            .field("arena_bytes", &self.arena.len())
            .finish()
    }
}

/// Borrowed view of one live node.
#[derive(Copy, Clone)]
pub struct NodeRef<'a> {
    doc: &'a Document,
    id: NodeId,
}

impl<'a> NodeRef<'a> {
    pub fn id(self) -> NodeId {
        self.id
    }

    pub fn kind(self) -> Option<NodeKind> {
        Some(self.node()?.kind)
    }

    pub fn parent(self) -> Option<NodeRef<'a>> {
        let index = self.node()?.parent?;
        self.doc.node_ref(index)
    }

    pub fn first_child(self) -> Option<NodeRef<'a>> {
        self.step(self.node()?.first_child, false)
    }

    pub fn last_child(self) -> Option<NodeRef<'a>> {
        self.step(self.node()?.last_child, true)
    }

    pub fn next_sibling(self) -> Option<NodeRef<'a>> {
        self.step(self.node()?.next, false)
    }

    pub fn prev_sibling(self) -> Option<NodeRef<'a>> {
        self.step(self.node()?.prev, true)
    }

    /// Live children, in sibling order.
    pub fn children(self) -> Nodes<'a> {
        Nodes::forward(self.doc, self.node().and_then(|node| node.first_child))
    }

    /// Live siblings after this node.
    pub fn following_siblings(self) -> Nodes<'a> {
        Nodes::forward(self.doc, self.node().and_then(|node| node.next))
    }

    /// Live siblings before this node, nearest first.
    pub fn preceding_siblings(self) -> Nodes<'a> {
        let next = self.node().and_then(|node| node.prev);
        Nodes {
            doc: self.doc,
            next,
            left: self.doc.nodes.len(),
            backward: true,
        }
    }

    pub fn name(self) -> Option<ExpandedName<'a>> {
        let node = self.node()?;
        let Payload::Element(index) = node.payload else {
            return None;
        };
        let element = self.doc.elements.get(index as usize)?;
        Some(ExpandedName {
            namespace: element.namespace.and_then(|span| self.doc.str_at(span)),
            prefix: element.prefix.and_then(|span| self.doc.str_at(span)),
            local: self.doc.str_at(element.local)?,
        })
    }

    pub fn attributes(self) -> Attrs<'a> {
        let range = self.element().map(|element| element.attrs);
        let (start, end) = match range {
            Some(range) => {
                let start = range.start as usize;
                let end = start.saturating_add(range.len as usize);
                let end = end.min(self.doc.attrs.len());
                (start.min(end), end)
            }
            None => (0, 0),
        };
        Attrs {
            doc: self.doc,
            index: start,
            end,
        }
    }

    /// Attribute in no namespace with this local name.
    pub fn attribute(self, local: &str) -> Option<&'a str> {
        self.attribute_ns(None, local)
    }

    /// Attribute with this expanded name. `None` is no namespace.
    pub fn attribute_ns(self, namespace: Option<&str>, local: &str) -> Option<&'a str> {
        self.attributes().find_map(|attr| {
            let same_ns = attr.namespace() == namespace;
            if same_ns && attr.local() == local {
                Some(attr.value())
            } else {
                None
            }
        })
    }

    /// Namespace URI in scope for `prefix`.
    ///
    /// `None` looks up the default namespace. `Some("xml")` and `Some("xmlns")`
    /// resolve to the built-in bindings when no declaration overrides them.
    /// An empty stored URI is returned as `Some("")`.
    pub fn lookup_namespace(self, prefix: Option<&str>) -> Option<&'a str> {
        let mut index = match self.node()?.kind {
            NodeKind::Element => Some(self.id.index),
            _ => self.node()?.parent,
        };
        let mut left = self.doc.nodes.len();
        while let Some(current) = index {
            if left == 0 {
                return None;
            }
            left -= 1;
            if let Some(uri) = self.declared_on(current, prefix) {
                return Some(uri);
            }
            index = self.doc.nodes.get(current as usize)?.parent;
        }
        match prefix {
            Some("xml") => Some(XML_NAMESPACE),
            Some("xmlns") => Some(XMLNS_NAMESPACE),
            _ => None,
        }
    }

    /// Character data of a text, CDATA, or comment node.
    pub fn text(self) -> Option<&'a str> {
        let node = self.node()?;
        match node.kind {
            NodeKind::Text | NodeKind::CData | NodeKind::Comment => match node.payload {
                Payload::Span(span) => self.doc.str_at(span),
                _ => None,
            },
            _ => None,
        }
    }

    /// Target and optional data of a processing instruction.
    pub fn pi(self) -> Option<(&'a str, Option<&'a str>)> {
        let node = self.node()?;
        let Payload::Pi { target, data } = node.payload else {
            return None;
        };
        let target = self.doc.str_at(target)?;
        let data = match data {
            Some(span) => Some(self.doc.str_at(span)?),
            None => None,
        };
        Some((target, data))
    }

    /// Name and external ids. No internal subset is stored.
    pub fn doctype(self) -> Option<DoctypeName<'a>> {
        let node = self.node()?;
        let Payload::Doctype {
            name,
            system,
            public,
        } = node.payload
        else {
            return None;
        };
        Some(DoctypeName {
            name: self.doc.str_at(name)?,
            system_id: system.and_then(|span| self.doc.str_at(span)),
            public_id: public.and_then(|span| self.doc.str_at(span)),
        })
    }

    fn node(self) -> Option<&'a Node> {
        if self.id.document != self.doc.id {
            return None;
        }
        let node = self.doc.nodes.get(self.id.index as usize)?;
        if node.alive && node.generation == self.id.generation {
            Some(node)
        } else {
            None
        }
    }

    fn element(self) -> Option<&'a ElementData> {
        let node = self.node()?;
        let Payload::Element(index) = node.payload else {
            return None;
        };
        self.doc.elements.get(index as usize)
    }

    fn step(self, index: Option<u32>, backward: bool) -> Option<NodeRef<'a>> {
        let mut index = index;
        let mut left = self.doc.nodes.len();
        while let Some(current) = index {
            if left == 0 {
                return None;
            }
            left -= 1;
            if self.doc.live(current).is_some() {
                return self.doc.node_ref(current);
            }
            index = self
                .doc
                .nodes
                .get(current as usize)
                .and_then(|node| if backward { node.prev } else { node.next });
        }
        None
    }

    fn declared_on(self, index: u32, prefix: Option<&str>) -> Option<&'a str> {
        let node = self.doc.nodes.get(index as usize)?;
        let Payload::Element(slot) = node.payload else {
            return None;
        };
        let element = self.doc.elements.get(slot as usize)?;
        let start = element.ns_decls.start as usize;
        let end = start.saturating_add(element.ns_decls.len as usize);
        let end = end.min(self.doc.ns_decls.len());
        let start = start.min(end);
        for decl in self.doc.ns_decls.get(start..end)? {
            let decl_prefix = decl.prefix.and_then(|span| self.doc.str_at(span));
            if decl_prefix == prefix {
                return self.doc.str_at(decl.uri);
            }
        }
        None
    }
}

impl std::fmt::Debug for NodeRef<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NodeRef")
            .field("id", &self.id)
            .field("kind", &self.kind())
            .finish()
    }
}

/// Expanded name stored on an element.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct ExpandedName<'a> {
    pub namespace: Option<&'a str>,
    pub prefix: Option<&'a str>,
    pub local: &'a str,
}

/// Doctype identity. System and public ids are optional; there is no subset.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct DoctypeName<'a> {
    pub name: &'a str,
    pub system_id: Option<&'a str>,
    pub public_id: Option<&'a str>,
}

/// Forward, or preceding, sibling walk. A cycle or a dead link ends the walk.
pub struct Nodes<'a> {
    doc: &'a Document,
    next: Option<u32>,
    left: usize,
    backward: bool,
}

impl<'a> Nodes<'a> {
    fn forward(doc: &'a Document, next: Option<u32>) -> Self {
        Self {
            doc,
            next,
            left: doc.nodes.len(),
            backward: false,
        }
    }
}

impl<'a> Iterator for Nodes<'a> {
    type Item = NodeRef<'a>;

    fn next(&mut self) -> Option<Self::Item> {
        while let Some(index) = self.next {
            if self.left == 0 {
                return None;
            }
            self.left -= 1;
            let node = self.doc.nodes.get(index as usize)?;
            self.next = if self.backward { node.prev } else { node.next };
            if node.alive {
                return self.doc.node_ref(index);
            }
        }
        None
    }
}

/// Attributes of one element, in stored order.
pub struct Attrs<'a> {
    doc: &'a Document,
    index: usize,
    end: usize,
}

impl<'a> Iterator for Attrs<'a> {
    type Item = AttrRef<'a>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.index >= self.end {
            return None;
        }
        let index = self.index;
        self.index += 1;
        self.doc.attrs.get(index).map(|_| AttrRef {
            doc: self.doc,
            index,
        })
    }
}

/// One attribute. `specified` is false for an injected default.
#[derive(Copy, Clone)]
pub struct AttrRef<'a> {
    doc: &'a Document,
    index: usize,
}

impl<'a> AttrRef<'a> {
    pub fn namespace(self) -> Option<&'a str> {
        self.data()?
            .namespace
            .and_then(|span| self.doc.str_at(span))
    }

    pub fn prefix(self) -> Option<&'a str> {
        self.data()?.prefix.and_then(|span| self.doc.str_at(span))
    }

    pub fn local(self) -> &'a str {
        self.data()
            .and_then(|attr| self.doc.str_at(attr.local))
            .unwrap_or("")
    }

    pub fn value(self) -> &'a str {
        self.data()
            .and_then(|attr| self.doc.str_at(attr.value))
            .unwrap_or("")
    }

    pub fn specified(self) -> bool {
        self.data().map(|attr| attr.specified).unwrap_or(false)
    }

    fn data(self) -> Option<&'a AttrData> {
        self.doc.attrs.get(self.index)
    }
}

impl std::fmt::Debug for AttrRef<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AttrRef")
            .field("local", &self.local())
            .field("specified", &self.specified())
            .finish()
    }
}

/// Fixture assembly for the skeleton tests. Not a public mutation API.
#[cfg(test)]
impl Document {
    fn intern(&mut self, text: &str) -> Result<StrSpan, DomError> {
        let start = u32::try_from(self.arena.len()).unwrap_or(u32::MAX);
        let len = u32::try_from(text.len()).unwrap_or(u32::MAX);
        if start == u32::MAX || len == u32::MAX || start.checked_add(len).is_none() {
            return Err(DomError::Limit {
                kind: DomLimit::DomBytes,
                limit: self.byte_cap(),
                actual: self.byte_cap().saturating_add(1),
            });
        }
        self.arena.push_str(text);
        Ok(StrSpan { start, len })
    }

    fn opt_str(&mut self, text: Option<&str>) -> Result<Option<StrSpan>, DomError> {
        match text {
            Some(text) => self.intern(text).map(Some),
            None => Ok(None),
        }
    }

    fn link_child(&mut self, parent: u32, child: u32) {
        let Some(parent_node) = self.nodes.get(parent as usize).copied() else {
            return;
        };
        if let Some(last) = parent_node.last_child {
            if let Some(prev) = self.nodes.get_mut(last as usize) {
                prev.next = Some(child);
            }
            if let Some(node) = self.nodes.get_mut(child as usize) {
                node.prev = Some(last);
            }
        } else if let Some(node) = self.nodes.get_mut(parent as usize) {
            node.first_child = Some(child);
        }
        if let Some(node) = self.nodes.get_mut(parent as usize) {
            node.last_child = Some(child);
            if node.first_child.is_none() {
                node.first_child = Some(child);
            }
        }
        if let Some(node) = self.nodes.get_mut(child as usize) {
            node.parent = Some(parent);
        }
    }

    fn alloc(
        &mut self,
        kind: NodeKind,
        payload: Payload,
        extra_bytes: u64,
    ) -> Result<u32, DomError> {
        let bytes = (size_of::<Node>() as u64).saturating_add(extra_bytes);
        self.reserve(1, bytes)?;
        let index = self.nodes.len() as u32;
        self.nodes.push(Node {
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
        Ok(index)
    }

    fn add_child(
        &mut self,
        parent: NodeId,
        kind: NodeKind,
        payload: Payload,
        extra_bytes: u64,
    ) -> Result<NodeId, DomError> {
        let Some(parent_index) = self.live_index(parent) else {
            return Err(DomError::Limit {
                kind: DomLimit::NodeCount,
                limit: self.node_cap(),
                actual: self.node_count(),
            });
        };
        let index = self.alloc(kind, payload, extra_bytes)?;
        self.link_child(parent_index, index);
        Ok(self.node_ref(index).expect("just allocated").id())
    }

    fn live_index(&self, id: NodeId) -> Option<u32> {
        self.get(id).map(|node| node.id.index)
    }

    fn add_element(
        &mut self,
        parent: NodeId,
        namespace: Option<&str>,
        prefix: Option<&str>,
        local: &str,
    ) -> Result<NodeId, DomError> {
        let extra = local.len()
            + namespace.map(str::len).unwrap_or(0)
            + prefix.map(str::len).unwrap_or(0)
            + size_of::<ElementData>();
        self.reserve(1, size_of::<Node>() as u64 + extra as u64)?;
        let namespace = self.opt_str(namespace)?;
        let prefix = self.opt_str(prefix)?;
        let local = self.intern(local)?;
        let slot = self.elements.len() as u32;
        self.elements.push(ElementData {
            namespace,
            prefix,
            local,
            attrs: Range { start: 0, len: 0 },
            ns_decls: Range { start: 0, len: 0 },
        });
        let index = self.nodes.len() as u32;
        self.nodes.push(Node {
            kind: NodeKind::Element,
            alive: true,
            generation: 1,
            parent: None,
            prev: None,
            next: None,
            first_child: None,
            last_child: None,
            payload: Payload::Element(slot),
        });
        if let Some(parent_index) = self.live_index(parent) {
            self.link_child(parent_index, index);
        }
        Ok(NodeId {
            document: self.id,
            index,
            generation: 1,
        })
    }

    fn add_text(&mut self, parent: NodeId, text: &str) -> Result<NodeId, DomError> {
        self.add_span(parent, NodeKind::Text, text)
    }

    fn add_cdata(&mut self, parent: NodeId, text: &str) -> Result<NodeId, DomError> {
        self.add_span(parent, NodeKind::CData, text)
    }

    fn add_comment(&mut self, parent: NodeId, text: &str) -> Result<NodeId, DomError> {
        self.add_span(parent, NodeKind::Comment, text)
    }

    fn add_span(&mut self, parent: NodeId, kind: NodeKind, text: &str) -> Result<NodeId, DomError> {
        self.reserve(1, size_of::<Node>() as u64 + text.len() as u64)?;
        let span = self.intern(text)?;
        self.add_child(parent, kind, Payload::Span(span), 0)
    }

    fn add_pi(
        &mut self,
        parent: NodeId,
        target: &str,
        data: Option<&str>,
    ) -> Result<NodeId, DomError> {
        let extra = target.len() + data.map(str::len).unwrap_or(0);
        self.reserve(1, size_of::<Node>() as u64 + extra as u64)?;
        let target = self.intern(target)?;
        let data = self.opt_str(data)?;
        self.add_child(parent, NodeKind::Pi, Payload::Pi { target, data }, 0)
    }

    fn add_doctype(
        &mut self,
        parent: NodeId,
        name: &str,
        system: Option<&str>,
        public: Option<&str>,
    ) -> Result<NodeId, DomError> {
        let extra =
            name.len() + system.map(str::len).unwrap_or(0) + public.map(str::len).unwrap_or(0);
        self.reserve(1, size_of::<Node>() as u64 + extra as u64)?;
        let name = self.intern(name)?;
        let system = self.opt_str(system)?;
        let public_id = self.opt_str(public)?;
        self.add_child(
            parent,
            NodeKind::Doctype,
            Payload::Doctype {
                name,
                system,
                public: public_id,
            },
            0,
        )
    }

    fn add_attr(
        &mut self,
        element: NodeId,
        namespace: Option<&str>,
        prefix: Option<&str>,
        local: &str,
        value: &str,
        specified: bool,
    ) -> Result<(), DomError> {
        let Some(slot) = self.element_slot(element) else {
            return Ok(());
        };
        let copy = self.attr_copy_len(slot);
        let extra = size_of::<AttrData>() * copy
            + local.len()
            + value.len()
            + namespace.map(str::len).unwrap_or(0)
            + prefix.map(str::len).unwrap_or(0);
        self.reserve(0, extra as u64)?;
        let namespace = self.opt_str(namespace)?;
        let prefix = self.opt_str(prefix)?;
        let local = self.intern(local)?;
        let value = self.intern(value)?;
        let attr = AttrData {
            namespace,
            prefix,
            local,
            value,
            specified,
        };
        self.store_attr(slot, attr);
        Ok(())
    }

    fn declare_ns(
        &mut self,
        element: NodeId,
        prefix: Option<&str>,
        uri: &str,
    ) -> Result<(), DomError> {
        let Some(slot) = self.element_slot(element) else {
            return Ok(());
        };
        let copy = self.ns_copy_len(slot);
        let extra = size_of::<NsDecl>() * copy + uri.len() + prefix.map(str::len).unwrap_or(0);
        self.reserve(0, extra as u64)?;
        let prefix = self.opt_str(prefix)?;
        let uri = self.intern(uri)?;
        self.store_ns(slot, NsDecl { prefix, uri });
        Ok(())
    }

    fn element_slot(&self, id: NodeId) -> Option<u32> {
        let node = self.get(id)?;
        match node.node()?.payload {
            Payload::Element(slot) => Some(slot),
            _ => None,
        }
    }

    fn attr_copy_len(&self, slot: u32) -> usize {
        let Some(element) = self.elements.get(slot as usize) else {
            return 1;
        };
        let at_end = element.attrs.start + element.attrs.len == self.attrs.len() as u32;
        if at_end || element.attrs.len == 0 {
            1
        } else {
            element.attrs.len as usize + 1
        }
    }

    fn ns_copy_len(&self, slot: u32) -> usize {
        let Some(element) = self.elements.get(slot as usize) else {
            return 1;
        };
        let at_end = element.ns_decls.start + element.ns_decls.len == self.ns_decls.len() as u32;
        if at_end || element.ns_decls.len == 0 {
            1
        } else {
            element.ns_decls.len as usize + 1
        }
    }

    fn store_attr(&mut self, slot: u32, attr: AttrData) {
        let Some(element) = self.elements.get(slot as usize).copied() else {
            return;
        };
        let at_end = element.attrs.len == 0
            || element.attrs.start + element.attrs.len == self.attrs.len() as u32;
        if at_end {
            let start = if element.attrs.len == 0 {
                self.attrs.len() as u32
            } else {
                element.attrs.start
            };
            self.attrs.push(attr);
            if let Some(element) = self.elements.get_mut(slot as usize) {
                element.attrs = Range {
                    start,
                    len: element.attrs.len + 1,
                };
            }
            return;
        }
        let start = element.attrs.start as usize;
        let end = start + element.attrs.len as usize;
        let mut copied = self.attrs.get(start..end).unwrap_or(&[]).to_vec();
        copied.push(attr);
        let new_start = self.attrs.len() as u32;
        let new_len = copied.len() as u32;
        self.attrs.extend(copied);
        if let Some(element) = self.elements.get_mut(slot as usize) {
            element.attrs = Range {
                start: new_start,
                len: new_len,
            };
        }
    }

    fn store_ns(&mut self, slot: u32, decl: NsDecl) {
        let Some(element) = self.elements.get(slot as usize).copied() else {
            return;
        };
        let at_end = element.ns_decls.len == 0
            || element.ns_decls.start + element.ns_decls.len == self.ns_decls.len() as u32;
        if at_end {
            let start = if element.ns_decls.len == 0 {
                self.ns_decls.len() as u32
            } else {
                element.ns_decls.start
            };
            self.ns_decls.push(decl);
            if let Some(element) = self.elements.get_mut(slot as usize) {
                element.ns_decls = Range {
                    start,
                    len: element.ns_decls.len + 1,
                };
            }
            return;
        }
        let start = element.ns_decls.start as usize;
        let end = start + element.ns_decls.len as usize;
        let mut copied = self.ns_decls.get(start..end).unwrap_or(&[]).to_vec();
        copied.push(decl);
        let new_start = self.ns_decls.len() as u32;
        let new_len = copied.len() as u32;
        self.ns_decls.extend(copied);
        if let Some(element) = self.elements.get_mut(slot as usize) {
            element.ns_decls = Range {
                start: new_start,
                len: new_len,
            };
        }
    }

    fn tombstone(&mut self, id: NodeId) {
        let Some(index) = self.live_index(id) else {
            return;
        };
        let Some(node) = self.nodes.get(index as usize).copied() else {
            return;
        };
        let parent = node.parent;
        let prev = node.prev;
        let next = node.next;
        if let Some(prev) = prev {
            if let Some(sibling) = self.nodes.get_mut(prev as usize) {
                sibling.next = next;
            }
        }
        if let Some(next) = next {
            if let Some(sibling) = self.nodes.get_mut(next as usize) {
                sibling.prev = prev;
            }
        }
        if let Some(parent) = parent {
            if let Some(parent_node) = self.nodes.get_mut(parent as usize) {
                if parent_node.first_child == Some(index) {
                    parent_node.first_child = next;
                }
                if parent_node.last_child == Some(index) {
                    parent_node.last_child = prev;
                }
            }
        }
        if let Some(node) = self.nodes.get_mut(index as usize) {
            node.alive = false;
            node.generation = node.generation.saturating_add(1);
            node.parent = None;
            node.prev = None;
            node.next = None;
        }
    }

    fn test_set_next(&mut self, id: NodeId, next: Option<u32>) {
        if let Some(index) = self.live_index(id) {
            if let Some(node) = self.nodes.get_mut(index as usize) {
                node.next = next;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        DEFAULT_MAX_DOM_BYTES, DEFAULT_MAX_NODES, ParserConfig, XML_NAMESPACE, XMLNS_NAMESPACE,
    };

    #[test]
    fn defaults_are_ten_million_nodes_and_one_gib() {
        let config = ParserConfig::default();
        assert_eq!(config.max_nodes, DEFAULT_MAX_NODES);
        assert_eq!(DEFAULT_MAX_NODES, 10_000_000);
        assert_eq!(config.max_dom_bytes, DEFAULT_MAX_DOM_BYTES);
        assert_eq!(DEFAULT_MAX_DOM_BYTES, 1024 * 1024 * 1024);
        let doc = Document::new();
        assert_eq!(doc.node_count(), 1);
        assert!(doc.approx_heap_bytes() >= size_of::<Node>() as u64);
        assert!(doc.root().is_some());
        assert_eq!(
            doc.root().and_then(|node| node.kind()),
            Some(NodeKind::Document)
        );
        assert!(doc.root().unwrap().parent().is_none());
    }

    #[test]
    fn traversal_covers_every_kind() {
        let mut doc = Document::new();
        let root = doc.root().unwrap().id();
        let doctype = doc
            .add_doctype(root, "r", Some("sys"), Some("pub"))
            .unwrap();
        let element = doc.add_element(root, None, None, "r").unwrap();
        let text = doc.add_text(element, "hi").unwrap();
        let cdata = doc.add_cdata(element, "CD").unwrap();
        let comment = doc.add_comment(element, "c").unwrap();
        let pi = doc.add_pi(element, "t", Some("d")).unwrap();
        let pi_empty = doc.add_pi(element, "u", None).unwrap();
        let child = doc
            .add_element(element, Some("urn:ex"), Some("p"), "child")
            .unwrap();

        assert_eq!(
            doc.get(doctype).unwrap().doctype(),
            Some(DoctypeName {
                name: "r",
                system_id: Some("sys"),
                public_id: Some("pub"),
            })
        );
        assert!(doc.get(doctype).unwrap().text().is_none());
        assert_eq!(doc.root_element().unwrap().id(), element);
        assert_eq!(doc.get(element).unwrap().name().unwrap().local, "r");
        assert_eq!(doc.get(text).unwrap().text(), Some("hi"));
        assert_eq!(doc.get(text).unwrap().kind(), Some(NodeKind::Text));
        assert_eq!(doc.get(cdata).unwrap().kind(), Some(NodeKind::CData));
        assert_eq!(doc.get(cdata).unwrap().text(), Some("CD"));
        assert_eq!(doc.get(comment).unwrap().text(), Some("c"));
        assert_eq!(doc.get(pi).unwrap().pi(), Some(("t", Some("d"))));
        assert_eq!(doc.get(pi_empty).unwrap().pi(), Some(("u", None)));
        let name = doc.get(child).unwrap().name().unwrap();
        assert_eq!(name.namespace, Some("urn:ex"));
        assert_eq!(name.prefix, Some("p"));
        assert_eq!(name.local, "child");

        assert_eq!(doc.get(text).unwrap().parent().unwrap().id(), element);
        assert_eq!(doc.get(text).unwrap().next_sibling().unwrap().id(), cdata);
        assert_eq!(doc.get(cdata).unwrap().prev_sibling().unwrap().id(), text);
        assert_eq!(doc.get(element).unwrap().first_child().unwrap().id(), text);
        assert_eq!(doc.get(element).unwrap().last_child().unwrap().id(), child);
        let kids: Vec<_> = doc
            .get(element)
            .unwrap()
            .children()
            .map(|node| node.kind().unwrap())
            .collect();
        assert_eq!(
            kids,
            vec![
                NodeKind::Text,
                NodeKind::CData,
                NodeKind::Comment,
                NodeKind::Pi,
                NodeKind::Pi,
                NodeKind::Element,
            ]
        );
        let after: Vec<_> = doc
            .get(text)
            .unwrap()
            .following_siblings()
            .map(|node| node.id())
            .collect();
        assert_eq!(after, vec![cdata, comment, pi, pi_empty, child]);
        assert_eq!(
            doc.get(cdata)
                .unwrap()
                .preceding_siblings()
                .next()
                .unwrap()
                .id(),
            text
        );
        assert_eq!(doc.node_count(), 9);
    }

    #[test]
    fn attributes_keep_the_specified_flag_and_namespaces_walk() {
        let mut doc = Document::new();
        let root = doc.root().unwrap().id();
        let element = doc.add_element(root, None, None, "r").unwrap();
        doc.declare_ns(element, None, "urn:def").unwrap();
        doc.declare_ns(element, Some("p"), "urn:ex").unwrap();
        doc.add_attr(element, None, None, "id", "1", true).unwrap();
        doc.add_attr(element, Some("urn:ex"), Some("p"), "a", "2", false)
            .unwrap();
        let child = doc.add_element(element, None, None, "c").unwrap();
        let other = doc.add_element(root, None, None, "o").unwrap();
        doc.add_attr(other, None, None, "z", "9", true).unwrap();
        // `other`'s attribute sits at the end of the attr arena, so `later`
        // has to copy `element`'s existing range aside.
        doc.add_attr(element, None, None, "later", "3", true)
            .unwrap();

        let attrs: Vec<_> = doc
            .get(element)
            .unwrap()
            .attributes()
            .map(|attr| {
                (
                    attr.local(),
                    attr.value(),
                    attr.specified(),
                    attr.namespace(),
                )
            })
            .collect();
        assert_eq!(
            attrs,
            vec![
                ("id", "1", true, None),
                ("a", "2", false, Some("urn:ex")),
                ("later", "3", true, None),
            ]
        );
        assert_eq!(doc.get(element).unwrap().attribute("id"), Some("1"));
        assert_eq!(
            doc.get(element).unwrap().attribute_ns(Some("urn:ex"), "a"),
            Some("2")
        );
        assert_eq!(doc.get(element).unwrap().attribute("a"), None);
        assert_eq!(
            doc.get(child).unwrap().lookup_namespace(None),
            Some("urn:def")
        );
        assert_eq!(
            doc.get(child).unwrap().lookup_namespace(Some("p")),
            Some("urn:ex")
        );
        assert_eq!(
            doc.get(child).unwrap().lookup_namespace(Some("xml")),
            Some(XML_NAMESPACE)
        );
        assert_eq!(
            doc.get(child).unwrap().lookup_namespace(Some("xmlns")),
            Some(XMLNS_NAMESPACE)
        );
        assert_eq!(
            doc.get(child).unwrap().lookup_namespace(Some("missing")),
            None
        );
        assert!(doc.get(child).unwrap().attributes().next().is_none());
        assert!(doc.get(child).unwrap().attribute("id").is_none());
    }

    #[test]
    fn tombstone_keeps_the_slot_and_rejects_stale_handles() {
        let mut doc = Document::new();
        let root = doc.root().unwrap().id();
        let element = doc.add_element(root, None, None, "r").unwrap();
        let text = doc.add_text(element, "x").unwrap();
        let before = doc.node_count();
        let bytes = doc.approx_heap_bytes();
        doc.tombstone(text);
        assert_eq!(doc.node_count(), before);
        assert_eq!(doc.approx_heap_bytes(), bytes);
        assert!(doc.get(text).is_none());
        assert!(doc.get(element).unwrap().first_child().is_none());
        let forged = NodeId {
            document: text.document_id(),
            index: text.index(),
            generation: text.generation().saturating_add(1),
        };
        assert!(doc.get(forged).is_none());
        let next = doc.add_text(element, "y").unwrap();
        assert!(next.index() > text.index());
        assert_ne!(next.index(), text.index());
        assert!(doc.get(text).is_none());
        assert_eq!(doc.get(next).unwrap().text(), Some("y"));
    }

    #[test]
    fn foreign_document_id_does_not_resolve() {
        let first = Document::new();
        let second = Document::new();
        let id = first.root().unwrap().id();
        assert_ne!(first.document_id(), second.document_id());
        assert!(second.get(id).is_none());
        assert_eq!(id.document_id(), first.document_id());
    }

    #[test]
    fn node_cap_counts_the_document_node_and_tombstones() {
        let config = ParserConfig::default().with_max_nodes(0);
        let err = Document::with_config(&config).unwrap_err();
        assert_eq!(
            err,
            DomError::Limit {
                kind: DomLimit::NodeCount,
                limit: 0,
                actual: 1,
            }
        );

        let config = ParserConfig::default().with_max_nodes(2);
        let mut doc = Document::with_config(&config).unwrap();
        let root = doc.root().unwrap().id();
        let element = doc.add_element(root, None, None, "r").unwrap();
        doc.tombstone(element);
        assert_eq!(doc.node_count(), 2);
        let err = doc.add_text(root, "z").unwrap_err();
        assert_eq!(
            err,
            DomError::Limit {
                kind: DomLimit::NodeCount,
                limit: 2,
                actual: 3,
            }
        );
        assert_eq!(doc.node_count(), 2);
    }

    #[test]
    fn byte_cap_is_logical_length() {
        let empty = size_of::<Node>() as u64;
        let config = ParserConfig::default().with_max_dom_bytes(empty);
        let mut doc = Document::with_config(&config).unwrap();
        assert_eq!(doc.approx_heap_bytes(), empty);
        let root = doc.root().unwrap().id();
        let err = doc.add_text(root, "a").unwrap_err();
        match err {
            DomError::Limit {
                kind: DomLimit::DomBytes,
                limit,
                actual,
            } => {
                assert_eq!(limit, empty);
                assert!(actual > limit);
            }
            other => panic!("expected DomBytes, got {other:?}"),
        }
        assert_eq!(doc.node_count(), 1);
        assert_eq!(doc.approx_heap_bytes(), empty);
    }

    #[test]
    fn broken_sibling_links_end_instead_of_panicking() {
        let mut doc = Document::new();
        let root = doc.root().unwrap().id();
        let element = doc.add_element(root, None, None, "r").unwrap();
        let text = doc.add_text(element, "a").unwrap();
        doc.add_text(element, "b").unwrap();
        doc.test_set_next(text, Some(u32::MAX));
        let kids: Vec<_> = doc.get(element).unwrap().children().collect();
        assert_eq!(kids.len(), 1);

        doc.test_set_next(text, Some(text.index()));
        let mut steps = 0;
        for _ in doc.get(element).unwrap().children() {
            steps += 1;
            assert!(steps <= doc.node_count() as usize);
        }
        assert!(steps <= doc.node_count() as usize);
    }
}
