//! fuzz 目标 reader_chunked（M2 计划 §6）：同一输入走 from_bytes、from_vec 和按任意块长序列切块的
//! from_reader，事件序列和错误（kind、line、column、offset）必须完全相同（TotalInput 只比最终错误）。
//! 输入的前 4 个字节决定块长序列和限额档位：档位 1 用很小的上限，让 probe 边界在小输入里就触发。
//!
//! 注意：不用 `BufReader::with_capacity(n, &[u8])` 来切块。IoInput 一次请求 8 KiB，
//! BufReader 在请求不小于容量时会绕过自己的缓冲，直接读切片，块长根本没有生效。
//!
//! 运行：cargo +nightly fuzz run reader_chunked -- -max_total_time=900 -timeout=5 -rss_limit_mb=2048
#![no_main]

use libfuzzer_sys::fuzz_target;

#[path = "common_reader.rs"]
mod common_reader;

fuzz_target!(|input: &[u8]| {
    if input.len() < 4 {
        return;
    }
    let sizes = [
        (input[0] as usize % 64) + 1,
        (input[1] as usize % 7) + 1,
        (input[2] as usize) * 37 % 9000 + 1,
    ];
    common_reader::check(&input[4..], sizes, input[3] & 1 == 1);
});
