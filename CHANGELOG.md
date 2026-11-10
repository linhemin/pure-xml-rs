# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- `Document::parse`, `parse_with_config`, `from_bytes`, `from_bytes_with_config`,
  `from_reader`, and `from_reader_with_config` build a DOM in linear time.
  An XML declaration is not stored. `to_string` is always UTF-8 and writes
  `<!DOCTYPE name>` without system or public identifiers. `omit_doctype`
  still drops the node. The identifiers remain readable on the node.

### Security

- **D-C3-2.** The writer's URI cache keyed non-canonical `Arc` addresses
  without holding those allocations. After the `Arc` was dropped, a new URI
  could reuse the address and be treated as the old one: distinct attributes
  were rejected, and real duplicates were written. The cache now keeps the
  `Arc` whose address it uses. Content comparison is unchanged.
- **D-C3-1.** Checking distinct attributes hashed the namespace URI on every
  attribute, and serialization allocated a fresh `Arc` for every element.
  A long URI therefore made writing much slower than reading. The writer now
  caches identity on the `Arc` address after one content compare, and
  serialization reuses one `Arc` per arena span. Two different allocations
  of the same URI are still the same attribute.

### Changed

- `LimitKind` is `#[non_exhaustive]` and adds `NodeCount` and `DomBytes`.
  `ErrorKind` is `#[non_exhaustive]`. A DOM node or byte cap is reported as
  `ErrorKind::SecurityLimit` at the event that would have exceeded it.
- Mutation stays O(depth) or O(subtree). Large documents should use `parse`.
- MSRV is 1.99.1, raised from 1.70 to follow the local stable toolchain.
  The workspace edition is 2024 (`purexml`, `purexml-core`, and `purexml-fuzz`).
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

### Security

- `>` characters in text were not counted toward `max_text_len`, allowing the
  limit to be bypassed; fixed.
