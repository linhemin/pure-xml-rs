//! Well-formedness of an internal general entity's replacement text.
//!
//! XML 1.0 requires that text to match `content`. References are left intact:
//! a general-entity reference is one content particle, not an invitation to
//! expand it here. That is what keeps tags and comments from crossing the
//! entity boundary.

use crate::name::{is_name_char, is_name_start, is_xml_char};

pub(crate) fn check_replacement_content(text: &str) -> Result<(), String> {
    let mut cursor = Cursor { text, index: 0 };
    cursor.content()?;
    if cursor.index != text.len() {
        return Err("entity replacement text is not well-formed content".to_string());
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
    fn content(&mut self) -> Result<(), String> {
        loop {
            self.char_data()?;
            if self.eof() {
                return Ok(());
            }
            match self.peek_char() {
                Some('<') => self.markup()?,
                Some('&') => self.reference()?,
                Some(_) => {
                    return Err("entity replacement text is not well-formed content".to_string());
                }
                None => return Ok(()),
            }
        }
    }

    fn markup(&mut self) -> Result<(), String> {
        if self.starts_with("<!--") {
            return self.comment();
        }
        if self.starts_with("<![CDATA[") {
            return self.cdata();
        }
        if self.starts_with("<?") {
            return self.pi();
        }
        if self.starts_with("</") {
            return Err("end tag in entity replacement text has no matching start tag".to_string());
        }
        self.element()
    }

    fn element(&mut self) -> Result<(), String> {
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
                None => return Err("unterminated start tag in entity replacement text".to_string()),
            }
        }
        loop {
            self.char_data()?;
            if self.starts_with("</") {
                self.bump();
                self.bump();
                let end = self.read_name()?;
                if end != name {
                    return Err(format!(
                        "mismatched end tag in entity replacement text: expected `{name}`, found `{end}`"
                    ));
                }
                self.skip_space();
                self.expect_char('>')?;
                return Ok(());
            }
            if self.eof() {
                return Err(format!(
                    "entity replacement text ends inside element `{name}`"
                ));
            }
            match self.peek_char() {
                Some('<') => self.markup()?,
                Some('&') => self.reference()?,
                Some(_) => {
                    return Err("entity replacement text is not well-formed content".to_string());
                }
                None => {
                    return Err(format!(
                        "entity replacement text ends inside element `{name}`"
                    ));
                }
            }
        }
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
                    )
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
                    return Err("unterminated CDATA section in entity replacement text".to_string())
                }
            }
        }
    }

    fn pi(&mut self) -> Result<(), String> {
        self.index += 2;
        let target = self.read_name()?;
        if target.eq_ignore_ascii_case("xml") {
            return Err("processing instruction target must not be `xml`".to_string());
        }
        match self.peek_char() {
            Some(ch) if ch.is_whitespace() => {
                self.bump();
            }
            Some('?') => {}
            Some(_) => {
                return Err("expected whitespace after a processing instruction target".to_string())
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
        self.read_name()?;
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

    #[test]
    fn accepts_balanced_markup_and_references() {
        assert!(check_replacement_content("").is_ok());
        assert!(check_replacement_content("hello").is_ok());
        assert!(check_replacement_content("&amp;").is_ok());
        assert!(check_replacement_content("<a b='v'>t</a>").is_ok());
        assert!(check_replacement_content("<a/>").is_ok());
        assert!(check_replacement_content("<!-- ok -->").is_ok());
        assert!(check_replacement_content("<![CDATA[x]]>").is_ok());
        assert!(check_replacement_content("<?pi data?>").is_ok());
        assert!(check_replacement_content("<a>&inner;</a>").is_ok());
    }

    #[test]
    fn rejects_markup_that_crosses_the_boundary() {
        assert!(check_replacement_content("<a>").is_err());
        assert!(check_replacement_content("</a>").is_err());
        assert!(check_replacement_content("<!--").is_err());
        assert!(check_replacement_content("<a></b>").is_err());
        assert!(check_replacement_content("]]>").is_err());
    }

    #[test]
    fn collapses_non_cdata_spaces() {
        assert_eq!(collapse_spaces("  x   y  "), "x y");
        assert_eq!(collapse_spaces("x"), "x");
        assert_eq!(collapse_spaces("   "), "");
    }
}
