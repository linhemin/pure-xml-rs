//! fuzz 目标 dom_mutation（计划 §6.1）：字节流驱动 DOM 修改操作。
//!
//! 输入布局：`[L 高字节][L 低字节][种子 XML，L 字节][操作流...]`。
//! 种子能解析并建树就从它开始，否则从 `<r/>` 开始。
//!
//! 操作：建元素（含带前缀/命名空间）、文本、CDATA、注释、PI；append / insert_before /
//! insert_after / detach / remove；set_attribute / remove_attribute；declare_namespace /
//! remove_namespace；set_text / set_pi_data；cmp_document_order。
//! 参数来自固定的“刁钻字符串池”和字节流里的原始字节（可能含非法字符）。
//!
//! 断言：
//! - 任何操作都只能返回 Ok 或 Err，不能 panic
//! - 每一步之后统一断言（Lead 18:45）：to_string 要么 Err，要么 Reader 读回无错且结构与树一致
//! - 失败的操作不能改树（结构指纹不变）
//! - Q-C2-1（Lead 19:25）：没有根元素时 to_string 必须返回 `WriteError::NoRootElement`
//! - O-C2-6（Lead 19:55）：文档节点的 NodeRef::write_to 与 to_string 一致（无根时同样报 NoRootElement、不写字节；
//!   有根时输出逐字节相同）。7c4b750 已修，dom_c2::known_doc_node_defect 自 v10.8 起返回 None
//! - D-C2-1（Lead 19:25）：往 CDATA/注释/PI 写入含 CR 的内容必须在修改阶段失败
//!   （create_cdata/create_comment/set_text/set_pi_data 返回 InvalidChar；create_pi 可能先报 InvalidName），
//!   且任何时刻树里（含未挂接节点）的 CDATA/注释/PI 都不含 CR
//!
//! 已知问题过滤：无。Q-C2-1、D-C2-1 的屏蔽在 v10.7 撤掉，O-C2-6 的登记在 v10.8 撤掉。
//!
//! 不调用 create_doctype。
//!
//! 运行：cargo +nightly fuzz run dom_mutation -- -max_total_time=60 -timeout=60 -rss_limit_mb=2048
#![no_main]

#[path = "../../tests/common/dom_c2.rs"]
mod dom_c2;

use libfuzzer_sys::fuzz_target;
use purexml::WriteError;
use purexml::dom::{Document, Name, NodeId, NodeKind};

const MAX_OPS: usize = 96;

const STRS: &[&str] = &[
    "",
    "a",
    "x y",
    "]]>",
    "]]",
    "--",
    "-",
    "a-",
    "?>",
    "<",
    "&",
    "\"",
    "'",
    ">",
    "\t",
    "\n",
    "\r",
    "\r\n",
    "&amp;",
    "&#0;",
    "\u{0}",
    "\u{1}",
    "\u{FFFE}",
    "\u{FFFF}",
    "\u{85}",
    "\u{2028}",
    "\u{10FFFF}",
    " lead",
    "trail ",
    "<!--",
    "<![CDATA[",
    "xmlns",
    "é",
];
const NAMES: &[&str] = &[
    "r",
    "a",
    "b",
    "x",
    "xml",
    "XML",
    "xmlns",
    "xml-stylesheet",
    "p:q",
    "1a",
    "",
    "a b",
    "é",
    "_",
    "a.b-c",
    "\u{0}",
];
const PREFIXES: &[&str] = &["p", "q", "xml", "xmlns", "", "1", "a:b"];
const URIS: &[&str] = &[
    "urn:a",
    "urn:b",
    "",
    "http://www.w3.org/XML/1998/namespace",
    "http://www.w3.org/2000/xmlns/",
    "urn:a<&\"",
];

struct Bytes<'a> {
    d: &'a [u8],
}

impl Bytes<'_> {
    fn u8(&mut self) -> Option<u8> {
        let (&b, rest) = self.d.split_first()?;
        self.d = rest;
        Some(b)
    }
    fn pick<'s>(&mut self, pool: &[&'s str]) -> Option<&'s str> {
        Some(pool[self.u8()? as usize % pool.len()])
    }
    /// 池里的字符串，或者字节流里的原始字节（有损转成 UTF-8）。
    fn text(&mut self) -> Option<String> {
        let b = self.u8()?;
        if b & 0x80 == 0 {
            Some(self.pick(STRS)?.to_string())
        } else {
            let n = (b & 0x1f) as usize;
            let n = n.min(self.d.len());
            let (raw, rest) = self.d.split_at(n);
            self.d = rest;
            Some(String::from_utf8_lossy(raw).into_owned())
        }
    }
    fn opt_text(&mut self) -> Option<Option<String>> {
        if self.u8()? & 1 == 0 {
            Some(None)
        } else {
            Some(Some(self.text()?))
        }
    }
    fn opt_prefix(&mut self) -> Option<Option<&'static str>> {
        let b = self.u8()?;
        if b & 1 == 0 {
            Some(None)
        } else {
            Some(Some(PREFIXES[(b >> 1) as usize % PREFIXES.len()]))
        }
    }
    fn node(&mut self, ids: &[NodeId]) -> Option<NodeId> {
        Some(ids[self.u8()? as usize % ids.len()])
    }
}

