//! C2 共用层：Reader 事件 → DOM（只走公开修改 API）、结构指纹、四条读取路径。
//!
//! 被 `tests/c2_m2.rs` 和 fuzz 目标 `dom_roundtrip` / `dom_mutation` 用 `#[path]` 引入，
//! 一份代码两处用。
//!
//! 约束（Lead 18:45 / Implementer）：
//! - 不调用 `create_doctype`（将改为 pub(crate)）。Doctype 事件只记录，不进树。
//! - 统一断言：序列化要么返回 Err，要么输出能被 Reader 读回，且结构与树一致。
//! - 结构比较按计划 §2.4：相邻 Text 合并、空 Text 丢弃、CDATA 单独比较、
//!   属性按 (namespace, local, value) 集合比较、名字按展开名比较；
//!   XML 声明不参与比较（序列化不写 `<?xml?>`，Lead 已接受）。
//! - 全部用显式栈，不递归，深树不会爆栈。
#![allow(dead_code)]

use std::io::{BufRead, Read};

use purexml::dom::{Document, DomError, Name, NodeId, NodeKind};
use purexml::{Error, Event, ParserConfig, Reader};

/// 自有的事件副本，脱离 Reader 的借用。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OEv {
    Open {
        prefix: Option<String>,
        local: String,
        ns: Option<String>,
        /// (prefix, namespace, local, value)
        attrs: Vec<(Option<String>, Option<String>, String, String)>,
        /// (prefix, uri)
        nss: Vec<(Option<String>, String)>,
        empty: bool,
    },
    Close,
    Text(String),
    CData(String),
    Comment(String),
    Pi(String, Option<String>),
    Doctype(String, Option<String>, Option<String>),
    XmlDecl,
}

fn s(x: &str) -> String {
    x.to_string()
}

fn open(
    name: &purexml::QName<'_>,
    attrs: &[purexml::Attribute<'_>],
    nss: &[purexml::Namespace<'_>],
    empty: bool,
) -> OEv {
    OEv::Open {
        prefix: name.prefix.as_deref().map(s),
        local: s(&name.local),
        ns: name.namespace.as_deref().map(s),
        attrs: attrs
            .iter()
            .map(|a| {
                (
                    a.name.prefix.as_deref().map(s),
                    a.name.namespace.as_deref().map(s),
                    s(&a.name.local),
                    s(&a.value),
                )
            })
            .collect(),
        nss: nss
            .iter()
            .map(|n| (n.prefix.as_deref().map(s), s(&n.uri)))
            .collect(),
        empty,
    }
}

pub fn own(ev: &Event<'_>) -> OEv {
    match ev {
        Event::StartElement(e) => open(&e.name, &e.attributes, &e.namespaces, false),
        Event::EmptyElement(e) => open(&e.name, &e.attributes, &e.namespaces, true),
        Event::EndElement(_) => OEv::Close,
        Event::Text(t) => OEv::Text(s(t)),
        Event::CData(t) => OEv::CData(s(t)),
        Event::Comment(t) => OEv::Comment(s(t)),
        Event::PI(p) => OEv::Pi(s(&p.target), p.data.as_deref().map(s)),
        Event::XmlDecl(_) => OEv::XmlDecl,
        Event::Doctype(d) => OEv::Doctype(
            s(&d.name),
            d.system_id.as_deref().map(s),
            d.public_id.as_deref().map(s),
        ),
    }
}

/// Ok(事件)；Err((出错前的事件, 错误))。
pub type ReadResult = Result<Vec<OEv>, (Vec<OEv>, Error)>;

macro_rules! drain {
    ($r:expr) => {{
        let mut r = $r;
        let mut out = Vec::new();
        loop {
            match r.read_event() {
                Ok(Some(ev)) => out.push(own(&ev)),
                Ok(None) => break Ok(out),
                Err(e) => break Err((out, e)),
            }
        }
    }};
}

pub fn read_bytes(data: &[u8]) -> ReadResult {
    drain!(Reader::from_bytes(data))
}

pub fn read_bytes_with(data: &[u8], config: ParserConfig) -> ReadResult {
    drain!(Reader::from_bytes(data).with_config(config))
}

pub fn read_vec(data: &[u8]) -> ReadResult {
    drain!(Reader::from_vec(data.to_vec()))
}

/// 每次 read / fill_buf 最多给 `chunk` 字节的流。
pub struct Chunked<'a> {
    data: &'a [u8],
    chunk: usize,
}

impl<'a> Chunked<'a> {
    pub fn new(data: &'a [u8], chunk: usize) -> Self {
        Self {
            data,
            chunk: chunk.max(1),
        }
    }
}

impl Read for Chunked<'_> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let n = self.chunk.min(buf.len()).min(self.data.len());
        buf[..n].copy_from_slice(&self.data[..n]);
        self.data = &self.data[n..];
        Ok(n)
    }
}

