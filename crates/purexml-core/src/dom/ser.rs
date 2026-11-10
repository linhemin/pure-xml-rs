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

fn write_at<'a, W: Write>(
    doc: &'a Document,
    index: u32,
    writer: &mut crate::Writer<W>,
    options: WriteOptions,
) -> Result<(), WriteError> {
    let mut scope = OutScope::empty();
    scope.push(Some("xml"), crate::event::XML_NAMESPACE);
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
                    name: element_name(doc, current)?,
                }))?;
            }
            Job::Open(current) => open_node(doc, current, &mut stack, writer, options, &mut scope)?,
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
            let name = element_name(doc, index)?;
            let attributes = attributes(doc, index);
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

fn element_name(doc: &Document, index: u32) -> Result<QName<'_>, WriteError> {
    let Payload::Element(slot) = doc.nodes[index as usize].payload else {
        return Err(WriteError::Syntax("missing element payload".to_string()));
    };
    let element = &doc.elements[slot as usize];
    Ok(QName {
        prefix: element
            .prefix
            .and_then(|span| doc.str_at(span))
            .map(XmlText::borrowed),
        local: XmlText::borrowed(doc.str_at(element.local).unwrap_or("")),
        namespace: element
            .namespace
            .and_then(|span| doc.str_at(span))
            .filter(|uri| !uri.is_empty())
            .map(Arc::<str>::from),
    })
}

fn attributes(doc: &Document, index: u32) -> Vec<Attribute<'_>> {
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
                    .and_then(|span| doc.str_at(span))
                    .filter(|uri| !uri.is_empty())
                    .map(Arc::<str>::from),
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
    map: HashMap<Option<String>, Vec<&'a str>>,
    undo: Vec<(usize, Option<String>)>,
    depth: usize,
}

impl<'a> OutScope<'a> {
    fn empty() -> Self {
        Self {
            map: HashMap::new(),
            undo: Vec::new(),
            depth: 0,
        }
    }

    fn lookup(&self, prefix: Option<&str>) -> Option<&'a str> {
        let key = prefix.map(str::to_string);
        self.map.get(&key).and_then(|stack| stack.last().copied())
    }

    fn push(&mut self, prefix: Option<&str>, uri: &'a str) {
        let key = prefix.map(str::to_string);
        self.map.entry(key.clone()).or_default().push(uri);
        self.undo.push((self.depth, key));
    }

    fn enter(&mut self) {
        self.depth += 1;
    }

    fn leave(&mut self) {
        while self
            .undo
            .last()
            .is_some_and(|(depth, _)| *depth == self.depth)
        {
            let (_, key) = self.undo.pop().expect("undo entry");
            if let Some(stack) = self.map.get_mut(&key) {
                stack.pop();
            }
        }
        self.depth = self.depth.saturating_sub(1);
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
        return if existing == uri {
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
