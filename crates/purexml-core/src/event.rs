//! Pull-parser events. Text is borrowed from the input when it needs no
//! normalization or entity expansion.

use std::borrow::Cow;

/// XML namespace URI bound to the `xml` prefix.
pub const XML_NAMESPACE: &str = "http://www.w3.org/XML/1998/namespace";

/// XML namespace URI bound to the `xmlns` prefix.
pub const XMLNS_NAMESPACE: &str = "http://www.w3.org/2000/xmlns/";

/// A qualified name after namespace resolution.
///
/// `namespace` is `None` when the name is in no namespace. The default
/// namespace applies to elements and not to unprefixed attributes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QName<'a> {
    /// Prefix, without the colon. `None` when the name was unprefixed.
    pub prefix: Option<Cow<'a, str>>,
    /// Local part.
    pub local: Cow<'a, str>,
    /// Resolved namespace URI.
    pub namespace: Option<Cow<'a, str>>,
}

/// One attribute. Namespace declarations are reported separately.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Attribute<'a> {
    /// Attribute name. Unprefixed attributes have `namespace: None`.
    pub name: QName<'a>,
    /// Value after attribute-value normalization and entity expansion.
    pub value: Cow<'a, str>,
}

/// A namespace declaration on a start or empty element.
///
/// `prefix == None` is the default namespace (`xmlns="..."`). An empty `uri`
/// on the default namespace undeclares it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Namespace<'a> {
    /// Bound prefix, or `None` for the default namespace.
    pub prefix: Option<Cow<'a, str>>,
    /// Namespace URI. Empty means the default namespace is undeclared.
    pub uri: Cow<'a, str>,
}

/// An element start tag.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StartElement<'a> {
    /// Element name.
    pub name: QName<'a>,
    /// Attributes in document order, excluding namespace declarations.
    pub attributes: Vec<Attribute<'a>>,
    /// Namespace declarations on this element, in document order.
    pub namespaces: Vec<Namespace<'a>>,
}

impl<'a> StartElement<'a> {
    /// Iterate attributes in document order.
    pub fn attributes(&self) -> std::slice::Iter<'_, Attribute<'a>> {
        self.attributes.iter()
    }

    /// First attribute with this local name, in any namespace.
    pub fn attribute(&self, local: &str) -> Option<&Attribute<'a>> {
        self.attributes
            .iter()
            .find(|attr| attr.name.local.as_ref() == local)
    }

    /// Attribute with this resolved namespace URI and local name.
    ///
    /// Pass `None` for an attribute in no namespace.
    pub fn attribute_ns(&self, namespace: Option<&str>, local: &str) -> Option<&Attribute<'a>> {
        self.attributes.iter().find(|attr| {
            attr.name.local.as_ref() == local && attr.name.namespace.as_deref() == namespace
        })
    }
}

/// An element end tag.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EndElement<'a> {
    /// Name, including the namespace that was in scope on the matching start tag.
    pub name: QName<'a>,
}

/// An empty element tag (`<name/>`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EmptyElement<'a> {
    /// Element name.
    pub name: QName<'a>,
    /// Attributes in document order, excluding namespace declarations.
    pub attributes: Vec<Attribute<'a>>,
    /// Namespace declarations on this element, in document order.
    pub namespaces: Vec<Namespace<'a>>,
}

impl<'a> EmptyElement<'a> {
    /// Iterate attributes in document order.
    pub fn attributes(&self) -> std::slice::Iter<'_, Attribute<'a>> {
        self.attributes.iter()
    }

    /// First attribute with this local name, in any namespace.
    pub fn attribute(&self, local: &str) -> Option<&Attribute<'a>> {
        self.attributes
            .iter()
            .find(|attr| attr.name.local.as_ref() == local)
    }
}

/// `<?xml ...?>` at the start of the document.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct XmlDecl<'a> {
    /// Version string, `1.0` or `1.1`.
    pub version: Cow<'a, str>,
    /// Encoding name, if declared. Only UTF-8 is accepted.
    pub encoding: Option<Cow<'a, str>>,
    /// Standalone flag, if declared. It never enables external loading.
    pub standalone: Option<bool>,
}

/// `<!DOCTYPE ...>`.
///
/// `system_id` and `public_id` are reported when present and are never opened.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Doctype<'a> {
    /// Document type name.
    pub name: Cow<'a, str>,
    /// External subset system identifier, if declared.
    pub system_id: Option<Cow<'a, str>>,
    /// External subset public identifier, if declared.
    pub public_id: Option<Cow<'a, str>>,
    /// Whether an internal subset (`[...]`) was present.
    pub has_internal_subset: bool,
}

/// A processing instruction. The XML declaration is [`Event::XmlDecl`], not a PI.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcessingInstruction<'a> {
    /// PI target.
    pub target: Cow<'a, str>,
    /// Data after the separating whitespace, if any.
    pub data: Option<Cow<'a, str>>,
}

/// One pull-parser event.
///
/// String data is [`Cow::Borrowed`] from the original input when the parser
/// did not need to normalize line endings or expand entities. Otherwise it is
/// [`Cow::Owned`]. The lifetime is tied to the [`crate::Reader`] borrow from
/// [`crate::Reader::read_event`], so an event cannot be held across the next
/// pull. Call [`Cow::into_owned`] to keep a copy.
///
/// The event set is part of the unstable 0.x API.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event<'a> {
    /// `<name ...>`.
    StartElement(StartElement<'a>),
    /// `</name>`.
    EndElement(EndElement<'a>),
    /// `<name .../>`.
    EmptyElement(EmptyElement<'a>),
    /// Character data, after end-of-line normalization and entity expansion.
    Text(Cow<'a, str>),
    /// CDATA section contents, after end-of-line normalization.
    CData(Cow<'a, str>),
    /// Comment body, without `<!--` and `-->`.
    Comment(Cow<'a, str>),
    /// Processing instruction.
    PI(ProcessingInstruction<'a>),
    /// XML declaration.
    XmlDecl(XmlDecl<'a>),
    /// Document type declaration.
    Doctype(Doctype<'a>),
}
