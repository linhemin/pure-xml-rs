//! DOM mutation. Every `Err` returns before the arena or the links change.

use std::cmp::Ordering;
use std::mem::size_of;

use crate::dom::document::{
    AttrData, Document, DomError, DomLimit, ElementData, Node, NodeId, NodeKind, NsDecl, Payload,
    Range, StrSpan,
};
use crate::event::{XML_NAMESPACE, XMLNS_NAMESPACE};
use crate::name::{is_pubid_char, is_xml_char};
use crate::rules::{self, BindingFault};

/// A name to store on an element or attribute.
///
/// `local` is the NCName. `prefix` is the lexical prefix, without a colon.
/// `namespace` is the URI that prefix must resolve to. `None` is no namespace.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct Name<'a> {
    /// Resolved namespace URI.
    pub namespace: Option<&'a str>,
    /// Lexical prefix. `None` means the name is unprefixed.
    pub prefix: Option<&'a str>,
    /// Local name.
    pub local: &'a str,
}

impl<'a> Name<'a> {
    /// An unprefixed name in no namespace.
    pub fn local(local: &'a str) -> Self {
        Self {
            namespace: None,
            prefix: None,
            local,
        }
    }

    /// A name with a resolved URI and an optional prefix.
    ///
    /// `Name::qualified("urn:x", Some("p"), "a")` is `p:a` in `urn:x`.
    /// `Name::qualified("urn:x", None, "a")` is an unprefixed name in that
    /// default namespace.
    pub fn qualified(namespace: &'a str, prefix: Option<&'a str>, local: &'a str) -> Self {
        Self {
            namespace: Some(namespace),
            prefix,
            local,
        }
    }
}

/// Options for [`Document::write_to_with`](Document::write_to_with).
#[derive(Copy, Clone, Debug, Default, Eq, PartialEq)]
pub struct WriteOptions {
    /// Skip document-type nodes. Default is to write them.
    pub omit_doctype: bool,
}

impl Document {
    /// Preorder comparison. The first call builds a cache; a later link change drops it.
    pub fn cmp_document_order(&mut self, a: NodeId, b: NodeId) -> Result<Ordering, DomError> {
        let left = self.require(a)?;
        let right = self.require(b)?;
        self.ensure_order();
        let cache = self
            .order_cache
            .as_ref()
            .expect("ensure_order fills the cache");
        let rank = |index: u32| cache.get(index as usize).copied().unwrap_or(u32::MAX);
        Ok(rank(left).cmp(&rank(right)).then(left.cmp(&right)))
    }

    /// `Some` after [`Self::cmp_document_order`] until the tree shape changes.
    #[doc(hidden)]
    pub fn order_cache_len(&self) -> Option<usize> {
        self.order_cache.as_ref().map(Vec::len)
    }

    /// A detached element. Insert it with [`Self::append_child`] or the insert methods.
    pub fn create_element(&mut self, name: Name<'_>) -> Result<NodeId, DomError> {
        self.check_element_name(name)?;
        let strings = name_bytes(name);
        self.reserve_new(1, size_of::<Node>() + size_of::<ElementData>() + strings)?;
        let mark = self.arena.len();
        let namespace = match name.namespace {
            Some(uri) => match self.intern_checked(uri) {
                Ok(span) => Some(span),
                Err(err) => {
                    self.arena.truncate(mark);
                    return Err(err);
                }
            },
            None => None,
        };
        let prefix = match name.prefix {
            Some(prefix) => match self.intern_checked(prefix) {
                Ok(span) => Some(span),
                Err(err) => {
                    self.arena.truncate(mark);
                    return Err(err);
                }
            },
            None => None,
        };
        let local = match self.intern_checked(name.local) {
            Ok(span) => span,
            Err(err) => {
                self.arena.truncate(mark);
                return Err(err);
            }
        };
        let slot = self.elements.len() as u32;
        self.elements.push(ElementData {
            namespace,
            prefix,
            local,
            attrs: Range { start: 0, len: 0 },
            ns_decls: Range { start: 0, len: 0 },
        });
        self.drop_order();
        Ok(self.push_detached(NodeKind::Element, Payload::Element(slot)))
    }

    /// A detached text node.
    pub fn create_text(&mut self, text: &str) -> Result<NodeId, DomError> {
        self.create_chars(NodeKind::Text, text)
    }

    /// A detached CDATA node. `]]>` and CR are rejected so a later parse stays one node.
    pub fn create_cdata(&mut self, text: &str) -> Result<NodeId, DomError> {
        if text.contains("]]>") || text.contains('\r') {
            return Err(DomError::InvalidChar);
        }
        self.create_chars(NodeKind::CData, text)
    }

    /// A detached comment. `--`, a trailing `-`, and CR are rejected.
    pub fn create_comment(&mut self, text: &str) -> Result<NodeId, DomError> {
        if text.contains("--") || text.ends_with('-') || text.contains('\r') {
            return Err(DomError::InvalidChar);
        }
        self.create_chars(NodeKind::Comment, text)
    }

    /// A detached processing instruction. The target `xml` is reserved.
    pub fn create_pi(&mut self, target: &str, data: Option<&str>) -> Result<NodeId, DomError> {
        self.check_pi(target, data)?;
        let bytes = target.len() + data.map(str::len).unwrap_or(0);
        self.reserve_new(1, size_of::<Node>() + bytes)?;
        let target = self.intern_checked(target)?;
        let data = match data {
            Some(data) => Some(self.intern_checked(data)?),
            None => None,
        };
        self.drop_order();
        Ok(self.push_detached(NodeKind::Pi, Payload::Pi { target, data }))
    }