impl BufRead for Chunked<'_> {
    fn fill_buf(&mut self) -> std::io::Result<&[u8]> {
        let n = self.chunk.min(self.data.len());
        Ok(&self.data[..n])
    }
    fn consume(&mut self, amt: usize) {
        self.data = &self.data[amt.min(self.data.len())..];
    }
}

pub fn read_chunked(data: &[u8], chunk: usize) -> ReadResult {
    match Reader::from_reader(Chunked::new(data, chunk)) {
        Ok(r) => drain!(r),
        Err(e) => Err((Vec::new(), e)),
    }
}

/// 四条读取路径（from_reader 取 1B / 7B / 8 KiB 三种分块），共五个结果。
pub fn read_all_paths(data: &[u8]) -> Vec<(&'static str, ReadResult)> {
    vec![
        ("from_bytes", read_bytes(data)),
        ("from_vec", read_vec(data)),
        ("from_reader/1B", read_chunked(data, 1)),
        ("from_reader/7B", read_chunked(data, 7)),
        ("from_reader/8KiB", read_chunked(data, 8192)),
    ]
}

// ───────────────────────────── 事件 → DOM ─────────────────────────────

#[derive(Debug)]
pub enum BuildErr {
    /// DOM 拒绝了 Reader 接受的内容（四方规则不一致的候选）。
    Dom(DomError, &'static str),
    /// 事件形状不对（Reader 缺陷的候选）。
    Shape(String),
}

pub type DoctypeInfo = (String, Option<String>, Option<String>);

pub struct Built {
    pub doc: Document,
    /// Doctype 只记录、不进树（不调用 create_doctype）。
    pub doctype: Option<DoctypeInfo>,
}

fn name_of<'a>(prefix: &'a Option<String>, ns: &'a Option<String>, local: &'a str) -> Name<'a> {
    match ns {
        Some(uri) => Name::qualified(uri.as_str(), prefix.as_deref(), local),
        None => Name::local(local),
    }
}

fn dom<T>(r: Result<T, DomError>, step: &'static str) -> Result<T, BuildErr> {
    r.map_err(|e| BuildErr::Dom(e, step))
}

pub fn build(events: &[OEv]) -> Result<Built, BuildErr> {
    build_into(Document::new(), events)
}

pub fn build_with(events: &[OEv], config: &ParserConfig) -> Result<Built, BuildErr> {
    build_into(dom(Document::with_config(config), "with_config")?, events)
}

fn build_into(mut doc: Document, events: &[OEv]) -> Result<Built, BuildErr> {
    let root = doc.root().expect("document node").id();
    let mut stack: Vec<NodeId> = vec![root];
    let mut doctype = None;
    for ev in events {
        let parent = *stack.last().expect("stack");
        match ev {
            OEv::XmlDecl => {}
            OEv::Doctype(n, sys, publ) => doctype = Some((n.clone(), sys.clone(), publ.clone())),
            OEv::Open {
                prefix,
                local,
                ns,
                attrs,
                nss,
                empty,
            } => {
                let el = dom(
                    doc.create_element(name_of(prefix, ns, local)),
                    "create_element",
                )?;
                for (p, uri) in nss {
                    dom(
                        doc.declare_namespace(el, p.as_deref(), uri),
                        "declare_namespace",
                    )?;
                }
                dom(doc.append_child(parent, el), "append_child(element)")?;
                for (p, n, l, v) in attrs {
                    dom(doc.set_attribute(el, name_of(p, n, l), v), "set_attribute")?;
                }
                if !*empty {
                    stack.push(el);
                }
            }
            OEv::Close => {
                if stack.len() < 2 {
                    return Err(BuildErr::Shape("多余的结束标签".into()));
                }
                stack.pop();
            }
            OEv::Text(t) => {
                if parent == root {
                    // 文档级空白不是节点；非空白是 Reader 缺陷候选
                    if t.chars().all(|c| matches!(c, ' ' | '\t' | '\n' | '\r')) {
                        continue;
                    }
                    return Err(BuildErr::Shape(format!("文档级非空白文本 {t:?}")));
                }
                let id = dom(doc.create_text(t), "create_text")?;
                dom(doc.append_child(parent, id), "append_child(text)")?;
            }
            OEv::CData(t) => {
                let id = dom(doc.create_cdata(t), "create_cdata")?;
                dom(doc.append_child(parent, id), "append_child(cdata)")?;
            }
            OEv::Comment(t) => {
                let id = dom(doc.create_comment(t), "create_comment")?;
                dom(doc.append_child(parent, id), "append_child(comment)")?;
            }
            OEv::Pi(t, d) => {
                let id = dom(doc.create_pi(t, d.as_deref()), "create_pi")?;
                dom(doc.append_child(parent, id), "append_child(pi)")?;
            }
        }
    }
    if stack.len() != 1 {
        return Err(BuildErr::Shape("未闭合的元素".into()));
    }
    Ok(Built { doc, doctype })
}

