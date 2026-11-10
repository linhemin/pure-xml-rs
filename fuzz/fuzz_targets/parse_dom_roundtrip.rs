//! fuzz 目标 parse_dom_roundtrip（C3 草案，计划 v10.9 §C3）。
//!
//! **v10.9：parse_dom 已按 C3 设计签名接到 `Document::from_bytes`，main efb8be1 上无法编译**
//! （C3-design.md §1）。四路径差分另走 `Document::from_reader`（1 B / 7 B / 8 KiB 分块）。
//!
//! 流程：input → C3 公开解析→DOM 入口建树1 → to_string 得 s1 → 再解析建树2 → s2。
//! 断言（与 dom_roundtrip 同口径，外加四路径差分）：
//! - 四路径差分：Reader 的 from_bytes / from_vec / from_reader(1B、7B、8 KiB) 和 C3 解析→DOM
//!   对同一输入的接受/拒绝结论一致；都接受时，树1 结构 == 每条 Reader 路径的事件结构
//! - Reader 接受而 C3 拒绝（或反之）算违例，限额拒绝除外（TODO(C3)：限额口径统一后收紧）
//! - 树1 的统一断言：序列化要么 Err，要么读回结构一致；来自合法输入的树必须能写出
//! - 树2 结构 == 树1；s1 == s2（逐字节）
//! - 无根元素：to_string 与文档节点 NodeRef::write_to 都精确返回 WriteError::NoRootElement
//! - DOM 四路径：Document::from_bytes 与 Document::from_reader(1B/7B/8KiB) 的结论、结构指纹、
//!   doctype、approx_heap_bytes 全部相同；失败时整个 purexml::Error 相同
//! - doctype 进树：与 Reader 的 Doctype 事件 name/system/public 相同；s1 里只有 `<!DOCTYPE name>`（Lead Q6）
//!
//! 运行：cargo +nightly fuzz run -O parse_dom_roundtrip -- -max_total_time=60 -timeout=60 -rss_limit_mb=2048
#![no_main]

#[path = "../../tests/common/dom_c2.rs"]
mod dom_c2;

use libfuzzer_sys::fuzz_target;
use purexml::dom::{Document, NodeKind};
use purexml::{ErrorKind, WriteError};

/// C3 解析→DOM 的结果。
enum Parsed {
    Ok(Document),
    /// 输入不合法（与 Reader 的拒绝对应）
    Rejected(String),
    /// 限额拒绝（DOM 与 Reader 限额口径不同时不算违例）
    Limit,
}

fn classify(r: Result<Document, purexml::Error>) -> Parsed {
    match r {
        Ok(doc) => Parsed::Ok(doc),
        // DOM 限额（NodeCount/DomBytes）Reader 不知道，不算违例
        Err(e) if matches!(e.kind, ErrorKind::SecurityLimit { .. }) => Parsed::Limit,
        Err(e) => Parsed::Rejected(format!("{e:?}")),
    }
}

/// C3 公开解析→DOM 入口（需要 C3）。
fn parse_dom(data: &[u8]) -> Parsed {
    classify(Document::from_bytes(data))
}

type Dt = (String, Option<String>, Option<String>);

fn doctype_of(doc: &Document) -> Option<Dt> {
    let mut c = doc.root()?.first_child();
    while let Some(n) = c {
        if n.kind() == Some(NodeKind::Doctype) {
            let d = n.doctype()?;
            return Some((
                d.name.to_string(),
                d.system_id.map(str::to_string),
                d.public_id.map(str::to_string),
            ));
        }
        c = n.next_sibling();
    }
    None
}