    /// A detached document-type node. A public id requires a system id.
    ///
    /// Crate-private: a public constructor would let callers emit a `DOCTYPE`
    /// with a `SYSTEM` id, which is an XXE hazard for a downstream parser.
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn create_doctype(
        &mut self,
        name: &str,
        system: Option<&str>,
        public: Option<&str>,
    ) -> Result<NodeId, DomError> {
        if !rules::is_qname(name) {
            return Err(DomError::InvalidName);
        }
        let (prefix, local) = match name.split_once(':') {
            Some((prefix, local)) => (Some(prefix), local),
            None => (None, name),
        };
        self.check_qname_len(prefix, local)?;
        if public.is_some() && system.is_none() {
            return Err(DomError::Hierarchy);
        }
        if let Some(public) = public
            && !public.chars().all(is_pubid_char)
        {
            return Err(DomError::InvalidChar);
        }
        if let Some(system) = system {
            self.check_chars_only(system)?;
            if system.contains('"') && system.contains('\'') {
                return Err(DomError::InvalidChar);
            }
        }
        let bytes =
            name.len() + system.map(str::len).unwrap_or(0) + public.map(str::len).unwrap_or(0);
        self.reserve_new(1, size_of::<Node>() + bytes)?;
        let mark = self.arena.len();
        let name_span = self.intern_checked(name);
        let system_span = self.optional_intern(system);
        let public_span = self.optional_intern(public);
        if let (Ok(name_span), Ok(system_span), Ok(public_span)) =
            (name_span, system_span, public_span)
        {
            self.drop_order();
            return Ok(self.push_detached(
                NodeKind::Doctype,
                Payload::Doctype {
                    name: name_span,
                    system: system_span,
                    public: public_span,
                },
            ));
        }
        self.arena.truncate(mark);
        Err(DomError::Limit {
            kind: DomLimit::DomBytes,
            limit: self.byte_cap(),
            actual: self.byte_cap().saturating_add(1),
        })
    }

    /// Link `child` as the last child of `parent`, moving it if it already has a parent.
    pub fn append_child(&mut self, parent: NodeId, child: NodeId) -> Result<(), DomError> {
        let parent = self.require(parent)?;
        let child = self.require(child)?;
        self.prepare_insert(parent, child, None)?;
        self.unlink(child);
        self.link_last(parent, child);
        self.drop_order();
        Ok(())
    }

    /// Insert `child` immediately before `reference`.
    pub fn insert_before(&mut self, child: NodeId, reference: NodeId) -> Result<(), DomError> {
        let reference = self.require(reference)?;
        let child = self.require(child)?;
        let parent = self.nodes[reference as usize]
            .parent
            .ok_or(DomError::Hierarchy)?;
        self.prepare_insert(parent, child, Some(reference))?;
        self.unlink(child);
        self.link_before(reference, child);
        self.drop_order();
        Ok(())
    }

    /// Insert `child` immediately after `reference`.
    pub fn insert_after(&mut self, child: NodeId, reference: NodeId) -> Result<(), DomError> {
        let reference = self.require(reference)?;
        let child = self.require(child)?;
        let parent = self.nodes[reference as usize]
            .parent
            .ok_or(DomError::Hierarchy)?;
        self.prepare_insert(parent, child, Some(reference))?;
        self.unlink(child);
        self.link_after(reference, child);
        self.drop_order();
        Ok(())
    }

    /// Unlink `node` and copy namespace bindings it still needs onto its root.
    ///
    /// The node stays live. After this returns, the subtree does not depend on
    /// an ancestor declaration.
    pub fn detach(&mut self, node: NodeId) -> Result<(), DomError> {
        let index = self.require(node)?;
        if self.nodes[index as usize].kind == NodeKind::Document {
            return Err(DomError::Hierarchy);
        }
        if self.nodes[index as usize].parent.is_none() {
            return Ok(());
        }
        let needed = self.external_bindings(index)?;
        self.install_bindings(index, &needed)?;
        self.unlink(index);
        self.drop_order();
        Ok(())
    }

    /// Unlink `node` and tombstone it and every descendant. Slots are not reused.
    pub fn remove(&mut self, node: NodeId) -> Result<(), DomError> {
        let index = self.require(node)?;
        if self.nodes[index as usize].kind == NodeKind::Document {
            return Err(DomError::Hierarchy);
        }
        self.unlink(index);
        let mut stack = vec![index];
        let mut guard = self.nodes.len();
        while let Some(current) = stack.pop() {
            if guard == 0 {
                break;
            }
            guard -= 1;
            let first = self.nodes[current as usize].first_child;
            let mut child = first;
            let mut kids = self.nodes.len();
            while let Some(next) = child {
                if kids == 0 {
                    break;
                }
                kids -= 1;
                stack.push(next);
                child = self.nodes[next as usize].next;
            }
            let node = &mut self.nodes[current as usize];
            node.alive = false;
            node.generation = node.generation.saturating_add(1);
            node.parent = None;
            node.prev = None;
            node.next = None;
            node.first_child = None;
            node.last_child = None;
        }
        self.drop_order();
        Ok(())
    }

    /// Set an attribute, replacing any attribute with the same expanded name.
    ///
    /// The expanded name is the namespace URI plus the local name. A second
    /// call with a different prefix still replaces the old attribute when the
    /// URI and local name match, so the element never keeps two attributes
    /// with that expanded name. The prefix must already resolve to `name`'s
    /// URI in scope; otherwise this returns [`DomError::Namespace`] and the
    /// element is unchanged. Serialization then writes a declaration for that
    /// prefix on the element when the element itself does not already declare
    /// it, including when the binding was inherited or the stored URI would
    /// disagree with an ancestor.
    ///
    /// `xmlns` and `xmlns:*` are rejected. Declare those with [`Self::declare_namespace`].
    pub fn set_attribute(
        &mut self,
        element: NodeId,
        name: Name<'_>,
        value: &str,
    ) -> Result<(), DomError> {
        let index = self.require(element)?;
        let slot = self.slot_of(index)?;
        self.check_attr_name(index, name)?;
        self.check_text(value, DomLimit::TextLength)?;
        if let Some(existing) = self.find_attr(slot, name) {
            self.reserve_new(0, value.len() + name_bytes(name))?;
            let attr = self.pack_attr(name, value)?;
            self.attrs[existing] = attr;
            return Ok(());
        }
        let count = self.attr_ns_count(slot) + 1;
        self.check_attr_count(count)?;
        let growth = self.attr_record_growth(slot, 1) + name_bytes(name) + value.len();
        self.reserve_new(0, growth)?;
        let attr = self.pack_attr(name, value)?;
        self.append_attr(slot, attr);
        Ok(())
    }

