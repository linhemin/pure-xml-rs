//! fuzz 目标 parse_small_stack：在 512 KiB 栈的线程里用默认配置完整解析，专门找无界递归或栈余量不足。
//! 建议不用 ASan（ASan 会放大栈帧），用 `-s none` 运行，让栈帧接近 release：
//! cargo +nightly fuzz run -s none parse_small_stack -- -max_total_time=600 -timeout=600 -max_len=65536 -dict=fuzz/xml.dict
#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let data = data.to_vec();
    std::thread::Builder::new()
        .stack_size(512 << 10)
        .spawn(move || {
            let mut r = purexml::Reader::from_bytes(&data);
            let mut n = 0usize;
            while let Ok(Some(_)) = r.read_event() {
                n += 1;
                assert!(n <= data.len() * 4 + 16);
            }
        })
        .expect("spawn")
        .join()
        .expect("parse thread panicked");
});
