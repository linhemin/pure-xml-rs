//! fuzz 目标 parse：任意字节 → 默认配置完整解析。只要求：不 panic、不挂起、不 OOM。
//! 运行：cargo +nightly fuzz run parse -- -max_total_time=900 -timeout=2 -rss_limit_mb=1024 -dict=fuzz/xml.dict
#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let mut r = purexml::Reader::from_bytes(data);
    // 每个事件至少消耗一个字节或结束，事件数不会超过输入长度的常数倍；
    // 上限只为防止实现死循环时 fuzzer 挂起（挂起会被 -timeout 抓到）。
    let mut n = 0usize;
    while let Ok(Some(_)) = r.read_event() {
        n += 1;
        assert!(
            n <= data.len() * 4 + 16,
            "事件数异常：{n} 个事件 / {} 字节",
            data.len()
        );
    }
});
