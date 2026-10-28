//! M2 cases owned by the implementer.
//!
//! PR-A: incremental attribute-value limits (D11 / D-Q4), the shared
//! NCName / QName rules (D23), and per-character comment, CDATA, and PI
//! lengths (D26). PR-B: streaming `from_reader` versus `from_bytes`.
//! Tester files stay untouched except where the `from_reader` return type
//! forces a signature change.

use std::cell::Cell;
use std::io::{self, BufRead, Read};
use std::path::Path;

use purexml::{
    Doctype, Error, ErrorKind, Event, IoInput, LimitKind, ParserConfig, ProcessingInstruction,
    Reader, Writer, XmlText,
};

fn config(max_text_len: u64) -> ParserConfig {
    ParserConfig {
        max_text_len,
        ..ParserConfig::default()
    }
}

fn pull(xml: &str, cfg: ParserConfig) -> Result<Vec<EventOwned>, Error> {
    let mut reader = Reader::from_str(xml).with_config(cfg);
    let mut out = Vec::new();
    loop {
        match reader.read_event() {
            Ok(None) => return Ok(out),
            Ok(Some(Event::Doctype(dtd))) => out.push(EventOwned::Doctype(dtd.name.into_owned())),
            Ok(Some(Event::PI(pi))) => out.push(EventOwned::Pi(pi.target.into_owned())),
            Ok(Some(_)) => out.push(EventOwned::Other),
            Err(err) => return Err(err),
        }
    }
}

fn must_ok(xml: &str) -> Vec<EventOwned> {
    pull(xml, ParserConfig::default()).unwrap_or_else(|err| panic!("{xml}\n{err}"))
}

fn must_syntax(xml: &str) -> Error {
    let err = pull(xml, ParserConfig::default()).expect_err(xml);
    assert!(
        matches!(err.kind, ErrorKind::Syntax),
        "{xml}\nexpected Syntax, got {err}"
    );
    err
}

fn must_limit(xml: &str, max_text_len: u64) -> Error {
    let err = pull(xml, config(max_text_len)).expect_err(xml);
    match err.kind {
        ErrorKind::SecurityLimit {
            kind: LimitKind::AttrValueLength,
            ..
        } => err,
        _ => panic!("{xml}\nexpected AttrValueLength, got {err}"),
    }
}

fn attr_actual(err: &Error) -> (u64, u64, u64) {
    match err.kind {
        ErrorKind::SecurityLimit {
            kind: LimitKind::AttrValueLength,
            limit,
            actual,
        } => (limit, actual, err.position.offset),
        _ => panic!("{err}"),
    }
}

#[derive(Debug)]
enum EventOwned {
    Doctype(String),
    Pi(String),
    Other,
}

#[test]
fn d11_plain_attr_stops_at_the_first_overflow() {
    let xml = "<r a=\"0123456789\"/>";
    let err = must_limit(xml, 4);
    let (limit, actual, offset) = attr_actual(&err);
    assert_eq!(limit, 4);
    assert_eq!(actual, 5);
    let value_at = xml.find('"').unwrap() + 1;
    assert_eq!(offset, (value_at + 5) as u64);
    assert!(offset < xml.rfind('"').unwrap() as u64);

    assert!(pull("<r a=\"0123\"/>", config(4)).is_ok());
    assert!(pull("<r a=\"\"/>", config(0)).is_ok());
    let err = must_limit("<r a=\"x\"/>", 0);
    assert_eq!(attr_actual(&err).1, 1);
}

#[test]
fn d11_slow_attr_does_not_buffer_the_tail() {
    let head = "<r a=\"&amp;";
    let mut xml = String::from(head);
    xml.push_str(&"x".repeat(4));
    xml.push_str(&"y".repeat(1 << 20));
    xml.push_str("\"/>");
    let err = must_limit(&xml, 4);
    let (limit, actual, offset) = attr_actual(&err);
    assert_eq!(limit, 4);
    // `&amp;` contributes one byte, then four `x`s make length 5.
    assert_eq!(actual, 5);
    assert_eq!(offset, (head.len() + 4) as u64);
    assert!(offset + 1_000 < xml.len() as u64);

    assert!(pull("<r a=\"&amp;xxx\"/>", config(4)).is_ok());
    let tab = "<r a=\"\txxx\"/>";
    assert!(pull(tab, config(4)).is_ok());
    let err = must_limit("<r a=\"\txxxx\"/>", 4);
    assert_eq!(attr_actual(&err).1, 5);
}

#[test]
fn d11_multibyte_char_reports_its_utf8_length() {
    // U+4E2D is three UTF-8 bytes. 2 + 3 crosses a cap of 4 at length 5.
    let xml = "<r a=\"ab&#x4E2D;xxxx\"/>";
    let err = must_limit(xml, 4);
    let (limit, actual, offset) = attr_actual(&err);
    assert_eq!(limit, 4);
    assert_eq!(actual, 5);
    assert!(offset < xml.len() as u64);
}

