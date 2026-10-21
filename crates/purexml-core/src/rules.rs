//! Name and namespace-binding rules shared by the reader and the writer.
//!
//! Namespaces in XML 1.0: a processing-instruction target, an entity name, and
//! a notation name are NCNames (no colon). A document-type name, an element
//! name, and an attribute name are QNames: an NCName, or two NCNames joined by
//! one colon. The DOM will call the same functions so a name the reader
//! accepts is a name the writer can emit.

use crate::event::{XMLNS_NAMESPACE, XML_NAMESPACE};
use crate::name::{is_ncname_char, is_ncname_start};

/// `true` when `name` is one NCName.
pub(crate) fn is_ncname(name: &str) -> bool {
    let mut chars = name.chars();
    match chars.next() {
        Some(first) if is_ncname_start(first) => chars.all(is_ncname_char),
        _ => false,
    }
}

/// `true` when `name` is a QName.
///
/// `a:b` is a QName. `r:`, `:r`, and `a:b:c` are not.
pub(crate) fn is_qname(name: &str) -> bool {
    match name.split_once(':') {
        Some((prefix, local)) => is_ncname(prefix) && is_ncname(local),
        None => is_ncname(name),
    }
}

/// Why a namespace declaration is rejected.
///
/// The reader and the writer map each fault onto their own message text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BindingFault {
    /// The prefix `xmlns` must not be declared.
    XmlnsPrefix,
    /// The prefix `xml` is bound to something other than the XML namespace.
    XmlPrefix,
    /// A non-default declaration uses an empty URI.
    EmptyPrefixed,
    /// The XML namespace is bound to a prefix other than `xml`, or to the
    /// default namespace.
    XmlNamespace,
    /// The xmlns namespace URI is used in a declaration.
    XmlnsNamespace,
}

/// Reserved-namespace rules for one `xmlns` declaration.
pub(crate) fn namespace_binding(prefix: Option<&str>, uri: &str) -> Result<(), BindingFault> {
    if prefix == Some("xmlns") {
        return Err(BindingFault::XmlnsPrefix);
    }
    if prefix == Some("xml") && uri != XML_NAMESPACE {
        return Err(BindingFault::XmlPrefix);
    }
    if prefix.is_some() && uri.is_empty() {
        return Err(BindingFault::EmptyPrefixed);
    }
    if uri == XML_NAMESPACE && prefix != Some("xml") {
        return Err(BindingFault::XmlNamespace);
    }
    if uri == XMLNS_NAMESPACE {
        return Err(BindingFault::XmlnsNamespace);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ncname_and_qname_agree_with_namespaces() {
        assert!(is_ncname("ab"));
        assert!(is_ncname("xml-stylesheet"));
        assert!(is_ncname("_1"));
        assert!(!is_ncname(""));
        assert!(!is_ncname(":a"));
        assert!(!is_ncname("a:"));
        assert!(!is_ncname("a:b"));
        assert!(!is_ncname("xmlns:"));
        assert!(!is_ncname(":ml"));

        assert!(is_qname("ab"));
        assert!(is_qname("a:b"));
        assert!(!is_qname(""));
        assert!(!is_qname("r:"));
        assert!(!is_qname(":r"));
        assert!(!is_qname("a:b:c"));
        assert!(!is_qname("a::b"));
        assert!(!is_qname(":"));
    }

    #[test]
    fn reserved_bindings_match_both_sides() {
        assert_eq!(
            namespace_binding(Some("xmlns"), "urn"),
            Err(BindingFault::XmlnsPrefix)
        );
        assert_eq!(
            namespace_binding(Some("xml"), "urn"),
            Err(BindingFault::XmlPrefix)
        );
        assert_eq!(namespace_binding(Some("xml"), XML_NAMESPACE), Ok(()));
        assert_eq!(
            namespace_binding(Some("p"), ""),
            Err(BindingFault::EmptyPrefixed)
        );
        assert_eq!(namespace_binding(None, ""), Ok(()));
        assert_eq!(
            namespace_binding(Some("p"), XML_NAMESPACE),
            Err(BindingFault::XmlNamespace)
        );
        assert_eq!(
            namespace_binding(None, XML_NAMESPACE),
            Err(BindingFault::XmlNamespace)
        );
        assert_eq!(
            namespace_binding(Some("p"), XMLNS_NAMESPACE),
            Err(BindingFault::XmlnsNamespace)
        );
        assert_eq!(
            namespace_binding(None, XMLNS_NAMESPACE),
            Err(BindingFault::XmlnsNamespace)
        );
        assert_eq!(namespace_binding(Some("p"), "urn:p"), Ok(()));
    }
}
