//! Serialize a document or a subtree through [`crate::Writer`].

use std::collections::HashMap;
use std::io::Write;
use std::sync::Arc;

use crate::WriteError;
use crate::dom::document::{Document, NodeKind, NodeRef, Payload, StrSpan};
use crate::dom::edit::WriteOptions;
use crate::event::{
    Attribute, Doctype, EmptyElement, EndElement, Event, Namespace, ProcessingInstruction, QName,
    StartElement, XmlText,
};

impl Document {
    /// Write the document as events. Doctypes are included.
    pub fn write_to<W: Write>(&self, writer: &mut crate::Writer<W>) -> Result<(), WriteError> {
        self.write_to_with(writer, WriteOptions::default())
    }

    /// Write the document. `omit_doctype` skips document-type nodes.
    ///
    /// A document type is written as `<!DOCTYPE name>`. System and public
    /// identifiers stay on the node and are not written. An option to write
    /// them is left for M3. The bytes are UTF-8, with no XML declaration.
    pub fn write_to_with<W: Write>(
        &self,
        writer: &mut crate::Writer<W>,
        options: WriteOptions,
    ) -> Result<(), WriteError> {
        write_document(self, writer, options)
    }

    /// `write_to` into a `String`.
    ///
    /// The bytes are always UTF-8. An XML declaration is not written: a
    /// declaration in the input is not stored on the document.
    pub fn to_string(&self) -> Result<String, WriteError> {
        self.to_string_with(WriteOptions::default())
    }

    /// `write_to_with` into a `String`.
    pub fn to_string_with(&self, options: WriteOptions) -> Result<String, WriteError> {
        let mut buf = Vec::new();
        {
            let mut writer = crate::Writer::new(&mut buf);
            self.write_to_with(&mut writer, options)?;
        }
        String::from_utf8(buf)
            .map_err(|_| WriteError::Syntax("writer emitted non-utf8".to_string()))
    }
}

impl NodeRef<'_> {
    /// Write this node and its descendants. A document node writes its children.
    pub fn write_to<W: Write>(self, writer: &mut crate::Writer<W>) -> Result<(), WriteError> {
        self.write_to_with(writer, WriteOptions::default())
    }

    /// Write this subtree. A document node uses the same root-element check as
    /// [`Document::write_to`]. `omit_doctype` applies only in that case.
    pub fn write_to_with<W: Write>(
        self,
        writer: &mut crate::Writer<W>,
        options: WriteOptions,
    ) -> Result<(), WriteError> {
        if self.kind() == Some(NodeKind::Document) {
            return write_document(self.doc, writer, options);
        }
        write_at(self.doc, self.id().index(), writer, options)
    }
}

fn write_document<W: Write>(
    doc: &Document,
    writer: &mut crate::Writer<W>,
    options: WriteOptions,
) -> Result<(), WriteError> {
    if doc.root_element().is_none() {
        return Err(WriteError::NoRootElement);
    }
    let Some(root) = doc.root() else {
        return Err(WriteError::NoRootElement);
    };
    write_at(doc, root.id().index(), writer, options)
}

enum Job {
    Open(u32),
    Close(u32),
}

fn write_at<W: Write>(
    doc: &Document,
    index: u32,
    writer: &mut crate::Writer<W>,
    options: WriteOptions,
) -> Result<(), WriteError> {
    let mut scope = OutScope::with_capacity(doc.ns_decls.len());
    scope.push(Some("xml"), crate::event::XML_NAMESPACE);
    let mut arcs = SpanArcs::default();
    let mut stack = vec![Job::Open(index)];
    let mut guard = doc.nodes.len().saturating_mul(4);
    while let Some(job) = stack.pop() {
        if guard == 0 {
            break;
        }
        guard -= 1;
        match job {
            Job::Close(current) => {
                scope.leave();
                writer.write_event(&Event::EndElement(EndElement {
                    name: element_name(doc, current, &mut arcs)?,
                }))?;
            }
            Job::Open(current) => open_node(
                doc, current, &mut stack, writer, options, &mut scope, &mut arcs,
            )?,
        }
    }
    Ok(())
}

