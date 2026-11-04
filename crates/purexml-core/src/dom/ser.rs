//! Serialize a document or a subtree through [`crate::Writer`].

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
    pub fn write_to_with<W: Write>(
        &self,
        writer: &mut crate::Writer<W>,
        options: WriteOptions,
    ) -> Result<(), WriteError> {
        let Some(root) = self.root() else {
            return Ok(());
        };
        write_at(self, root.id().index(), writer, options)
    }

    /// `write_to` into a `String`.
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

    /// Write this subtree. `omit_doctype` applies when the node is the document.
    pub fn write_to_with<W: Write>(
        self,
        writer: &mut crate::Writer<W>,
        options: WriteOptions,
    ) -> Result<(), WriteError> {
        write_at(self.doc, self.id().index(), writer, options)
    }
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
    let mut stack = vec![Job::Open(index)];
    let mut guard = doc.nodes.len().saturating_mul(4);
    while let Some(job) = stack.pop() {
        if guard == 0 {
            break;
        }
        guard -= 1;
        match job {
            Job::Close(current) => writer.write_event(&Event::EndElement(EndElement {
                name: element_name(doc, current)?,
            }))?,
            Job::Open(current) => open_node(doc, current, &mut stack, writer, options)?,
        }
    }
    Ok(())
}

fn open_node<W: Write>(
    doc: &Document,
    index: u32,
    stack: &mut Vec<Job>,
    writer: &mut crate::Writer<W>,
    options: WriteOptions,
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
            let namespaces = covered_namespaces(doc, index)?;
            let name = element_name(doc, index)?;
            let attributes = attributes(doc, index);
            let kids = children(doc, index);
            if kids.is_empty() {
                writer.write_event(&Event::EmptyElement(EmptyElement {
                    name,
                    attributes,
                    namespaces,
                }))
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
            let Payload::Doctype {
                name,
                system,
                public,
            } = node.payload
            else {
                return Ok(());
            };
            writer.write_event(&Event::Doctype(Doctype {
                name: borrowed(doc, name),
                system_id: system.map(|span| borrowed(doc, span)),
                public_id: public.map(|span| borrowed(doc, span)),
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

/// Declarations stored on the element, plus any binding its name or attributes
/// need so the start tag stays namespace-well-formed on its own.
fn covered_namespaces<'a>(doc: &'a Document, index: u32) -> Result<Vec<Namespace<'a>>, WriteError> {
    let Payload::Element(slot) = doc.nodes[index as usize].payload else {
        return Ok(Vec::new());
    };
    let element = &doc.elements[slot as usize];
    let mut bindings = stored_bindings(doc, index);
    require_binding(
        &mut bindings,
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
        require_binding(
            &mut bindings,
            attr.prefix.and_then(|span| doc.str_at(span)),
            attr.namespace
                .and_then(|span| doc.str_at(span))
                .unwrap_or(""),
        )?;
    }
    Ok(bindings
        .into_iter()
        .map(|binding| Namespace {
            prefix: binding.prefix.map(XmlText::borrowed),
            uri: XmlText::borrowed(binding.uri),
        })
        .collect())
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

fn require_binding<'a>(
    bindings: &mut Vec<Binding<'a>>,
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
    // An unprefixed attribute is in no namespace and does not use the default.
    if prefix.is_none() && uri.is_empty() {
        if let Some(existing) = bindings.iter().find(|binding| binding.prefix.is_none())
            && !existing.uri.is_empty()
        {
            return Err(WriteError::Syntax(
                "default namespace does not match the element name".to_string(),
            ));
        }
        return Ok(());
    }
    if let Some(existing) = bindings.iter().find(|binding| binding.prefix == prefix) {
        return if existing.uri == uri {
            Ok(())
        } else {
            Err(WriteError::Syntax(format!(
                "prefix `{}` is bound to `{}` and `{}`",
                prefix.unwrap_or(""),
                existing.uri,
                uri
            )))
        };
    }
    if prefix.is_some() && uri.is_empty() {
        return Err(WriteError::Syntax(
            "a prefixed name has no namespace URI".to_string(),
        ));
    }
    bindings.push(Binding { prefix, uri });
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

#[cfg(test)]
mod tests {
    use crate::dom::Document;
    use crate::dom::edit::WriteOptions;

    #[test]
    fn omit_doctype_drops_a_system_identifier() {
        let mut doc = Document::new();
        let root = doc.root().unwrap().id();
        let dtd = doc.create_doctype("r", Some("sys"), None).unwrap();
        doc.append_child(root, dtd).unwrap();
        let element = doc.create_element(crate::dom::Name::local("r")).unwrap();
        doc.append_child(root, element).unwrap();
        let full = doc.to_string().unwrap();
        assert!(full.contains("<!DOCTYPE r SYSTEM \"sys\">"));
        let omitted = doc
            .to_string_with(WriteOptions { omit_doctype: true })
            .unwrap();
        assert!(!omitted.contains("<!DOCTYPE"));
        assert!(omitted.contains("<r"));
    }
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
