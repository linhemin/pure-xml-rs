//! XML 1.0 Name / NCName character classes.

pub(crate) fn is_xml_char(c: char) -> bool {
    matches!(
        c,
        '\u{9}'
            | '\u{A}'
            | '\u{D}'
            | '\u{20}'..='\u{D7FF}'
            | '\u{E000}'..='\u{FFFD}'
            | '\u{10000}'..='\u{10FFFF}'
    )
}

pub(crate) fn is_name_start(c: char) -> bool {
    if c.is_ascii() {
        return c.is_ascii_alphabetic() || c == '_' || c == ':';
    }
    matches!(
        c,
        '\u{C0}'..='\u{D6}'
            | '\u{D8}'..='\u{F6}'
            | '\u{F8}'..='\u{2FF}'
            | '\u{370}'..='\u{37D}'
            | '\u{37F}'..='\u{1FFF}'
            | '\u{200C}'..='\u{200D}'
            | '\u{2070}'..='\u{218F}'
            | '\u{2C00}'..='\u{2FEF}'
            | '\u{3001}'..='\u{D7FF}'
            | '\u{F900}'..='\u{FDCF}'
            | '\u{FDF0}'..='\u{FFFD}'
            | '\u{10000}'..='\u{EFFFF}'
    )
}

pub(crate) fn is_name_char(c: char) -> bool {
    if c.is_ascii() {
        return is_name_start(c) || c.is_ascii_digit() || c == '-' || c == '.';
    }
    is_name_start(c)
        || matches!(
            c,
            '\u{B7}' | '\u{0300}'..='\u{036F}' | '\u{203F}'..='\u{2040}'
        )
}

pub(crate) fn is_ncname_start(c: char) -> bool {
    c != ':' && is_name_start(c)
}

pub(crate) fn is_ncname_char(c: char) -> bool {
    c != ':' && is_name_char(c)
}

pub(crate) fn is_pubid_char(c: char) -> bool {
    matches!(
        c,
        ' ' | '\r'
            | '\n'
            | 'a'..='z'
            | 'A'..='Z'
            | '0'..='9'
            | '-'
            | '\''
            | '('
            | ')'
            | '+'
            | ','
            | '.'
            | '/'
            | ':'
            | '='
            | '?'
            | ';'
            | '!'
            | '*'
            | '#'
            | '@'
            | '$'
            | '_'
            | '%'
    )
}

pub(crate) fn is_whitespace(b: u8) -> bool {
    matches!(b, b' ' | b'\t' | b'\n' | b'\r')
}

/// Decode one scalar value. Rejects overlong encodings, surrogates, and
/// sequences that are not UTF-8.
pub(crate) fn decode_utf8(bytes: &[u8]) -> Result<(char, usize), ()> {
    if bytes.is_empty() {
        return Err(());
    }
    let b0 = bytes[0];
    if b0 < 0x80 {
        return Ok((b0 as char, 1));
    }
    let (width, min_cp, mut cp) = if b0 & 0xE0 == 0xC0 {
        (2usize, 0x80u32, u32::from(b0 & 0x1F))
    } else if b0 & 0xF0 == 0xE0 {
        (3, 0x800, u32::from(b0 & 0x0F))
    } else if b0 & 0xF8 == 0xF0 {
        (4, 0x1_0000, u32::from(b0 & 0x07))
    } else {
        return Err(());
    };
    if bytes.len() < width {
        return Err(());
    }
    for b in bytes.iter().take(width).skip(1) {
        if b & 0xC0 != 0x80 {
            return Err(());
        }
        cp = (cp << 6) | u32::from(b & 0x3F);
    }
    if cp < min_cp || cp > 0x10_FFFF || (0xD800..=0xDFFF).contains(&cp) {
        return Err(());
    }
    char::from_u32(cp).map(|ch| (ch, width)).ok_or(())
}

/// Index of the byte that proves `bytes` is not UTF-8.
///
/// `None` when `bytes` starts with a well-formed character or with a prefix
/// that is still legal. The proving byte is not always the lead: a 4-byte
/// lead such as `0xF2` stays legal until a continuation fails.
pub(crate) fn utf8_invalid_index(bytes: &[u8]) -> Option<usize> {
    if bytes.is_empty() {
        return None;
    }
    let b0 = bytes[0];
    if b0 < 0x80 {
        return None;
    }
    // 0x80..=0xC1 and 0xF5..=0xFF cannot start a well-formed sequence.
    if !(0xC2..=0xF4).contains(&b0) {
        return Some(0);
    }
    let width = if b0 < 0xE0 {
        2
    } else if b0 < 0xF0 {
        3
    } else {
        4
    };
    if bytes.len() == 1 {
        return None;
    }
    let second_ok = match b0 {
        0xE0 => (0xA0..=0xBF).contains(&bytes[1]),
        0xED => (0x80..=0x9F).contains(&bytes[1]),
        0xF0 => (0x90..=0xBF).contains(&bytes[1]),
        0xF4 => (0x80..=0x8F).contains(&bytes[1]),
        _ => bytes[1] & 0xC0 == 0x80,
    };
    if !second_ok {
        return Some(1);
    }
    for i in 2..width {
        if bytes.len() <= i {
            return None;
        }
        if bytes[i] & 0xC0 != 0x80 {
            return Some(i);
        }
    }
    None
}

/// A non-empty prefix of a well-formed UTF-8 sequence that runs off the slice.
///
/// Invalid lead bytes and bad continuations are not incomplete; they are
/// malformed. Callers use this to tell a probe cut from a real encoding error.
pub(crate) fn utf8_incomplete(bytes: &[u8]) -> bool {
    if bytes.is_empty() {
        return false;
    }
    let width = match bytes[0] {
        byte if byte & 0xE0 == 0xC0 => 2,
        byte if byte & 0xF0 == 0xE0 => 3,
        byte if byte & 0xF8 == 0xF0 => 4,
        _ => return false,
    };
    if bytes.len() >= width {
        return false;
    }
    bytes[1..].iter().all(|byte| byte & 0xC0 == 0x80)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ascii_names() {
        assert!(is_ncname_start('A'));
        assert!(is_ncname_start('_'));
        assert!(!is_ncname_start(':'));
        assert!(is_name_start(':'));
        assert!(is_name_char('-'));
        assert!(is_name_char('5'));
        assert!(!is_name_start('5'));
    }

    #[test]
    fn rejects_overlong_and_surrogate_utf8() {
        assert!(decode_utf8(&[0xC0, 0x80]).is_err());
        assert!(decode_utf8(&[0xED, 0xA0, 0x80]).is_err());
        assert_eq!(decode_utf8(&[0x24]).unwrap(), ('$', 1));
        assert_eq!(decode_utf8("é".as_bytes()).unwrap().0, 'é');
        assert_eq!(utf8_invalid_index(&[0xC0, 0x80]), Some(0));
        assert_eq!(utf8_invalid_index(&[0xF2, 0xBE, 0xBE]), None);
        assert_eq!(utf8_invalid_index(&[0xF2, 0xBE, 0xBE, 0x02]), Some(3));
        assert_eq!(utf8_invalid_index(&[0xE0, 0x80, 0x80]), Some(1));
    }
}