fn open_node<'a, W: Write>(
    doc: &'a Document,
    index: u32,
    stack: &mut Vec<Job>,
    writer: &mut crate::Writer<W>,
    options: WriteOptions,
    scope: &mut OutScope<'a>,
    arcs: &mut SpanArcs,
) -> Result<(), WriteError> {
    let node = &doc.nodes[index as usize];
    if !node.alive {
        return Ok(());
    }
    match node.kind {
        NodeKind::Document => {
            for child in children(doc, index).into_iter().rev() {
                if options.omit_doctype && doc.nodes[child as usize].kind == NodeKind::Doctype {
                    continue;
                }
                stack.push(Job::Open(child));
            }
            Ok(())
        }
        NodeKind::Element => {
            let namespaces = namespaces_in_scope(doc, index, scope)?;
            let name = element_name(doc, index, arcs)?;
            let attributes = attributes(doc, index, arcs);
            let kids = children(doc, index);
            if kids.is_empty() {
                writer.write_event(&Event::EmptyElement(EmptyElement {
                    name,
                    attributes,
                    namespaces,
                }))?;
                scope.leave();
                Ok(())
            } else {
                stack.push(Job::Close(index));
                for child in kids.into_iter().rev() {
                    stack.push(Job::Open(child));
                }
                writer.write_event(&Event::StartElement(StartElement {
                    name,
                    attributes,
                    namespaces,
                }))
            }
        }
        NodeKind::Text => writer.write_event(&Event::Text(text_of(doc, index))),
        NodeKind::CData => writer.write_event(&Event::CData(text_of(doc, index))),
        NodeKind::Comment => writer.write_event(&Event::Comment(text_of(doc, index))),
        NodeKind::Pi => {
            let Payload::Pi { target, data } = node.payload else {
                return Ok(());
            };
            writer.write_event(&Event::PI(ProcessingInstruction {
                target: borrowed(doc, target),
                data: data.map(|span| borrowed(doc, span)),
            }))
        }
        NodeKind::Doctype => {
            if options.omit_doctype {
                return Ok(());
            }
            let Payload::Doctype { name, .. } = node.payload else {
                return Ok(());
            };
            writer.write_event(&Event::Doctype(Doctype {
                name: borrowed(doc, name),
                // External ids stay on the node. Writing them is an M3 option.
                system_id: None,
                public_id: None,
                has_internal_subset: false,
            }))
        }
    }
}

/// One `Arc` per arena span. The address is reused for every element that
/// shares that span, so the writer does not hash the URI bytes again.
#[derive(Default)]
struct SpanArcs {
    by_span: HashMap<(u32, u32), Arc<str>>,
}

fn arc_at(doc: &Document, span: StrSpan, arcs: &mut SpanArcs) -> Arc<str> {
    let key = (span.start, span.len);
    if let Some(existing) = arcs.by_span.get(&key) {
        return Arc::clone(existing);
    }
    let text = doc.str_at(span).unwrap_or("");
    let arc: Arc<str> = Arc::from(text);
    arcs.by_span.insert(key, Arc::clone(&arc));
    arc
}

fn element_name<'a>(
    doc: &'a Document,
    index: u32,
    arcs: &mut SpanArcs,
) -> Result<QName<'a>, WriteError> {
    let Payload::Element(slot) = doc.nodes[index as usize].payload else {
        return Err(WriteError::Syntax("missing element payload".to_string()));
    };
    let element = &doc.elements[slot as usize];
    let namespace = element.namespace.and_then(|span| {
        let text = doc.str_at(span)?;
        if text.is_empty() {
            None
        } else {
            Some(arc_at(doc, span, arcs))
        }
    });
    Ok(QName {
        prefix: element
            .prefix
            .and_then(|span| doc.str_at(span))
            .map(XmlText::borrowed),
        local: XmlText::borrowed(doc.str_at(element.local).unwrap_or("")),
        namespace,
    })
}

