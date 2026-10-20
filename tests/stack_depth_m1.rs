//! D18（M1 第二轮新发现，严重）：实体替换文本的良构检查（`wf.rs` 的 `Cursor::element` ↔ `Cursor::markup`）
//! 是无深度上限的递归下降。替换文本里嵌套足够多的 `<a>`，在第一次引用该实体时就会栈溢出，
//! 整个进程中止（Rust 无法捕获栈溢出）。
//!
//! 实测（release）：2 MiB 线程栈下约 1.1 万层（输入约 33 KB）即溢出，8 MiB 下约 4.4 万层（约 132 KB）。
//! 默认 `max_depth = 256` 对这条检查路径不起作用。
//!
//! 栈溢出会让整个测试二进制中止，所以单独放在这个文件里，不影响其他测试的结果。
//! 修复前本测试会以 SIGABRT 失败，保持开启。

use std::time::Duration;

fn parse_in_thread(input: String, stack: usize) -> Result<(), String> {
    let h = std::thread::Builder::new()
        .stack_size(stack)
        .spawn(move || {
            let mut r = purexml::Reader::from_bytes(input.as_bytes());
            loop {
                match r.read_event() {
                    Ok(Some(_)) => {}
                    Ok(None) => return Ok(()),
                    Err(e) => return Err(format!("{:?}", e.kind)),
                }
            }
        })
        .unwrap();
    h.join().unwrap()
}

/// 2 MiB 栈（tokio 等运行时工作线程的常见默认值）下，10 万层嵌套的实体替换文本（输入约 300 KB）
/// 必须返回错误（Syntax 或 SecurityLimit(ElementDepth)），而不是栈溢出
#[test]
fn d18_deeply_nested_entity_replacement_does_not_overflow_stack() {
    let start = std::time::Instant::now();
    for (open, close) in [("<a>", ""), ("<a><!--x-->", "")] {
        let n = 100_000;
        let s = format!(
            "<!DOCTYPE r [<!ENTITY e \"{}{}\">]><r>&e;</r>",
            open.repeat(n),
            close
        );
        let r = parse_in_thread(s, 2 << 20);
        assert!(r.is_err(), "10 万层未闭合嵌套应报错");
    }
    // 平衡但超过 max_depth 的嵌套同样要在限额内报错
    let n = 50_000;
    let s = format!(
        "<!DOCTYPE r [<!ENTITY e \"{}{}\">]><r>&e;</r>",
        "<a>".repeat(n),
        "</a>".repeat(n)
    );
    let r = parse_in_thread(s, 2 << 20);
    assert!(r.is_err(), "5 万层平衡嵌套超过 max_depth 应报错：{r:?}");
    assert!(start.elapsed() < Duration::from_secs(10));
}