#[test]
fn d23_pi_entity_and_notation_names_are_ncnames() {
    for xml in [
        "<r><?a:b x?></r>",
        "<r><?a: x?></r>",
        "<?p:q?><r/>",
        "<?:a ?><r/>",
        "<?xmlns: ?><r/>",
        "<?:ml ?><r/>",
        "<!DOCTYPE r [<?a:b x?>]><r/>",
        "<!DOCTYPE r [<!ENTITY a:b \"x\">]><r/>",
        "<!DOCTYPE r [<!ENTITY % a:b \"x\">]><r/>",
        "<!DOCTYPE r [<!NOTATION a:b SYSTEM \"x\">]><r/>",
        "<!DOCTYPE r [<!ATTLIST r a NOTATION (a:b) #IMPLIED>]><r/>",
        "<!DOCTYPE r [<!ENTITY e SYSTEM \"u\" NDATA a:b>]><r/>",
        "<r>&a:b;</r>",
        "<!DOCTYPE r [<!ENTITY e \"&a:b;\">]><r/>",
        "<!DOCTYPE r [<!ENTITY e \"<?a:b x?>\">]><r>&e;</r>",
    ] {
        let err = must_syntax(xml);
        assert!(
            !err.to_string().contains("undefined entity"),
            "{xml}\n{err}"
        );
    }

    let err = must_syntax("<r>&missing;</r>");
    assert!(err.to_string().contains("undefined entity"), "{err}");

    let events = must_ok("<r><?ab x?></r>");
    assert!(events
        .iter()
        .any(|ev| matches!(ev, EventOwned::Pi(t) if t == "ab")));
    let events = must_ok("<?xml-stylesheet type=\"text/xsl\" href=\"a\"?><r/>");
    assert!(events
        .iter()
        .any(|ev| matches!(ev, EventOwned::Pi(t) if t == "xml-stylesheet")));
}

#[test]
fn d23_doctype_name_is_a_qname() {
    for xml in [
        "<!DOCTYPE r: SYSTEM \"x.dtd\"><r/>",
        "<!DOCTYPE :r><r/>",
        "<!DOCTYPE a:b:c><r/>",
        "<!DOCTYPE r:><r/>",
    ] {
        must_syntax(xml);
    }
    let events = must_ok("<!DOCTYPE a:b><r/>");
    assert!(events
        .iter()
        .any(|ev| matches!(ev, EventOwned::Doctype(name) if name == "a:b")));
}

#[test]
fn d23_writer_uses_the_same_name_rules() {
    for target in [":a", "a:", "xml:x", "xmlns:", ":ml", "a:b", "p:q"] {
        let mut writer = Writer::new(Vec::new());
        assert!(writer.write_pi_str(target, Some("x")).is_err(), "{target}");
        assert!(writer.get_ref().is_empty(), "{target}");
        let event = Event::PI(ProcessingInstruction {
            target: XmlText::borrowed(target),
            data: None,
        });
        let mut writer = Writer::new(Vec::new());
        assert!(writer.write_event(&event).is_err(), "{target}");
        assert!(writer.get_ref().is_empty(), "{target}");
    }

    let mut writer = Writer::new(Vec::new());
    writer.write_pi_str("ab", Some("x")).unwrap();
    assert_eq!(writer.get_ref(), b"<?ab x?>");

    for name in ["r:", ":r", "a:b:c", "a::b"] {
        let mut writer = Writer::new(Vec::new());
        let event = doctype(name);
        assert!(writer.write_event(&event).is_err(), "{name}");
        assert!(writer.get_ref().is_empty(), "{name}");
    }
    let mut writer = Writer::new(Vec::new());
    writer.write_event(&doctype("a:b")).unwrap();
    assert_eq!(writer.get_ref(), b"<!DOCTYPE a:b>");

    let events = must_ok("<?ab data?><r/>");
    let mut reader = Reader::from_str("<?ab data?><r/>");
    let pi = reader.read_event().unwrap().unwrap();
    let mut writer = Writer::new(Vec::new());
    writer.write_event(&pi).unwrap();
    assert!(events.iter().any(|ev| matches!(ev, EventOwned::Pi(_))));
    assert_eq!(&writer.get_ref()[..], b"<?ab data?>");
}

fn doctype(name: &str) -> Event<'_> {
    Event::Doctype(Doctype {
        name: XmlText::borrowed(name),
        system_id: None,
        public_id: None,
        has_internal_subset: false,
    })
}

#[derive(Debug, PartialEq, Eq)]
struct Snap {
    events: Vec<String>,
    err: Option<ErrSnap>,
}

#[derive(Debug, PartialEq, Eq)]
struct ErrSnap {
    kind: ErrorKind,
    line: u64,
    column: u64,
    offset: u64,
}

/// Events borrow the reader, so a closure cannot return them. The macro keeps
/// the borrow inside the caller's loop.
macro_rules! snap_of {
    ($reader:expr) => {{
        let reader = &mut $reader;
        let mut events = Vec::new();
        let err = loop {
            match reader.read_event() {
                Ok(Some(event)) => events.push(format!("{event:?}")),
                Ok(None) => break None,
                Err(err) => {
                    break Some(ErrSnap {
                        kind: err.kind,
                        line: err.position.line,
                        column: err.position.column,
                        offset: err.position.offset,
                    })
                }
            }
        };
        Snap { events, err }
    }};
}

fn snap_bytes(data: &[u8], config: ParserConfig) -> Snap {
    let mut reader = Reader::from_bytes(data).with_config(config);
    snap_of!(reader)
}

fn snap_vec(data: Vec<u8>, config: ParserConfig) -> Snap {
    let mut reader = Reader::from_vec(data).with_config(config);
    snap_of!(reader)
}

fn snap_chunks(data: &[u8], chunk: usize, config: ParserConfig) -> Snap {
    let mut reader = Reader::from_reader(Chunks {
        data,
        pos: 0,
        chunk: chunk.max(1),
    })
    .unwrap()
    .with_config(config);
    snap_of!(reader)
}

fn assert_equivalent(label: &str, data: &[u8], chunk: usize, config: ParserConfig) {
    let left = snap_bytes(data, config.clone());
    let right = snap_chunks(data, chunk, config);
    assert_eq!(left, right, "{label} chunk {chunk}");
}