fn attributes<'a>(doc: &'a Document, index: u32, arcs: &mut SpanArcs) -> Vec<Attribute<'a>> {
    let Payload::Element(slot) = doc.nodes[index as usize].payload else {
        return Vec::new();
    };
    let range = doc.elements[slot as usize].attrs;
    let start = range.start as usize;
    let end = start + range.len as usize;
    doc.attrs
        .get(start..end)
        .unwrap_or(&[])
        .iter()
        .map(|attr| Attribute {
            name: QName {
                prefix: attr
                    .prefix
                    .and_then(|span| doc.str_at(span))
                    .map(XmlText::borrowed),
                local: XmlText::borrowed(doc.str_at(attr.local).unwrap_or("")),
                namespace: attr
                    .namespace
                    .filter(|span| doc.str_at(*span).is_some_and(|uri| !uri.is_empty()))
                    .map(|span| arc_at(doc, span, arcs)),
            },
            value: XmlText::borrowed(doc.str_at(attr.value).unwrap_or("")),
        })
        .collect()
}

struct Binding<'a> {
    prefix: Option<&'a str>,
    uri: &'a str,
}

/// Declarations already written by an ancestor, plus the ones this element adds.
///
/// Stored declarations are always emitted, including ones an ancestor already
/// wrote. A binding is invented only when the name needs one that is not in
/// this scope. A subtree write starts from a fresh scope, so its root stays
/// self-contained.
struct OutScope<'a> {
    /// One frame per open element, plus a base frame for `xml`.
    ///
    /// Declarations are appended here instead of inserted into a map of every
    /// prefix. A map that grows with the declaration count rehashes and makes
    /// `ns_scaled` look superlinear. Lookups scan the open frames; a cache
    /// remembers a prefix until the next push or pop.
    frames: Vec<Vec<(Option<&'a str>, &'a str)>>,
    defaults: Vec<&'a str>,
    cache: HashMap<&'a str, (u64, &'a str)>,
    epoch: u64,
    /// Arena-pointer of a URI already interned, mapping to the one canonical
    /// `&str` for that content. Each address is hashed at most once.
    proven: HashMap<usize, &'a str>,
    /// Content intern table. The key is the canonical string itself.
    by_text: HashMap<&'a str, &'a str>,
}

impl<'a> OutScope<'a> {
    fn with_capacity(decls: usize) -> Self {
        let mut frames = Vec::with_capacity(64);
        frames.push(Vec::new());
        Self {
            frames,
            defaults: Vec::new(),
            cache: HashMap::new(),
            epoch: 1,
            proven: HashMap::with_capacity(decls.min(64)),
            by_text: HashMap::with_capacity(decls.min(64)),
        }
    }

    fn lookup(&mut self, prefix: Option<&str>) -> Option<&'a str> {
        match prefix {
            None => self.defaults.last().copied(),
            Some(prefix) => self.lookup_prefix(prefix),
        }
    }

    fn lookup_prefix(&mut self, prefix: &str) -> Option<&'a str> {
        if let Some(&(epoch, uri)) = self.cache.get(prefix)
            && epoch == self.epoch
        {
            return Some(uri);
        }
        for frame in self.frames.iter().rev() {
            for (bound, uri) in frame.iter().rev() {
                if bound.is_some_and(|bound| bound == prefix) {
                    let uri = *uri;
                    let key = bound.unwrap();
                    self.cache.insert(key, (self.epoch, uri));
                    return Some(uri);
                }
            }
        }
        None
    }

    fn push(&mut self, prefix: Option<&'a str>, uri: &'a str) {
        if prefix.is_none() {
            self.defaults.push(uri);
        }
        self.frames
            .last_mut()
            .expect("base frame")
            .push((prefix, uri));
        self.epoch = self.epoch.wrapping_add(1);
    }

    fn enter(&mut self) {
        self.frames.push(Vec::new());
    }

    fn leave(&mut self) {
        let frame = self.frames.pop().expect("element frame");
        let defaults = frame.iter().filter(|(prefix, _)| prefix.is_none()).count();
        let len = self.defaults.len();
        self.defaults.truncate(len.saturating_sub(defaults));
        self.epoch = self.epoch.wrapping_add(1);
    }

    /// One id per URI content. The address is only a fast path; the first
    /// sight of an address hashes the bytes and joins the content table.
    fn canonical(&mut self, uri: &'a str) -> &'a str {
        let ptr = uri.as_ptr() as usize;
        if let Some(canon) = self.proven.get(&ptr).copied() {
            return canon;
        }
        if let Some(canon) = self.by_text.get(uri).copied() {
            self.proven.insert(ptr, canon);
            return canon;
        }
        self.by_text.insert(uri, uri);
        self.proven.insert(ptr, uri);
        uri
    }

    /// True when both strings are the same namespace URI.
    fn same_uri(&mut self, existing: &'a str, uri: &'a str) -> bool {
        let left = self.canonical(existing);
        let right = self.canonical(uri);
        left.as_ptr() == right.as_ptr() && left.len() == right.len()
    }
}