    /// Remove the attribute with this expanded name. Missing is success.
    pub fn remove_attribute(&mut self, element: NodeId, name: Name<'_>) -> Result<(), DomError> {
        let index = self.require(element)?;
        let slot = self.slot_of(index)?;
        let Some(found) = self.find_attr(slot, name) else {
            return Ok(());
        };
        let range = self.elements[slot as usize].attrs;
        let start = range.start as usize;
        let end = start + range.len as usize;
        let mut kept = self.attrs.get(start..end).unwrap_or(&[]).to_vec();
        let local_index = found - start;
        if local_index < kept.len() {
            kept.remove(local_index);
        }
        self.reserve_new(0, kept.len() * size_of::<AttrData>())?;
        let new_start = self.attrs.len() as u32;
        let new_len = kept.len() as u32;
        self.attrs.extend(kept);
        self.elements[slot as usize].attrs = Range {
            start: new_start,
            len: new_len,
        };
        Ok(())
    }

    /// Declare `xmlns:prefix` or the default `xmlns` on an element.
    pub fn declare_namespace(
        &mut self,
        element: NodeId,
        prefix: Option<&str>,
        uri: &str,
    ) -> Result<(), DomError> {
        let index = self.require(element)?;
        let slot = self.slot_of(index)?;
        self.check_binding(prefix, uri)?;
        if !self.names_match_binding(index, prefix, uri) {
            return Err(DomError::Namespace);
        }
        if let Some(existing) = self.find_ns(slot, prefix) {
            let current = self.str_at(self.ns_decls[existing].uri).unwrap_or("");
            if current == uri {
                return Ok(());
            }
            if self.binding_in_use(index, prefix) {
                return Err(DomError::Namespace);
            }
            self.reserve_new(0, uri.len())?;
            let uri = self.intern_checked(uri)?;
            self.ns_decls[existing].uri = uri;
            return Ok(());
        }
        let count = self.attr_ns_count(slot) + 1;
        self.check_attr_count(count)?;
        let growth = self.ns_record_growth(slot, 1) + prefix.map(str::len).unwrap_or(0) + uri.len();
        self.reserve_new(0, growth)?;
        let prefix = self.optional_intern(prefix)?;
        let uri = self.intern_checked(uri)?;
        self.append_ns(slot, NsDecl { prefix, uri });
        Ok(())
    }

    /// Remove one declaration. A declaration that names in the subtree still use is kept.
    pub fn remove_namespace(
        &mut self,
        element: NodeId,
        prefix: Option<&str>,
    ) -> Result<(), DomError> {
        let index = self.require(element)?;
        let slot = self.slot_of(index)?;
        let Some(found) = self.find_ns(slot, prefix) else {
            return Ok(());
        };
        if self.binding_in_use(index, prefix) {
            return Err(DomError::Namespace);
        }
        let range = self.elements[slot as usize].ns_decls;
        let start = range.start as usize;
        let mut kept = self
            .ns_decls
            .get(start..start + range.len as usize)
            .unwrap_or(&[])
            .to_vec();
        let local_index = found - start;
        if local_index < kept.len() {
            kept.remove(local_index);
        }
        self.reserve_new(0, kept.len() * size_of::<NsDecl>())?;
        let new_start = self.ns_decls.len() as u32;
        let new_len = kept.len() as u32;
        self.ns_decls.extend(kept);
        self.elements[slot as usize].ns_decls = Range {
            start: new_start,
            len: new_len,
        };
        Ok(())
    }

    /// Replace the character data of a text, CDATA, or comment node.
    pub fn set_text(&mut self, node: NodeId, text: &str) -> Result<(), DomError> {
        let index = self.require(node)?;
        match self.nodes[index as usize].kind {
            NodeKind::Text => {}
            NodeKind::Comment => {
                if text.contains("--") || text.ends_with('-') || text.contains('\r') {
                    return Err(DomError::InvalidChar);
                }
            }
            NodeKind::CData => {
                if text.contains("]]>") || text.contains('\r') {
                    return Err(DomError::InvalidChar);
                }
            }
            _ => return Err(DomError::WrongKind),
        }
        self.check_text(text, DomLimit::TextLength)?;
        self.reserve_new(0, text.len())?;
        let span = self.intern_checked(text)?;
        self.nodes[index as usize].payload = Payload::Span(span);
        Ok(())
    }

    /// Replace the data of a processing instruction. `None` clears it.
    pub fn set_pi_data(&mut self, node: NodeId, data: Option<&str>) -> Result<(), DomError> {
        let index = self.require(node)?;
        let Payload::Pi { target, .. } = self.nodes[index as usize].payload else {
            return Err(DomError::WrongKind);
        };
        let target_str = self.str_at(target).unwrap_or("");
        self.check_pi(target_str, data)?;
        let bytes = data.map(str::len).unwrap_or(0);
        self.reserve_new(0, bytes)?;
        let data = self.optional_intern(data)?;
        self.nodes[index as usize].payload = Payload::Pi { target, data };
        Ok(())
    }

    fn create_chars(&mut self, kind: NodeKind, text: &str) -> Result<NodeId, DomError> {
        self.check_text(text, DomLimit::TextLength)?;
        self.reserve_new(1, size_of::<Node>() + text.len())?;
        let span = self.intern_checked(text)?;
        self.drop_order();
        Ok(self.push_detached(kind, Payload::Span(span)))
    }

    fn push_detached(&mut self, kind: NodeKind, payload: Payload) -> NodeId {
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
        NodeId::mint(self.id, index, 1)
    }

    fn prepare_insert(
        &self,
        parent: u32,
        child: u32,
        reference: Option<u32>,
    ) -> Result<(), DomError> {
        if parent == child || self.nodes[child as usize].kind == NodeKind::Document {
            return Err(DomError::Hierarchy);
        }
        if self.ancestor_of(child, parent) {
            return Err(DomError::Hierarchy);
        }
        if let Some(reference) = reference {
            if reference == child {
                return Err(DomError::Hierarchy);
            }
            if self.nodes[reference as usize].parent != Some(parent) {
                return Err(DomError::Hierarchy);
            }
        }
        self.check_parent_kind(parent, child)?;
        self.check_depth(parent, child)?;
        self.check_subtree_namespaces(child, Some(parent))?;
        Ok(())
    }