struct Chunks<'a> {
    data: &'a [u8],
    pos: usize,
    chunk: usize,
}

impl Read for Chunks<'_> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.pos >= self.data.len() || buf.is_empty() {
            return Ok(0);
        }
        let n = self.chunk.min(buf.len()).min(self.data.len() - self.pos);
        buf[..n].copy_from_slice(&self.data[self.pos..self.pos + n]);
        self.pos += n;
        Ok(n)
    }
}

impl BufRead for Chunks<'_> {
    fn fill_buf(&mut self) -> io::Result<&[u8]> {
        let end = (self.pos + self.chunk).min(self.data.len());
        Ok(&self.data[self.pos..end])
    }

    fn consume(&mut self, amt: usize) {
        self.pos = (self.pos + amt).min(self.data.len());
    }
}

fn corpus_files() -> Vec<std::path::PathBuf> {
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
    // Same root as `w3c_runner`: `PUREXML_W3C_DIR`, or the gitignored cache
    // populated by tests/w3c/fetch.sh. The xmltest directories are not in the
    // repo, so a hardcoded cache path sees only the local data files.
    let w3c = std::env::var_os("PUREXML_W3C_DIR").map_or_else(
        || manifest.join("tests/w3c/cache/xmlconf"),
        std::path::PathBuf::from,
    );
    let mut out = Vec::new();
    for root in [
        w3c.join("xmltest/valid/sa"),
        w3c.join("xmltest/not-wf/sa"),
        w3c.join("xmltest/valid/not-sa"),
        manifest.join("tests/data/attacks"),
        manifest.join("tests/data/fuzz-regressions"),
    ] {
        let mut stack = vec![root];
        while let Some(dir) = stack.pop() {
            let entries = match std::fs::read_dir(&dir) {
                Ok(entries) => entries,
                Err(_) => continue,
            };
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    stack.push(path);
                    continue;
                }
                let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
                if name.ends_with(".md") || name.ends_with(".py") || name.ends_with(".txt") {
                    continue;
                }
                out.push(path);
            }
        }
    }
    out.sort();
    out
}

#[test]
fn streaming_matches_bytes_across_chunks() {
    let files = corpus_files();
    assert!(files.len() > 100, "corpus missing: {}", files.len());
    for path in files {
        let data = std::fs::read(&path).unwrap();
        let label = path.display().to_string();
        let mut chunks: Vec<usize> = (1..=17).collect();
        chunks.extend([64, 1024, 8192]);
        if data.len() > 48 * 1024 {
            chunks.retain(|chunk| matches!(chunk, 1 | 7 | 16 | 64 | 1024 | 8192));
        }
        for chunk in chunks {
            assert_equivalent(&label, &data, chunk, ParserConfig::default());
        }
    }
}

#[test]
fn utf8_crlf_and_bom_survive_chunk_boundaries() {
    let samples = [
        "<?xml version=\"1.0\"?><r>é你😀</r>",
        "<r a=\"é你😀\">é</r>",
        "<é你/>",
        "<r>a\r\nb</r>",
        "<r>a\rb</r>",
        "\u{feff}<r/>",
        "<!--é你--><r/>",
        "<![CDATA[é你]]><r/>",
        "<?pi é你?><r/>",
        "<!DOCTYPE r [<!ENTITY e \"é你\">]><r>&e;</r>",
    ];
    for sample in samples {
        let data = sample.as_bytes();
        for chunk in 1..=sample.len().max(4) {
            assert_equivalent(sample, data, chunk, ParserConfig::default());
        }
    }
    for (label, data) in [
        ("utf16-be", b"\xFE\xFF\x00<".as_slice()),
        ("utf16-le", b"\xFF\xFE<\x00".as_slice()),
        ("bad-utf8", b"<r>\xFF</r>".as_slice()),
        ("split-bad", b"<r>\xC3\x28</r>".as_slice()),
    ] {
        for chunk in 1..=data.len() {
            assert_equivalent(label, data, chunk, ParserConfig::default());
        }
    }
}

