//! C3 合并门槛（Lead 2026-11-10 15:35 东京时间 C3 设计裁定“额外要求”；计划 v10.9 §C3）。
//!
//! **v10.9 状态：按 C3 设计签名起草，当前 main efb8be1 上无法编译**——依赖
//! `Document::{parse, from_bytes, from_reader, parse_with_config, from_bytes_with_config,
//! from_reader_with_config}`（C3-design.md §1）和 `LimitKind::{NodeCount, DomBytes}`（§2 / Q1）。
//!
//! 1. `attacks_four_path_parse_matches_reader`：tests/data/attacks/*.xml 每个样本经
//!    Document 的四条路径（parse/from_bytes、BufReader 8 KiB、分块 1 B、分块 7 B）解析，
//!    与对应的 Reader 路径结论完全一致：都成功时结构指纹（canon_doc == canon_events）和
//!    doctype（name/system/public）相同；都失败时整个 `purexml::Error` 相同（kind 含
//!    limit/actual、position、message——设计 §2 要求“原样返回”）。
//! 2. `max_nodes_boundary_differential` / `max_dom_bytes_boundary_differential`（v10.9.1，
//!    Lead 11-10 16:50 裁定 3）：“实际用量”从建好的树上量出（节点数靠遍历，DOM 字节靠
//!    approx_heap_bytes 计量口径），不用实现的 reserve 预估。三档：恰等于 → 五条路径都成功；
//!    少 1 → 五条路径都报 SecurityLimit{NodeCount|DomBytes}（limit=实际−1，actual>limit），
//!    kind/position 五路一致；+1 → 都成功。另对每条路径二分“最小可成功限值”，必须 = 实际用量。
//!    **89f6e78 上 max_dom_bytes 预期失败**（long-uri：预留 > 实际，恰等于被拒）。
//! 3. `specified_default_attrs_all_paths`：ATTLIST 默认属性 specified=false（T-Q11 / Q4），五路一致。
//!
//! 运行：cargo test --test c3_gate（debug 即可，不计时）
#![allow(dead_code)]

#[path = "common/dom_c2.rs"]
mod dom_c2;

use std::io::BufReader;
use std::path::Path;

use dom_c2::{Chunked, OEv, canon_doc, canon_events};
use purexml::dom::{Document, NodeKind, NodeRef};
use purexml::{Error, ErrorKind, LimitKind, ParserConfig};

type DomResult = Result<Document, Error>;
type Dt = (String, Option<String>, Option<String>);

// ───────────────────────────── 路径 ─────────────────────────────

/// 默认配置的 C3 入口（不带 _with_config）：(路径名, 对应的 dom_c2::read_all_paths 路径名, 结果)。
fn dom_paths_default(data: &[u8]) -> Vec<(&'static str, &'static str, DomResult)> {
    let mut v = Vec::new();
    // parse(&str) 只对 UTF-8 输入适用；非 UTF-8（S10*）只走字节/reader 路径
    if let Ok(s) = std::str::from_utf8(data) {
        v.push(("parse", "from_bytes", Document::parse(s)));
    }
    v.push(("from_bytes", "from_bytes", Document::from_bytes(data)));
    v.push((
        "from_reader(BufReader 8KiB)",
        "from_reader/8KiB",
        Document::from_reader(BufReader::with_capacity(8192, data)),
    ));
    v.push((
        "from_reader(1B)",
        "from_reader/1B",
        Document::from_reader(Chunked::new(data, 1)),
    ));
    v.push((
        "from_reader(7B)",
        "from_reader/7B",
        Document::from_reader(Chunked::new(data, 7)),
    ));
    v
}

/// 带配置的 C3 入口，五条路径。
fn dom_paths_cfg(data: &[u8], cfg: &ParserConfig) -> Vec<(&'static str, DomResult)> {
    let mut v = Vec::new();
    if let Ok(s) = std::str::from_utf8(data) {
        v.push(("parse_with_config", Document::parse_with_config(s, cfg)));
    }
    v.push((
        "from_bytes_with_config",
        Document::from_bytes_with_config(data, cfg),
    ));
    v.push((
        "from_reader_with_config/8KiB",
        Document::from_reader_with_config(BufReader::with_capacity(8192, data), cfg),
    ));
    v.push((
        "from_reader_with_config/1B",
        Document::from_reader_with_config(Chunked::new(data, 1), cfg),
    ));
    v.push((
        "from_reader_with_config/7B",
        Document::from_reader_with_config(Chunked::new(data, 7), cfg),
    ));
    v
}

// ───────────────────────────── 工具 ─────────────────────────────

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

fn reader_doctype(evs: &[OEv]) -> Option<Dt> {
    evs.iter().find_map(|e| match e {
        OEv::Doctype(n, s, p) => Some((n.clone(), s.clone(), p.clone())),
        _ => None,
    })
}

