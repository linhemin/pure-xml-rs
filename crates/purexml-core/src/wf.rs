//! Well-formedness of an internal general entity's replacement text.
//!
//! XML 1.0 requires that text to match `content`. References are left intact:
//! a general-entity reference is one content particle, not an invitation to
//! expand it here. That is what keeps tags and comments from crossing the
//! entity boundary.

use crate::name::{is_name_char, is_name_start, is_ncname_char, is_ncname_start, is_xml_char};

pub(crate) enum WfFail {
    Syntax(String),
    /// Nested elements in the replacement exceed `max_depth`.
    TooDeep {
        limit: u64,
        actual: u64,
    },
}

impl From<String> for WfFail {
    fn from(message: String) -> Self {
        Self::Syntax(message)
    }
}

pub(crate) fn check_replacement_content(text: &str, max_depth: usize) -> Result<(), WfFail> {
    let mut cursor = Cursor { text, index: 0 };
    cursor.scan(max_depth)?;
    if cursor.index != text.len() {
        return Err(WfFail::Syntax(
            "entity replacement text is not well-formed content".to_string(),
        ));
    }
    Ok(())
}

/// Discard leading and trailing spaces and collapse internal runs.
/// Used for non-CDATA attribute types.
pub(crate) fn collapse_spaces(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    let mut pending_space = false;
    for ch in value.chars() {
        if ch == ' ' {
            if !out.is_empty() {
                pending_space = true;
            }
        } else {
            if pending_space {
                out.push(' ');
                pending_space = false;
            }
            out.push(ch);
        }
    }
    out
}

struct Cursor<'a> {
    text: &'a str,
    index: usize,
}