#[test]
fn token_text_and_subset_limits_match_and_use_limit_plus_one() {
    let token = ParserConfig {
        max_token_len: 6,
        ..ParserConfig::default()
    };
    let xml = b"<root/>";
    assert_equivalent("token", xml, 1, token.clone());
    let err = snap_bytes(xml, token.clone()).err.unwrap();
    match err.kind {
        ErrorKind::SecurityLimit {
            kind: LimitKind::TokenLength,
            limit,
            actual,
        } => {
            assert_eq!(limit, 6);
            assert_eq!(actual, 7);
            // `<root/>`: the 7th byte, `>`, is the offending byte.
            assert_eq!(err.offset, 6);
            assert_eq!(err.line, 1);
            assert_eq!(err.column, 7);
        }
        other => panic!("{other:?}"),
    }
    // F-1: actual stays limit + 1 on every chunk size, and the position stays
    // on that same byte.
    for chunk in [1usize, 2, 3, 6, 7, 64] {
        let streamed = snap_chunks(xml, chunk, token.clone());
        let slice = snap_bytes(xml, token.clone());
        assert_eq!(
            streamed.err.as_ref().map(|err| &err.kind),
            slice.err.as_ref().map(|err| &err.kind),
            "chunk {chunk}"
        );
        assert_eq!(
            streamed
                .err
                .as_ref()
                .map(|err| (err.line, err.column, err.offset)),
            slice
                .err
                .as_ref()
                .map(|err| (err.line, err.column, err.offset)),
            "chunk {chunk}"
        );
    }
    let exact = ParserConfig {
        max_token_len: 7,
        ..ParserConfig::default()
    };
    assert!(snap_bytes(b"<root/>", exact.clone()).err.is_none());
    assert_equivalent("exact-token", b"<root/>", 1, exact);

    let text = ParserConfig {
        max_text_len: 4,
        ..ParserConfig::default()
    };
    assert_equivalent("comment", b"<!--abcdef--><r/>", 1, text.clone());
    match snap_bytes(b"<!--abcdef--><r/>", text).err.unwrap().kind {
        ErrorKind::SecurityLimit {
            kind: LimitKind::TextLength,
            limit,
            actual,
        } => {
            assert_eq!(limit, 4);
            assert_eq!(actual, 5);
        }
        other => panic!("{other:?}"),
    }

    let subset = ParserConfig {
        max_internal_subset_len: 4,
        ..ParserConfig::default()
    };
    let doc = b"<!DOCTYPE r [<!ENTITY e \"ab\">]><r/>";
    assert_equivalent("subset", doc, 3, subset.clone());
    let subset_err = snap_bytes(doc, subset).err.unwrap();
    match subset_err.kind {
        ErrorKind::SecurityLimit {
            kind: LimitKind::InternalSubsetLength,
            limit,
            actual,
        } => {
            assert_eq!(limit, 4);
            assert_eq!(actual, 5);
        }
        other => panic!("{other:?}"),
    }
    // `<!DOCTYPE r [` is 13 bytes. The 5th subset byte is `T` in `<!ENTITY`.
    assert_eq!(subset_err.offset, 17);
    assert_eq!(subset_err.line, 1);
    assert_eq!(subset_err.column, 18);
    assert!(snap_bytes(
        b"<!DOCTYPE r []><r/>",
        ParserConfig {
            max_internal_subset_len: 0,
            ..ParserConfig::default()
        },
    )
    .err
    .is_none());
}

#[test]
fn name_length_points_just_after_the_offending_character() {
    // `é` is two bytes. `<a` is already one name byte, so the cap of 1 fails
    // on `é` and the cursor sits after that character.
    let xml = "<aé/>";
    let config = ParserConfig {
        max_name_len: 1,
        ..ParserConfig::default()
    };
    let err = snap_bytes(xml.as_bytes(), config.clone()).err.unwrap();
    match &err.kind {
        ErrorKind::SecurityLimit {
            kind: LimitKind::NameLength,
            limit,
            actual,
        } => {
            assert_eq!(*limit, 1);
            assert_eq!(*actual, 3);
        }
        other => panic!("{other:?}"),
    }
    assert_eq!((err.line, err.column, err.offset), (1, 4, 4));
    let streamed = snap_chunks(xml.as_bytes(), 1, config);
    assert_eq!(streamed.err.as_ref().map(|e| &e.kind), Some(&err.kind));
    assert_eq!(
        streamed.err.as_ref().map(|e| (e.line, e.column, e.offset)),
        Some((err.line, err.column, err.offset))
    );

    // Prolog whitespace is not part of the token. `\n<root/>` starts the
    // token at `<` (offset 1). Cap 1 fails on `r`: line 2, column 2, offset 2.
    let broken = b"\n<root/>";
    let token = ParserConfig {
        max_token_len: 1,
        ..ParserConfig::default()
    };
    let err = snap_bytes(broken, token.clone()).err.unwrap();
    assert!(matches!(
        err.kind,
        ErrorKind::SecurityLimit {
            kind: LimitKind::TokenLength,
            limit: 1,
            actual: 2,
        }
    ));
    assert_eq!((err.line, err.column, err.offset), (2, 2, 2));
    let streamed = snap_chunks(broken, 1, token);
    assert_eq!(
        streamed.err.as_ref().map(|e| (e.line, e.column, e.offset)),
        Some((2, 2, 2))
    );
}

/// A UTF-8 BOM occupies no column. Offsets 0–2 (inside the BOM) and offset 3
/// (the first byte after it) are line 1, column 1. The offset stays the raw
/// byte index. A total-input cap inside the 4-byte BOM sniff is TotalInput
/// on every path.
#[test]
fn bom_columns_and_total_input_agree_across_paths() {
    let mut doc = vec![0xEF, 0xBB, 0xBF];
    doc.extend_from_slice(b"<r/>");
    for limit in [0u64, 1, 2, 3] {
        let cfg = ParserConfig {
            max_total_input: limit,
            ..ParserConfig::default()
        };
        let want = ErrSnap {
            kind: ErrorKind::SecurityLimit {
                kind: LimitKind::TotalInput,
                limit,
                actual: limit + 1,
            },
            line: 1,
            column: 1,
            offset: limit,
        };
        let label = format!("BOM tot={limit}");
        assert_eq!(
            snap_bytes(&doc, cfg.clone()).err.as_ref(),
            Some(&want),
            "{label} from_bytes"
        );
        assert_eq!(
            snap_vec(doc.clone(), cfg.clone()).err.as_ref(),
            Some(&want),
            "{label} from_vec"
        );
        for chunk in [1usize, 2, 3, 4, 7] {
            assert_eq!(
                snap_chunks(&doc, chunk, cfg.clone()).err.as_ref(),
                Some(&want),
                "{label} chunk {chunk}"
            );
        }
    }

    // The first byte after the BOM is column 1, even when it is a syntax error.
    let mut junk = vec![0xEF, 0xBB, 0xBF];
    junk.push(b'x');
    let cfg = ParserConfig::default();
    let want = ErrSnap {
        kind: ErrorKind::Syntax,
        line: 1,
        column: 1,
        offset: 3,
    };
    assert_eq!(snap_bytes(&junk, cfg.clone()).err.as_ref(), Some(&want));
    assert_eq!(
        snap_vec(junk.clone(), cfg.clone()).err.as_ref(),
        Some(&want)
    );
    for chunk in [1usize, 3, 4] {
        assert_eq!(
            snap_chunks(&junk, chunk, cfg.clone()).err.as_ref(),
            Some(&want),
            "syntax after BOM chunk {chunk}"
        );
    }
}