/// 文档节点 + 全部后代（显式栈）。解析建出的树没有墓碑，等于 max_nodes 的计数口径。
fn count_nodes(doc: &Document) -> u64 {
    let mut n = 0u64;
    let mut stack: Vec<NodeRef<'_>> = vec![doc.root().expect("document node")];
    while let Some(x) = stack.pop() {
        n += 1;
        stack.extend(x.children());
    }
    n
}

fn files(dir: &Path) -> Vec<std::path::PathBuf> {
    let mut v: Vec<_> = std::fs::read_dir(dir)
        .unwrap()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "xml"))
        .collect();
    v.sort();
    v
}

// ───────────────────────────── 1. attacks 四路径 ─────────────────────────────

fn check_against_reader(id: &str, data: &[u8]) -> Vec<String> {
    let mut fails = Vec::new();
    let reader = dom_c2::read_all_paths(data);
    for (path, key, dom) in dom_paths_default(data) {
        let (_, oracle) = reader
            .iter()
            .find(|(p, _)| *p == key)
            .expect("reader 路径名对应");
        match (oracle, &dom) {
            (Ok(evs), Ok(doc)) => {
                if canon_doc(doc) != canon_events(evs) {
                    fails.push(format!("{id} {path}：DOM 结构 != Reader 事件"));
                }
                if doctype_of(doc) != reader_doctype(evs) {
                    fails.push(format!(
                        "{id} {path}：doctype {:?} != Reader {:?}",
                        doctype_of(doc),
                        reader_doctype(evs)
                    ));
                }
            }
            (Err((_, re)), Err(de)) => {
                if re != de {
                    fails.push(format!(
                        "{id} {path}：错误不一致\n  Reader {re:?}\n  DOM    {de:?}"
                    ));
                }
            }
            (Ok(_), Err(de)) => fails.push(format!("{id} {path}：Reader 接受，DOM 拒绝 {de:?}")),
            (Err((_, re)), Ok(_)) => {
                fails.push(format!("{id} {path}：Reader 拒绝 {re:?}，DOM 接受"))
            }
        }
    }
    fails
}

#[test]
fn attacks_four_path_parse_matches_reader() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data/attacks");
    let fs = files(&dir);
    // main efb8be1 / 89f6e78 上为 85 个；防止目录被误删后静默通过
    assert!(fs.len() >= 80, "attacks 样本只有 {} 个", fs.len());
    let mut fails = Vec::new();
    for p in &fs {
        let id = p.file_name().unwrap().to_string_lossy().to_string();
        fails.extend(check_against_reader(&id, &std::fs::read(p).unwrap()));
    }
    assert!(
        fails.is_empty(),
        "{} 处不一致：\n{}",
        fails.len(),
        fails.join("\n")
    );
}

// ───────────────────────────── 2. 限额边界 ─────────────────────────────

fn long_uri_doc() -> String {
    let mut s = format!("<r xmlns:p=\"{}\">", "u".repeat(64 << 10));
    for _ in 0..16 {
        s.push_str("<p:e p:a=\"\"/>");
    }
    s + "</r>"
}

