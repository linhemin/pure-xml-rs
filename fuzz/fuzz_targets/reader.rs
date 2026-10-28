//! fuzz 目标 reader：同一输入分别走 from_bytes 与 from_reader（BufReader 容量 1..=64），
//! 检查两种入口结果一致（事件序列 Debug 文本 + 最终错误类别与位置），并检查不变量：
//! - Ok 时 StartElement/EndElement 平衡，且元素深度 ≤ 256（默认 max_depth）
//!
//! 运行：cargo +nightly fuzz run reader -- -max_total_time=900 -timeout=2 -rss_limit_mb=1024
#![no_main]

use libfuzzer_sys::fuzz_target;
use purexml::{Event, Reader};

#[derive(Debug, PartialEq)]
struct Trace {
    events: Vec<String>,
    end: Result<(), (purexml::ErrorKind, u64, u64, u64)>,
    balanced: bool,
    max_depth: usize,
}

/// `from_reader` 的事件借用阅读器，闭包无法把该借用交出来。
macro_rules! drain {
    ($reader:expr) => {{
        let reader = &mut $reader;
        let mut events = Vec::new();
        let (mut depth, mut max_depth, mut balanced) = (0usize, 0usize, true);
        let end = loop {
            match reader.read_event() {
                Ok(Some(ev)) => {
                    match &ev {
                        Event::StartElement(_) => {
                            depth += 1;
                            max_depth = max_depth.max(depth);
                        }
                        Event::EndElement(_) => {
                            if depth == 0 {
                                balanced = false;
                            } else {
                                depth -= 1;
                            }
                        }
                        Event::EmptyElement(_) => max_depth = max_depth.max(depth + 1),
                        _ => {}
                    }
                    events.push(format!("{ev:?}"));
                }
                Ok(None) => break Ok(()),
                Err(e) => {
                    break Err((
                        e.kind.clone(),
                        e.position.line,
                        e.position.column,
                        e.position.offset,
                    ))
                }
            }
        };
        Trace {
            events,
            end,
            balanced: balanced && depth == 0,
            max_depth,
        }
    }};
}

fuzz_target!(|input: (u8, &[u8])| {
    let (chunk, data) = input;
    let chunk = (chunk as usize % 64) + 1;
    let mut bytes = Reader::from_bytes(data);
    let a = drain!(bytes);
    let br = std::io::BufReader::with_capacity(chunk, data);
    match Reader::from_reader(br) {
        Ok(mut streamed) => {
            let b = drain!(streamed);
            assert_eq!(a, b, "from_bytes 与 from_reader(chunk={chunk}) 结果不一致");
        }
        Err(e) => {
            // from_reader 只在读入阶段失败（IO / 超过 1 GiB），对内存切片不应发生
            panic!("from_reader 读入失败：{e:?}");
        }
    }
    if a.end.is_ok() {
        assert!(a.balanced, "Ok 但起止标签不平衡");
        assert!(a.max_depth <= 256, "Ok 但深度 {} > 256", a.max_depth);
    }
});