fn has_root_element(doc: &Document) -> bool {
    doc.root_element().is_some()
}

/// D-C2-1 不变量：已知节点（含未挂接的）里，CDATA/注释/PI 不能带 CR。
fn assert_no_cr_in_raw_nodes(doc: &Document, ids: &[NodeId], step: usize, op: u8) {
    for &id in ids {
        let Some(n) = doc.get(id) else { continue };
        let bad = match n.kind() {
            Some(NodeKind::CData) | Some(NodeKind::Comment) => {
                n.text().is_some_and(|t| t.contains('\r'))
            }
            Some(NodeKind::Pi) => n
                .pi()
                .and_then(|(_, d)| d)
                .is_some_and(|d| d.contains('\r')),
            _ => false,
        };
        assert!(
            !bad,
            "第 {step} 步 op {op} 后 {:?} 节点里有 CR（D-C2-1）",
            n.kind()
        );
    }
}

/// D-C2-1：带 CR 的写入必须失败，且（除 create_pi 的名字先错外）错误是 InvalidChar。
fn expect_cr_rejected(res: &Result<(), String>, allow_name: bool, what: &str) {
    match res {
        Ok(()) => panic!("{what}：内容含 CR，却返回 Ok（D-C2-1）"),
        Err(e) => assert!(
            e.starts_with("InvalidChar") || (allow_name && e.starts_with("InvalidName")),
            "{what}：内容含 CR，期望 InvalidChar，实际 {e}"
        ),
    }
}

fn is_kind(doc: &Document, n: NodeId, kinds: &[NodeKind]) -> bool {
    doc.get(n)
        .and_then(|x| x.kind())
        .is_some_and(|k| kinds.contains(&k))
}

fn start_doc(seed: &[u8]) -> Document {
    if let Ok(evs) = dom_c2::read_bytes(seed)
        && let Ok(b) = dom_c2::build(&evs)
    {
        return b.doc;
    }
    let mut doc = Document::new();
    let root = doc.root().expect("document node").id();
    let r = doc.create_element(Name::local("r")).expect("r");
    doc.append_child(root, r).expect("append r");
    doc
}

fn all_ids(doc: &Document) -> Vec<NodeId> {
    let mut out = Vec::new();
    let Some(root) = doc.root() else { return out };
    let mut stack = vec![root.id()];
    while let Some(id) = stack.pop() {
        out.push(id);
        if let Some(n) = doc.get(id) {
            stack.extend(n.children().map(|c| c.id()));
        }
    }
    out
}

fn check(doc: &Document, ids: &[NodeId], step: usize, op: u8) {
    assert_no_cr_in_raw_nodes(doc, ids, step, op);
    if !has_root_element(doc) {
        // Q-C2-1：必须报 NoRootElement，不能 Ok 加不良构输出
        match doc.to_string() {
            Err(WriteError::NoRootElement) => {}
            other => {
                panic!("第 {step} 步 op {op} 后没有根元素，期望 Err(NoRootElement)，实际 {other:?}")
            }
        }
        // O-C2-6（Lead 19:55）：文档节点的 NodeRef::write_to 也必须报 NoRootElement
        if let Err(msg) = dom_c2::check_doc_node_write(doc, &Err(WriteError::NoRootElement)) {
            panic!("第 {step} 步 op {op}：{msg}");
        }
        return;
    }
    if let Err(msg) = dom_c2::unified(doc, false) {
        panic!("dom_mutation 第 {step} 步（op {op}）后统一断言失败：{msg}");
    }
}

