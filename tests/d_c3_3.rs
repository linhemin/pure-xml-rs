//! D-C3-3 回归（staging v10.9.2，2026-11-10）：DOM 序列化的 URI 身份不能只按起始地址判定。
//!
//! 缺陷（PR #8 1081296 实测）：`dom/ser.rs` 的 `OutScope::canonical` 用
//! `proven: HashMap<usize, &str>`，键只有 `as_ptr()`，没有长度。DOM arena 每次 `intern_span`
//! 都追加到末尾，空串 span 的起点 = 下一个 span 的起点，所以“空 URI”和紧随其后的非空 URI
//! 是**同一起始地址、不同长度**的两个 `&str`。空 URI 先进 `proven` 后，同址的非空 URI 被当成
//! 空 URI：`same_uri` 误判为相同，序列化漏写 `xmlns="…"`，元素读回后掉出命名空间。
//!
//! 公开 API 能造出“同址、不同长度”的状态，但造不出会触发缺陷的比较（Tester 11-10 实测）：
//! - `Document::parse("<r xmlns=\"\"/>")` 后 `create_element` 第一个 intern 的 URI 与
//!   `xmlns=""` 的空 span 同址（用 `lookup_namespace(None)` / `name().namespace` 的 `as_ptr()` 自检）；
//! - 但缺陷只在 `same_uri(existing="", uri=非空)` 时出现，即“作用域默认命名空间是空、元素自身却在
//!   非空默认命名空间里且自己不声明”。mutation API 的 `check_resolved_name` 会以
//!   `DomError::Namespace` 拒绝这种插入；parse 建树时元素总有同内容的声明在作用域里；
//!   带前缀的名字按前缀查找，不会拿到空 URI；arena 只追加，非空 span 不会共享起点。
//! - 随机探针（scratch，30000 个 parse+mutation 序列，62040 次成功操作）：2148 次出现同址状态，
//!   往返不一致 0 次。
//!
//! 所以本文件是**回归**（修复前后都应通过），确定性触发需要 src 单元测试用内部构造
//! （直接构造 `OutScope`，或绕过 `check_resolved_name` 写入 ElementData）。
//! `d_c3_3_guard_rejects_unsafe_shape` 锁定 mutation 的拒绝：若以后放开这条限制，用例会转而
//! 走往返断言，那时修复前必定失败。
//!
//! 断言：parse → 修改 → to_string / write_to → 五条路径（parse、from_bytes、
//! from_reader BufReader 8 KiB、分块 1 B、分块 7 B）再解析，结构与命名空间（canon_doc，
//! 含每个元素/属性的展开名）都与原树一致，且目标元素读回的命名空间正确。
//!
//! 运行：cargo test --test d_c3_3（debug 即可）

#[path = "common/dom_c2.rs"]
mod dom_c2;

use std::io::BufReader;

use dom_c2::{Chunked, canon_doc};
use purexml::Writer;
use purexml::dom::{Document, Name, NodeId};

/// 五条再解析路径。
fn reparse_all(out: &str) -> Vec<(&'static str, Result<Document, purexml::Error>)> {
    let b = out.as_bytes();
    vec![
        ("parse", Document::parse(out)),
        ("from_bytes", Document::from_bytes(b)),
        (
            "from_reader(BufReader 8KiB)",
            Document::from_reader(BufReader::with_capacity(8192, b)),
        ),
        ("from_reader(1B)", Document::from_reader(Chunked::new(b, 1))),
        ("from_reader(7B)", Document::from_reader(Chunked::new(b, 7))),
    ]
}

/// 第 `path` 条子元素路径（从根元素起，按元素子节点下标）上的元素命名空间。
fn ns_at(doc: &Document, path: &[usize]) -> Option<String> {
    let mut n = doc.root_element()?;
    for &i in path {
        n = n.children().filter(|c| c.name().is_some()).nth(i)?;
    }
    n.name()?.namespace.map(str::to_string)
}

