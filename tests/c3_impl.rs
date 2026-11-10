//! C3 checks for the public DOM build entry. This file belongs to the implementer.

use std::io::{BufRead, BufReader, Read};

use purexml::dom::{Document, WriteOptions};
use purexml::{Error, ErrorKind, ParserConfig, Reader};

struct OneByte<'a> {
    rest: &'a [u8],
}

impl Read for OneByte<'_> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if buf.is_empty() || self.rest.is_empty() {
            return Ok(0);
        }
        buf[0] = self.rest[0];
        self.rest = &self.rest[1..];
        Ok(1)
    }
}

impl BufRead for OneByte<'_> {
    fn fill_buf(&mut self) -> std::io::Result<&[u8]> {
        Ok(if self.rest.is_empty() {
            self.rest
        } else {
            &self.rest[..1]
        })
    }

    fn consume(&mut self, amt: usize) {
        let amt = amt.min(self.rest.len());
        self.rest = &self.rest[amt..];
    }
}

fn reader_end(bytes: &[u8]) -> Result<(), Error> {
    let mut reader = Reader::from_bytes(bytes);
    while reader.read_event()?.is_some() {}
    Ok(())
}

fn key(err: &Error) -> (String, u64, u64, u64) {
    (
        format!("{:?}", err.kind),
        err.position.line,
        err.position.column,
        err.position.offset,
    )
}

fn assert_same_end(label: &str, bytes: &[u8], built: &Result<Document, Error>) {
    match (reader_end(bytes), built) {
        (Ok(()), Ok(_)) => {}
        (Err(left), Err(right)) => {
            assert_eq!(key(&left), key(right), "{label}");
        }
        (Ok(()), Err(err)) => panic!("{label}: reader accepted, DOM rejected {err:?}"),
        (Err(err), Ok(_)) => panic!("{label}: reader rejected {err:?}, DOM accepted"),
    }
}

fn fingerprint(doc: &Document) -> String {
    fn walk(doc: &Document, id: purexml::dom::NodeId, out: &mut String) {
        let Some(node) = doc.get(id) else {
            return;
        };
        match node.kind() {
            Some(purexml::dom::NodeKind::Element) => {
                let name = node.name().unwrap();
                out.push('E');
                out.push_str(name.local);
                out.push('|');
                for attr in node.attributes() {
                    out.push_str(attr.local());
                    out.push('=');
                    out.push_str(attr.value());
                    out.push(if attr.specified() { 'S' } else { 'D' });
                    out.push(';');
                }
            }
            Some(purexml::dom::NodeKind::Text) => {
                out.push_str("T:");
                out.push_str(node.text().unwrap_or(""));
            }
            Some(purexml::dom::NodeKind::CData) => {
                out.push_str("C:");
                out.push_str(node.text().unwrap_or(""));
            }
            Some(purexml::dom::NodeKind::Comment) => {
                out.push_str("M:");
                out.push_str(node.text().unwrap_or(""));
            }
            Some(purexml::dom::NodeKind::Pi) => {
                let (target, data) = node.pi().unwrap();
                out.push_str("P:");
                out.push_str(target);
                out.push(' ');
                out.push_str(data.unwrap_or(""));
            }
            Some(purexml::dom::NodeKind::Doctype) => {
                let dtd = node.doctype().unwrap();
                out.push_str("D:");
                out.push_str(dtd.name);
                out.push('|');
                out.push_str(dtd.system_id.unwrap_or(""));
                out.push('|');
                out.push_str(dtd.public_id.unwrap_or(""));
            }
            _ => {}
        }
        if let Some(child) = node.first_child() {
            walk(doc, child.id(), out);
        }
        if let Some(next) = node.next_sibling() {
            walk(doc, next.id(), out);
        }
    }
    let mut out = String::new();
    if let Some(root) = doc.root() {
        walk(doc, root.id(), &mut out);
    }
    out
}

fn four(bytes: &[u8]) -> [Result<Document, Error>; 4] {
    let text = std::str::from_utf8(bytes).ok();
    let parsed = match text {
        Some(text) => Document::parse(text),
        None => Document::from_bytes(bytes),
    };
    [
        parsed,
        Document::from_bytes(bytes),
        Document::from_reader(BufReader::new(bytes)),
        Document::from_reader(OneByte { rest: bytes }),
    ]
}

