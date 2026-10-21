# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Changed

- **D31.** `TotalInput` is positional. It is reported only when parsing needs
  the byte at offset `max_total_input`. The position is that byte and
  `actual` is `limit + 1`. An error at an earlier offset wins.
  `Reader::from_bytes` and `Reader::from_vec` no longer reject the buffer
  up front, so every entry point yields the same events before that byte.
- Entity-reference names are NCNames. `&a:b;` is a name error
  (`ErrorKind::Syntax`) at the colon. It is no longer reported as an
  undefined entity. A reference such as `&missing;`, whose name is an NCName
  but has no internal declaration, is still `Syntax`, and that message still
  says `undefined entity`.

### Fixed

- **D11.** Attribute-value length is checked as each character is accepted, on
  the plain scan and on the normalizing path. The limit kind is
  `AttrValueLength`. `actual` is the length at the first character past
  `max_text_len` (the limit plus that character's UTF-8 byte length). The
  position is the cursor after that character.
- **D23.** PI targets, general-entity names, parameter-entity names, and
  notation names (including an `NDATA` name) must be NCNames. A document-type
  name must be a QName: no colon, or exactly one colon with an NCName on each
  side. The reader and the writer share these rules.
- **D26.** Comment, CDATA, and processing-instruction content is checked
  against `max_text_len` per character, including when the token never reaches
  its terminator. The limit kind is `TextLength`. `actual` is the length at
  the first character past the cap, including that character's UTF-8 bytes.
  The position is the cursor after that character. An unterminated comment
  that exceeds the cap reports `TextLength`.