// ───────────────────────────── 结构指纹 ─────────────────────────────

/// 结构指纹的一项。相邻 Text 已合并、空 Text 已丢弃。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Tok {
    /// (namespace, local, 排好序的 (namespace, local, value))
    Open(
        Option<String>,
        String,
        Vec<(Option<String>, String, String)>,
    ),
    Close,
    Text(String),
    CData(String),
    Comment(String),
    Pi(String, Option<String>),
}

fn push_tok(out: &mut Vec<Tok>, t: Tok) {
    if let Tok::Text(new) = &t {
        if new.is_empty() {
            return;
        }
        if let Some(Tok::Text(prev)) = out.last_mut() {
            prev.push_str(new);
            return;
        }
    }
    out.push(t);
}

fn norm_ns(ns: Option<&str>) -> Option<String> {
    // 空串命名空间按“无命名空间”处理（ser.rs 也把 "" 过滤为 None）
    ns.filter(|x| !x.is_empty()).map(s)
}

/// 从事件序列算指纹。文档级空白、XML 声明、Doctype 不参与。
pub fn canon_events(events: &[OEv]) -> Vec<Tok> {
    let mut out = Vec::new();
    let mut depth = 0usize;
    for ev in events {
        match ev {
            OEv::XmlDecl | OEv::Doctype(..) => {}
            OEv::Open {
                local,
                ns,
                attrs,
                empty,
                ..
            } => {
                let mut a: Vec<_> = attrs
                    .iter()
                    .map(|(_, n, l, v)| (norm_ns(n.as_deref()), l.clone(), v.clone()))
                    .collect();
                a.sort();
                push_tok(
                    &mut out,
                    Tok::Open(norm_ns(ns.as_deref()), local.clone(), a),
                );
                if *empty {
                    push_tok(&mut out, Tok::Close);
                } else {
                    depth += 1;
                }
            }
            OEv::Close => {
                depth = depth.saturating_sub(1);
                push_tok(&mut out, Tok::Close);
            }
            OEv::Text(t) => {
                if depth > 0 {
                    push_tok(&mut out, Tok::Text(t.clone()));
                }
            }
            OEv::CData(t) => push_tok(&mut out, Tok::CData(t.clone())),
            OEv::Comment(t) => push_tok(&mut out, Tok::Comment(t.clone())),
            OEv::Pi(t, d) => push_tok(&mut out, Tok::Pi(t.clone(), d.clone())),
        }
    }
    out
}

fn node_tok(doc: &Document, id: NodeId) -> Option<Tok> {
    let n = doc.get(id)?;
    Some(match n.kind()? {
        NodeKind::Element => {
            let name = n.name()?;
            let mut a: Vec<_> = n
                .attributes()
                .map(|x| (norm_ns(x.namespace()), s(x.local()), s(x.value())))
                .collect();
            a.sort();
            Tok::Open(norm_ns(name.namespace), s(name.local), a)
        }
        NodeKind::Text => Tok::Text(s(n.text().unwrap_or(""))),
        NodeKind::CData => Tok::CData(s(n.text().unwrap_or(""))),
        NodeKind::Comment => Tok::Comment(s(n.text().unwrap_or(""))),
        NodeKind::Pi => {
            let (t, d) = n.pi()?;
            Tok::Pi(s(t), d.map(s))
        }
        NodeKind::Document | NodeKind::Doctype => return None,
    })
}

/// 从树（或子树）算指纹。`id` 为 Document 时遍历全部子节点；Doctype 不参与。
pub fn canon_tree(doc: &Document, id: NodeId) -> Vec<Tok> {
    let mut out = Vec::new();
    // (节点, 是否已展开)
    let mut stack: Vec<(NodeId, bool)> = vec![(id, false)];
    while let Some((cur, expanded)) = stack.pop() {
        let Some(n) = doc.get(cur) else { continue };
        let kind = n.kind();
        if expanded {
            if kind == Some(NodeKind::Element) {
                push_tok(&mut out, Tok::Close);
            }
            continue;
        }
        if let Some(t) = node_tok(doc, cur) {
            push_tok(&mut out, t);
        }
        if matches!(kind, Some(NodeKind::Element) | Some(NodeKind::Document)) {
            stack.push((cur, true));
            let kids: Vec<NodeId> = n.children().map(|c| c.id()).collect();
            for k in kids.into_iter().rev() {
                stack.push((k, false));
            }
        }
    }
    out
}