fn namespaces_in_scope<'a>(
    doc: &'a Document,
    index: u32,
    scope: &mut OutScope<'a>,
) -> Result<Vec<Namespace<'a>>, WriteError> {
    let Payload::Element(slot) = doc.nodes[index as usize].payload else {
        return Ok(Vec::new());
    };
    let element = &doc.elements[slot as usize];
    scope.enter();
    let mut out = Vec::new();
    for binding in stored_bindings(doc, index) {
        scope.push(binding.prefix, binding.uri);
        out.push(namespace_of(binding));
    }
    require_element(
        scope,
        &mut out,
        element.prefix.and_then(|span| doc.str_at(span)),
        element
            .namespace
            .and_then(|span| doc.str_at(span))
            .unwrap_or(""),
    )?;
    let range = element.attrs;
    let start = range.start as usize;
    let end = start + range.len as usize;
    for attr in doc.attrs.get(start..end).unwrap_or(&[]) {
        require_attribute(
            scope,
            &mut out,
            attr.prefix.and_then(|span| doc.str_at(span)),
            attr.namespace
                .and_then(|span| doc.str_at(span))
                .unwrap_or(""),
        )?;
    }
    Ok(out)
}

fn namespace_of(binding: Binding<'_>) -> Namespace<'_> {
    Namespace {
        prefix: binding.prefix.map(XmlText::borrowed),
        uri: XmlText::borrowed(binding.uri),
    }
}

fn stored_bindings(doc: &Document, index: u32) -> Vec<Binding<'_>> {
    let Payload::Element(slot) = doc.nodes[index as usize].payload else {
        return Vec::new();
    };
    let range = doc.elements[slot as usize].ns_decls;
    let start = range.start as usize;
    let end = start + range.len as usize;
    doc.ns_decls
        .get(start..end)
        .unwrap_or(&[])
        .iter()
        .filter_map(|decl| {
            Some(Binding {
                prefix: decl.prefix.and_then(|span| doc.str_at(span)),
                uri: doc.str_at(decl.uri)?,
            })
        })
        .collect()
}

/// The element name uses the default namespace when it has no prefix.
fn require_element<'a>(
    scope: &mut OutScope<'a>,
    out: &mut Vec<Namespace<'a>>,
    prefix: Option<&'a str>,
    uri: &'a str,
) -> Result<(), WriteError> {
    if prefix == Some("xml") {
        return if uri == crate::event::XML_NAMESPACE {
            Ok(())
        } else {
            Err(WriteError::Syntax(
                "the xml prefix is bound to the XML namespace".to_string(),
            ))
        };
    }
    if prefix.is_none() && uri.is_empty() {
        if scope
            .lookup(None)
            .is_some_and(|existing| !existing.is_empty())
        {
            scope.push(None, "");
            out.push(Namespace {
                prefix: None,
                uri: XmlText::borrowed(""),
            });
        }
        return Ok(());
    }
    align_scope(scope, out, prefix, uri)
}

/// An unprefixed attribute is in no namespace. The default namespace does not
/// apply to it, so serialization must not invent a prefix or a declaration.
fn require_attribute<'a>(
    scope: &mut OutScope<'a>,
    out: &mut Vec<Namespace<'a>>,
    prefix: Option<&'a str>,
    uri: &'a str,
) -> Result<(), WriteError> {
    if prefix.is_none() {
        return Ok(());
    }
    if prefix == Some("xml") {
        return if uri == crate::event::XML_NAMESPACE {
            Ok(())
        } else {
            Err(WriteError::Syntax(
                "the xml prefix is bound to the XML namespace".to_string(),
            ))
        };
    }
    align_scope(scope, out, prefix, uri)
}

