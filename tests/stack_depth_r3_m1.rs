//! M1 第三轮：D20 内容模型递归的上限与栈余量（`reader.rs` 的 `parse_content_group` ↔ `parse_cp`）。
//!
//! 6b29230 给这段递归加了上限 `min(max_depth, 1024)`，但递归本身还在。实测栈消耗：
//! - release 每层约 170 B：1024 层需要约 170 KiB 栈，默认 256 层需要约 51 KiB。
//! - debug 每层约 4.8 KiB：1024 层需要约 4.8 MiB 栈，默认 256 层需要约 1.26 MiB；2 MiB 栈最多撑 419 层，512 KiB 栈最多 100 层。
//!
//! 因此在 debug 构建（`cargo test` 的默认配置）下，本文件会因栈溢出 SIGABRT，这是 D20 修复不完整的表现，保持开启。
//! 栈溢出会让整个测试二进制中止，所以单独放在这个文件里。

fn run(s: String, stack: usize, max_depth: usize) -> Result<(), String> {
    std::thread::Builder::new()
        .stack_size(stack)
        .spawn(move || {
            let cfg = purexml::ParserConfig::default().with_max_depth(max_depth);
            let mut r = purexml::Reader::from_bytes(s.as_bytes()).with_config(cfg);
            loop {
                match r.read_event() {
                    Ok(Some(_)) => {}
                    Ok(None) => return Ok(()),
                    Err(e) => return Err(format!("{:?}", e.kind)),
                }
            }
        })
        .unwrap()
        .join()
        .unwrap()
}

fn model(n: usize) -> String {
    format!(
        "<!DOCTYPE r [<!ELEMENT r {}a{}>]><r/>",
        "(".repeat(n),
        ")".repeat(n)
    )
}

/// 硬上限 1024：调高 max_depth 后，1024 层在 2 MiB 栈上必须通过，1025 层报 ElementDepth（limit = 1024）
#[test]
fn d20_ceiling_1024_on_2mib_stack() {
    const MIB2: usize = 2 << 20;
    assert_eq!(run(model(1024), MIB2, 1024), Ok(()));
    let e = run(model(1025), MIB2, 1024).unwrap_err();
    assert!(
        e.contains("ElementDepth") && e.contains("limit: 1024"),
        "{e}"
    );
    let e = run(model(100_000), MIB2, 10_000_000).unwrap_err();
    assert!(
        e.contains("ElementDepth") && e.contains("limit: 1024"),
        "{e}"
    );
}

/// 默认配置（256 层）在 512 KiB 小栈上也必须安全
#[test]
fn d20_default_depth_on_512kib_stack() {
    assert_eq!(run(model(256), 512 << 10, 256), Ok(()));
    assert!(run(model(257), 512 << 10, 256).is_err());
}