#[test]
fn total_input_reports_limit_plus_one_at_the_offending_byte() {
    let config = ParserConfig {
        max_total_input: 4,
        ..ParserConfig::default()
    };
    let data = b"<a/><b/>";
    let slice = snap_bytes(data, config.clone());
    let streamed = snap_chunks(data, 3, config);
    match &slice.err.as_ref().unwrap().kind {
        ErrorKind::SecurityLimit {
            kind: LimitKind::TotalInput,
            limit,
            actual,
        } => {
            assert_eq!(*limit, 4);
            assert_eq!(*actual, 5);
        }
        other => panic!("{other:?}"),
    }
    // `<a/>` occupies offsets 0..3. The next byte is offset 4, so that
    // element's events are yielded and then TotalInput is reported there.
    let prefix = snap_bytes(b"<a/>", ParserConfig::default());
    assert!(prefix.err.is_none());
    assert_eq!(slice.events, prefix.events);
    assert!(!slice.events.is_empty());
    assert_eq!(slice.err.as_ref().unwrap().offset, 4);
    assert_eq!(slice, streamed);
}

#[test]
fn errors_stick_on_both_paths() {
    let mut slice = Reader::from_bytes(b"<");
    let first = slice.read_event().unwrap_err();
    let again = slice.read_event().unwrap_err();
    assert_eq!(first.kind, again.kind);
    assert_eq!(first.position, again.position);

    let mut streamed = Reader::from_reader(Chunks {
        data: b"<r".as_slice(),
        pos: 0,
        chunk: 1,
    })
    .unwrap();
    let first = streamed.read_event().unwrap_err();
    let again = streamed.read_event().unwrap_err();
    assert_eq!(first.kind, again.kind);
    assert_eq!(first.position, again.position);
}

#[test]
fn interrupted_is_retried_and_would_block_poisons() {
    let interrupts = Cell::new(0usize);
    let src = Interrupt {
        data: b"<r>ok</r>",
        pos: 0,
        left: 1000,
        hits: &interrupts,
    };
    let mut reader = Reader::from_reader(src).unwrap();
    let snap = snap_of!(reader);
    assert!(snap.err.is_none(), "{snap:?}");
    assert!(interrupts.get() >= 1000);

    let mut reader = Reader::from_reader(Block {
        data: b"<r/>",
        pos: 0,
        armed: true,
    })
    .unwrap();
    let err = reader.read_event().unwrap_err();
    assert!(matches!(err.kind, ErrorKind::Io), "{err}");
    assert_eq!(err.position.offset, 0);
    let again = reader.read_event().unwrap_err();
    assert_eq!(err.kind, again.kind);
    assert_eq!(err.position, again.position);
}

#[test]
fn zero_read_is_eof_and_a_lying_read_is_io() {
    let reads = Cell::new(0usize);
    let mut reader = Reader::from_reader(ZeroThen {
        data: b"<r/>",
        reads: &reads,
    })
    .unwrap();
    let snap = snap_of!(reader);
    assert_eq!(snap, snap_bytes(b"", ParserConfig::default()));
    assert_eq!(reads.get(), 1);

    let mut reader = Reader::from_reader(Liar).unwrap();
    let err = reader.read_event().unwrap_err();
    assert!(matches!(err.kind, ErrorKind::Io), "{err}");
}

#[test]
fn from_vec_matches_bytes_and_streaming_reader_is_send() {
    fn assert_send<T: Send>() {}
    assert_send::<Reader<IoInput<std::io::Cursor<Vec<u8>>>>>();

    let data = b"<r a=\"x\">y</r>".to_vec();
    let mut owned = Reader::from_vec(data.clone());
    let snap = snap_of!(owned);
    assert_eq!(snap, snap_bytes(&data, ParserConfig::default()));
}

struct Interrupt<'a> {
    data: &'a [u8],
    pos: usize,
    left: usize,
    hits: &'a Cell<usize>,
}

impl Read for Interrupt<'_> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.left > 0 {
            self.left -= 1;
            self.hits.set(self.hits.get() + 1);
            return Err(io::Error::new(io::ErrorKind::Interrupted, "intr"));
        }
        if self.pos >= self.data.len() || buf.is_empty() {
            return Ok(0);
        }
        buf[0] = self.data[self.pos];
        self.pos += 1;
        Ok(1)
    }
}

impl BufRead for Interrupt<'_> {
    fn fill_buf(&mut self) -> io::Result<&[u8]> {
        Ok(&[])
    }
    fn consume(&mut self, _: usize) {}
}

struct Block<'a> {
    data: &'a [u8],
    pos: usize,
    armed: bool,
}

impl Read for Block<'_> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.armed {
            self.armed = false;
            return Err(io::Error::new(io::ErrorKind::WouldBlock, "later"));
        }
        if self.pos >= self.data.len() || buf.is_empty() {
            return Ok(0);
        }
        buf[0] = self.data[self.pos];
        self.pos += 1;
        Ok(1)
    }
}

impl BufRead for Block<'_> {
    fn fill_buf(&mut self) -> io::Result<&[u8]> {
        Ok(&[])
    }
    fn consume(&mut self, _: usize) {}
}

struct ZeroThen<'a> {
    data: &'a [u8],
    reads: &'a Cell<usize>,
}

