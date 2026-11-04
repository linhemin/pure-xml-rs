//! fuzz 目标 dom_roundtrip（计划 §6.1）：
//! Reader 读入 → 用修改 API 建树1 → to_string 得 s1 → 再读、再建树2 → to_string 得 s2。
//! - 输入解析失败：跳过
//! - 树1 结构 == 输入事件结构；树2 结构 == 树1；s1 == s2（逐字节）
//! - 统一断言（Lead 18:45）：序列化要么 Err，要么读回结构一致；来自合法输入的树必须能写出
//! - Reader 接受但 DOM 拒绝（限额以外）算违例
//! - 不调用 create_doctype；Doctype 事件只记录、不进树
//!
//! 共用逻辑在 tests/common/dom_c2.rs（与 tests/c2_m2.rs 共用一份）。
//!
//! 运行：cargo +nightly fuzz run dom_roundtrip -- -max_total_time=60 -timeout=60 -rss_limit_mb=2048
#![no_main]

#[path = "../../tests/common/dom_c2.rs"]
mod dom_c2;

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    if data.len() > 256 << 10 {
        return;
    }
    if let Err(msg) = dom_c2::roundtrip(data) {
        panic!("dom_roundtrip 违例：{msg}");
    }
});
