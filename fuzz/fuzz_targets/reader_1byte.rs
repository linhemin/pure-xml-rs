//! fuzz 目标 reader_1byte（M2 计划 §6）：from_reader 每次只读 1 字节，与 from_bytes、from_vec 比较。
//! 输入第 1 个字节的最低位选择默认限额或很小的限额（同 reader_chunked）。
//!
//! 运行：cargo +nightly fuzz run reader_1byte -- -max_total_time=900 -timeout=5 -rss_limit_mb=2048
#![no_main]

use libfuzzer_sys::fuzz_target;

#[path = "common_reader.rs"]
mod common_reader;

fuzz_target!(|input: &[u8]| {
    if input.is_empty() {
        return;
    }
    common_reader::check(&input[1..], [1, 1, 1], input[0] & 1 == 1);
});