/// 自检：`parent` 在作用域里的默认命名空间（空串）与 `child` 的命名空间同址、不同长度。
fn assert_same_address(doc: &Document, parent: NodeId, child: NodeId, what: &str) {
    let p = doc.get(parent).expect("parent");
    let c = doc.get(child).expect("child");
    let empty = p.lookup_namespace(None).unwrap_or_else(|| {
        panic!("{what}：父元素没有默认命名空间声明（期望 xmlns=\"\"），用例无效")
    });
    let uri = c
        .name()
        .and_then(|n| n.namespace)
        .unwrap_or_else(|| panic!("{what}：子元素没有命名空间，用例无效"));
    assert!(
        empty.is_empty() && !uri.is_empty(),
        "{what}：期望空 URI 与非空 URI，实际 {empty:?} / {uri:?}，用例无效"
    );
    assert!(
        empty.as_ptr() == uri.as_ptr() && empty.len() != uri.len(),
        "{what}：没能用公开 API 造出“同一起始地址、不同长度”的 URI 切片 \
         （空 URI @{:p} len {}，子元素 URI @{:p} len {}）。DOM arena 布局变了？\
         用例无法触发 D-C3-3，不得视为通过；请改用 src 单元测试的内部构造。",
        empty.as_ptr(),
        empty.len(),
        uri.as_ptr(),
        uri.len()
    );
}

/// 序列化（to_string 与 write_to 两个入口）→ 五路再解析，与原树比较。
fn roundtrip_fails(doc: &Document, target: &[usize], want_ns: &str, what: &str) -> Vec<String> {
    let mut fails = Vec::new();
    let want = canon_doc(doc);
    assert_eq!(
        ns_at(doc, target).as_deref(),
        Some(want_ns),
        "{what}：原树目标元素命名空间不对"
    );
    let via_to_string = doc.to_string().map_err(|e| format!("{e}"));
    let via_write_to = {
        let mut buf = Vec::new();
        let r = {
            let mut w = Writer::new(&mut buf);
            doc.write_to(&mut w)
        };
        r.map(|()| String::from_utf8(buf).expect("UTF-8"))
            .map_err(|e| format!("{e}"))
    };
    for (entry, out) in [("to_string", via_to_string), ("write_to", via_write_to)] {
        let out = match out {
            Ok(s) => s,
            Err(e) => {
                fails.push(format!("{what} {entry}：序列化失败 {e}"));
                continue;
            }
        };
        for (path, r) in reparse_all(&out) {
            match r {
                Err(e) => fails.push(format!(
                    "{what} {entry}→{path}：读回失败 {e:?}\n  输出 {out}"
                )),
                Ok(back) => {
                    let got_ns = ns_at(&back, target);
                    if got_ns.as_deref() != Some(want_ns) {
                        fails.push(format!(
                            "{what} {entry}→{path}：目标元素命名空间 {got_ns:?}，应为 {want_ns:?}（漏写 xmlns？）\n  输出 {out}"
                        ));
                    }
                    if canon_doc(&back) != want {
                        fails.push(format!(
                            "{what} {entry}→{path}：结构/命名空间与原树不同\n  输出 {out}"
                        ));
                    }
                }
            }
        }
    }
    fails
}