fn align_scope<'a>(
    scope: &mut OutScope<'a>,
    out: &mut Vec<Namespace<'a>>,
    prefix: Option<&'a str>,
    uri: &'a str,
) -> Result<(), WriteError> {
    if let Some(existing) = scope.lookup(prefix) {
        return if scope.same_uri(existing, uri) {
            Ok(())
        } else {
            Err(WriteError::Syntax(format!(
                "prefix `{}` is bound to `{existing}` and `{uri}`",
                prefix.unwrap_or(""),
            )))
        };
    }
    if prefix.is_some() && uri.is_empty() {
        return Err(WriteError::Syntax(
            "a prefixed name has no namespace URI".to_string(),
        ));
    }
    scope.push(prefix, uri);
    out.push(Namespace {
        prefix: prefix.map(XmlText::borrowed),
        uri: XmlText::borrowed(uri),
    });
    Ok(())
}

fn text_of(doc: &Document, index: u32) -> XmlText<'_> {
    let Payload::Span(span) = doc.nodes[index as usize].payload else {
        return XmlText::borrowed("");
    };
    borrowed(doc, span)
}

fn borrowed(doc: &Document, span: StrSpan) -> XmlText<'_> {
    XmlText::borrowed(doc.str_at(span).unwrap_or(""))
}

fn children(doc: &Document, parent: u32) -> Vec<u32> {
    let mut out = Vec::new();
    let mut child = doc
        .nodes
        .get(parent as usize)
        .and_then(|node| node.first_child);
    let mut guard = doc.nodes.len();
    while let Some(index) = child {
        if guard == 0 {
            break;
        }
        guard -= 1;
        let node = &doc.nodes[index as usize];
        if node.alive {
            out.push(index);
        }
        child = node.next;
    }
    out
}

#[cfg(test)]
mod tests {
    use crate::dom::Document;
    use crate::dom::edit::{Name, WriteOptions};

    #[test]
    fn omit_doctype_drops_a_system_identifier() {
        let mut doc = Document::new();
        let root = doc.root().unwrap().id();
        let dtd = doc.create_doctype("r", Some("sys"), None).unwrap();
        doc.append_child(root, dtd).unwrap();
        let element = doc.create_element(crate::dom::Name::local("r")).unwrap();
        doc.append_child(root, element).unwrap();
        let full = doc.to_string().unwrap();
        assert!(full.starts_with("<!DOCTYPE r>"));
        assert!(!full.contains("SYSTEM"));
        let stored = doc.get(dtd).unwrap().doctype().unwrap();
        assert_eq!(stored.system_id, Some("sys"));
        let omitted = doc
            .to_string_with(WriteOptions { omit_doctype: true })
            .unwrap();
        assert!(!omitted.contains("<!DOCTYPE"));
        assert!(omitted.contains("<r"));
    }

    #[test]
    fn ancestor_declarations_are_not_invented_again() {
        let xml = r#"<r xmlns="urn:b" xmlns:p="urn:a"><c xmlns="urn:b"><p:d xmlns:p="urn:a" p:x="1"/></c></r>"#;
        let doc = Document::parse(xml).unwrap();
        let once = doc.to_string().unwrap();
        assert_eq!(once, xml);
        let twice = Document::parse(&once).unwrap().to_string().unwrap();
        assert_eq!(twice, once);
    }

    #[test]
    fn no_namespace_child_clears_a_default_namespace() {
        let mut doc = Document::new();
        let root = doc.root().unwrap().id();
        let element = doc
            .create_element(Name::qualified("urn:b", None, "r"))
            .unwrap();
        doc.declare_namespace(element, None, "urn:b").unwrap();
        doc.append_child(root, element).unwrap();
        let child = doc.create_element(Name::local("c")).unwrap();
        doc.link_last(element.index(), child.index());
        assert_eq!(
            doc.to_string().unwrap(),
            r#"<r xmlns="urn:b"><c xmlns=""/></r>"#
        );
    }
}
