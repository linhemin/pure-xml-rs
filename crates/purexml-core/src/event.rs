//! Pull-parser events.
//!
//! Event strings are [`XmlText`]: a borrow of the input when the parser did
//! not normalize or expand them, or an [`Arc<str>`] when the bytes are owned.
//! [`QName::namespace`] stays an interned [`Arc<str>`] so every name that
//! shares a binding shares one allocation. `Event`, `XmlText`, `QName`,
//! `Attribute`, and [`crate::Error`] are `Send + Sync`.

use std::borrow::Cow;
use std::fmt;
use std::hash::{Hash, Hasher};
use std::ops::Deref;
use std::sync::Arc;

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
    pub prefix: Option<XmlText<'a>>,
    /// Local part.
    pub local: XmlText<'a>,
    /// Resolved namespace URI.
    ///
    /// Interned with [`Arc`] so every element and prefixed attribute that
    /// shares a binding shares one allocation. Compare with [`Option::as_deref`].
    pub namespace: Option<Arc<str>>,
}

/// Text borrowed from the input, or shared owned text.
///
/// Literals borrow the input (`is_borrowed()`). A value that is not a slice
/// of the input — a normalized attribute, expanded text, or a shared DTD
/// default — is one [`Arc`]. Equality, ordering of the bytes, and hashing
/// follow the string contents, not which variant holds them.
#[derive(Clone, Debug)]
pub struct XmlText<'a> {
    inner: XmlTextInner<'a>,
}

#[derive(Clone, Debug)]
enum XmlTextInner<'a> {
    Borrowed(&'a str),
    Shared(Arc<str>),
}

impl<'a> XmlText<'a> {
    /// Text that points into the caller's input.
    pub fn borrowed(text: &'a str) -> Self {
        Self {
            inner: XmlTextInner::Borrowed(text),
        }
    }

    /// Text that lives in a shared allocation.
    pub fn shared(text: Arc<str>) -> Self {
        Self {
            inner: XmlTextInner::Shared(text),
        }
    }

    /// Borrow the text.
    pub fn as_str(&self) -> &str {
        match &self.inner {
            XmlTextInner::Borrowed(text) => text,
            XmlTextInner::Shared(text) => text.as_ref(),
        }
    }

    /// True when the bytes are a slice of the caller's input.
    pub fn is_borrowed(&self) -> bool {
        matches!(self.inner, XmlTextInner::Borrowed(_))
    }

    /// Copy the text into an owned [`String`].
    pub fn into_owned(self) -> String {
        match self.inner {
            XmlTextInner::Borrowed(text) => text.to_string(),
            XmlTextInner::Shared(text) => text.as_ref().to_string(),
        }
    }

    /// Borrow when the text is a slice of the input, and own a copy otherwise.
    pub fn into_cow(self) -> Cow<'a, str> {
        match self.inner {
            XmlTextInner::Borrowed(text) => Cow::Borrowed(text),
            XmlTextInner::Shared(text) => Cow::Owned(text.as_ref().to_string()),
        }
    }
}

impl PartialEq for XmlText<'_> {
    fn eq(&self, other: &Self) -> bool {
        self.as_str() == other.as_str()
    }
}

impl Eq for XmlText<'_> {}

impl PartialEq<str> for XmlText<'_> {
    fn eq(&self, other: &str) -> bool {
        self.as_str() == other
    }
}

impl PartialEq<&str> for XmlText<'_> {
    fn eq(&self, other: &&str) -> bool {
        self.as_str() == *other
    }
}

impl PartialEq<String> for XmlText<'_> {
    fn eq(&self, other: &String) -> bool {
        self.as_str() == other
    }
}

impl PartialEq<XmlText<'_>> for str {
    fn eq(&self, other: &XmlText<'_>) -> bool {
        self == other.as_str()
    }
}

impl PartialEq<XmlText<'_>> for &str {
    fn eq(&self, other: &XmlText<'_>) -> bool {
        *self == other.as_str()
    }
}

impl PartialEq<XmlText<'_>> for String {
    fn eq(&self, other: &XmlText<'_>) -> bool {
        self == other.as_str()
    }
}

impl Hash for XmlText<'_> {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.as_str().hash(state);
    }
}