impl Cursor<'_> {
    /// Walk `content` with an explicit element stack.
    ///
    /// Nesting is attacker-controlled. Recursing `markup` into `element` overflows
    /// a 2 MiB thread stack at about ten thousand tags, before `max_depth` can
    /// run. The stack here is on the heap and stops at `max_depth`.
    fn scan(&mut self, max_depth: usize) -> Result<(), WfFail> {
        let mut open: Vec<String> = Vec::new();
        loop {
            self.char_data()?;
            if self.starts_with("</") {
                let Some(name) = open.pop() else {
                    return Err(WfFail::Syntax(
                        "end tag in entity replacement text has no matching start tag".to_string(),
                    ));
                };
                self.index += 2;
                let end = self.read_name()?;
                if end != name {
                    return Err(WfFail::Syntax(format!(
                        "mismatched end tag in entity replacement text: expected `{name}`, found `{end}`"
                    )));
                }
                self.skip_space();
                self.expect_char('>')?;
                continue;
            }
            if self.eof() {
                if let Some(name) = open.last() {
                    return Err(WfFail::Syntax(format!(
                        "entity replacement text ends inside element `{name}`"
                    )));
                }
                return Ok(());
            }
            match self.peek_char() {
                Some('<') => self.markup(&mut open, max_depth)?,
                Some('&') => self.reference()?,
                Some(_) => {
                    return Err(WfFail::Syntax(
                        "entity replacement text is not well-formed content".to_string(),
                    ));
                }
                None => {
                    if let Some(name) = open.last() {
                        return Err(WfFail::Syntax(format!(
                            "entity replacement text ends inside element `{name}`"
                        )));
                    }
                    return Ok(());
                }
            }
        }
    }

    fn markup(&mut self, open: &mut Vec<String>, max_depth: usize) -> Result<(), WfFail> {
        if self.starts_with("<!--") {
            return Ok(self.comment()?);
        }
        if self.starts_with("<![CDATA[") {
            return Ok(self.cdata()?);
        }
        if self.starts_with("<?") {
            return Ok(self.pi()?);
        }
        if self.starts_with("</") {
            return Err(WfFail::Syntax(
                "end tag in entity replacement text has no matching start tag".to_string(),
            ));
        }
        self.bump();
        let name = self.read_name()?;
        loop {
            self.skip_space();
            match self.peek_char() {
                Some('/') => {
                    self.bump();
                    self.expect_char('>')?;
                    return Ok(());
                }
                Some('>') => {
                    self.bump();
                    break;
                }
                Some(_) => self.attribute()?,
                None => {
                    return Err(WfFail::Syntax(
                        "unterminated start tag in entity replacement text".to_string(),
                    ));
                }
            }
        }
        if open.len() >= max_depth {
            return Err(WfFail::TooDeep {
                limit: max_depth as u64,
                actual: (open.len() + 1) as u64,
            });
        }
        open.push(name);
        Ok(())
    }

    fn attribute(&mut self) -> Result<(), String> {
        self.read_name()?;
        self.skip_space();
        self.expect_char('=')?;
        self.skip_space();
        let quote = self
            .bump()
            .ok_or_else(|| "unterminated attribute value".to_string())?;
        if quote != '"' && quote != '\'' {
            return Err("expected a quoted attribute value".to_string());
        }
        loop {
            match self.bump() {
                Some(ch) if ch == quote => return Ok(()),
                Some('<') => {
                    return Err(
                        "`<` is not allowed in an attribute value inside an entity".to_string()
                    );
                }
                Some('&') => {
                    self.index -= '&'.len_utf8();
                    self.reference()?;
                }
                Some(ch) if is_xml_char(ch) => {}
                Some(ch) => {
                    return Err(format!(
                        "illegal XML character U+{:04X} in entity replacement text",
                        u32::from(ch)
                    ));
                }
                None => {
                    return Err(
                        "unterminated attribute value in entity replacement text".to_string()
                    );
                }
            }
        }
    }

    fn comment(&mut self) -> Result<(), String> {
        self.index += "<!--".len();
        loop {
            if self.starts_with("-->") {
                self.index += 3;
                return Ok(());
            }
            if self.starts_with("--") {
                return Err("`--` is not allowed inside a comment".to_string());
            }
            match self.bump() {
                Some(ch) if is_xml_char(ch) => {}
                Some(ch) => {
                    return Err(format!(
                        "illegal XML character U+{:04X} in a comment",
                        u32::from(ch)
                    ));
                }
                None => return Err("unterminated comment in entity replacement text".to_string()),
            }
        }
    }

    fn cdata(&mut self) -> Result<(), String> {
        self.index += "<![CDATA[".len();
        loop {
            if self.starts_with("]]>") {
                self.index += 3;
                return Ok(());
            }
            match self.bump() {
                Some(ch) if is_xml_char(ch) => {}
                Some(ch) => {
                    return Err(format!(
                        "illegal XML character U+{:04X} in CDATA",
                        u32::from(ch)
                    ));
                }
                None => {
                    return Err("unterminated CDATA section in entity replacement text".to_string());
                }
            }
        }
    }

    fn pi(&mut self) -> Result<(), String> {
        self.index += 2;
        let target = self.read_ncname()?;
        if target.eq_ignore_ascii_case("xml") {
            return Err("processing instruction target must not be `xml`".to_string());
        }
        match self.peek_char() {
            Some(ch) if ch.is_whitespace() => {
                self.bump();
            }
            Some('?') => {}
            Some(_) => {
                return Err("expected whitespace after a processing instruction target".to_string());
            }
            None => return Err("unterminated processing instruction".to_string()),
        }
        loop {
            if self.starts_with("?>") {
                self.index += 2;
                return Ok(());
            }
            match self.bump() {
                Some(ch) if is_xml_char(ch) => {}
                Some(ch) => {
                    return Err(format!(
                        "illegal XML character U+{:04X} in a processing instruction",
                        u32::from(ch)
                    ));
                }
                None => return Err("unterminated processing instruction".to_string()),
            }
        }
    }

    fn reference(&mut self) -> Result<(), String> {
        self.expect_char('&')?;
        if self.peek_char() == Some('#') {
            self.bump();
            if self.peek_char() == Some('X') {
                return Err("hexadecimal character references must use a lowercase 'x'".to_string());
            }
            let hex = self.peek_char() == Some('x');
            if hex {
                self.bump();
            }
            let mut digits = 0u32;
            let mut value: u32 = 0;
            let base: u32 = if hex { 16 } else { 10 };
            loop {
                let Some(ch) = self.peek_char() else {
                    return Err("truncated character reference".to_string());
                };
                let digit = if ch.is_ascii_digit() {
                    u32::from(ch as u8 - b'0')
                } else if hex && ch.is_ascii_hexdigit() {
                    u32::from(ch.to_ascii_lowercase() as u8 - b'a' + 10)
                } else {
                    break;
                };
                digits += 1;
                if value > (0x10_FFFF - digit) / base {
                    return Err("invalid character reference".to_string());
                }
                value = value * base + digit;
                self.bump();
            }
            if digits == 0 {
                return Err("empty character reference".to_string());
            }
            let Some(ch) = char::from_u32(value) else {
                return Err("invalid code point in character reference".to_string());
            };
            if !is_xml_char(ch) {
                return Err(format!("illegal XML character U+{value:04X}"));
            }
            self.expect_char(';')?;
            return Ok(());
        }
        self.read_ncname()?;
        self.expect_char(';')
    }

    fn char_data(&mut self) -> Result<(), String> {
        let mut brackets = 0u8;
        while let Some(ch) = self.peek_char() {
            if ch == '<' || ch == '&' {
                break;
            }
            if !is_xml_char(ch) {
                return Err(format!(
                    "illegal XML character U+{:04X} in entity replacement text",
                    u32::from(ch)
                ));
            }
            if ch == ']' {
                brackets = (brackets + 1).min(2);
            } else if ch == '>' && brackets >= 2 {
                return Err("`]]>` is not allowed in entity replacement text".to_string());
            } else {
                brackets = 0;
            }
            self.bump();
        }
        Ok(())
    }

    /// PI targets and entity-reference names are NCNames. A colon, as in
    /// `&a:b;`, is a name error rather than an undefined entity.
    fn read_ncname(&mut self) -> Result<String, String> {
        let Some(first) = self.bump() else {
            return Err("expected a name".to_string());
        };
        if !is_ncname_start(first) {
            return Err(format!("invalid name start U+{:04X}", u32::from(first)));
        }
        let mut name = String::new();
        name.push(first);
        while let Some(ch) = self.peek_char() {
            if !is_ncname_char(ch) {
                break;
            }
            name.push(ch);
            self.bump();
        }
        if self.peek_char() == Some(':') {
            return Err("name must be an NCName".to_string());
        }
        Ok(name)
    }

    fn read_name(&mut self) -> Result<String, String> {
        let Some(first) = self.bump() else {
            return Err("expected a name".to_string());
        };
        if !is_name_start(first) {
            return Err(format!("invalid name start U+{:04X}", u32::from(first)));
        }
        let mut name = String::new();
        name.push(first);
        while let Some(ch) = self.peek_char() {
            if !is_name_char(ch) {
                break;
            }
            name.push(ch);
            self.bump();
        }
        Ok(name)
    }

    fn skip_space(&mut self) {
        while let Some(ch) = self.peek_char() {
            if ch != ' ' && ch != '\t' && ch != '\n' && ch != '\r' {
                break;
            }
            self.bump();
        }
    }

    fn expect_char(&mut self, expected: char) -> Result<(), String> {
        match self.bump() {
            Some(ch) if ch == expected => Ok(()),
            Some(ch) => Err(format!("expected `{expected}`, found `{ch}`")),
            None => Err(format!("expected `{expected}`")),
        }
    }

    fn peek_char(&self) -> Option<char> {
        self.text[self.index..].chars().next()
    }

    fn bump(&mut self) -> Option<char> {
        let ch = self.peek_char()?;
        self.index += ch.len_utf8();
        Some(ch)
    }

    fn starts_with(&self, pattern: &str) -> bool {
        self.text[self.index..].starts_with(pattern)
    }

    fn eof(&self) -> bool {
        self.index >= self.text.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn check(text: &str) -> Result<(), WfFail> {
        check_replacement_content(text, 64)
    }

    #[test]
    fn accepts_balanced_markup_and_references() {
        assert!(check("").is_ok());
        assert!(check("hello").is_ok());
        assert!(check("&amp;").is_ok());
        assert!(check("<a b='v'>t</a>").is_ok());
        assert!(check("<a/>").is_ok());
        assert!(check("<!-- ok -->").is_ok());
        assert!(check("<![CDATA[x]]>").is_ok());
        assert!(check("<?pi data?>").is_ok());
        assert!(check("<a:b></a:b>").is_ok());
        assert!(check("<a>&inner;</a>").is_ok());
        assert!(check("<a><b></b></a>").is_ok());
    }

    #[test]
    fn rejects_colons_in_pi_targets_and_entity_references() {
        assert!(check("<?a:b x?>").is_err());
        assert!(check("<?a: x?>").is_err());
        assert!(check("&a:b;").is_err());
    }

    #[test]
    fn rejects_markup_that_crosses_the_boundary() {
        assert!(check("<a>").is_err());
        assert!(check("</a>").is_err());
        assert!(check("<!--").is_err());
        assert!(check("<a></b>").is_err());
        assert!(check("]]>").is_err());
    }

    #[test]
    fn nesting_stops_at_the_depth_limit() {
        let err = check_replacement_content("<a><a><a></a></a></a>", 2).unwrap_err();
        assert!(matches!(
            err,
            WfFail::TooDeep {
                limit: 2,
                actual: 3
            }
        ));
    }

    #[test]
    fn collapses_non_cdata_spaces() {
        assert_eq!(collapse_spaces("  x   y  "), "x y");
        assert_eq!(collapse_spaces("x"), "x");
        assert_eq!(collapse_spaces("   "), "");
    }
}