/// DOM 自身的四路径：from_bytes 与 from_reader(1B/7B/8KiB) 完全一致。
fn dom_paths(data: &[u8]) -> Result<(), String> {
    let base = Document::from_bytes(data);
    for chunk in [1usize, 7, 8192] {
        let other = Document::from_reader(dom_c2::Chunked::new(data, chunk));
        match (&base, &other) {
            (Ok(a), Ok(b)) => {
                if dom_c2::canon_doc(a) != dom_c2::canon_doc(b)
                    || doctype_of(a) != doctype_of(b)
                    || a.approx_heap_bytes() != b.approx_heap_bytes()
                {
                    return Err(format!("DOM from_bytes 与 from_reader/{chunk}B 结果不同"));
                }
            }
            (Err(a), Err(b)) if a == b => {}
            (a, b) => {
                return Err(format!(
                    "DOM from_bytes {:?} 与 from_reader/{chunk}B {:?} 结论不同",
                    a.as_ref().err(),
                    b.as_ref().err()
                ));
            }
        }
    }
    Ok(())
}

/// 四路径差分：每条 Reader 路径的结论都要与 C3 一致。
fn differential(data: &[u8], parsed: &Parsed) -> Result<(), String> {
    for (path, r) in dom_c2::read_all_paths(data) {
        match (parsed, r) {
            (Parsed::Ok(doc), Ok(evs)) => {
                let want = dom_c2::canon_events(&evs);
                if dom_c2::canon_doc(doc) != want {
                    return Err(format!("{path}：C3 树结构与 Reader 事件不一致"));
                }
                let rd = evs.iter().find_map(|e| match e {
                    dom_c2::OEv::Doctype(n, s, p) => Some((n.clone(), s.clone(), p.clone())),
                    _ => None,
                });
                if doctype_of(doc) != rd {
                    return Err(format!("{path}：doctype 与 Reader 不一致"));
                }
            }
            (Parsed::Rejected(_), Err(_)) | (Parsed::Limit, _) => {}
            (Parsed::Ok(_), Err((_, e))) => {
                return Err(format!("C3 接受，但 {path} 拒绝：{e:?}"));
            }
            (Parsed::Rejected(c3), Ok(_)) => {
                return Err(format!("{path} 接受，但 C3 拒绝：{c3}"));
            }
        }
    }
    Ok(())
}

fn run(data: &[u8]) -> Result<(), String> {
    let parsed = parse_dom(data);
    differential(data, &parsed)?;
    dom_paths(data)?;
    let Parsed::Ok(doc1) = parsed else {
        return Ok(());
    };
    // 无根元素：两条序列化入口都精确报 NoRootElement
    if doc1.root_element().is_none() {
        if !matches!(doc1.to_string(), Err(WriteError::NoRootElement)) {
            return Err("无根元素但 to_string 未报 NoRootElement".into());
        }
        return dom_c2::check_doc_node_write(&doc1, &Err(WriteError::NoRootElement));
    }
    let s1 = match dom_c2::unified(&doc1, false)? {
        dom_c2::Unified::Ok(x) => x,
        dom_c2::Unified::SerErr(e) => return Err(format!("合法输入建出的树序列化失败：{e}")),
    };
    dom_c2::check_doc_node_write(&doc1, &Ok(s1.clone()))?;
    // Lead Q6：默认序列化不写外部标识
    if let Some((name, _, _)) = doctype_of(&doc1)
        && !s1.contains(&format!("<!DOCTYPE {name}>"))
    {
        return Err(format!("doctype 默认应只写 <!DOCTYPE {name}>：{s1:?}"));
    }
    let Parsed::Ok(doc2) = parse_dom(s1.as_bytes()) else {
        return Err(format!("s1 解析不回：{s1:?}"));
    };
    if dom_c2::canon_doc(&doc2) != dom_c2::canon_doc(&doc1) {
        return Err("树2 结构与树1 不一致".into());
    }
    let s2 = doc2
        .to_string()
        .map_err(|e| format!("树2 序列化失败：{e}"))?;
    if s1 != s2 {
        return Err(format!("s1 != s2\n  s1 = {s1:?}\n  s2 = {s2:?}"));
    }
    Ok(())
}

fuzz_target!(|data: &[u8]| {
    if data.len() > 256 << 10 {
        return;
    }
    if let Err(msg) = run(data) {
        panic!("parse_dom_roundtrip 违例：{msg}");
    }
});