    fn check_parent_kind(&self, parent: u32, child: u32) -> Result<(), DomError> {
        let parent_kind = self.nodes[parent as usize].kind;
        let child_kind = self.nodes[child as usize].kind;
        match parent_kind {
            NodeKind::Document => match child_kind {
                NodeKind::Element => {
                    if self.document_child_count(parent, child, NodeKind::Element) > 0 {
                        return Err(DomError::Hierarchy);
                    }
                }
                NodeKind::Doctype => {
                    if self.document_child_count(parent, child, NodeKind::Doctype) > 0 {
                        return Err(DomError::Hierarchy);
                    }
                }
                NodeKind::Comment | NodeKind::Pi => {}
                _ => return Err(DomError::Hierarchy),
            },
            NodeKind::Element => match child_kind {
                NodeKind::Element
                | NodeKind::Text
                | NodeKind::CData
                | NodeKind::Comment
                | NodeKind::Pi => {}
                _ => return Err(DomError::Hierarchy),
            },
            _ => return Err(DomError::Hierarchy),
        }
        Ok(())
    }

    fn document_child_count(&self, parent: u32, ignore: u32, kind: NodeKind) -> usize {
        self.child_indexes(parent)
            .into_iter()
            .filter(|index| *index != ignore && self.nodes[*index as usize].kind == kind)
            .count()
    }

    fn check_depth(&self, parent: u32, child: u32) -> Result<(), DomError> {
        let depth = self.element_depth(parent) + self.element_height(child);
        if depth > self.max_depth {
            return Err(DomError::Limit {
                kind: DomLimit::ElementDepth,
                limit: self.max_depth as u64,
                actual: depth as u64,
            });
        }
        Ok(())
    }

    fn element_depth(&self, index: u32) -> usize {
        let mut depth = 0usize;
        let mut current = Some(index);
        let mut guard = self.nodes.len();
        while let Some(at) = current {
            if guard == 0 {
                break;
            }
            guard -= 1;
            if self.nodes[at as usize].kind == NodeKind::Element {
                depth += 1;
            }
            current = self.nodes[at as usize].parent;
        }
        depth
    }

    fn element_height(&self, index: u32) -> usize {
        let start = usize::from(self.nodes[index as usize].kind == NodeKind::Element);
        let mut best = start;
        let mut stack = vec![(index, start)];
        let mut guard = self.nodes.len().saturating_mul(2);
        while let Some((at, depth)) = stack.pop() {
            if guard == 0 {
                break;
            }
            guard -= 1;
            best = best.max(depth);
            for child in self.child_indexes(at) {
                let next = if self.nodes[child as usize].kind == NodeKind::Element {
                    depth + 1
                } else {
                    depth
                };
                stack.push((child, next));
            }
        }
        best
    }

    fn ancestor_of(&self, ancestor: u32, mut node: u32) -> bool {
        let mut guard = self.nodes.len();
        while let Some(parent) = self.nodes[node as usize].parent {
            if guard == 0 {
                return false;
            }
            guard -= 1;
            if parent == ancestor {
                return true;
            }
            node = parent;
        }
        false
    }

    fn check_subtree_namespaces(&self, root: u32, host: Option<u32>) -> Result<(), DomError> {
        for index in self.subtree_indexes(root) {
            let node = &self.nodes[index as usize];
            if let Payload::Element(slot) = node.payload {
                let element = &self.elements[slot as usize];
                self.check_resolved_name(
                    index,
                    root,
                    host,
                    element.prefix,
                    element.namespace,
                    false,
                )?;
                let attrs = element.attrs;
                let start = attrs.start as usize;
                let end = start + attrs.len as usize;
                for attr in self.attrs.get(start..end).unwrap_or(&[]) {
                    self.check_resolved_name(index, root, host, attr.prefix, attr.namespace, true)?;
                }
            }
        }
        Ok(())
    }

    fn check_resolved_name(
        &self,
        node: u32,
        root: u32,
        host: Option<u32>,
        prefix: Option<StrSpan>,
        namespace: Option<StrSpan>,
        attribute: bool,
    ) -> Result<(), DomError> {
        let prefix = prefix.and_then(|span| self.str_at(span));
        let namespace = namespace.and_then(|span| self.str_at(span));
        if attribute && prefix.is_none() {
            return if namespace.is_none() {
                Ok(())
            } else {
                Err(DomError::Namespace)
            };
        }
        let resolved = self.lookup_inserted(node, root, host, prefix);
        let expected = match namespace {
            Some("") => None,
            other => other,
        };
        let resolved = match resolved {
            Some("") => None,
            other => other,
        };
        if resolved == expected {
            Ok(())
        } else {
            Err(DomError::Namespace)
        }
    }

    fn lookup_inserted(
        &self,
        node: u32,
        root: u32,
        host: Option<u32>,
        prefix: Option<&str>,
    ) -> Option<&str> {
        let mut current = Some(node);
        let mut guard = self.nodes.len() + 1;
        let mut jumped = false;
        while let Some(at) = current {
            if guard == 0 {
                break;
            }
            guard -= 1;
            if self.nodes[at as usize].kind == NodeKind::Element
                && let Some(uri) = self.binding_on(at, prefix)
            {
                return Some(uri);
            }
            if !jumped && at == root {
                jumped = true;
                current = host;
                continue;
            }
            current = self.nodes[at as usize].parent;
        }
        match prefix {
            Some("xml") => Some(XML_NAMESPACE),
            Some("xmlns") => Some(XMLNS_NAMESPACE),
            _ => None,
        }
    }

    fn binding_on(&self, index: u32, prefix: Option<&str>) -> Option<&str> {
        let Payload::Element(slot) = self.nodes[index as usize].payload else {
            return None;
        };
        let range = self.elements[slot as usize].ns_decls;
        let start = range.start as usize;
        let end = start + range.len as usize;
        for decl in self.ns_decls.get(start..end)? {
            let declared = decl.prefix.and_then(|span| self.str_at(span));
            if declared == prefix {
                return self.str_at(decl.uri);
            }
        }
        None
    }

