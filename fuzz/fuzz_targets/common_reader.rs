//! reader_chunked 和 reader_1byte 共用的比较逻辑（不是 fuzz 目标本身）。
#![allow(dead_code)]

use purexml::{LimitKind, ParserConfig, Reader};
use std::io::Read;

#[derive(Debug, PartialEq)]
struct Trace {
    events: Vec<String>,
    end: Result<(), (purexml::ErrorKind, u64, u64, u64)>,
}

macro_rules! drain {
    ($reader:expr) => {{
        let reader = &mut $reader;
        let mut events = Vec::new();
        let end = loop {
            match reader.read_event() {
                Ok(Some(ev)) => events.push(format!("{ev:?}")),
                Ok(None) => break Ok(()),
                Err(e) => {
                    break Err((e.kind.clone(), e.position.line, e.position.column, e.position.offset))
                }
            }
        };
        Trace { events, end }
    }};
}

struct Chunked<'a> {
    data: &'a [u8],
    sizes: [usize; 3],
    i: usize,
}

impl Read for Chunked<'_> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let n = self.sizes[self.i % 3].min(buf.len()).min(self.data.len());
        self.i += 1;
        buf[..n].copy_from_slice(&self.data[..n]);
        self.data = &self.data[n..];
        Ok(n)
    }
}

impl std::io::BufRead for Chunked<'_> {
    fn fill_buf(&mut self) -> std::io::Result<&[u8]> {
        unreachable!("IoInput 只用 read")
    }
    fn consume(&mut self, _: usize) {}
}

pub fn check(data: &[u8], sizes: [usize; 3], tight: bool) {
    let config = if tight {
        ParserConfig::default()
            .with_max_token_len(48)
            .with_max_internal_subset_len(40)
            .with_max_text_len(12)
            .with_max_name_len(6)
            .with_max_total_input(512)
    } else {
        ParserConfig::default()
    };
    let mut bytes = Reader::from_bytes(data).with_config(config.clone());
    let a = drain!(bytes);
    let mut owned = Reader::from_vec(data.to_vec()).with_config(config.clone());
    assert_eq!(a, drain!(owned), "from_vec 与 from_bytes 不一致");
    let mut streamed =
        Reader::from_reader_with_config(Chunked { data, sizes, i: 0 }, config).expect("from_reader");
    let b = drain!(streamed);
    let total = matches!(
        &a.end,
        Err((purexml::ErrorKind::SecurityLimit { kind: LimitKind::TotalInput, .. }, ..))
    );
    if total {
        assert_eq!(a.end, b.end, "TotalInput 最终错误不一致（块长 {sizes:?}）");
    } else {
        assert_eq!(a, b, "from_bytes 与 from_reader（块长 {sizes:?}）不一致");
    }
}

