//! Optional W3C XML conformance score.
//!
//! `tests/w3c/fetch.sh` downloads the suite. The tree is not committed.
//! When it is absent this test passes and prints a skip message.
//! The scored set is the canonical `xmltest` standalone slice:
//! `xmltest/not-wf/sa` must be rejected and `xmltest/valid/sa` must parse.
//! The combined pass rate must be at least 95%. Failures are rewritten to
//! `tests/w3c/known-failures.txt` only when `PUREXML_WRITE_FAILURES=1`.

use std::fs;
use std::path::{Path, PathBuf};

use purexml_core::Reader;

fn suite_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/w3c/xmlconf")
}

fn failures_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/w3c/known-failures.txt")
}

#[test]
fn w3c_not_wf_and_sa_pass_rate() {
    let root = suite_root();
    if !root.is_dir() {
        eprintln!("W3C suite not fetched; run tests/w3c/fetch.sh");
        return;
    }
    let mut files = Vec::new();
    collect_xml(&root, &root, &mut files);
    let mut considered = 0u64;
    let mut passed = 0u64;
    let mut failures = Vec::new();
    for (rel, kind) in files {
        let Some(kind) = kind else {
            continue;
        };
        considered += 1;
        let path = root.join(&rel);
        let bytes = fs::read(&path).unwrap_or_default();
        let ok = document_parses(&bytes);
        let good = match kind {
            Kind::NotWf => !ok,
            Kind::Standalone => ok,
        };
        if good {
            passed += 1;
        } else {
            failures.push(rel);
        }
    }
    failures.sort();
    let rate = if considered == 0 {
        0.0
    } else {
        passed as f64 / considered as f64
    };
    eprintln!(
        "W3C not-wf/sa: {passed}/{considered} = {:.2}% ({} failures)",
        rate * 100.0,
        failures.len()
    );
    if std::env::var_os("PUREXML_WRITE_FAILURES").is_some() {
        let mut body = String::from(
            "# Relative paths under tests/w3c/xmlconf that this parser still mishandles.\n",
        );
        for failure in &failures {
            body.push_str(failure);
            body.push('\n');
        }
        fs::write(failures_path(), body).unwrap();
    }
    assert!(
        rate >= 0.95,
        "W3C not-wf/sa pass rate {rate:.4} is below 95% ({passed}/{considered})"
    );
}

#[derive(Clone, Copy)]
enum Kind {
    NotWf,
    Standalone,
}

fn classify(rel: &str) -> Option<Kind> {
    let lower = rel.replace('\\', "/");
    if !lower.starts_with("xmltest/") || lower.contains("/out/") {
        return None;
    }
    if lower.contains("not-wf/sa/") {
        return Some(Kind::NotWf);
    }
    if lower.contains("valid/sa/") {
        return Some(Kind::Standalone);
    }
    None
}

fn collect_xml(root: &Path, dir: &Path, out: &mut Vec<(String, Option<Kind>)>) {
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(_) => return,
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_xml(root, &path, out);
            continue;
        }
        let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
        if !(name.ends_with(".xml") || name.ends_with(".XML")) {
            continue;
        }
        let rel = path
            .strip_prefix(root)
            .unwrap_or(&path)
            .to_string_lossy()
            .replace('\\', "/");
        let kind = classify(&rel);
        out.push((rel, kind));
    }
}

fn document_parses(bytes: &[u8]) -> bool {
    let mut reader = Reader::from_bytes(bytes);
    loop {
        match reader.read_event() {
            Ok(None) => return true,
            Ok(Some(_)) => {}
            Err(_) => return false,
        }
    }
}