    fn external_bindings(&self, root: u32) -> Result<Vec<(Option<String>, String)>, DomError> {
        if self.nodes[root as usize].kind != NodeKind::Element {
            return Ok(Vec::new());
        }
        let mut needed: Vec<(Option<String>, String)> = Vec::new();
        for index in self.subtree_indexes(root) {
            self.note_external(root, index, false, &mut needed)?;
            if let Payload::Element(slot) = self.nodes[index as usize].payload {
                let range = self.elements[slot as usize].attrs;
                let start = range.start as usize;
                let end = start + range.len as usize;
                if self.attrs.get(start..end).is_some() {
                    self.note_external(root, index, true, &mut needed)?;
                }
            }
        }
        Ok(needed
            .into_iter()
            .filter(|(prefix, _)| self.binding_on(root, prefix.as_deref()).is_none())
            .collect())
    }

    fn note_external(
        &self,
        root: u32,
        index: u32,
        attribute: bool,
        needed: &mut Vec<(Option<String>, String)>,
    ) -> Result<(), DomError> {
        let Payload::Element(slot) = self.nodes[index as usize].payload else {
            return Ok(());
        };
        let element = &self.elements[slot as usize];
        if attribute {
            let range = element.attrs;
            let start = range.start as usize;
            let end = start + range.len as usize;
            for attr in self.attrs.get(start..end).unwrap_or(&[]) {
                let Some(prefix) = attr.prefix.and_then(|span| self.str_at(span)) else {
                    continue;
                };
                self.note_one(root, index, Some(prefix), needed);
            }
            return Ok(());
        }
        let prefix = element.prefix.and_then(|span| self.str_at(span));
        self.note_one(root, index, prefix, needed);
        Ok(())
    }

    fn note_one(
        &self,
        root: u32,
        index: u32,
        prefix: Option<&str>,
        needed: &mut Vec<(Option<String>, String)>,
    ) {
        if prefix == Some("xml") {
            return;
        }
        let declarer = self.declarer(index, prefix);
        let outside = match declarer {
            Some(at) => !self.in_subtree(at, root),
            None => prefix.is_none(),
        };
        if !outside {
            return;
        }
        let uri = if prefix.is_none() && declarer.is_none() {
            let Payload::Element(slot) = self.nodes[index as usize].payload else {
                return;
            };
            self.elements[slot as usize]
                .namespace
                .and_then(|span| self.str_at(span))
                .unwrap_or("")
        } else {
            self.lookup_inserted(index, root, self.nodes[root as usize].parent, prefix)
                .unwrap_or("")
        };
        let key = prefix.map(str::to_string);
        if needed.iter().any(|(have, _)| have.as_deref() == prefix) {
            return;
        }
        needed.push((key, uri.to_string()));
    }

    fn declarer(&self, node: u32, prefix: Option<&str>) -> Option<u32> {
        let mut current = Some(node);
        let mut guard = self.nodes.len();
        while let Some(at) = current {
            if guard == 0 {
                return None;
            }
            guard -= 1;
            if self.binding_on(at, prefix).is_some() {
                return Some(at);
            }
            current = self.nodes[at as usize].parent;
        }
        None
    }

    fn in_subtree(&self, mut index: u32, root: u32) -> bool {
        let mut guard = self.nodes.len();
        loop {
            if index == root {
                return true;
            }
            if guard == 0 {
                return false;
            }
            guard -= 1;
            match self.nodes[index as usize].parent {
                Some(parent) => index = parent,
                None => return false,
            }
        }
    }

    fn install_bindings(
        &mut self,
        root: u32,
        bindings: &[(Option<String>, String)],
    ) -> Result<(), DomError> {
        let Payload::Element(slot) = self.nodes[root as usize].payload else {
            return if bindings.is_empty() {
                Ok(())
            } else {
                Err(DomError::Namespace)
            };
        };
        if bindings.is_empty() {
            return Ok(());
        }
        for (prefix, uri) in bindings {
            self.check_binding(prefix.as_deref(), uri)?;
        }
        let count = self.attr_ns_count(slot) + bindings.len() as u64;
        self.check_attr_count(count)?;
        let strings: usize = bindings
            .iter()
            .map(|(prefix, uri)| prefix.as_ref().map(String::len).unwrap_or(0) + uri.len())
            .sum();
        let growth = self.ns_record_growth(slot, bindings.len()) + strings;
        self.reserve_new(0, growth)?;
        let saved_ns = self.ns_decls.len();
        let saved_arena = self.arena.len();
        let saved_element = self.elements[slot as usize];
        for (prefix, uri) in bindings {
            let prefix = match self.optional_intern(prefix.as_deref()) {
                Ok(span) => span,
                Err(err) => {
                    self.rewind_ns(slot, saved_ns, saved_arena, saved_element);
                    return Err(err);
                }
            };
            let uri = match self.intern_checked(uri) {
                Ok(span) => span,
                Err(err) => {
                    self.rewind_ns(slot, saved_ns, saved_arena, saved_element);
                    return Err(err);
                }
            };
            self.append_ns(slot, NsDecl { prefix, uri });
        }
        Ok(())
    }

    fn rewind_ns(&mut self, slot: u32, ns_len: usize, arena_len: usize, element: ElementData) {
        self.ns_decls.truncate(ns_len);
        self.arena.truncate(arena_len);
        self.elements[slot as usize] = element;
    }

    /// Names this declaration would cover already store `uri`.
    fn names_match_binding(&self, owner: u32, prefix: Option<&str>, uri: &str) -> bool {
        for index in self.subtree_indexes(owner) {
            if let Some(stored) = self.stored_ns(index, prefix, false)
                && self.covered_by(index, owner, prefix)
                && stored != uri
            {
                return false;
            }
            if prefix.is_some()
                && let Some(stored) = self.stored_ns(index, prefix, true)
                && self.covered_by(index, owner, prefix)
                && stored != uri
            {
                return false;
            }
        }
        true
    }

