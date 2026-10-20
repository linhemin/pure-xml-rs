//! D20（M1 第二轮新发现，严重）：ELEMENT 声明内容模型的解析（`reader.rs` 的 `parse_cp` ↔ `parse_content_group`）
//! 是无深度上限的递归。`<!ELEMENT r ((((…a))))>` 嵌套足够多层括号就会栈溢出，整个进程中止。
//!
//! 实测（release）：2 MiB 线程栈下约 1.47 万层（输入约 29 KB）即溢出，8 MiB 下约 5.9 万层（约 117 KB）。
//!
//! 与 D18 是两条不同的代码路径。栈溢出会让整个测试二进制中止，所以单独放在一个文件里。
//! 修复前本测试会以 SIGABRT 失败，保持开启。

fn parse_in_thread(input: String, stack: usize) -> Result<(), String> {
    std::thread::Builder::new()
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
        .unwrap()
        .join()
        .unwrap()
}

/// 2 MiB 栈下，10 万层括号嵌套的内容模型（输入约 200 KB）必须返回错误或正常完成，不能栈溢出。
/// 期望实现设一个嵌套上限（例如复用 max_depth = 256），超过时报 Syntax 或 SecurityLimit。
#[test]
fn d20_deeply_nested_content_model_does_not_overflow_stack() {
    for (n, close) in [(100_000usize, true), (100_000, false)] {
        let s = format!(
            "<!DOCTYPE r [<!ELEMENT r {}a{}>]><r/>",
            "(".repeat(n),
            if close { ")".repeat(n) } else { String::new() }
        );
        let r = parse_in_thread(s, 2 << 20);
        if !close {
            assert!(r.is_err(), "未闭合的内容模型应报错");
        }
    }
    // 适度嵌套（100 层）照常解析
    let s = format!(
        "<!DOCTYPE r [<!ELEMENT r {}a{}>]><r/>",
        "(".repeat(100),
        ")".repeat(100)
    );
    assert_eq!(parse_in_thread(s, 2 << 20), Ok(()));
}