/// 覆盖各种节点、命名空间、doctype、实体展开、默认属性、长 URI（共享 span）、多兄弟。
fn limit_corpus() -> Vec<(&'static str, String)> {
    vec![
        ("simple", "<r/>".into()),
        (
            "mixed",
            r#"<r a="1"><!--c--><?p d?>t<![CDATA[x]]><e>y</e></r>"#.into(),
        ),
        (
            "prolog",
            "<?xml version=\"1.0\"?><!--a--><?p?>\n<r/>\n<!--z-->".into(),
        ),
        (
            "ns",
            r#"<r xmlns="urn:a" xmlns:p="urn:b"><p:e p:x="1"/><e xmlns=""/></r>"#.into(),
        ),
        ("doctype", r#"<!DOCTYPE r SYSTEM "r.dtd"><r/>"#.into()),
        (
            "entity",
            r#"<!DOCTYPE r [<!ENTITY e "a<b/>c">]><r>&e;&e;</r>"#.into(),
        ),
        (
            "default-attr",
            r#"<!DOCTYPE r [<!ATTLIST r d CDATA "v" xmlns:q CDATA "urn:q">]><r x="1"/>"#.into(),
        ),
        ("long-uri", long_uri_doc()),
        ("siblings", format!("<r>{}</r>", "<e/>".repeat(1000))),
    ]
}

fn expect_limit(e: &Error, want: LimitKind, limit: u64) -> Result<u64, String> {
    match &e.kind {
        ErrorKind::SecurityLimit {
            kind,
            limit: l,
            actual,
        } if *kind == want && *l == limit => {
            if *actual > *l {
                Ok(*actual)
            } else {
                Err(format!("actual {actual} 不大于 limit {l}"))
            }
        }
        k => Err(format!(
            "期望 SecurityLimit{{{want:?}, limit={limit}}}，实际 {k:?}"
        )),
    }
}

type SetLimit = fn(ParserConfig, u64) -> ParserConfig;

/// 单条路径在限值 `lim` 下是否成功（路径按名字取，五条路径各自独立二分）。
fn path_ok(data: &[u8], set: SetLimit, lim: u64, path: &str) -> bool {
    dom_paths_cfg(data, &set(ParserConfig::default(), lim))
        .into_iter()
        .find(|(p, _)| *p == path)
        .map(|(_, r)| r.is_ok())
        .expect("路径存在")
}

/// 二分：该路径能成功的最小限值（在 [0, hi] 内；hi 必须能成功）。
/// 只依赖“成功/失败”这一外部可观察结果，不读实现的 reserve 预估。
fn bisect_min_ok(data: &[u8], set: SetLimit, hi: u64, path: &str) -> Option<u64> {
    if !path_ok(data, set, hi, path) {
        return None;
    }
    let (mut lo, mut hi) = (0u64, hi); // 不变式：hi 成功；lo 之下未知/失败
    if path_ok(data, set, 0, path) {
        return Some(0);
    }
    // lo=0 失败
    while hi - lo > 1 {
        let mid = lo + (hi - lo) / 2;
        if path_ok(data, set, mid, path) {
            hi = mid;
        } else {
            lo = mid;
        }
    }
    Some(hi)
}

/// 三档边界（Lead 11-10 16:50 裁定 3）：`need` = 实际用量（由调用方从建好的树测出）。
/// - 限值 = need     → 五条路径都成功，结构相同；
/// - 限值 = need − 1 → 五条路径都报 SecurityLimit{kind, limit=need−1, actual>limit}，kind/position 五路一致；
/// - 限值 = need + 1 → 五条路径都成功，结构相同；
/// - 另：每条路径二分出“能成功的最小限值”，必须恰好 = need（抓“预留 > 实际”导致的提前拒绝）。
fn boundary(
    id: &str,
    data: &[u8],
    need: u64,
    kind: LimitKind,
    set: SetLimit,
    exact_actual: bool,
) -> Vec<String> {
    let mut fails = Vec::new();
    let base = Document::from_bytes(data).expect("语料须合法");
    let want = canon_doc(&base);
    // 恰等于 / +1
    for lim in [need, need + 1] {
        let tag = if lim == need { "恰等于" } else { "+1" };
        for (p, r) in dom_paths_cfg(data, &set(ParserConfig::default(), lim)) {
            match r {
                Ok(doc) => {
                    if canon_doc(&doc) != want {
                        fails.push(format!("{id} {kind:?}={lim}（{tag}）{p}：结构不同"));
                    }
                    if doc.approx_heap_bytes() != base.approx_heap_bytes() {
                        fails.push(format!(
                            "{id} {kind:?}={lim}（{tag}）{p}：approx_heap_bytes {} != {}",
                            doc.approx_heap_bytes(),
                            base.approx_heap_bytes()
                        ));
                    }
                }
                Err(e) => fails.push(format!(
                    "{id} {kind:?}={lim}（{tag}，实际用量 {need}）{p} 被拒绝：{e:?}"
                )),
            }
        }
    }
    // 少 1
    if need == 0 {
        fails.push(format!("{id} {kind:?}：实际用量为 0，语料无效"));
        return fails;
    }
    let lim = need - 1;
    let mut first: Option<(String, Error)> = None;
    for (p, r) in dom_paths_cfg(data, &set(ParserConfig::default(), lim)) {
        match r {
            Ok(_) => fails.push(format!(
                "{id} {kind:?}={lim}（少 1，实际用量 {need}）{p}：未报错"
            )),
            Err(e) => {
                match expect_limit(&e, kind, lim) {
                    Ok(actual) if exact_actual && actual != need => fails.push(format!(
                        "{id} {kind:?}={lim} {p}：actual {actual} != 实际用量 {need}"
                    )),
                    Ok(_) => {}
                    Err(m) => fails.push(format!("{id} {kind:?}={lim}（少 1）{p}：{m}")),
                }
                if e.position.offset > data.len() as u64 {
                    fails.push(format!("{id} {p}：offset {} 超出输入", e.position.offset));
                }
                match &first {
                    None => first = Some((p.to_string(), e)),
                    Some((p0, e0)) => {
                        if (&e0.kind, e0.position) != (&e.kind, e.position) {
                            fails.push(format!(
                                "{id} {kind:?}={lim}：{p0} {e0:?} 与 {p} {e:?} 不一致"
                            ));
                        }
                    }
                }
            }
        }
    }
    // 二分：最小可成功限值 = 实际用量（上界取 4×need+64，给“预留过大”留出可测范围）
    let paths: Vec<&str> = dom_paths_cfg(data, &set(ParserConfig::default(), u64::MAX))
        .into_iter()
        .map(|(p, _)| p)
        .collect();
    for p in paths {
        match bisect_min_ok(data, set, need.saturating_mul(4).saturating_add(64), p) {
            Some(m) if m == need => {}
            Some(m) => fails.push(format!(
                "{id} {kind:?} {p}：二分得最小可成功限值 {m}，实际用量 {need}（差 {:+}）",
                m as i64 - need as i64
            )),
            None => fails.push(format!(
                "{id} {kind:?} {p}：限值 4×实际用量+64 仍被拒绝（实际用量 {need}）"
            )),
        }
    }
    fails
}

#[test]
fn max_nodes_boundary_differential() {
    let mut fails = Vec::new();
    for (id, s) in limit_corpus() {
        // 实际用量：遍历建好的树数出的节点数（文档节点 + 全部后代），不读实现内部计数
        let n = count_nodes(&Document::from_bytes(s.as_bytes()).unwrap());
        // 节点逐个计数，失败时 actual = 需要数
        fails.extend(boundary(
            id,
            s.as_bytes(),
            n,
            LimitKind::NodeCount,
            |c, v| c.with_max_nodes(v),
            true,
        ));
    }
    // max_nodes=0：文档节点本身就超，报在 offset 0（设计 §2）
    for (p, r) in dom_paths_cfg(b"<r/>", &ParserConfig::default().with_max_nodes(0)) {
        match r {
            Err(e)
                if e.position.offset == 0 && expect_limit(&e, LimitKind::NodeCount, 0).is_ok() => {}
            other => fails.push(format!("max_nodes=0 {p}：{:?}", other.map(|_| ()))),
        }
    }
    assert!(fails.is_empty(), "{}", fails.join("\n"));
}

/// 实际用量：不限额解析后在建好的树上量出的 DOM 字节（`approx_heap_bytes` 只读各 Vec 长度，
/// 是 max_dom_bytes 的计量口径本身，不是 reserve 的预估）。五条路径各量一次，必须相同。
fn measured_dom_bytes(id: &str, data: &[u8], fails: &mut Vec<String>) -> u64 {
    let mut got: Vec<(&str, u64)> = Vec::new();
    for (p, r) in dom_paths_cfg(data, &ParserConfig::default()) {
        got.push((p, r.expect("语料须合法").approx_heap_bytes()));
    }
    let b = got[0].1;
    if got.iter().any(|(_, v)| *v != b) {
        fails.push(format!("{id}：各路径实际 DOM 字节不同 {got:?}"));
    }
    b
}

#[test]
fn max_dom_bytes_boundary_differential() {
    let mut fails = Vec::new();
    for (id, s) in limit_corpus() {
        let b = measured_dom_bytes(id, s.as_bytes(), &mut fails);
        // DomBytes 的 actual 取决于失败那次 reserve 的量，只要求 > limit 且五路一致。
        // 预留必须是上界、且不得提前拒绝：恰等于实际用量必须通过（裁定 3）。
        fails.extend(boundary(
            id,
            s.as_bytes(),
            b,
            LimitKind::DomBytes,
            |c, v| c.with_max_dom_bytes(v),
            false,
        ));
    }
    for (p, r) in dom_paths_cfg(b"<r/>", &ParserConfig::default().with_max_dom_bytes(0)) {
        match r {
            Err(e)
                if e.position.offset == 0 && expect_limit(&e, LimitKind::DomBytes, 0).is_ok() => {}
            other => fails.push(format!("max_dom_bytes=0 {p}：{:?}", other.map(|_| ()))),
        }
    }
    assert!(fails.is_empty(), "{}", fails.join("\n"));
}

// ───────────────────────────── 3. specified ─────────────────────────────

#[test]
fn specified_default_attrs_all_paths() {
    let s = r#"<!DOCTYPE r [<!ATTLIST r d CDATA "v" e CDATA "w">]><r x="1" e="explicit"/>"#;
    for (p, r) in dom_paths_cfg(s.as_bytes(), &ParserConfig::default()) {
        let doc = r.unwrap_or_else(|e| panic!("{p}: {e:?}"));
        let el = doc.root_element().unwrap();
        let got: Vec<(String, String, bool)> = el
            .attributes()
            .map(|a| (a.local().to_string(), a.value().to_string(), a.specified()))
            .collect();
        // 显式在前、默认在后（Q4：last_defaulted 计数）；显式给出的 e 不被默认值覆盖
        assert_eq!(
            got,
            vec![
                ("x".into(), "1".into(), true),
                ("e".into(), "explicit".into(), true),
                ("d".into(), "v".into(), false),
            ],
            "{p}"
        );
    }
}