    fn stored_ns(&self, index: u32, prefix: Option<&str>, attribute: bool) -> Option<String> {
        let Payload::Element(slot) = self.nodes[index as usize].payload else {
            return None;
        };
        if attribute {
            let range = self.elements[slot as usize].attrs;
            let start = range.start as usize;
            let end = start + range.len as usize;
            for attr in self.attrs.get(start..end).unwrap_or(&[]) {
                if attr.prefix.and_then(|span| self.str_at(span)) == prefix {
                    let uri = attr
                        .namespace
                        .and_then(|span| self.str_at(span))
                        .unwrap_or("");
                    return Some(uri.to_string());
                }
            }
            return None;
        }
        let element = &self.elements[slot as usize];
        let name_prefix = element.prefix.and_then(|span| self.str_at(span));
        let uses = if prefix.is_none() {
            name_prefix.is_none()
        } else {
            name_prefix == prefix
        };
        if !uses {
            return None;
        }
        Some(
            element
                .namespace
                .and_then(|span| self.str_at(span))
                .unwrap_or("")
                .to_string(),
        )
    }

    fn covered_by(&self, index: u32, owner: u32, prefix: Option<&str>) -> bool {
        let mut current = Some(index);
        let mut guard = self.nodes.len();
        while let Some(at) = current {
            if guard == 0 {
                return false;
            }
            guard -= 1;
            if at == owner {
                return true;
            }
            if self.binding_on(at, prefix).is_some() {
                return false;
            }
            current = self.nodes[at as usize].parent;
        }
        false
    }

    fn binding_in_use(&self, owner: u32, prefix: Option<&str>) -> bool {
        for index in self.subtree_indexes(owner) {
            if self.element_uses(index, owner, prefix) || self.attr_uses(index, owner, prefix) {
                return true;
            }
        }
        false
    }

    fn element_uses(&self, index: u32, owner: u32, prefix: Option<&str>) -> bool {
        let Payload::Element(slot) = self.nodes[index as usize].payload else {
            return false;
        };
        let name_prefix = self.elements[slot as usize]
            .prefix
            .and_then(|span| self.str_at(span));
        let uses_default = prefix.is_none() && name_prefix.is_none();
        let uses_prefix = prefix.is_some() && name_prefix == prefix;
        (uses_default || uses_prefix) && self.declarer(index, prefix) == Some(owner)
    }

    fn attr_uses(&self, index: u32, owner: u32, prefix: Option<&str>) -> bool {
        if prefix.is_none() {
            return false;
        }
        let Payload::Element(slot) = self.nodes[index as usize].payload else {
            return false;
        };
        let range = self.elements[slot as usize].attrs;
        let start = range.start as usize;
        let end = start + range.len as usize;
        self.attrs
            .get(start..end)
            .unwrap_or(&[])
            .iter()
            .any(|attr| {
                attr.prefix.and_then(|span| self.str_at(span)) == prefix
                    && self.declarer(index, prefix) == Some(owner)
            })
    }

    fn check_element_name(&self, name: Name<'_>) -> Result<(), DomError> {
        self.check_ncname_piece(name.local)?;
        if let Some(prefix) = name.prefix {
            self.check_ncname_piece(prefix)?;
            if prefix == "xmlns" {
                return Err(DomError::Namespace);
            }
        }
        self.check_qname_len(name.prefix, name.local)?;
        if let Some(uri) = name.namespace {
            self.check_chars_only(uri)?;
        }
        if name.prefix == Some("xml") && name.namespace != Some(XML_NAMESPACE) {
            return Err(DomError::Namespace);
        }
        if name.prefix.is_some() && name.namespace.is_none() {
            return Err(DomError::Namespace);
        }
        if name.namespace == Some(XML_NAMESPACE) && name.prefix != Some("xml") {
            return Err(DomError::Namespace);
        }
        if name.namespace == Some(XMLNS_NAMESPACE) {
            return Err(DomError::Namespace);
        }
        Ok(())
    }

    fn check_attr_name(&self, element: u32, name: Name<'_>) -> Result<(), DomError> {
        if name.prefix == Some("xmlns") || (name.prefix.is_none() && name.local == "xmlns") {
            return Err(DomError::Namespace);
        }
        self.check_element_name(name)?;
        let resolved = self.lookup_inserted(
            element,
            element,
            self.nodes[element as usize].parent,
            name.prefix,
        );
        if name.prefix.is_none() {
            return if name.namespace.is_none() {
                Ok(())
            } else {
                Err(DomError::Namespace)
            };
        }
        let resolved = match resolved {
            Some("") => None,
            other => other,
        };
        if resolved == name.namespace {
            Ok(())
        } else {
            Err(DomError::Namespace)
        }
    }

    fn check_binding(&self, prefix: Option<&str>, uri: &str) -> Result<(), DomError> {
        if let Some(prefix) = prefix {
            self.check_ncname_piece(prefix)?;
        }
        self.check_chars_only(uri)?;
        match rules::namespace_binding(prefix, uri) {
            Ok(()) => Ok(()),
            Err(
                BindingFault::XmlnsPrefix
                | BindingFault::XmlPrefix
                | BindingFault::EmptyPrefixed
                | BindingFault::XmlNamespace
                | BindingFault::XmlnsNamespace,
            ) => Err(DomError::Namespace),
        }
    }

    fn check_pi(&self, target: &str, data: Option<&str>) -> Result<(), DomError> {
        self.check_ncname_piece(target)?;
        if target.eq_ignore_ascii_case("xml") {
            return Err(DomError::InvalidName);
        }
        if let Some(data) = data {
            self.check_text(data, DomLimit::TextLength)?;
            if data.contains("?>") || data.contains('\r') {
                return Err(DomError::InvalidChar);
            }
        }
        Ok(())
    }

    fn check_ncname_piece(&self, name: &str) -> Result<(), DomError> {
        if !rules::is_ncname(name) {
            return Err(DomError::InvalidName);
        }
        self.check_chars_only(name)?;
        Ok(())
    }

    fn check_qname_len(&self, prefix: Option<&str>, local: &str) -> Result<(), DomError> {
        let mut acc = 0u64;
        if let Some(prefix) = prefix {
            acc = self.grow_len(prefix, acc, DomLimit::NameLength)?;
            acc = acc.saturating_add(1);
            if acc > self.max_name_len as u64 {
                return Err(DomError::Limit {
                    kind: DomLimit::NameLength,
                    limit: self.max_name_len as u64,
                    actual: acc,
                });
            }
        }
        self.grow_len(local, acc, DomLimit::NameLength)?;
        Ok(())
    }