#[test]
fn four_paths_match_each_other_and_the_reader() {
    let samples = [
        r#"<r a="1"><t>hi</t><!--c--><![CDATA[x]]><?p d?></r>"#,
        r#"<!DOCTYPE r PUBLIC "-//Example//EN" "r.dtd"><r/>"#,
        r#"<r xmlns="urn:b" xmlns:p="urn:a"><c xmlns="urn:b"><p:d xmlns:p="urn:a" p:x="1"/></c></r>"#,
        r#"<!DOCTYPE r [<!ATTLIST r a CDATA "1">]><r b="2"/>"#,
        "<not-xml",
    ];
    for xml in samples {
        let built = four(xml.as_bytes());
        for (index, doc) in built.iter().enumerate() {
            assert_same_end(&format!("sample {xml} path {index}"), xml.as_bytes(), doc);
        }
        if reader_end(xml.as_bytes()).is_err() {
            continue;
        }
        let marks: Vec<_> = built
            .iter()
            .map(|doc| fingerprint(doc.as_ref().unwrap()))
            .collect();
        assert!(marks.iter().all(|mark| mark == &marks[0]), "{marks:?}");
    }
}

#[test]
fn limits_match_at_the_boundary() {
    let xml = "<r>t</r>";
    let nodes = Document::parse(xml).unwrap().node_count();
    let exact = ParserConfig::default().with_max_nodes(nodes);
    assert!(Document::parse_with_config(xml, &exact).is_ok());
    let short = ParserConfig::default().with_max_nodes(nodes - 1);
    let err = Document::parse_with_config(xml, &short).unwrap_err();
    assert!(matches!(
        err.kind,
        ErrorKind::SecurityLimit {
            kind: purexml::LimitKind::NodeCount,
            ..
        }
    ));

    let bytes = Document::parse(xml).unwrap().approx_heap_bytes();
    let exact = ParserConfig::default().with_max_dom_bytes(bytes);
    assert!(Document::parse_with_config(xml, &exact).is_ok());
    let short = ParserConfig::default().with_max_dom_bytes(bytes - 1);
    let err = Document::parse_with_config(xml, &short).unwrap_err();
    assert!(
        matches!(
            err.kind,
            ErrorKind::SecurityLimit {
                kind: purexml::LimitKind::DomBytes,
                ..
            }
        ),
        "{err:?}"
    );
}

#[test]
fn redundant_declarations_stay_and_invented_ones_do_not() {
    let xml = r#"<r xmlns="urn:b" xmlns:p="urn:a"><c xmlns="urn:b"><p:d xmlns:p="urn:a" p:x="1"/></c></r>"#;
    let doc = Document::parse(xml).unwrap();
    let once = doc.to_string().unwrap();
    assert_eq!(once, xml);
    assert_eq!(Document::parse(&once).unwrap().to_string().unwrap(), once);
}

#[test]
fn doctype_serialization_keeps_the_name_only() {
    let doc = Document::parse(r#"<!DOCTYPE r PUBLIC "-//Example//EN" "r.dtd"><r/>"#).unwrap();
    assert_eq!(doc.to_string().unwrap(), "<!DOCTYPE r><r/>");
    let stored = doc
        .root()
        .unwrap()
        .first_child()
        .unwrap()
        .doctype()
        .unwrap();
    assert_eq!(stored.name, "r");
    assert_eq!(stored.system_id, Some("r.dtd"));
    assert_eq!(stored.public_id, Some("-//Example//EN"));
    assert_eq!(
        doc.to_string_with(WriteOptions { omit_doctype: true })
            .unwrap(),
        "<r/>"
    );
}

#[test]
fn attack_corpus_matches_the_reader_on_four_paths() {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data/attacks");
    let mut files: Vec<_> = std::fs::read_dir(&dir)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "xml"))
        .collect();
    files.sort();
    assert!(files.len() > 50, "attack corpus missing");
    for path in files {
        let bytes = std::fs::read(&path).unwrap();
        let label = path.display().to_string();
        let built = four(&bytes);
        for (index, doc) in built.iter().enumerate() {
            assert_same_end(&format!("{label} path {index}"), &bytes, doc);
        }
    }
}