/// 形态 1：`<r xmlns=""/>` 下挂 `p:e`（urn:b，自己声明 p），元素 URI 与空默认 URI 同址。
#[test]
fn d_c3_3_prefixed_element_adjacent_to_empty_default() {
    let mut doc = Document::parse(r#"<r xmlns=""/>"#).unwrap();
    let r = doc.root_element().unwrap().id();
    let e = doc
        .create_element(Name::qualified("urn:b", Some("p"), "e"))
        .unwrap();
    doc.declare_namespace(e, Some("p"), "urn:b").unwrap();
    doc.append_child(r, e).unwrap();
    assert_same_address(&doc, r, e, "形态1");
    let fails = roundtrip_fails(&doc, &[0], "urn:b", "形态1");
    assert!(fails.is_empty(), "{}", fails.join("\n"));
}

/// 形态 2：外层默认 urn:a，中层 `xmlns=""`，内层 `p:e` 的 URI 与外层默认同内容、与中层空 URI 同址；
/// 内层再挂一个无前缀、无命名空间的子元素和带前缀属性。
#[test]
fn d_c3_3_undeclared_then_same_content_as_outer() {
    let mut doc = Document::parse(r#"<r xmlns="urn:a"><m xmlns=""/></r>"#).unwrap();
    let m = doc
        .root_element()
        .unwrap()
        .children()
        .find(|c| c.name().is_some())
        .unwrap()
        .id();
    let e = doc
        .create_element(Name::qualified("urn:a", Some("p"), "e"))
        .unwrap();
    doc.declare_namespace(e, Some("p"), "urn:a").unwrap();
    doc.append_child(m, e).unwrap();
    assert_same_address(&doc, m, e, "形态2");
    doc.set_attribute(e, Name::qualified("urn:a", Some("p"), "x"), "1")
        .unwrap();
    let f = doc.create_element(Name::local("f")).unwrap();
    doc.append_child(e, f).unwrap();
    let fails = roundtrip_fails(&doc, &[0, 0], "urn:a", "形态2");
    assert!(fails.is_empty(), "{}", fails.join("\n"));
}

/// 形态 3：`<r xmlns:q="urn:q" xmlns=""/>`（空默认声明排在最后），挂一个继承 q 绑定、自己不声明的
/// `q:e`，其 URI 与空默认 URI 同址；再加带前缀属性和无命名空间子元素。
#[test]
fn d_c3_3_inherited_prefix_adjacent_to_empty_default() {
    let mut doc = Document::parse(r#"<r xmlns:q="urn:q" xmlns=""/>"#).unwrap();
    let r = doc.root_element().unwrap().id();
    let e = doc
        .create_element(Name::qualified("urn:q", Some("q"), "e"))
        .unwrap();
    doc.append_child(r, e).unwrap();
    assert_same_address(&doc, r, e, "形态3");
    doc.set_attribute(e, Name::qualified("urn:q", Some("q"), "a"), "v")
        .unwrap();
    let g = doc.create_element(Name::local("g")).unwrap();
    doc.append_child(e, g).unwrap();
    let fails = roundtrip_fails(&doc, &[0], "urn:q", "形态3");
    assert!(fails.is_empty(), "{}", fails.join("\n"));
}

/// 会触发 D-C3-3 的形态：`xmlns=""` 作用域下挂一个无前缀、默认命名空间非空且自己不声明的元素。
/// 当前 mutation API 以 `DomError::Namespace` 拒绝（这正是公开 API 触发不了的原因）。
/// 若以后允许插入，则必须同址并且往返正确——修复前必定失败。
#[test]
fn d_c3_3_guard_rejects_unsafe_shape() {
    let mut doc = Document::parse(r#"<r xmlns=""/>"#).unwrap();
    let r = doc.root_element().unwrap().id();
    let e = doc
        .create_element(Name::qualified("urn:b", None, "e"))
        .unwrap();
    match doc.append_child(r, e) {
        Err(purexml::dom::DomError::Namespace) => {}
        Err(other) => panic!("期望 DomError::Namespace，实际 {other:?}"),
        Ok(()) => {
            assert_same_address(&doc, r, e, "unsafe 形态");
            let fails = roundtrip_fails(&doc, &[0], "urn:b", "unsafe 形态");
            assert!(fails.is_empty(), "{}", fails.join("\n"));
        }
    }
}

/// 只用 parse 的形态（空默认命名空间紧跟非空声明、前缀 URI、属性前缀组合）：
/// 公开 API 下不会进入同址比较，作为往返回归，修复前后都应通过。
#[test]
fn parse_only_shapes_roundtrip() {
    let cases = [
        r#"<r xmlns=""><e xmlns="urn:b"/></r>"#,
        r#"<r xmlns="" xmlns:p="urn:b"><p:e p:a=""/></r>"#,
        r#"<r xmlns="urn:a"><e xmlns=""><f xmlns="urn:b"/><g/></e></r>"#,
        r#"<r xmlns:p="urn:a"><p:e xmlns:q="urn:ab" q:x="1" p:y="2"/></r>"#,
        r#"<r xmlns="urn:ab"><e xmlns="urn:a"><f xmlns="urn:ab"/></e></r>"#,
        r#"<p:r xmlns:p="urn:a" xmlns:q="urn:a"><q:e p:a="" q:b=""/></p:r>"#,
        r#"<r xmlns="urn:x"><a xmlns=""/><b xmlns="urn:xx"/><c xmlns="urn:x"/></r>"#,
    ];
    let mut fails = Vec::new();
    for s in cases {
        let d1 = Document::parse(s).unwrap_or_else(|e| panic!("{s}: {e:?}"));
        let want = canon_doc(&d1);
        let out = match d1.to_string() {
            Ok(o) => o,
            Err(e) => {
                fails.push(format!("{s}：to_string 失败 {e}"));
                continue;
            }
        };
        for (path, r) in reparse_all(&out) {
            match r {
                Ok(back) if canon_doc(&back) == want => {}
                Ok(_) => fails.push(format!("{s} {path}：往返不同\n  输出 {out}")),
                Err(e) => fails.push(format!("{s} {path}：读回失败 {e:?}\n  输出 {out}")),
            }
        }
    }
    assert!(fails.is_empty(), "{}", fails.join("\n"));
}