impl Read for ZeroThen<'_> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.reads.set(self.reads.get() + 1);
        if self.reads.get() == 1 {
            return Ok(0);
        }
        if self.data.is_empty() || buf.is_empty() {
            return Ok(0);
        }
        buf[0] = self.data[0];
        Ok(1)
    }
}

impl BufRead for ZeroThen<'_> {
    fn fill_buf(&mut self) -> io::Result<&[u8]> {
        Ok(&[])
    }
    fn consume(&mut self, _: usize) {}
}

struct Liar;

impl Read for Liar {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        Ok(buf.len().saturating_add(8))
    }
}

impl BufRead for Liar {
    fn fill_buf(&mut self) -> io::Result<&[u8]> {
        Ok(&[])
    }
    fn consume(&mut self, _: usize) {}
}

/// Bytes of `<r>` + `<a/>` * n + `</r>`, generated without storing the document.
struct Gen {
    n: u64,
    index: u64,
    chunk: usize,
}

impl Gen {
    fn total(&self) -> u64 {
        3 + self.n * 4 + 4
    }

    fn byte_at(&self, index: u64) -> u8 {
        if index < 3 {
            return b"<r>"[index as usize];
        }
        let body = self.n * 4;
        if index < 3 + body {
            return b"<a/>"[((index - 3) % 4) as usize];
        }
        b"</r>"[(index - 3 - body) as usize]
    }
}

impl Read for Gen {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.index >= self.total() || buf.is_empty() {
            return Ok(0);
        }
        let n = self
            .chunk
            .min(buf.len())
            .min((self.total() - self.index) as usize);
        for (offset, slot) in buf[..n].iter_mut().enumerate() {
            *slot = self.byte_at(self.index + offset as u64);
        }
        self.index += n as u64;
        Ok(n)
    }
}

impl BufRead for Gen {
    fn fill_buf(&mut self) -> io::Result<&[u8]> {
        Ok(&[])
    }
    fn consume(&mut self, _: usize) {}
}

#[test]
fn streaming_buffer_stays_bounded_for_a_huge_tiny_token_document() {
    // A few megabytes of `<a/>`. A full-buffer reader would retain all of it.
    let n = 600_000u64;
    for chunk in [1usize, 7] {
        let mut reader = Reader::from_reader(Gen { n, index: 0, chunk }).unwrap();
        let mut seen = 0usize;
        loop {
            match reader.read_event() {
                Ok(Some(_)) => seen += 1,
                Ok(None) => break,
                Err(err) => panic!("chunk {chunk}: {err}"),
            }
            if seen % 50_000 == 0 {
                assert!(
                    reader.buffered_capacity() <= 1024 * 1024,
                    "chunk {chunk}: capacity {} after {seen} events",
                    reader.buffered_capacity()
                );
            }
        }
        assert!(seen > 100_000, "{seen}");
        assert!(
            reader.buffered_capacity() <= 1024 * 1024,
            "chunk {chunk}: final capacity {}",
            reader.buffered_capacity()
        );
    }
}

fn text_actual(err: &Error) -> (u64, u64, u64) {
    match err.kind {
        ErrorKind::SecurityLimit {
            kind: LimitKind::TextLength,
            limit,
            actual,
        } => (limit, actual, err.position.offset),
        _ => panic!("expected TextLength, got {err}"),
    }
}

/// `from_bytes` and `from_reader` report the same TextLength, including offset.
fn assert_text_limit(xml: &str, max: u64, actual: u64, body_at: usize) {
    let slice = pull(xml, config(max)).expect_err(xml);
    let (limit, got, offset) = text_actual(&slice);
    assert_eq!(limit, max, "{xml}");
    assert_eq!(got, actual, "{xml}");
    assert_eq!(offset, (body_at + actual as usize) as u64, "{xml}");
    assert!(offset < xml.len() as u64, "{xml}");

    let streamed = read_reader(xml, max).expect_err(xml);
    assert_eq!(slice.kind, streamed.kind, "{xml}");
    assert_eq!(slice.position, streamed.position, "{xml}");
}

fn read_reader(xml: &str, max: u64) -> Result<Vec<EventOwned>, Error> {
    let mut streamed = Reader::from_reader(std::io::Cursor::new(xml.as_bytes()))
        .unwrap()
        .with_config(config(max));
    let mut out = Vec::new();
    loop {
        match streamed.read_event() {
            Ok(None) => return Ok(out),
            Ok(Some(Event::Doctype(dtd))) => out.push(EventOwned::Doctype(dtd.name.into_owned())),
            Ok(Some(Event::PI(pi))) => out.push(EventOwned::Pi(pi.target.into_owned())),
            Ok(Some(_)) => out.push(EventOwned::Other),
            Err(err) => return Err(err),
        }
    }
}

#[test]
fn d26_comment_cdata_and_pi_stop_at_the_first_overflow() {
    let comment = format!("<!--{}--><r/>", "0123456789");
    assert_text_limit(&comment, 4, 5, comment.find('0').unwrap());
    let long = format!("<!--{}--><r/>", "x".repeat(100_000));
    assert_text_limit(&long, 4, 5, long.find('x').unwrap());
    assert!(pull("<!--0123--><r/>", config(4)).is_ok());
    assert!(pull("<!----><r/>", config(0)).is_ok());
    let one = "<!--x--><r/>";
    assert_text_limit(one, 0, 1, one.find('x').unwrap());

    let cdata = format!("<r><![CDATA[{}]]></r>", "0123456789");
    assert_text_limit(&cdata, 4, 5, cdata.find('0').unwrap());
    assert!(pull("<r><![CDATA[0123]]></r>", config(4)).is_ok());

    let pi = format!("<?t {}?><r/>", "0123456789");
    assert_text_limit(&pi, 4, 5, pi.find('0').unwrap());
    assert!(pull("<?t 0123?><r/>", config(4)).is_ok());
    assert!(pull("<?t?><r/>", config(0)).is_ok());
    assert!(pull("<?t ?><r/>", config(0)).is_ok());
}