    fn grow_len(&self, text: &str, mut acc: u64, kind: DomLimit) -> Result<u64, DomError> {
        let limit = match kind {
            DomLimit::NameLength => self.max_name_len as u64,
            DomLimit::TextLength => self.max_text_len,
            _ => self.max_text_len,
        };
        for ch in text.chars() {
            if !is_xml_char(ch) {
                return Err(DomError::InvalidChar);
            }
            acc = acc.saturating_add(ch.len_utf8() as u64);
            if acc > limit {
                return Err(DomError::Limit {
                    kind,
                    limit,
                    actual: acc,
                });
            }
        }
        Ok(acc)
    }

    fn check_text(&self, text: &str, kind: DomLimit) -> Result<(), DomError> {
        self.grow_len(text, 0, kind)?;
        Ok(())
    }

    fn check_chars_only(&self, text: &str) -> Result<(), DomError> {
        if text.chars().all(is_xml_char) {
            Ok(())
        } else {
            Err(DomError::InvalidChar)
        }
    }

    fn check_attr_count(&self, count: u64) -> Result<(), DomError> {
        if count > self.max_attrs as u64 {
            Err(DomError::Limit {
                kind: DomLimit::Attributes,
                limit: self.max_attrs as u64,
                actual: count,
            })
        } else {
            Ok(())
        }
    }

    fn require(&self, id: NodeId) -> Result<u32, DomError> {
        self.get(id)
            .map(|node| node.id().index())
            .ok_or(DomError::RemovedNode)
    }

    fn slot_of(&self, index: u32) -> Result<u32, DomError> {
        match self.nodes[index as usize].payload {
            Payload::Element(slot) => Ok(slot),
            _ => Err(DomError::WrongKind),
        }
    }

    fn reserve_new(&self, nodes: u64, bytes: usize) -> Result<(), DomError> {
        self.reserve(nodes, bytes as u64)
    }

    fn intern_checked(&mut self, text: &str) -> Result<StrSpan, DomError> {
        let start = u32::try_from(self.arena.len()).map_err(|_| DomError::Limit {
            kind: DomLimit::DomBytes,
            limit: self.byte_cap(),
            actual: self.byte_cap().saturating_add(1),
        })?;
        let len = u32::try_from(text.len()).map_err(|_| DomError::Limit {
            kind: DomLimit::DomBytes,
            limit: self.byte_cap(),
            actual: self.byte_cap().saturating_add(1),
        })?;
        if start.checked_add(len).is_none() {
            return Err(DomError::Limit {
                kind: DomLimit::DomBytes,
                limit: self.byte_cap(),
                actual: self.byte_cap().saturating_add(1),
            });
        }
        self.arena.push_str(text);
        Ok(StrSpan { start, len })
    }

    fn pack_attr(&mut self, name: Name<'_>, value: &str) -> Result<AttrData, DomError> {
        let mark = self.arena.len();
        let namespace = self.optional_intern(name.namespace);
        let prefix = self.optional_intern(name.prefix);
        let local = self.intern_checked(name.local);
        let value = self.intern_checked(value);
        if let (Ok(namespace), Ok(prefix), Ok(local), Ok(value)) = (namespace, prefix, local, value)
        {
            return Ok(AttrData {
                namespace,
                prefix,
                local,
                value,
                specified: true,
            });
        }
        self.arena.truncate(mark);
        Err(DomError::Limit {
            kind: DomLimit::DomBytes,
            limit: self.byte_cap(),
            actual: self.byte_cap().saturating_add(1),
        })
    }

    fn optional_intern(&mut self, text: Option<&str>) -> Result<Option<StrSpan>, DomError> {
        match text {
            Some(text) => self.intern_checked(text).map(Some),
            None => Ok(None),
        }
    }

    fn child_indexes(&self, parent: u32) -> Vec<u32> {
        let mut out = Vec::new();
        let mut child = self
            .nodes
            .get(parent as usize)
            .and_then(|node| node.first_child);
        let mut guard = self.nodes.len();
        while let Some(index) = child {
            if guard == 0 {
                break;
            }
            guard -= 1;
            let node = &self.nodes[index as usize];
            if node.alive {
                out.push(index);
            }
            child = node.next;
        }
        out
    }

    fn subtree_indexes(&self, root: u32) -> Vec<u32> {
        let mut out = Vec::new();
        let mut stack = vec![root];
        let mut guard = self.nodes.len().saturating_mul(2);
        while let Some(index) = stack.pop() {
            if guard == 0 {
                break;
            }
            guard -= 1;
            if !self.nodes[index as usize].alive && index != root {
                continue;
            }
            out.push(index);
            for child in self.child_indexes(index).into_iter().rev() {
                stack.push(child);
            }
        }
        out
    }

    fn attr_ns_count(&self, slot: u32) -> u64 {
        let element = &self.elements[slot as usize];
        u64::from(element.attrs.len).saturating_add(u64::from(element.ns_decls.len))
    }

    fn attr_record_growth(&self, slot: u32, extra: usize) -> usize {
        record_growth(self.elements[slot as usize].attrs, self.attrs.len(), extra)
            * size_of::<AttrData>()
    }

    fn ns_record_growth(&self, slot: u32, extra: usize) -> usize {
        record_growth(
            self.elements[slot as usize].ns_decls,
            self.ns_decls.len(),
            extra,
        ) * size_of::<NsDecl>()
    }

    fn find_attr(&self, slot: u32, name: Name<'_>) -> Option<usize> {
        let range = self.elements[slot as usize].attrs;
        let start = range.start as usize;
        let end = start + range.len as usize;
        self.attrs
            .get(start..end)?
            .iter()
            .position(|attr| {
                let local = self.str_at(attr.local);
                let namespace = attr.namespace.and_then(|span| self.str_at(span));
                local == Some(name.local) && namespace == name.namespace
            })
            .map(|offset| start + offset)
    }

