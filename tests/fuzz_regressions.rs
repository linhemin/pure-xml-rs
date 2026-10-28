//! fuzz 回归样本：tests/data/fuzz-regressions/ 下的每个输入都必须不 panic、限时结束、
//! 峰值内存受控，并且 from_bytes 与 from_reader（多种缓冲大小）结果一致。

mod common;

use common::{run_guarded, Guarded, SOFT_CAP};
use std::path::Path;
use std::time::Duration;

/// `from_reader` returns `Reader<IoInput<R>>`, whose events borrow the reader.
/// A closure cannot hand that borrow back out.
macro_rules! drain {
    ($reader:expr) => {{
        let reader = &mut $reader;
        let mut evs = Vec::new();
        let end = loop {
            match reader.read_event() {
                Ok(Some(e)) => evs.push(format!("{e:?}")),
                Ok(None) => break None,
                Err(e) => break Some(format!("{:?}@{:?}", e.kind, e.position)),
            }
        };
        (evs, end)
    }};
}

#[test]
fn fuzz_regressions() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data/fuzz-regressions");
    let mut files: Vec<_> = std::fs::read_dir(&dir)
        .unwrap()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_none_or(|x| x != "md"))
        .collect();
    files.sort();
    for p in files {
        let name = p.file_name().unwrap().to_string_lossy().to_string();
        let data = std::fs::read(&p).unwrap();
        match run_guarded(&name, Duration::from_secs(2), move || {
            let mut bytes = purexml::Reader::from_bytes(&data);
            let a = drain!(bytes);
            for cap in [1usize, 7, 64] {
                let mut streamed =
                    purexml::Reader::from_reader(std::io::BufReader::with_capacity(cap, &data[..]))
                        .expect("from_reader 读内存切片不应失败");
                assert_eq!(a, drain!(streamed), "from_reader(cap={cap}) 结果不同");
            }
        }) {
            Guarded::Done { peak_bytes, .. } => {
                assert!(peak_bytes < SOFT_CAP, "{name}: 峰值 {peak_bytes} 字节")
            }
            Guarded::TimedOut => panic!("{name}: 超时"),
            Guarded::Panicked(m) => panic!("{name}: panic {m}"),
        }
    }
}