#[cfg(not(debug_assertions))]
fn slope(points: &[(f64, f64)]) -> f64 {
    let n = points.len() as f64;
    let mut sx = 0.0;
    let mut sy = 0.0;
    let mut sxx = 0.0;
    let mut sxy = 0.0;
    for (x, y) in points {
        sx += x;
        sy += y;
        sxx += x * x;
        sxy += x * y;
    }
    (n * sxy - sx * sy) / (n * sxx - sx * sx)
}

#[cfg(not(debug_assertions))]
fn time_iters(mut body: impl FnMut()) -> f64 {
    use std::time::{Duration, Instant};
    body();
    let start = Instant::now();
    let mut runs = 0u32;
    while start.elapsed() < Duration::from_millis(30) || runs < 3 {
        body();
        runs += 1;
        if runs == 200 {
            break;
        }
    }
    start.elapsed().as_secs_f64() / f64::from(runs)
}

#[cfg(not(debug_assertions))]
fn fit(label: &str, samples: &[(usize, f64)]) -> f64 {
    let points: Vec<_> = samples
        .iter()
        .map(|(bytes, secs)| ((*bytes as f64).ln(), secs.max(1e-9).ln()))
        .collect();
    let value = slope(&points);
    println!("{label} slope={value:.3} points={samples:?}");
    assert!(value <= 1.5, "{label} slope {value} is above 1.5");
    value
}

#[test]
#[ignore = "release complexity probe: cargo test --release --test c3_impl -- --ignored complexity_probe --nocapture"]
fn complexity_probe() {
    #[cfg(debug_assertions)]
    panic!("run this probe with --release; debug timing is not a slope");
    #[cfg(not(debug_assertions))]
    complexity_probe_release();
}

#[cfg(not(debug_assertions))]
fn complexity_probe_release() {
    let wide = ParserConfig::default()
        .with_max_depth(10_000)
        .with_max_attrs(4_000)
        .with_max_nodes(2_000_000);

    let mut depth_points = Vec::new();
    for n in [64u32, 128, 256, 512] {
        let mut xml = String::new();
        for _ in 0..n {
            xml.push_str("<a>");
        }
        for _ in 0..n {
            xml.push_str("</a>");
        }
        let bytes = xml.len();
        let secs = time_iters(|| {
            Document::parse_with_config(&xml, &wide).unwrap();
        });
        depth_points.push((bytes, secs));
    }
    fit("depth", &depth_points);

    let mut attr_points = Vec::new();
    for n in [64u32, 128, 256, 512] {
        let mut xml = String::from("<r");
        for i in 0..n {
            xml.push_str(&format!(" a{i}=\"v\""));
        }
        xml.push_str("/>");
        let bytes = xml.len();
        let secs = time_iters(|| {
            Document::parse_with_config(&xml, &wide).unwrap();
        });
        attr_points.push((bytes, secs));
    }
    fit("attributes", &attr_points);

    let mut prefix_points = Vec::new();
    for n in [32u32, 64, 128, 256] {
        let mut xml = String::from("<r");
        for i in 0..n {
            xml.push_str(&format!(" xmlns:p{i}=\"urn:{i}\""));
        }
        xml.push_str("/>");
        let bytes = xml.len();
        let secs = time_iters(|| {
            Document::parse_with_config(&xml, &wide).unwrap();
        });
        prefix_points.push((bytes, secs));
    }
    fit("prefixes", &prefix_points);

    let uri = "u".repeat(64 * 1024);
    let mut uri_points = Vec::new();
    for leaves in [16u32, 32, 64, 128] {
        let mut xml = format!("<r xmlns:p=\"{uri}\">");
        for i in 0..leaves {
            xml.push_str(&format!("<p:e a=\"{i}\"/>"));
        }
        xml.push_str("</r>");
        let bytes = xml.len();
        let secs = time_iters(|| {
            let doc = Document::parse_with_config(&xml, &wide).unwrap();
            let _ = doc.to_string().unwrap();
        });
        uri_points.push((bytes, secs));
    }
    fit("long-uri-serialize", &uri_points);
}