#[test]
fn d26_multibyte_crlf_and_unterminated_comment() {
    // U+00E9 is two bytes. Four ASCII bytes sit on the cap, so actual is 6.
    let comment = "<!--xxxxéyyy--><r/>";
    assert_text_limit(comment, 4, 6, comment.find('x').unwrap());
    // Three ASCII bytes, then a 4-byte character, cap 4: actual is 7.
    let crossed = "<!--xxx😀--><r/>";
    assert_text_limit(crossed, 4, 7, crossed.find('x').unwrap());
    let cdata = "<r><![CDATA[xxxx€]]></r>";
    assert_text_limit(cdata, 4, 7, cdata.find('x').unwrap());
    let pi = "<?t xxxx😀?><r/>";
    assert_text_limit(pi, 4, 8, pi.find('x').unwrap());

    // CRLF is one content character. Four `x`s plus that newline is length 5,
    // and the cursor sits after both raw bytes.
    let crlf = "<!--xxxx\r\nyyy--><r/>";
    let err = pull(crlf, config(4)).expect_err(crlf);
    let (limit, actual, offset) = text_actual(&err);
    assert_eq!((limit, actual), (4, 5));
    assert_eq!(offset, (crlf.find('x').unwrap() + 6) as u64);
    let streamed = read_reader(crlf, 4).expect_err(crlf);
    assert_eq!(err.kind, streamed.kind);
    assert_eq!(err.position, streamed.position);
    assert!(pull("<!--xxx\r\n--><r/>", config(4)).is_ok());

    let mut open = String::from("<!--");
    open.push_str(&"x".repeat(100_000));
    assert_text_limit(&open, 4, 5, open.find('x').unwrap());
}

fn assert_all_paths(label: &str, data: &[u8], cfg: &ParserConfig) -> Snap {
    let base = snap_bytes(data, cfg.clone());
    assert_eq!(
        snap_vec(data.to_vec(), cfg.clone()),
        base,
        "{label} from_vec"
    );
    for chunk in [1usize, 2, 3, 7] {
        assert_eq!(
            snap_chunks(data, chunk, cfg.clone()),
            base,
            "{label} chunk {chunk}"
        );
    }
    base
}

fn assert_byte_limit(label: &str, snap: &Snap, kind: LimitKind, limit: u64, offset: u64) {
    let err = snap.err.as_ref().unwrap_or_else(|| panic!("{label}: ok"));
    match err.kind {
        ErrorKind::SecurityLimit {
            kind: got,
            limit: got_limit,
            actual,
        } => assert_eq!(
            (got, got_limit, actual, err.offset),
            (kind, limit, limit + 1, offset),
            "{label} line {} col {}",
            err.line,
            err.column
        ),
        ref other => panic!("{label}: {other:?} @{}", err.offset),
    }
}

/// D40: a subset keyword mismatch at or after an armed byte-cap is the cap,
/// on every path. A mismatch strictly before the cap stays Syntax at `<`.
#[test]
fn d40_subset_keyword_mismatch_respects_armed_byte() {
    const PRE: &str = "<!DOCTYPE r [";
    let start = PRE.len();
    let keywords: &[(&str, &str)] = &[
        ("ELEMENT", "<!ELEMENT"),
        ("ENTITY", "<!ENTITY"),
        ("ATTLIST", "<!ATTLIST"),
        ("NOTATION", "<!NOTATION"),
        ("INCLUDE", "<![INCLUDE["),
        ("IGNORE", "<![IGNORE["),
        ("comment", "<!--"),
        ("pi", "<?"),
    ];
    for (name, keyword) in keywords {
        let mut broken = keyword.as_bytes().to_vec();
        let at = broken.len() - 1;
        broken[at] = if broken[at] == b'x' { b'y' } else { b'x' };
        let doc = format!("{PRE}{}]><r/>", String::from_utf8(broken).unwrap());
        let doc = doc.as_bytes();
        let mismatch = start + at;
        for (which, cap) in [
            ("before", mismatch + 1),
            ("at", mismatch),
            ("after", mismatch - 1),
        ] {
            let token = ParserConfig::default()
                .with_max_token_len(cap as u64)
                .with_max_internal_subset_len((mismatch + 8) as u64);
            let snap = assert_all_paths(&format!("{name} token {which}"), doc, &token);
            if which == "before" {
                let err = snap.err.as_ref().unwrap();
                assert!(
                    matches!(err.kind, ErrorKind::Syntax),
                    "{name} token before: {:?}",
                    err.kind
                );
                assert_eq!(err.offset, start as u64, "{name} token before");
            } else {
                assert_byte_limit(
                    &format!("{name} token {which}"),
                    &snap,
                    LimitKind::TokenLength,
                    cap as u64,
                    cap as u64,
                );
            }
            let subset_limit = (cap - start) as u64;
            let subset = ParserConfig::default()
                .with_max_internal_subset_len(subset_limit)
                .with_max_token_len((mismatch + 8) as u64);
            let snap = assert_all_paths(&format!("{name} subset {which}"), doc, &subset);
            if which == "before" {
                let err = snap.err.as_ref().unwrap();
                assert!(
                    matches!(err.kind, ErrorKind::Syntax),
                    "{name} subset before: {:?}",
                    err.kind
                );
                assert_eq!(err.offset, start as u64, "{name} subset before");
            } else {
                assert_byte_limit(
                    &format!("{name} subset {which}"),
                    &snap,
                    LimitKind::InternalSubsetLength,
                    subset_limit,
                    cap as u64,
                );
            }
        }
    }

    let tight = ParserConfig::default()
        .with_max_token_len(48)
        .with_max_internal_subset_len(40)
        .with_max_text_len(12)
        .with_max_name_len(6)
        .with_max_total_input(512);
    for (label, doc) in [
        (
            "min-9318",
            &b"<!DOCTYPE r [\n<!ENTITY a \"\">\n<!ENTITY a \"\"><!NOTAt"[..],
        ),
        (
            "min-f0f2",
            &b"<!DOCTYPE r [<!ENTITY a \"\">\n<!ENTITY a \"\"><!NOTATt"[..],
        ),
    ] {
        let snap = assert_all_paths(label, doc, &tight);
        assert_byte_limit(label, &snap, LimitKind::TokenLength, 48, 48);
    }
}