fn run(data: &[u8]) -> Option<()> {
    if data.len() < 2 {
        return None;
    }
    let l = u16::from_be_bytes([data[0], data[1]]) as usize % 4096;
    let l = l.min(data.len() - 2);
    let seed = &data[2..2 + l];
    let mut b = Bytes { d: &data[2 + l..] };
    let mut doc = start_doc(seed);
    let mut ids = all_ids(&doc);
    check(&doc, &ids, 0, 255);
    for step in 1..=MAX_OPS {
        let op = b.u8()?;
        let before = dom_c2::canon_doc(&doc);
        let res: Result<(), String> = match op % 18 {
            0 => doc
                .create_element(Name::local(b.pick(NAMES)?))
                .map(|id| ids.push(id))
                .map_err(|e| format!("{e:?}")),
            1 => {
                let uri = b.pick(URIS)?;
                let p = b.opt_prefix()?;
                let l = b.pick(NAMES)?;
                doc.create_element(Name::qualified(uri, p, l))
                    .map(|id| ids.push(id))
                    .map_err(|e| format!("{e:?}"))
            }
            2 => {
                let t = b.text()?;
                doc.create_text(&t)
                    .map(|id| ids.push(id))
                    .map_err(|e| format!("{e:?}"))
            }
            3 => {
                let t = b.text()?;
                let r = doc
                    .create_cdata(&t)
                    .map(|id| ids.push(id))
                    .map_err(|e| format!("{e:?}"));
                if t.contains('\r') {
                    expect_cr_rejected(&r, false, "create_cdata");
                }
                r
            }
            4 => {
                let t = b.text()?;
                let r = doc
                    .create_comment(&t)
                    .map(|id| ids.push(id))
                    .map_err(|e| format!("{e:?}"));
                if t.contains('\r') {
                    expect_cr_rejected(&r, false, "create_comment");
                }
                r
            }
            5 => {
                let t = b.pick(NAMES)?;
                let d = b.opt_text()?;
                let r = doc
                    .create_pi(t, d.as_deref())
                    .map(|id| ids.push(id))
                    .map_err(|e| format!("{e:?}"));
                if d.as_deref().is_some_and(|d| d.contains('\r')) {
                    expect_cr_rejected(&r, true, "create_pi");
                }
                r
            }
            6 => {
                let (p, c) = (b.node(&ids)?, b.node(&ids)?);
                doc.append_child(p, c).map_err(|e| format!("{e:?}"))
            }
            7 => {
                let (c, r) = (b.node(&ids)?, b.node(&ids)?);
                doc.insert_before(c, r).map_err(|e| format!("{e:?}"))
            }
            8 => {
                let (c, r) = (b.node(&ids)?, b.node(&ids)?);
                doc.insert_after(c, r).map_err(|e| format!("{e:?}"))
            }
            9 => {
                let n = b.node(&ids)?;
                doc.detach(n).map_err(|e| format!("{e:?}"))
            }
            10 => {
                let n = b.node(&ids)?;
                doc.remove(n).map_err(|e| format!("{e:?}"))
            }
            11 => {
                let n = b.node(&ids)?;
                let sel = b.u8()?;
                let l = b.pick(NAMES)?;
                let v = b.text()?;
                let name = if sel & 1 == 0 {
                    Name::local(l)
                } else {
                    Name::qualified(URIS[(sel >> 1) as usize % URIS.len()], b.opt_prefix()?, l)
                };
                doc.set_attribute(n, name, &v).map_err(|e| format!("{e:?}"))
            }
            12 => {
                let n = b.node(&ids)?;
                let sel = b.u8()?;
                let l = b.pick(NAMES)?;
                let name = if sel & 1 == 0 {
                    Name::local(l)
                } else {
                    Name::qualified(URIS[(sel >> 1) as usize % URIS.len()], b.opt_prefix()?, l)
                };
                doc.remove_attribute(n, name).map_err(|e| format!("{e:?}"))
            }
            13 => {
                let n = b.node(&ids)?;
                let p = b.opt_prefix()?;
                let u = b.pick(URIS)?;
                doc.declare_namespace(n, p, u).map_err(|e| format!("{e:?}"))
            }
            14 => {
                let n = b.node(&ids)?;
                let p = b.opt_prefix()?;
                doc.remove_namespace(n, p).map_err(|e| format!("{e:?}"))
            }
            15 => {
                let n = b.node(&ids)?;
                let t = b.text()?;
                let raw = is_kind(&doc, n, &[NodeKind::CData, NodeKind::Comment]);
                let r = doc.set_text(n, &t).map_err(|e| format!("{e:?}"));
                if raw && t.contains('\r') {
                    expect_cr_rejected(&r, false, "CDATA/注释 set_text");
                }
                r
            }
            16 => {
                let n = b.node(&ids)?;
                let d = b.opt_text()?;
                let pi = is_kind(&doc, n, &[NodeKind::Pi]);
                let r = doc
                    .set_pi_data(n, d.as_deref())
                    .map_err(|e| format!("{e:?}"));
                if pi && d.as_deref().is_some_and(|d| d.contains('\r')) {
                    expect_cr_rejected(&r, false, "set_pi_data");
                }
                r
            }
            _ => {
                let (x, y) = (b.node(&ids)?, b.node(&ids)?);
                let r1 = doc.cmp_document_order(x, y);
                let r2 = doc.cmp_document_order(y, x);
                if let (Ok(a), Ok(c)) = (&r1, &r2) {
                    assert_eq!(*a, c.reverse(), "cmp_document_order 不对称");
                }
                Ok(())
            }
        };
        if res.is_err() {
            assert_eq!(
                dom_c2::canon_doc(&doc),
                before,
                "第 {step} 步 op {op} 返回 {res:?}，但树被改了"
            );
        }
        check(&doc, &ids, step, op);
    }
    Some(())
}

fuzz_target!(|data: &[u8]| {
    let _ = run(data);
});