impl Deref for XmlText<'_> {
    type Target = str;

    fn deref(&self) -> &str {
        self.as_str()
    }
}

impl AsRef<str> for XmlText<'_> {
    fn as_ref(&self) -> &str {
        self.as_str()
    }
}

impl fmt::Display for XmlText<'_> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl<'a> From<XmlText<'a>> for Cow<'a, str> {
    fn from(text: XmlText<'a>) -> Self {
        text.into_cow()
    }
}

/// One attribute. Namespace declarations are reported separately.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Attribute<'a> {
    /// Attribute name. Unprefixed attributes have `namespace: None`.
    pub name: QName<'a>,
    /// Value after attribute-value normalization and entity expansion.
    pub value: XmlText<'a>,
}

/// A namespace declaration on a start or empty element.
///
/// `prefix == None` is the default namespace (`xmlns="..."`). An empty `uri`
/// on the default namespace undeclares it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Namespace<'a> {
    /// Bound prefix, or `None` for the default namespace.
    pub prefix: Option<XmlText<'a>>,
    /// Namespace URI. Empty means the default namespace is undeclared.
    pub uri: XmlText<'a>,
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
    pub version: XmlText<'a>,
    /// Encoding name, if declared. Only UTF-8 is accepted.
    pub encoding: Option<XmlText<'a>>,
    /// Standalone flag, if declared. It never enables external loading.
    pub standalone: Option<bool>,
}

/// `<!DOCTYPE ...>`.
///
/// `system_id` and `public_id` are reported when present and are never opened.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Doctype<'a> {
    /// Document type name.
    pub name: XmlText<'a>,
    /// External subset system identifier, if declared.
    pub system_id: Option<XmlText<'a>>,
    /// External subset public identifier, if declared.
    pub public_id: Option<XmlText<'a>>,
    /// Whether an internal subset (`[...]`) was present.
    pub has_internal_subset: bool,
}

/// A processing instruction. The XML declaration is [`Event::XmlDecl`], not a PI.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcessingInstruction<'a> {
    /// PI target.
    pub target: XmlText<'a>,
    /// Data after the separating whitespace, if any.
    pub data: Option<XmlText<'a>>,
}

/// One pull-parser event.
///
/// String fields are [`XmlText`]. They borrow the original input when the
/// parser did not need to normalize line endings or expand entities.
/// Otherwise they are a shared [`Arc`]. The lifetime is tied to the
/// [`crate::Reader`] borrow from [`crate::Reader::read_event`], so an event
/// cannot be held across the next pull. Call [`XmlText::into_owned`] to keep
/// a copy.
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
    Text(XmlText<'a>),
    /// CDATA section contents, after end-of-line normalization.
    CData(XmlText<'a>),
    /// Comment body, without `<!--` and `-->`.
    Comment(XmlText<'a>),
    /// Processing instruction.
    PI(ProcessingInstruction<'a>),
    /// XML declaration.
    XmlDecl(XmlDecl<'a>),
    /// Document type declaration.
    Doctype(Doctype<'a>),
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;
    use std::sync::Arc;

    use super::XmlText;

    #[test]
    fn xml_text_compares_by_contents_and_converts() {
        let borrowed = XmlText::borrowed("ab");
        let shared = XmlText::shared(Arc::from("ab"));
        assert!(borrowed.is_borrowed());
        assert!(!shared.is_borrowed());
        assert_eq!(borrowed.as_str(), "ab");
        assert_eq!(&*borrowed, "ab");
        assert_eq!(borrowed, shared);
        assert_eq!(borrowed, "ab");
        assert_eq!("ab", borrowed);
        assert_eq!(borrowed, "ab".to_string());
        assert_eq!("ab".to_string(), borrowed);
        assert_eq!(borrowed.clone().into_owned(), "ab");
        assert!(matches!(
            borrowed.clone().into_cow(),
            std::borrow::Cow::Borrowed("ab")
        ));
        let owned: std::borrow::Cow<'_, str> = shared.clone().into();
        assert_eq!(owned, "ab");
        let mut set = HashSet::new();
        set.insert(shared);
        assert!(set.contains(&borrowed));
    }
}