/// Sites that already used the prolog keyword cut, plus the subset closer
/// and a parameter-entity reference, stay on the same rule.
#[test]
fn d40_other_markup_matches_the_armed_byte() {
    // Prolog `<!DOCTYPE`, mismatch at the `P`.
    let prolog = b"<!DOCTYx r>";
    let mismatch = 7usize;
    for (which, cap) in [("before", 8usize), ("at", 7usize), ("after", 6usize)] {
        let cfg = ParserConfig::default().with_max_token_len(cap as u64);
        let snap = assert_all_paths(&format!("prolog {which}"), prolog, &cfg);
        if which == "before" {
            let err = snap.err.unwrap();
            assert!(matches!(err.kind, ErrorKind::Syntax), "{:?}", err.kind);
            assert_eq!(err.offset, 0);
        } else {
            assert_byte_limit(
                "prolog",
                &snap,
                LimitKind::TokenLength,
                cap as u64,
                cap as u64,
            );
        }
        let _ = mismatch;
    }

    // Content `<![CDATA[`. Token starts at offset 3.
    let cdata = b"<r><![CDATAx[</r>";
    let origin = 3usize;
    let mismatch = 11usize;
    for (which, cap) in [("before", 12usize), ("at", 11usize), ("after", 10usize)] {
        let cfg = ParserConfig::default().with_max_token_len((cap - origin) as u64);
        let snap = assert_all_paths(&format!("cdata {which}"), cdata, &cfg);
        if which == "before" {
            let err = snap.err.unwrap();
            assert!(matches!(err.kind, ErrorKind::Syntax), "{:?}", err.kind);
            assert_eq!(err.offset, origin as u64);
        } else {
            assert_byte_limit(
                "cdata",
                &snap,
                LimitKind::TokenLength,
                (cap - origin) as u64,
                cap as u64,
            );
        }
        let _ = mismatch;
    }

    // Parameter-entity reference. `%` is the first subset byte.
    let pe = b"<!DOCTYPE r [%a]>";
    let pct = b"<!DOCTYPE r [".len();
    let name = pct + 1;
    for (which, cap) in [("before", name + 1), ("at", name), ("after", pct)] {
        let cfg = ParserConfig::default()
            .with_max_token_len(cap as u64)
            .with_max_internal_subset_len((cap - pct) as u64);
        let snap = assert_all_paths(&format!("pe {which}"), pe, &cfg);
        if which == "before" {
            let err = snap.err.unwrap();
            assert!(
                matches!(err.kind, ErrorKind::ExternalEntityForbidden),
                "{:?}",
                err.kind
            );
            assert_eq!(err.offset, pct as u64);
        } else if cap == pct {
            assert_byte_limit(
                "pe token",
                &snap,
                LimitKind::TokenLength,
                cap as u64,
                cap as u64,
            );
        } else {
            // The name byte is inside the subset, and it is also a token byte.
            // The earlier cap is the one under test; here both sit on `name`
            // only when cap == name. Token origin is 0, so TokenLength's
            // offset is `cap`. Subset starts at `pct`, so its offset is `cap`
            // too when the subset limit equals `cap - pct`. They tie on the
            // same byte: TotalInput is unset, TokenLength outranks subset.
            assert_byte_limit("pe", &snap, LimitKind::TokenLength, cap as u64, cap as u64);
        }
    }

    // `]>` closer. The byte after `]` is the cap.
    let closer = b"<!DOCTYPE r []X>";
    let bad = closer.len() - 2; // `X`
    let cfg = ParserConfig::default().with_max_token_len(bad as u64);
    let snap = assert_all_paths("closer at cap", closer, &cfg);
    assert_byte_limit(
        "closer",
        &snap,
        LimitKind::TokenLength,
        bad as u64,
        bad as u64,
    );
    let later = ParserConfig::default().with_max_token_len((bad + 1) as u64);
    let snap = assert_all_paths("closer before cap", closer, &later);
    let err = snap.err.unwrap();
    assert!(matches!(err.kind, ErrorKind::Syntax), "{:?}", err.kind);
    assert_eq!(err.offset, bad as u64);
}

/// A `--` whose first dash already crosses `max_text_len` is TextLength.
/// The second dash is not required, and a short stream window does not have it.
#[test]
fn comment_dash_at_text_cap_is_text_length() {
    let doc = b"<!--ssssssssss<!--sssssssssssssssssss";
    let cfg = ParserConfig::default()
        .with_max_text_len(12)
        .with_max_token_len(48);
    let snap = assert_all_paths("comment dash text cap", doc, &cfg);
    assert_byte_limit(
        "comment dash text cap",
        &snap,
        LimitKind::TextLength,
        12,
        17,
    );
}