    fn find_ns(&self, slot: u32, prefix: Option<&str>) -> Option<usize> {
        let range = self.elements[slot as usize].ns_decls;
        let start = range.start as usize;
        let end = start + range.len as usize;
        self.ns_decls
            .get(start..end)?
            .iter()
            .position(|decl| decl.prefix.and_then(|span| self.str_at(span)) == prefix)
            .map(|offset| start + offset)
    }

    fn append_attr(&mut self, slot: u32, attr: AttrData) {
        let element = self.elements[slot as usize];
        let at_end = element.attrs.len == 0
            || element.attrs.start + element.attrs.len == self.attrs.len() as u32;
        if at_end {
            let start = if element.attrs.len == 0 {
                self.attrs.len() as u32
            } else {
                element.attrs.start
            };
            self.attrs.push(attr);
            self.elements[slot as usize].attrs = Range {
                start,
                len: element.attrs.len + 1,
            };
            return;
        }
        let start = element.attrs.start as usize;
        let end = start + element.attrs.len as usize;
        let mut copied = self.attrs.get(start..end).unwrap_or(&[]).to_vec();
        copied.push(attr);
        let new_start = self.attrs.len() as u32;
        let new_len = copied.len() as u32;
        self.attrs.extend(copied);
        self.elements[slot as usize].attrs = Range {
            start: new_start,
            len: new_len,
        };
    }

    fn append_ns(&mut self, slot: u32, decl: NsDecl) {
        let element = self.elements[slot as usize];
        let at_end = element.ns_decls.len == 0
            || element.ns_decls.start + element.ns_decls.len == self.ns_decls.len() as u32;
        if at_end {
            let start = if element.ns_decls.len == 0 {
                self.ns_decls.len() as u32
            } else {
                element.ns_decls.start
            };
            self.ns_decls.push(decl);
            self.elements[slot as usize].ns_decls = Range {
                start,
                len: element.ns_decls.len + 1,
            };
            return;
        }
        let start = element.ns_decls.start as usize;
        let end = start + element.ns_decls.len as usize;
        let mut copied = self.ns_decls.get(start..end).unwrap_or(&[]).to_vec();
        copied.push(decl);
        let new_start = self.ns_decls.len() as u32;
        let new_len = copied.len() as u32;
        self.ns_decls.extend(copied);
        self.elements[slot as usize].ns_decls = Range {
            start: new_start,
            len: new_len,
        };
    }

    fn unlink(&mut self, index: u32) {
        let node = self.nodes[index as usize];
        let parent = node.parent;
        let prev = node.prev;
        let next = node.next;
        if let Some(prev) = prev
            && let Some(sibling) = self.nodes.get_mut(prev as usize)
        {
            sibling.next = next;
        }
        if let Some(next) = next
            && let Some(sibling) = self.nodes.get_mut(next as usize)
        {
            sibling.prev = prev;
        }
        if let Some(parent) = parent
            && let Some(parent_node) = self.nodes.get_mut(parent as usize)
        {
            if parent_node.first_child == Some(index) {
                parent_node.first_child = next;
            }
            if parent_node.last_child == Some(index) {
                parent_node.last_child = prev;
            }
        }
        let node = &mut self.nodes[index as usize];
        node.parent = None;
        node.prev = None;
        node.next = None;
    }

    fn link_last(&mut self, parent: u32, child: u32) {
        let last = self.nodes[parent as usize].last_child;
        if let Some(last) = last {
            self.nodes[last as usize].next = Some(child);
            self.nodes[child as usize].prev = Some(last);
        } else {
            self.nodes[parent as usize].first_child = Some(child);
            self.nodes[child as usize].prev = None;
        }
        self.nodes[parent as usize].last_child = Some(child);
        let child_node = &mut self.nodes[child as usize];
        child_node.parent = Some(parent);
        child_node.next = None;
    }

    fn link_before(&mut self, reference: u32, child: u32) {
        let parent = self.nodes[reference as usize]
            .parent
            .expect("parent checked");
        let prev = self.nodes[reference as usize].prev;
        if let Some(prev) = prev {
            self.nodes[prev as usize].next = Some(child);
        } else {
            self.nodes[parent as usize].first_child = Some(child);
        }
        self.nodes[reference as usize].prev = Some(child);
        let child_node = &mut self.nodes[child as usize];
        child_node.parent = Some(parent);
        child_node.prev = prev;
        child_node.next = Some(reference);
    }

    fn link_after(&mut self, reference: u32, child: u32) {
        let parent = self.nodes[reference as usize]
            .parent
            .expect("parent checked");
        let next = self.nodes[reference as usize].next;
        if let Some(next) = next {
            self.nodes[next as usize].prev = Some(child);
        } else {
            self.nodes[parent as usize].last_child = Some(child);
        }
        self.nodes[reference as usize].next = Some(child);
        let child_node = &mut self.nodes[child as usize];
        child_node.parent = Some(parent);
        child_node.prev = Some(reference);
        child_node.next = next;
    }

    fn ensure_order(&mut self) {
        if self
            .order_cache
            .as_ref()
            .is_some_and(|cache| cache.len() == self.nodes.len())
        {
            return;
        }
        let mut ranks = vec![u32::MAX; self.nodes.len()];
        if self.nodes.first().is_some_and(|node| node.alive) {
            let mut rank = 0u32;
            let mut stack = vec![0u32];
            let mut guard = self.nodes.len().saturating_mul(2);
            while let Some(index) = stack.pop() {
                if guard == 0 {
                    break;
                }
                guard -= 1;
                if !self.nodes[index as usize].alive {
                    continue;
                }
                ranks[index as usize] = rank;
                rank = rank.saturating_add(1);
                for child in self.child_indexes(index).into_iter().rev() {
                    stack.push(child);
                }
            }
        }
        self.order_cache = Some(ranks);
    }

    fn drop_order(&mut self) {
        self.order_cache = None;
    }
}

fn name_bytes(name: Name<'_>) -> usize {
    name.local.len()
        + name.prefix.map(str::len).unwrap_or(0)
        + name.namespace.map(str::len).unwrap_or(0)
}

fn record_growth(range: Range, len: usize, extra: usize) -> usize {
    if extra == 0 {
        return 0;
    }
    let at_end = range.len == 0 || range.start + range.len == len as u32;
    if at_end {
        extra
    } else {
        range.len as usize + extra
    }
}