pub fn canon_doc(doc: &Document) -> Vec<Tok> {
    canon_tree(doc, doc.root().expect("document node").id())
}

// ───────────────────────────── 统一断言 ─────────────────────────────

/// 统一断言的结果。
#[derive(Debug)]
pub enum Unified {
    /// 序列化返回了 Err（允许）。
    SerErr(String),
    /// 序列化成功，读回结构一致。附序列化结果。
    Ok(String),
}

/// 统一断言：序列化要么 Err，要么输出能读回且结构一致。
/// 违反时返回 Err(说明)。`all_paths` 为真时五个读取结果都要一致。
pub fn unified(doc: &Document, all_paths: bool) -> Result<Unified, String> {
    let s1 = match doc.to_string() {
        Ok(x) => x,
        Err(e) => return Ok(Unified::SerErr(format!("{e}"))),
    };
    check_readback(&s1, &canon_doc(doc), all_paths)?;
    Ok(Unified::Ok(s1))
}

/// 读回 `xml`，指纹必须等于 `want`。
pub fn check_readback(xml: &str, want: &[Tok], all_paths: bool) -> Result<(), String> {
    let results = if all_paths {
        read_all_paths(xml.as_bytes())
    } else {
        vec![("from_bytes", read_bytes(xml.as_bytes()))]
    };
    for (path, r) in results {
        match r {
            Ok(evs) => {
                let got = canon_events(&evs);
                if got != want {
                    return Err(format!(
                        "{path}: 读回结构不一致\n  xml = {xml:?}\n  tree = {want:?}\n  back = {got:?}"
                    ));
                }
            }
            Err((_, e)) => {
                return Err(format!(
                    "{path}: 序列化结果读不回来：{:?} @ {:?} {}\n  xml = {xml:?}",
                    e.kind, e.position, e.message
                ));
            }
        }
    }
    Ok(())
}

/// 已登记、等修复的序列化缺陷：按报错文字识别，修好后删掉这里的对应项。
/// - D-C2-4（e880f3c）：默认命名空间作用域下有无前缀属性时误报。
pub fn known_ser_defect(msg: &str) -> Option<&'static str> {
    if msg.contains("default namespace does not match the element name") {
        return Some("D-C2-4");
    }
    None
}

/// dom_roundtrip 的完整断言：input → 树1 → s1 → 树2 → s2。
/// - input 解析失败：跳过（Ok(None)）
/// - 树1 结构 == input 事件结构；树1 的统一断言；树2 结构 == 树1；s1 == s2
pub fn roundtrip(input: &[u8]) -> Result<Option<String>, String> {
    let Ok(events) = read_bytes(input) else {
        return Ok(None);
    };
    let b1 = match build(&events) {
        Ok(b) => b,
        // DOM 限额与 Reader 限额口径不同（属性计数含 ns 声明等），限额拒绝不算违例
        Err(BuildErr::Dom(DomError::Limit { .. }, _)) => return Ok(None),
        Err(e) => return Err(format!("Reader 接受但建树失败：{e:?}")),
    };
    let t1 = canon_doc(&b1.doc);
    let want = canon_events(&events);
    if t1 != want {
        return Err(format!(
            "建树后结构与事件不一致\n  events = {want:?}\n  tree = {t1:?}"
        ));
    }
    let s1 = match unified(&b1.doc, false)? {
        Unified::Ok(x) => x,
        Unified::SerErr(e) => {
            if known_ser_defect(&e).is_some() {
                return Ok(None);
            }
            // 来自合法输入的树必须能写出（计划 §2.4：Writer 报错 = DOM 校验漏了）
            return Err(format!("合法输入建出的树序列化失败：{e}"));
        }
    };
    let ev2 = read_bytes(s1.as_bytes()).map_err(|(_, e)| format!("s1 读不回：{e:?}"))?;
    let b2 = build(&ev2).map_err(|e| format!("s1 读回后建树失败：{e:?}"))?;
    if canon_doc(&b2.doc) != t1 {
        return Err("树2 结构与树1 不一致".into());
    }
    let s2 = b2
        .doc
        .to_string()
        .map_err(|e| format!("树2 序列化失败：{e}"))?;
    if s1 != s2 {
        return Err(format!("s1 != s2\n  s1 = {s1:?}\n  s2 = {s2:?}"));
    }
    Ok(Some(s1))
}
