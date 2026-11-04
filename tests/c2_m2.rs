//! C2（M2）Tester 集成测试：DOM 修改 + 序列化。
//!
//! 统一断言（Lead 18:45）：序列化要么返回 Err，要么输出能被 Reader 读回且结构与树一致。
//! 结构比较规则见 `common/dom_c2.rs`（计划 §2.4）。
//!
//! 不调用 `create_doctype`（Implementer：将改为 pub(crate)）。Doctype 相关用例经
//! [`make_doctype`] 适配，等 M2 收尾的公开构建入口，标 `#[ignore]`。
//!
//! 运行：cargo test --test c2_m2
//! 深树/大量兄弟用例默认运行；W3C 子集需要 `PUREXML_W3C_DIR` 或 tests/w3c/cache/xmlconf，缺失则跳过。

#[path = "common/dom_c2.rs"]
mod dom_c2;

use dom_c2::{
    OEv, Tok, Unified, build, build_with, canon_doc, canon_events, canon_tree, check_readback,
    read_all_paths, read_bytes, read_bytes_with, roundtrip, unified,
};
use purexml::dom::{Document, DomError, Name, NodeId, NodeKind, WriteOptions};
use purexml::{ParserConfig, WriteError, Writer};

const XML_NS: &str = "http://www.w3.org/XML/1998/namespace";
const XMLNS_NS: &str = "http://www.w3.org/2000/xmlns/";
const A: &str = "urn:a";
const B: &str = "urn:b";

fn root(doc: &Document) -> NodeId {
    doc.root().unwrap().id()
}

fn new_doc_with_root(name: Name<'_>) -> (Document, NodeId) {
    let mut doc = Document::new();
    let r = root(&doc);
    let el = doc.create_element(name).unwrap();
    doc.append_child(r, el).unwrap();
    (doc, el)
}

/// 先在元素上声明命名空间，再挂到文档节点下（带前缀的根元素必须先有绑定）。
fn new_doc_with_root_ns(name: Name<'_>, decls: &[(Option<&str>, &str)]) -> (Document, NodeId) {
    let mut doc = Document::new();
    let r = root(&doc);
    let el = doc.create_element(name).unwrap();
    for (p, uri) in decls {
        doc.declare_namespace(el, *p, uri).unwrap();
    }
    doc.append_child(r, el).unwrap();
    (doc, el)
}

fn add_text(doc: &mut Document, parent: NodeId, t: &str) -> NodeId {
    let n = doc.create_text(t).unwrap();
    doc.append_child(parent, n).unwrap();
    n
}

fn add_el(doc: &mut Document, parent: NodeId, name: Name<'_>) -> NodeId {
    let el = doc.create_element(name).unwrap();
    doc.append_child(parent, el).unwrap();
    el
}

/// 统一断言（五个读取结果都要一致）。违反时 panic。
#[track_caller]
fn assert_unified(doc: &Document) -> Unified {
    match unified(doc, true) {
        Ok(u) => u,
        Err(msg) => panic!("统一断言失败：{msg}"),
    }
}

/// 统一断言，并且要求序列化成功，返回输出。
#[track_caller]
fn assert_serializes(doc: &Document) -> String {
    match assert_unified(doc) {
        Unified::Ok(s) => s,
        Unified::SerErr(e) => panic!("期望序列化成功，实际 Err：{e}"),
    }
}

#[track_caller]
fn assert_err_kind(r: Result<impl std::fmt::Debug, DomError>, want: &str) {
    match r {
        Ok(v) => panic!("期望 Err({want})，实际 Ok({v:?})"),
        Err(e) => {
            let got = format!("{e:?}");
            assert!(got.starts_with(want), "期望 Err({want})，实际 {got}");
        }
    }
}

// ───────────────────────────── 转义安全：文本 ─────────────────────────────

#[test]
fn esc_text_markup_chars_roundtrip() {
    for t in [
        "a]]>b",
        "]]>",
        "]]]]>>",
        "a&b",
        "&amp;",
        "<x/>",
        "a>b",
        "&#60;",
        "x\ry",
        "x\r\ny",
        "\r",
        "tab\tand\nlf",
        "\u{85}\u{2028}",
        "\u{10FFFF}\u{FFFD}",
        "",
    ] {
        let (mut doc, r) = new_doc_with_root(Name::local("r"));
        let id = doc.create_text(t).unwrap();
        doc.append_child(r, id).unwrap();
        let s = assert_serializes(&doc);
        assert!(!s.contains("]]>"), "文本里的 ]]> 必须转义：{s:?}");
        assert!(!s.contains('\r'), "文本里的 CR 必须写成字符引用：{s:?}");
    }
}

#[test]
fn esc_set_text_on_text_node_roundtrip() {
    let (mut doc, r) = new_doc_with_root(Name::local("r"));
    let id = doc.create_text("ok").unwrap();
    doc.append_child(r, id).unwrap();
    doc.set_text(id, "]]>&<\r").unwrap();
    assert_serializes(&doc);
}

// ───────────────────────────── 转义安全：属性 ─────────────────────────────

#[test]
fn esc_attribute_values_roundtrip() {
    for v in [
        "<",
        "&",
        "\"",
        "'",
        "\"'",
        "a\tb",
        "a\nb",
        "a\rb",
        "a\r\nb",
        "  lead and trail  ",
        "&#9;",
        ">",
        "]]>",
        "",
    ] {
        let (mut doc, r) = new_doc_with_root(Name::local("r"));
        doc.set_attribute(r, Name::local("a"), v).unwrap();
        let s = assert_serializes(&doc);
        for raw in ['\t', '\n', '\r'] {
            assert!(
                !s.contains(raw),
                "属性里的 {raw:?} 必须写成字符引用，否则读回会被规范化为空格：{s:?}"
            );
        }
    }
}

#[test]
fn esc_namespaced_attribute_value_roundtrip() {
    let (mut doc, r) = new_doc_with_root(Name::local("r"));
    doc.declare_namespace(r, Some("p"), A).unwrap();
    doc.set_attribute(r, Name::qualified(A, Some("p"), "a"), "x\"<&\t\n\ry")
        .unwrap();
    assert_serializes(&doc);
}

// ───────────────────────────── 转义安全：CDATA / 注释 / PI ─────────────────────────────

#[test]
fn esc_cdata_with_end_marker_rejected() {
    let mut doc = Document::new();
    assert_err_kind(doc.create_cdata("a]]>b"), "InvalidChar");
    assert_err_kind(doc.create_cdata("]]>"), "InvalidChar");
    // set_text 也要拒绝
    let (mut doc, r) = new_doc_with_root(Name::local("r"));
    let c = doc.create_cdata("ok ]] > ]>").unwrap();
    doc.append_child(r, c).unwrap();
    assert_err_kind(doc.set_text(c, "x]]>y"), "InvalidChar");
    assert_eq!(
        doc.get(c).unwrap().text(),
        Some("ok ]] > ]>"),
        "被拒的 set_text 不能改值"
    );
    assert_serializes(&doc);
}

#[test]
fn esc_cdata_markup_and_cr_roundtrip() {
    for t in ["<&>", "]]", "]>", "a]] >", "", "x\ty\nz"] {
        let (mut doc, r) = new_doc_with_root(Name::local("r"));
        let c = doc.create_cdata(t).unwrap();
        doc.append_child(r, c).unwrap();
        assert_unified(&doc);
    }
}

/// D-C2-1（Lead 19:25 裁定，c0f49a0 修复）：CDATA 里的 CR 读回会被规范化成 LF，又没法写字符引用，
/// 所以修改阶段直接返回 InvalidChar，文档不变。
#[test]
fn esc_cdata_with_cr() {
    for t in ["a\rb", "a\r\nb", "\r", "x]]\r"] {
        let mut doc = Document::new();
        assert!(
            matches!(doc.create_cdata(t), Err(DomError::InvalidChar)),
            "create_cdata({t:?}) 应返回 InvalidChar"
        );
        let (mut doc, r) = new_doc_with_root(Name::local("r"));
        let c = doc.create_cdata("ok").unwrap();
        doc.append_child(r, c).unwrap();
        let before = assert_serializes(&doc);
        let (nodes, bytes) = (doc.node_count(), doc.approx_heap_bytes());
        assert!(
            matches!(doc.set_text(c, t), Err(DomError::InvalidChar)),
            "CDATA set_text({t:?}) 应返回 InvalidChar"
        );
        assert_eq!(
            doc.get(c).unwrap().text(),
            Some("ok"),
            "被拒的 set_text 不能改值"
        );
        assert_eq!((doc.node_count(), doc.approx_heap_bytes()), (nodes, bytes));
        assert_eq!(assert_serializes(&doc), before);
    }
}

#[test]
fn esc_comment_rules() {
    let mut doc = Document::new();
    assert_err_kind(doc.create_comment("a--b"), "InvalidChar");
    assert_err_kind(doc.create_comment("--"), "InvalidChar");
    assert_err_kind(doc.create_comment("ends-"), "InvalidChar");
    assert_err_kind(doc.create_comment("-"), "InvalidChar");
    let (mut doc, r) = new_doc_with_root(Name::local("r"));
    let c = doc.create_comment("a-b <&> ]]> ?>").unwrap();
    doc.append_child(r, c).unwrap();
    assert_err_kind(doc.set_text(c, "x--y"), "InvalidChar");
    assert_err_kind(doc.set_text(c, "x-"), "InvalidChar");
    assert_eq!(doc.get(c).unwrap().text(), Some("a-b <&> ]]> ?>"));
    assert_serializes(&doc);
}

/// D-C2-1：注释里的 CR 在修改阶段返回 InvalidChar，文档不变。
#[test]
fn esc_comment_with_cr() {
    for t in ["a\rb", "\r\n", "\r"] {
        let mut doc = Document::new();
        assert!(
            matches!(doc.create_comment(t), Err(DomError::InvalidChar)),
            "create_comment({t:?}) 应返回 InvalidChar"
        );
        let (mut doc, r) = new_doc_with_root(Name::local("r"));
        let c = doc.create_comment("ok").unwrap();
        doc.append_child(r, c).unwrap();
        let before = assert_serializes(&doc);
        let (nodes, bytes) = (doc.node_count(), doc.approx_heap_bytes());
        assert!(
            matches!(doc.set_text(c, t), Err(DomError::InvalidChar)),
            "注释 set_text({t:?}) 应返回 InvalidChar"
        );
        assert_eq!(doc.get(c).unwrap().text(), Some("ok"));
        assert_eq!((doc.node_count(), doc.approx_heap_bytes()), (nodes, bytes));
        assert_eq!(assert_serializes(&doc), before);
    }
}

#[test]
fn esc_pi_rules() {
    let mut doc = Document::new();
    assert_err_kind(doc.create_pi("t", Some("a?>b")), "InvalidChar");
    assert_err_kind(doc.create_pi("t", Some("?>")), "InvalidChar");
    for t in ["xml", "XML", "xMl"] {
        assert_err_kind(doc.create_pi(t, None), "InvalidName");
    }
    assert_err_kind(doc.create_pi("", None), "InvalidName");
    assert_err_kind(doc.create_pi("a b", None), "InvalidName");
    let (mut doc, r) = new_doc_with_root(Name::local("r"));
    let p = doc.create_pi("t", Some("a ? > <&>")).unwrap();
    doc.append_child(r, p).unwrap();
    assert_err_kind(doc.set_pi_data(p, Some("x?>")), "InvalidChar");
    assert_eq!(doc.get(p).unwrap().pi(), Some(("t", Some("a ? > <&>"))));
    assert_serializes(&doc);
}

/// PI 数据开头的空白、空数据、CR：写出后读回可能变样。统一断言兜底。
#[test]
fn esc_pi_data_edge_cases() {
    for d in [
        Some(""),
        Some(" lead"),
        Some("\tlead"),
        Some("\nlead"),
        Some("trail "),
        None,
    ] {
        let (mut doc, r) = new_doc_with_root(Name::local("r"));
        let Ok(p) = doc.create_pi("t", d) else {
            continue;
        };
        doc.append_child(r, p).unwrap();
        assert_unified(&doc);
    }
}

/// D-C2-1：PI 数据里的 CR 在修改阶段返回 InvalidChar，文档不变。
#[test]
fn esc_pi_data_with_cr() {
    for d in ["a\rb", "a\r\nb", "\r", " \rlead"] {
        let mut doc = Document::new();
        assert!(
            matches!(doc.create_pi("t", Some(d)), Err(DomError::InvalidChar)),
            "create_pi(t, {d:?}) 应返回 InvalidChar"
        );
        let (mut doc, r) = new_doc_with_root(Name::local("r"));
        let p = doc.create_pi("t", Some("ok")).unwrap();
        doc.append_child(r, p).unwrap();
        let before = assert_serializes(&doc);
        let (nodes, bytes) = (doc.node_count(), doc.approx_heap_bytes());
        assert!(
            matches!(doc.set_pi_data(p, Some(d)), Err(DomError::InvalidChar)),
            "set_pi_data({d:?}) 应返回 InvalidChar"
        );
        assert_eq!(doc.get(p).unwrap().pi(), Some(("t", Some("ok"))));
        assert_eq!((doc.node_count(), doc.approx_heap_bytes()), (nodes, bytes));
        assert_eq!(assert_serializes(&doc), before);
    }
}

/// D-C2-1 另一半：文本和属性里的 CR 写成 `&#xD;`，读回仍是 CR（统一断言比较结构）。
#[test]
fn esc_text_and_attr_cr_written_as_char_ref() {
    for (t, want_t) in [
        ("a\rb", "a&#xD;b"),
        ("\r", "&#xD;"),
        ("x\r\ny", "x&#xD;\ny"),
    ] {
        let (mut doc, r) = new_doc_with_root(Name::local("r"));
        doc.set_attribute(r, Name::local("a"), t).unwrap();
        add_text(&mut doc, r, t);
        let s = assert_serializes(&doc);
        let want_a = want_t.replace('\n', "&#10;");
        assert_eq!(s, format!("<r a=\"{want_a}\">{want_t}</r>"));
        // 读回后文本、属性里仍是原样的 CR
        let back = build(&read_bytes(s.as_bytes()).unwrap()).unwrap().doc;
        let br = back.root_element().unwrap();
        assert_eq!(br.attribute("a"), Some(t));
        assert_eq!(br.children().next().unwrap().text(), Some(t));
    }
}

// ───────────────────────────── 非法字符与非法名字 ─────────────────────────────

#[test]
fn illegal_chars_rejected_everywhere() {
    let bad = [
        "\u{0}", "a\u{1}b", "\u{8}", "\u{B}", "\u{C}", "\u{1F}", "\u{FFFE}", "\u{FFFF}",
    ];
    for c in bad {
        let (mut doc, r) = new_doc_with_root(Name::local("r"));
        assert_err_kind(doc.create_text(c), "InvalidChar");
        assert_err_kind(doc.create_cdata(c), "InvalidChar");
        assert_err_kind(doc.create_comment(c), "InvalidChar");
        assert_err_kind(doc.create_pi("t", Some(c)), "InvalidChar");
        assert_err_kind(doc.set_attribute(r, Name::local("a"), c), "InvalidChar");
        let t = doc.create_text("ok").unwrap();
        doc.append_child(r, t).unwrap();
        assert_err_kind(doc.set_text(t, c), "InvalidChar");
        assert_eq!(doc.get(r).unwrap().attribute("a"), None);
        assert_serializes(&doc);
    }
}

#[test]
fn illegal_names_rejected() {
    let mut doc = Document::new();
    for n in ["", "1a", "a b", "a:b", ":a", "-a", "a\u{0}", "a>"] {
        assert!(
            doc.create_element(Name::local(n)).is_err(),
            "元素名 {n:?} 应被拒"
        );
    }
    let (mut doc, r) = new_doc_with_root(Name::local("r"));
    for n in ["", "1a", "a b", "a:b", "a=", "a\""] {
        assert!(
            doc.set_attribute(r, Name::local(n), "v").is_err(),
            "属性名 {n:?} 应被拒"
        );
    }
    assert!(
        doc.create_element(Name::qualified(A, Some("1p"), "x"))
            .is_err()
    );
    assert!(
        doc.create_element(Name::qualified(A, Some("p:q"), "x"))
            .is_err()
    );
    assert_serializes(&doc);
}

// ───────────────────────────── 命名空间 ─────────────────────────────

#[test]
fn ns_rebinding_in_child() {
    let (mut doc, r) = new_doc_with_root_ns(Name::qualified(A, Some("p"), "r"), &[(Some("p"), A)]);
    let c = doc
        .create_element(Name::qualified(B, Some("p"), "c"))
        .unwrap();
    doc.declare_namespace(c, Some("p"), B).unwrap();
    doc.append_child(r, c).unwrap();
    doc.set_attribute(c, Name::qualified(B, Some("p"), "a"), "1")
        .unwrap();
    let g = add_el(&mut doc, c, Name::qualified(B, Some("p"), "g"));
    assert_eq!(doc.get(g).unwrap().lookup_namespace(Some("p")), Some(B));
    assert_serializes(&doc);
}

#[test]
fn ns_default_rebinding_and_undeclare() {
    let (mut doc, r) = new_doc_with_root_ns(Name::qualified(A, None, "r"), &[(None, A)]);
    let c = doc.create_element(Name::qualified(B, None, "c")).unwrap();
    doc.declare_namespace(c, None, B).unwrap();
    doc.append_child(r, c).unwrap();
    // 子元素回到“无命名空间”：要么插入/序列化报错，要么输出带 xmlns=""
    let n = doc.create_element(Name::local("n")).unwrap();
    if doc.append_child(c, n).is_ok() {
        assert_unified(&doc);
    }
}

/// 父元素有默认命名空间，子元素是无命名空间的 `Name::local`，且没有声明 xmlns=""。
#[test]
fn ns_unqualified_child_under_default_namespace() {
    let (mut doc, r) = new_doc_with_root_ns(Name::qualified(A, None, "r"), &[(None, A)]);
    let c = doc.create_element(Name::local("c")).unwrap();
    match doc.append_child(r, c) {
        Err(e) => assert!(matches!(e, DomError::Namespace), "{e:?}"),
        Ok(()) => {
            assert_unified(&doc);
        }
    }
}

/// detach 后，子树根要带上原来继承的绑定；挪到另一个把同一前缀绑到别处的父元素下，
/// 元素的命名空间不能变。
#[test]
fn ns_reresolve_after_detach_and_move() {
    let (mut doc, r) = new_doc_with_root(Name::local("r"));
    let a = add_el(&mut doc, r, Name::local("a"));
    doc.declare_namespace(a, Some("p"), A).unwrap();
    let x = add_el(&mut doc, a, Name::qualified(A, Some("p"), "x"));
    doc.set_attribute(x, Name::qualified(A, Some("p"), "k"), "v")
        .unwrap();
    let b = add_el(&mut doc, r, Name::local("b"));
    doc.declare_namespace(b, Some("p"), B).unwrap();

    doc.detach(x).unwrap();
    assert_eq!(doc.get(x).unwrap().lookup_namespace(Some("p")), Some(A));
    match doc.append_child(b, x) {
        Ok(()) => {
            let n = doc.get(x).unwrap();
            assert_eq!(n.name().unwrap().namespace, Some(A));
            assert_eq!(n.lookup_namespace(Some("p")), Some(A));
            assert_serializes(&doc);
        }
        Err(e) => panic!("detach 后带着自己的绑定，应能插入：{e:?}"),
    }
}

/// 不先 detach，直接把已挂在树上的节点插到别处（偏差：移动语义）。
/// 新位置的前缀绑定不同：要么 Err(Namespace) 且树不变，要么移动后命名空间不变且可往返。
#[test]
fn ns_move_attached_into_conflicting_scope() {
    let (mut doc, r) = new_doc_with_root(Name::local("r"));
    let a = add_el(&mut doc, r, Name::local("a"));
    doc.declare_namespace(a, Some("p"), A).unwrap();
    let x = add_el(&mut doc, a, Name::qualified(A, Some("p"), "x"));
    let b = add_el(&mut doc, r, Name::local("b"));
    doc.declare_namespace(b, Some("p"), B).unwrap();
    let before = canon_doc(&doc);
    match doc.append_child(b, x) {
        Err(e) => {
            assert!(matches!(e, DomError::Namespace), "{e:?}");
            assert_eq!(canon_doc(&doc), before, "失败的插入不能改树");
            assert_eq!(doc.get(x).unwrap().parent().map(|p| p.id()), Some(a));
        }
        Ok(()) => {
            assert_eq!(doc.get(x).unwrap().name().unwrap().namespace, Some(A));
            assert_unified(&doc);
        }
    }
}

#[test]
fn ns_move_attached_into_compatible_scope() {
    let (mut doc, r) = new_doc_with_root(Name::local("r"));
    doc.declare_namespace(r, Some("p"), A).unwrap();
    let a = add_el(&mut doc, r, Name::local("a"));
    let x = add_el(&mut doc, a, Name::qualified(A, Some("p"), "x"));
    let b = add_el(&mut doc, r, Name::local("b"));
    doc.append_child(b, x).unwrap();
    assert_eq!(doc.get(x).unwrap().parent().map(|p| p.id()), Some(b));
    assert_eq!(doc.get(a).unwrap().first_child().map(|c| c.id()), None);
    assert_serializes(&doc);
}

#[test]
fn ns_prefix_conflicts() {
    let (mut doc, r) = new_doc_with_root(Name::local("r"));
    doc.declare_namespace(r, Some("p"), A).unwrap();
    // 属性前缀 p 已绑定 A，却要求 B
    assert_err_kind(
        doc.set_attribute(r, Name::qualified(B, Some("p"), "a"), "v"),
        "Namespace",
    );
    // 未绑定的前缀
    assert_err_kind(
        doc.set_attribute(r, Name::qualified(B, Some("q"), "a"), "v"),
        "Namespace",
    );
    // 同一元素重复声明同一前缀为不同 URI：要么拒绝，要么替换后可往返
    if doc.declare_namespace(r, Some("p"), B).is_ok() {
        assert_unified(&doc);
    }
    // 使用中的绑定不能删
    let (mut doc, r) = new_doc_with_root_ns(Name::qualified(A, Some("p"), "r"), &[(Some("p"), A)]);
    assert_err_kind(doc.remove_namespace(r, Some("p")), "Namespace");
    assert_serializes(&doc);
}

/// 两个前缀绑到同一 URI，属性展开名相同 → 同一属性（替换），不能写出重复属性。
#[test]
fn ns_same_expanded_attr_via_two_prefixes() {
    let (mut doc, r) = new_doc_with_root(Name::local("r"));
    doc.declare_namespace(r, Some("p"), A).unwrap();
    doc.declare_namespace(r, Some("q"), A).unwrap();
    doc.set_attribute(r, Name::qualified(A, Some("p"), "a"), "1")
        .unwrap();
    match doc.set_attribute(r, Name::qualified(A, Some("q"), "a"), "2") {
        Ok(()) => {
            assert_eq!(doc.get(r).unwrap().attributes().count(), 1);
            assert_eq!(doc.get(r).unwrap().attribute_ns(Some(A), "a"), Some("2"));
        }
        Err(e) => assert!(matches!(e, DomError::DuplicateAttribute), "{e:?}"),
    }
    assert_serializes(&doc);
}

/// 无前缀但带命名空间的属性：属性不受默认命名空间影响，必须拒绝或正确写出。
#[test]
fn ns_attribute_with_namespace_but_no_prefix() {
    let (mut doc, r) = new_doc_with_root_ns(Name::qualified(A, None, "r"), &[(None, A)]);
    if doc
        .set_attribute(r, Name::qualified(A, None, "a"), "v")
        .is_ok()
    {
        assert_unified(&doc);
    }
}

#[test]
fn ns_reserved_prefixes() {
    let mut doc = Document::new();
    assert_err_kind(
        doc.create_element(Name::qualified(A, Some("xmlns"), "x")),
        "Namespace",
    );
    assert_err_kind(
        doc.create_element(Name::qualified(XMLNS_NS, Some("p"), "x")),
        "Namespace",
    );
    assert_err_kind(
        doc.create_element(Name::qualified(A, Some("xml"), "x")),
        "Namespace",
    );
    let (mut doc, r) = new_doc_with_root(Name::local("r"));
    assert!(doc.declare_namespace(r, Some("xml"), A).is_err());
    assert!(doc.declare_namespace(r, Some("xmlns"), A).is_err());
    assert!(doc.declare_namespace(r, Some("xmlns"), XMLNS_NS).is_err());
    assert!(doc.declare_namespace(r, Some("p"), XML_NS).is_err());
    assert!(doc.declare_namespace(r, None, XML_NS).is_err());
    assert!(doc.declare_namespace(r, Some("p"), XMLNS_NS).is_err());
    assert!(doc.declare_namespace(r, None, XMLNS_NS).is_err());
    // XML 1.0 不允许取消前缀绑定
    assert!(doc.declare_namespace(r, Some("p"), "").is_err());
    assert_err_kind(doc.set_attribute(r, Name::local("xmlns"), A), "Namespace");
    assert_err_kind(
        doc.set_attribute(r, Name::qualified(XMLNS_NS, Some("xmlns"), "p"), A),
        "Namespace",
    );
    // xml 前缀不用声明，xml:lang 可直接用；写出时不能出现 xmlns:xml
    doc.set_attribute(r, Name::qualified(XML_NS, Some("xml"), "lang"), "en")
        .unwrap();
    let s = assert_serializes(&doc);
    assert!(!s.contains("xmlns:xml"), "{s:?}");
    // 允许显式把 xml 绑到它自己的 URI；写出后仍可往返
    if doc.declare_namespace(r, Some("xml"), XML_NS).is_ok() {
        assert_serializes(&doc);
    }
}

/// 子树单独写出（NodeRef::write_to）要自带用到的命名空间声明（计划 §2.4），
/// 或者报错；不能写出未绑定的前缀。
#[test]
fn ns_subtree_write_is_self_contained() {
    let (mut doc, r) =
        new_doc_with_root_ns(Name::qualified(B, None, "r"), &[(Some("p"), A), (None, B)]);
    let x = add_el(&mut doc, r, Name::qualified(A, Some("p"), "x"));
    doc.set_attribute(x, Name::qualified(A, Some("p"), "k"), "v")
        .unwrap();
    add_el(&mut doc, x, Name::qualified(B, None, "y"));
    let mut buf = Vec::new();
    let mut w = Writer::new(&mut buf);
    match doc.get(x).unwrap().write_to(&mut w) {
        Err(e) => eprintln!("子树写出报错（允许）：{e}"),
        Ok(()) => {
            let s = String::from_utf8(buf).unwrap();
            if let Err(msg) = check_readback(&s, &canon_tree(&doc, x), true) {
                panic!("子树写出不自足：{msg}");
            }
        }
    }
}

// ───────────────────────────── 偏差锁定（Lead 已接受） ─────────────────────────────

/// 偏差 1：set_attribute 同展开名直接替换旧值，不报 DuplicateAttribute。
#[test]
fn deviation_set_attribute_replaces() {
    let (mut doc, r) = new_doc_with_root(Name::local("r"));
    doc.set_attribute(r, Name::local("a"), "1").unwrap();
    doc.set_attribute(r, Name::local("b"), "x").unwrap();
    doc.set_attribute(r, Name::local("a"), "2").unwrap();
    let n = doc.get(r).unwrap();
    assert_eq!(n.attributes().count(), 2);
    assert_eq!(n.attribute("a"), Some("2"));
    let s = assert_serializes(&doc);
    assert_eq!(s.matches("a=").count(), 1, "{s:?}");
    // 命名空间属性同理
    doc.declare_namespace(r, Some("p"), A).unwrap();
    doc.set_attribute(r, Name::qualified(A, Some("p"), "a"), "1")
        .unwrap();
    doc.set_attribute(r, Name::qualified(A, Some("p"), "a"), "3")
        .unwrap();
    assert_eq!(doc.get(r).unwrap().attribute_ns(Some(A), "a"), Some("3"));
    assert_eq!(doc.get(r).unwrap().attributes().count(), 3);
    assert_serializes(&doc);
}

/// 偏差 2：序列化不写 `<?xml ...?>`，即使原文有 XML 声明。
#[test]
fn deviation_no_xml_declaration() {
    let input = "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\n<r a=\"1\">t</r>";
    let events = read_bytes(input.as_bytes()).unwrap();
    assert!(events.iter().any(|e| matches!(e, OEv::XmlDecl)));
    let b = build(&events).unwrap();
    let s = b.doc.to_string().unwrap();
    assert!(!s.starts_with("<?xml"), "{s:?}");
    assert!(!s.contains("<?xml"), "{s:?}");
    assert_eq!(s, "<r a=\"1\">t</r>");
    let (doc, _) = new_doc_with_root(Name::local("r"));
    assert_eq!(doc.to_string().unwrap(), "<r/>");
}

/// 偏差 3（锁定移动语义）：把已挂在树上的节点再插入，相当于先摘下再插入。
#[test]
fn deviation_reinsert_moves() {
    let (mut doc, r) = new_doc_with_root(Name::local("r"));
    let a = add_el(&mut doc, r, Name::local("a"));
    let b = add_el(&mut doc, r, Name::local("b"));
    let t = doc.create_text("t").unwrap();
    doc.append_child(a, t).unwrap();
    doc.append_child(b, t).unwrap();
    assert_eq!(doc.to_string().unwrap(), "<r><a/><b>t</b></r>");
    doc.insert_before(b, a).unwrap();
    assert_eq!(doc.to_string().unwrap(), "<r><b>t</b><a/></r>");
    assert_serializes(&doc);
}

/// omit_doctype 在没有 doctype 的文档上不改变输出。
#[test]
fn omit_doctype_noop_without_doctype() {
    let (doc, _) = new_doc_with_root(Name::local("r"));
    let a = doc.to_string().unwrap();
    let b = doc
        .to_string_with(WriteOptions { omit_doctype: true })
        .unwrap();
    assert_eq!(a, b);
}

// ───────────────────────────── Doctype（待定：等新 API） ─────────────────────────────

/// Doctype 适配层。`create_doctype` 将改为 pub(crate)，这里不能调用。
/// 新的公开入口落地后只改这里。返回 None 表示当前没有合法入口。
fn make_doctype(
    _doc: &mut Document,
    _name: &str,
    _system: Option<&str>,
    _public: Option<&str>,
) -> Option<Result<NodeId, DomError>> {
    None
}

#[test]
#[ignore = "等 M2 收尾的公开构建入口；ser.rs 暂无对应单元测试（规则在 edit.rs create_doctype，PUBLIC 无 SYSTEM → Hierarchy）"]
fn doctype_public_without_system_rejected() {
    let mut doc = Document::new();
    let r = make_doctype(&mut doc, "r", None, Some("-//X//Y"))
        .expect("待定：没有合法的 doctype 公开入口");
    assert!(r.is_err(), "PUBLIC 必须带 SYSTEM");
}

#[test]
#[ignore = "等 M2 收尾的公开构建入口；当前由 ser.rs 单元测试覆盖"]
fn doctype_omit_option_drops_doctype() {
    let mut doc = Document::new();
    let d = make_doctype(&mut doc, "r", Some("r.dtd"), None)
        .expect("待定：没有合法的 doctype 公开入口")
        .unwrap();
    let rt = root(&doc);
    doc.append_child(rt, d).unwrap();
    add_el(&mut doc, rt, Name::local("r"));
    let full = doc.to_string().unwrap();
    assert!(
        full.starts_with("<!DOCTYPE r SYSTEM \"r.dtd\">"),
        "{full:?}"
    );
    let omitted = doc
        .to_string_with(WriteOptions { omit_doctype: true })
        .unwrap();
    assert_eq!(omitted, "<r/>");
}

// ───────────────────────────── 文档形状 ─────────────────────────────

/// Q-C2-1（Lead 19:25 裁定，c0f49a0 实现）：没有根元素的 Document 序列化返回
/// `WriteError::NoRootElement`；单个节点子树（NodeRef::write_to）不受影响。
#[test]
fn doc_without_root_element() {
    fn write_doc(doc: &Document) -> (Result<(), WriteError>, Vec<u8>) {
        let mut buf = Vec::new();
        let r = doc.write_to(&mut Writer::new(&mut buf));
        (r, buf)
    }
    let doc = Document::new();
    assert!(matches!(doc.to_string(), Err(WriteError::NoRootElement)));
    let (r, buf) = write_doc(&doc);
    assert!(matches!(r, Err(WriteError::NoRootElement)) && buf.is_empty());

    // 只有注释/PI，没有元素
    let mut doc = Document::new();
    let rt = root(&doc);
    let c = doc.create_comment("only").unwrap();
    doc.append_child(rt, c).unwrap();
    let p = doc.create_pi("t", Some("d")).unwrap();
    doc.append_child(rt, p).unwrap();
    assert!(matches!(doc.to_string(), Err(WriteError::NoRootElement)));
    assert!(matches!(
        doc.to_string_with(WriteOptions { omit_doctype: true }),
        Err(WriteError::NoRootElement)
    ));
    let (r, buf) = write_doc(&doc);
    assert!(matches!(r, Err(WriteError::NoRootElement)) && buf.is_empty());
    // 统一断言接受 Err
    assert!(matches!(assert_unified(&doc), Unified::SerErr(_)));

    // 子树不受影响：注释节点、未挂接的元素子树都能写
    let mut buf = Vec::new();
    doc.get(c)
        .unwrap()
        .write_to(&mut Writer::new(&mut buf))
        .unwrap();
    assert_eq!(String::from_utf8(buf).unwrap(), "<!--only-->");
    let e = doc.create_element(Name::local("e")).unwrap();
    add_text(&mut doc, e, "t");
    let mut buf = Vec::new();
    doc.get(e)
        .unwrap()
        .write_to(&mut Writer::new(&mut buf))
        .unwrap();
    assert_eq!(String::from_utf8(buf).unwrap(), "<e>t</e>");
    assert!(
        matches!(doc.to_string(), Err(WriteError::NoRootElement)),
        "未挂接的元素不算根"
    );

    // 删掉根元素以后同样报错；挂回元素后恢复
    let (mut doc, r) = new_doc_with_root(Name::local("r"));
    doc.remove(r).unwrap();
    assert!(matches!(doc.to_string(), Err(WriteError::NoRootElement)));
    let rt = root(&doc);
    add_el(&mut doc, rt, Name::local("s"));
    assert_eq!(assert_serializes(&doc), "<s/>");
}

#[test]
fn doc_root_removed_then_replaced() {
    let (mut doc, r) = new_doc_with_root(Name::local("r"));
    doc.remove(r).unwrap();
    assert!(doc.get(r).is_none());
    let rt = root(&doc);
    add_el(&mut doc, rt, Name::local("s"));
    assert_eq!(assert_serializes(&doc), "<s/>");
}

#[test]
fn doc_level_misc_nodes_roundtrip() {
    let mut doc = Document::new();
    let rt = root(&doc);
    let c = doc.create_comment(" c ").unwrap();
    doc.append_child(rt, c).unwrap();
    let p = doc.create_pi("pi", Some("d")).unwrap();
    doc.append_child(rt, p).unwrap();
    add_el(&mut doc, rt, Name::local("r"));
    let c2 = doc.create_comment("after").unwrap();
    doc.append_child(rt, c2).unwrap();
    assert_serializes(&doc);
    // 文档节点下不能放文本、第二个根元素
    let t = doc.create_text("x").unwrap();
    assert_err_kind(doc.append_child(rt, t), "Hierarchy");
    let e = doc.create_element(Name::local("e")).unwrap();
    assert_err_kind(doc.append_child(rt, e), "Hierarchy");
    assert_serializes(&doc);
}

/// 删掉大量节点后再写出：ser.rs 的循环护栏不能导致静默截断。
#[test]
fn ser_after_many_removals_not_truncated() {
    let (mut doc, r) = new_doc_with_root(Name::local("r"));
    let mut ids = Vec::new();
    for i in 0..2000 {
        let e = add_el(&mut doc, r, Name::local("e"));
        doc.set_attribute(e, Name::local("i"), &i.to_string())
            .unwrap();
        let t = doc.create_text("x").unwrap();
        doc.append_child(e, t).unwrap();
        ids.push(e);
    }
    for (i, id) in ids.iter().enumerate() {
        if i % 3 != 0 {
            doc.remove(*id).unwrap();
        }
    }
    for i in 0..500 {
        let e = add_el(&mut doc, r, Name::local("n"));
        if i % 2 == 0 {
            doc.detach(e).unwrap();
            doc.append_child(r, e).unwrap();
        }
    }
    let s = assert_serializes(&doc);
    assert_eq!(s.matches("<e ").count(), 667);
    // detach 会在子树根上补 xmlns=""（复制外部绑定），所以按 "<n" 计数
    assert_eq!(s.matches("<n").count(), 500);
}

// ───────────────────────────── 深树与大量兄弟 ─────────────────────────────

const DEEP: usize = 100_000;

/// 10 万层：建树、写出、读回都不能爆栈（测试线程默认 2 MiB 栈）。
/// 19bb758 上 debug 跑通（不爆栈），但 DOM 插入每次 O(depth)，建树 O(depth²)，
/// debug 约 7 分钟，所以默认忽略；常规跑 2 万层版本。见观察 P-C2-1。
#[test]
#[ignore = "慢：插入 O(depth) 使 10 万层建树 O(depth²)，debug 约 7 分钟；cargo test --release -- --ignored 运行"]
fn deep_tree_100k_serializes_without_stack_overflow() {
    deep_tree(DEEP);
}

#[test]
fn deep_tree_20k_serializes_without_stack_overflow() {
    deep_tree(20_000);
}

fn deep_tree(depth: usize) {
    let cfg = ParserConfig::default().with_max_depth(depth + 16);
    let mut doc = Document::with_config(&cfg).unwrap();
    let mut parent = root(&doc);
    for i in 0..depth {
        let e = doc.create_element(Name::local("d")).unwrap();
        doc.append_child(parent, e).unwrap();
        if i % 1000 == 0 {
            doc.set_attribute(e, Name::local("i"), &i.to_string())
                .unwrap();
        }
        parent = e;
    }
    let t = doc.create_text("leaf&<").unwrap();
    doc.append_child(parent, t).unwrap();
    let s = doc.to_string().expect("深树写出");
    assert_eq!(s.matches("<d").count(), depth);
    let events = read_bytes_with(s.as_bytes(), cfg.clone()).expect("深树读回");
    assert_eq!(canon_events(&events), canon_doc(&doc));
    // 再建一次（事件 → DOM 的路径也不能爆栈）
    let b = build_with(&events, &cfg).expect("深树重建");
    assert_eq!(b.doc.to_string().unwrap(), s);
    drop(b);
    drop(doc);
}

#[test]
fn many_siblings_200k() {
    let (mut doc, r) = new_doc_with_root(Name::local("r"));
    for i in 0..200_000u32 {
        if i % 2 == 0 {
            add_el(&mut doc, r, Name::local("s"));
        } else {
            let t = doc.create_text("&").unwrap();
            doc.append_child(r, t).unwrap();
        }
    }
    let s = doc.to_string().unwrap();
    assert_eq!(s.matches("<s/>").count(), 100_000);
    let events = read_bytes(s.as_bytes()).unwrap();
    assert_eq!(canon_events(&events), canon_doc(&doc));
}

// ───────────────────────────── 往返：样例与 W3C ─────────────────────────────

const SAMPLES: &[&str] = &[
    "<r/>",
    "<r a=\"1\" b='2'>t<!--c--><?p d?><![CDATA[x]]>y</r>",
    "<?xml version=\"1.0\"?><!-- pre --><r/><?post?>",
    "<p:r xmlns:p=\"urn:a\" xmlns=\"urn:b\" p:a=\"1\" a=\"2\"><c xmlns=\"\"/><p:c xmlns:p=\"urn:c\"/></p:r>",
    "<r xml:lang=\"en\" xml:space=\"preserve\">  </r>",
    "<r a=\"&#9;&#10;&#13;&lt;&amp;&quot;\">&#13;]]&gt;&amp;&lt;</r>",
    "<!DOCTYPE r [<!ENTITY e \"ent\"><!ATTLIST r d CDATA \"def\">]><r>&e;</r>",
    "<r><![CDATA[a]]]]><![CDATA[>b]]></r>",
    "<r>a<![CDATA[]]>b</r>",
    "<r>\r\n x \r y</r>",
];

#[test]
fn roundtrip_samples() {
    for s in SAMPLES {
        match roundtrip(s.as_bytes()) {
            Ok(Some(_)) => {}
            Ok(None) => {
                // 解析成功但返回 None = 命中已登记缺陷（known_ser_defect）或 DOM 限额
                assert!(read_bytes(s.as_bytes()).is_ok(), "样例应能解析：{s:?}");
                eprintln!("已登记缺陷，跳过：{s:?}");
            }
            Err(m) => panic!("往返失败 {s:?}：{m}"),
        }
    }
}

/// 属性默认值（DTD 补出的 specified=false 属性）写出后变成显式属性，结构仍一致。
#[test]
fn roundtrip_defaulted_attribute_becomes_explicit() {
    let s = roundtrip(b"<!DOCTYPE r [<!ATTLIST r d CDATA \"def\">]><r/>")
        .unwrap()
        .unwrap();
    assert_eq!(s, "<r d=\"def\"/>");
}

#[test]
fn roundtrip_staging_data_files() {
    let base = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data");
    let mut stack = vec![base];
    let (mut ok, mut skipped) = (0usize, 0usize);
    let mut fails = Vec::new();
    while let Some(dir) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&dir) else {
            continue;
        };
        for e in rd.flatten() {
            let p = e.path();
            if p.is_dir() {
                stack.push(p);
                continue;
            }
            let Ok(bytes) = std::fs::read(&p) else {
                continue;
            };
            if bytes.len() > 4 << 20 {
                continue;
            }
            match roundtrip(&bytes) {
                Ok(Some(_)) => ok += 1,
                Ok(None) => skipped += 1,
                Err(m) => fails.push(format!("{}: {m}", p.display())),
            }
        }
    }
    eprintln!(
        "tests/data 往返：ok={ok} skipped={skipped} fail={}",
        fails.len()
    );
    assert!(fails.is_empty(), "{}", fails.join("\n"));
}

fn w3c_dir() -> std::path::PathBuf {
    std::env::var_os("PUREXML_W3C_DIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| {
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/w3c/cache/xmlconf")
        })
}

/// W3C xmltest valid/sa 往返。解析失败（已知 policy/缺陷）跳过，只统计。
#[test]
fn roundtrip_w3c_valid_subset() {
    let dir = w3c_dir().join("xmltest/valid/sa");
    let Ok(rd) = std::fs::read_dir(&dir) else {
        eprintln!("跳过：没有 W3C 数据 {}", dir.display());
        return;
    };
    let mut files: Vec<_> = rd
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "xml"))
        .collect();
    files.sort();
    let (mut ok, mut skipped) = (0usize, 0usize);
    let mut fails = Vec::new();
    for p in &files {
        let bytes = std::fs::read(p).unwrap();
        match roundtrip(&bytes) {
            Ok(Some(_)) => ok += 1,
            Ok(None) => skipped += 1,
            Err(m) => fails.push(format!("{}: {m}", p.display())),
        }
    }
    eprintln!(
        "W3C valid/sa 往返：ok={ok} skipped={skipped} fail={}",
        fails.len()
    );
    assert!(ok > 0);
    assert!(fails.is_empty(), "{}", fails.join("\n"));
}

// ───────────────────────────── 四路径差分 ─────────────────────────────

/// 同一份输入走五个读取结果（from_bytes / from_vec / from_reader 1B·7B·8KiB）：
/// 成功时建出的树写出结果逐字节相同；失败时错误种类和位置相同。
fn diff_paths(input: &[u8]) -> Result<(), String> {
    let results = read_all_paths(input);
    let (p0, r0) = &results[0];
    let key = |r: &dom_c2::ReadResult| -> Result<String, String> {
        match r {
            Ok(evs) => match build(evs) {
                Ok(b) => b.doc.to_string().map(|s| format!("ok {s}")).or_else(|e| {
                    let m = format!("{e}");
                    match dom_c2::known_ser_defect(&m) {
                        Some(id) => Ok(format!("known {id} {m}")),
                        None => Err(format!("ser {m}")),
                    }
                }),
                Err(e) => Ok(format!("build-err {e:?}")),
            },
            Err((_, e)) => Ok(format!("err {:?} {:?}", e.kind, e.position)),
        }
    };
    let k0 = key(r0)?;
    for (p, r) in &results[1..] {
        let k = key(r)?;
        if k != k0 {
            return Err(format!("{p0} 与 {p} 不一致\n  {p0}: {k0}\n  {p}: {k}"));
        }
    }
    Ok(())
}

#[test]
fn differential_read_paths_samples_and_data() {
    let mut inputs: Vec<(String, Vec<u8>)> = SAMPLES
        .iter()
        .map(|s| (format!("{s:?}"), s.as_bytes().to_vec()))
        .collect();
    for extra in [
        "<r>&#xD;&#xD;\r\r\n</r>",
        "<r a=\"x\r\ny\"/>",
        "<r>\u{10000}\u{FFFD}</r>",
        "<r>unterminated",
        "<r><a></r>",
        "\u{FEFF}<r/>",
        "<r>]]></r>",
    ] {
        inputs.push((format!("{extra:?}"), extra.as_bytes().to_vec()));
    }
    let base = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data");
    let mut stack = vec![base];
    while let Some(dir) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&dir) else {
            continue;
        };
        for e in rd.flatten() {
            let p = e.path();
            if p.is_dir() {
                stack.push(p);
            } else if let Ok(b) = std::fs::read(&p)
                && b.len() <= 256 << 10
            {
                inputs.push((p.display().to_string(), b));
            }
        }
    }
    let mut fails = Vec::new();
    for (name, b) in &inputs {
        if let Err(m) = diff_paths(b) {
            fails.push(format!("{name}: {m}"));
        }
    }
    eprintln!(
        "四路径差分：{} 个输入，{} 个不一致",
        inputs.len(),
        fails.len()
    );
    assert!(fails.is_empty(), "{}", fails.join("\n"));
}

/// 树写出的结果走五条读取路径，结构都和树一致（统一断言的 all_paths 版本）。
#[test]
fn differential_readback_of_serialized_tree() {
    let (mut doc, r) = new_doc_with_root_ns(Name::qualified(A, Some("p"), "r"), &[(Some("p"), A)]);
    doc.set_attribute(r, Name::local("a"), "\t\n\r<&\"")
        .unwrap();
    for i in 0..300 {
        let t = doc.create_text(&format!("]]>&{i}\r")).unwrap();
        doc.append_child(r, t).unwrap();
        let c = doc.create_cdata("<cdata>").unwrap();
        doc.append_child(r, c).unwrap();
        add_el(&mut doc, r, Name::qualified(A, Some("p"), "e"));
    }
    assert_serializes(&doc);
}

// ───────────────────────────── 其他断言 ─────────────────────────────

#[test]
fn removed_node_ops_fail_cleanly() {
    let (mut doc, r) = new_doc_with_root(Name::local("r"));
    let e = add_el(&mut doc, r, Name::local("e"));
    doc.remove(e).unwrap();
    assert_err_kind(doc.append_child(r, e), "RemovedNode");
    assert_err_kind(doc.set_attribute(e, Name::local("a"), "v"), "RemovedNode");
    assert_err_kind(doc.detach(e), "RemovedNode");
    assert_err_kind(doc.remove(e), "RemovedNode");
    // 新建节点不复用旧 id
    let f = doc.create_element(Name::local("f")).unwrap();
    assert_ne!(e, f);
    assert!(doc.get(e).is_none());
    assert_eq!(assert_serializes(&doc), "<r/>");
}

#[test]
fn hierarchy_cycles_rejected() {
    let (mut doc, r) = new_doc_with_root(Name::local("r"));
    let a = add_el(&mut doc, r, Name::local("a"));
    let b = add_el(&mut doc, a, Name::local("b"));
    assert_err_kind(doc.append_child(b, a), "Hierarchy");
    assert_err_kind(doc.append_child(a, a), "Hierarchy");
    let rt = root(&doc);
    assert_err_kind(doc.append_child(b, rt), "Hierarchy");
    let t = doc.create_text("t").unwrap();
    doc.append_child(b, t).unwrap();
    let u = doc.create_text("u").unwrap();
    assert_err_kind(doc.append_child(t, u), "Hierarchy");
    assert_err_kind(doc.insert_before(u, rt), "Hierarchy");
    assert_eq!(assert_serializes(&doc), "<r><a><b>t</b></a></r>");
}

#[test]
fn node_kinds_reported() {
    let (mut doc, r) = new_doc_with_root(Name::local("r"));
    let t = doc.create_text("t").unwrap();
    doc.append_child(r, t).unwrap();
    assert_eq!(doc.get(t).unwrap().kind(), Some(NodeKind::Text));
    assert_eq!(doc.get(r).unwrap().kind(), Some(NodeKind::Element));
    let toks = canon_doc(&doc);
    assert_eq!(
        toks,
        vec![
            Tok::Open(None, "r".into(), vec![]),
            Tok::Text("t".into()),
            Tok::Close
        ]
    );
}

// ---- 新增声明不能悄悄改变已有名字的命名空间（fuzz dom_mutation 冒烟在 19bb758 发现 D-C2-3，e880f3c 已修，回归锁定） ----

/// 无命名空间的元素上新增默认声明：要么 Err(Namespace)，要么输出仍与树一致。
#[test]
fn ns_new_default_decl_on_unqualified_element() {
    let (mut doc, r) = new_doc_with_root(Name::local("r"));
    match doc.declare_namespace(r, None, "urn:a") {
        Err(e) => assert!(matches!(e, DomError::Namespace), "{e:?}"),
        Ok(()) => {
            assert_unified(&doc);
        }
    }
}

/// 中间元素上新增默认声明，下面无命名空间的孙元素会被“吸进去”。
#[test]
fn ns_new_default_decl_captures_unqualified_descendant() {
    let (mut doc, r) = new_doc_with_root(Name::local("r"));
    let c = add_el(&mut doc, r, Name::local("c"));
    add_el(&mut doc, c, Name::local("g"));
    match doc.declare_namespace(c, None, "urn:a") {
        Err(e) => assert!(matches!(e, DomError::Namespace), "{e:?}"),
        Ok(()) => {
            assert_unified(&doc);
        }
    }
}

/// 中间元素上新增同前缀声明，遮蔽祖先绑定，而孙元素仍在用祖先的绑定。
#[test]
fn ns_new_prefixed_decl_shadows_binding_in_use() {
    let (mut doc, r) = new_doc_with_root_ns(
        Name::qualified("urn:a", Some("p"), "r"),
        &[(Some("p"), "urn:a")],
    );
    let c = add_el(&mut doc, r, Name::local("c"));
    add_el(&mut doc, c, Name::qualified("urn:a", Some("p"), "g"));
    match doc.declare_namespace(c, Some("p"), "urn:b") {
        Err(e) => assert!(matches!(e, DomError::Namespace), "{e:?}"),
        Ok(()) => {
            assert_unified(&doc);
        }
    }
}

// ---- e880f3c 行为（Implementer 19:0x 说明）：属性前缀解析、自动补声明 ----

/// 带前缀的属性：前缀必须已解析到同一 URI，否则 Err(Namespace) 且元素不变。
#[test]
fn attr_prefix_must_resolve_to_its_uri() {
    let (mut doc, r) = new_doc_with_root(Name::local("r"));
    assert!(matches!(
        doc.set_attribute(r, Name::qualified(A, Some("p"), "k"), "v"),
        Err(DomError::Namespace)
    ));
    assert_eq!(doc.get(r).unwrap().attributes().count(), 0);
    doc.declare_namespace(r, Some("p"), B).unwrap();
    assert!(matches!(
        doc.set_attribute(r, Name::qualified(A, Some("p"), "k"), "v"),
        Err(DomError::Namespace)
    ));
    assert_eq!(doc.get(r).unwrap().attributes().count(), 0);
    assert_eq!(assert_serializes(&doc), r#"<r xmlns:p="urn:b"/>"#);
}

/// 继承来的前缀：序列化在元素上补声明，补的声明不能和已有声明冲突，往返一致。
#[test]
fn attr_inherited_prefix_auto_declared_roundtrip() {
    let (mut doc, r) = new_doc_with_root_ns(Name::qualified(A, Some("p"), "r"), &[(Some("p"), A)]);
    let c = add_el(&mut doc, r, Name::local("c"));
    doc.set_attribute(c, Name::qualified(A, Some("p"), "k"), "v")
        .unwrap();
    let s = assert_serializes(&doc);
    assert!(s.matches(r#"xmlns:p="urn:a""#).count() <= 2, "{s}");
    assert!(!s.contains(r#"xmlns:p="urn:b""#), "{s}");
}

/// 相同展开名、不同前缀：替换旧值，只留一个属性。
#[test]
fn attr_same_expanded_name_other_prefix_replaces() {
    let (mut doc, r) = new_doc_with_root_ns(Name::local("r"), &[(Some("p"), A), (Some("q"), A)]);
    doc.set_attribute(r, Name::qualified(A, Some("p"), "k"), "1")
        .unwrap();
    doc.set_attribute(r, Name::qualified(A, Some("q"), "k"), "2")
        .unwrap();
    let n = doc.get(r).unwrap();
    assert_eq!(n.attributes().count(), 1);
    assert_eq!(n.attribute_ns(Some(A), "k"), Some("2"));
    assert_serializes(&doc);
}

/// 带前缀属性的元素被移进前缀绑定到别处的作用域：要么 Err(Namespace)，要么自动补声明后往返一致。
#[test]
fn attr_auto_declaration_after_move_into_conflicting_scope() {
    let (mut doc, r) = new_doc_with_root_ns(Name::qualified(A, Some("p"), "r"), &[(Some("p"), A)]);
    let other = add_el(&mut doc, r, Name::local("o"));
    doc.declare_namespace(other, Some("p"), B).unwrap();
    let c = add_el(&mut doc, r, Name::local("c"));
    doc.set_attribute(c, Name::qualified(A, Some("p"), "k"), "v")
        .unwrap();
    let before = canon_doc(&doc);
    match doc.append_child(other, c) {
        Err(e) => {
            assert!(matches!(e, DomError::Namespace), "{e:?}");
            assert_eq!(canon_doc(&doc), before, "失败的移动不能改树");
        }
        Ok(()) => {
            assert_eq!(
                doc.get(c).unwrap().attribute_ns(Some(A), "k"),
                Some("v"),
                "移动后属性仍在 urn:a"
            );
        }
    }
    assert_serializes(&doc);
}

/// 元素自己声明 p→B 后，再加 p:k（urn:a）属性：必须 Err(Namespace)（同一元素上 p 不能有两个绑定）。
#[test]
fn attr_prefix_conflicts_with_own_declaration() {
    let (mut doc, r) = new_doc_with_root_ns(Name::qualified(A, Some("p"), "r"), &[(Some("p"), A)]);
    let c = add_el(&mut doc, r, Name::local("c"));
    doc.set_attribute(c, Name::qualified(A, Some("p"), "k"), "v")
        .unwrap();
    // c 上新增 p→B 会改变已有属性的命名空间：要么拒绝，要么往返一致
    match doc.declare_namespace(c, Some("p"), B) {
        Err(e) => assert!(matches!(e, DomError::Namespace), "{e:?}"),
        Ok(()) => {
            assert_unified(&doc);
        }
    }
    assert_serializes(&doc);
}

/// 注释以单个 - 结尾会和 --> 拼成 --->：创建和 set_text 都必须 InvalidChar；以 - 开头是合法的。
#[test]
fn esc_comment_trailing_dash_exact() {
    let mut doc = Document::new();
    for t in ["-", "a-", "a -", "x--", "--", "a--b"] {
        assert!(
            matches!(doc.create_comment(t), Err(DomError::InvalidChar)),
            "{t:?}"
        );
    }
    let (mut doc, r) = new_doc_with_root(Name::local("r"));
    let c = doc.create_comment("-a").unwrap();
    doc.append_child(r, c).unwrap();
    for t in ["-", "b-", "--"] {
        assert!(
            matches!(doc.set_text(c, t), Err(DomError::InvalidChar)),
            "{t:?}"
        );
    }
    assert_eq!(doc.get(c).unwrap().text(), Some("-a"));
    let s = assert_serializes(&doc);
    assert_eq!(s, "<r><!---a--></r>");
    assert!(!s.contains("--->"));
}

/// 精确拒绝：每个修改入口遇到非法字符 / 结束标记都返回 InvalidChar，且不改树。
#[test]
fn invalid_char_exact_on_every_mutation_entry() {
    for bad in ["\u{1}", "\u{FFFE}", "a\u{0}b", "\u{FFFF}"] {
        let (mut doc, r) = new_doc_with_root_ns(Name::local("r"), &[(Some("p"), A)]);
        let t = add_text(&mut doc, r, "t");
        let cm = doc.create_comment("c").unwrap();
        doc.append_child(r, cm).unwrap();
        let cd = doc.create_cdata("d").unwrap();
        doc.append_child(r, cd).unwrap();
        let pi = doc.create_pi("t", Some("p")).unwrap();
        doc.append_child(r, pi).unwrap();
        let before = canon_doc(&doc);
        assert!(matches!(doc.set_text(t, bad), Err(DomError::InvalidChar)));
        assert!(matches!(doc.set_text(cm, bad), Err(DomError::InvalidChar)));
        assert!(matches!(doc.set_text(cd, bad), Err(DomError::InvalidChar)));
        assert!(matches!(
            doc.set_pi_data(pi, Some(bad)),
            Err(DomError::InvalidChar)
        ));
        assert!(matches!(
            doc.set_attribute(r, Name::local("a"), bad),
            Err(DomError::InvalidChar)
        ));
        assert!(matches!(
            doc.set_attribute(r, Name::qualified(A, Some("p"), "a"), bad),
            Err(DomError::InvalidChar)
        ));
        assert!(matches!(doc.create_text(bad), Err(DomError::InvalidChar)));
        assert!(matches!(
            doc.create_comment(bad),
            Err(DomError::InvalidChar)
        ));
        assert!(matches!(doc.create_cdata(bad), Err(DomError::InvalidChar)));
        assert!(matches!(
            doc.create_pi("t", Some(bad)),
            Err(DomError::InvalidChar)
        ));
        assert_eq!(canon_doc(&doc), before, "{bad:?}");
    }
    let (mut doc, r) = new_doc_with_root(Name::local("r"));
    let cd = doc.create_cdata("d").unwrap();
    doc.append_child(r, cd).unwrap();
    let pi = doc.create_pi("t", None).unwrap();
    doc.append_child(r, pi).unwrap();
    assert!(matches!(
        doc.create_cdata("]]>"),
        Err(DomError::InvalidChar)
    ));
    assert!(matches!(
        doc.set_text(cd, "a]]>"),
        Err(DomError::InvalidChar)
    ));
    assert!(matches!(
        doc.create_pi("t", Some("?>")),
        Err(DomError::InvalidChar)
    ));
    assert!(matches!(
        doc.set_pi_data(pi, Some("a?>b")),
        Err(DomError::InvalidChar)
    ));
    assert_eq!(assert_serializes(&doc), "<r><![CDATA[d]]><?t?></r>");
}

/// 文本和属性一律转义成字符引用（Implementer 说明），输出精确锁定。
#[test]
fn esc_text_and_attr_exact_output() {
    let (mut doc, r) = new_doc_with_root(Name::local("r"));
    doc.set_attribute(r, Name::local("a"), "<&\"'>\t\n\r")
        .unwrap();
    add_text(&mut doc, r, "<&>\r");
    let s = assert_serializes(&doc);
    for raw in ['\t', '\n', '\r', '<'] {
        let start_tag = &s[..s.find('>').unwrap()];
        assert!(
            !start_tag[1..].contains(raw),
            "属性里有原样的 {raw:?}：{s:?}"
        );
    }
    assert!(!s.contains('\r'), "文本里的 CR 必须写成字符引用：{s:?}");
}

/// D-C2-4（e880f3c 回归，c0f49a0 修复）：默认命名空间作用域下的无前缀属性不属于任何命名空间，
/// 序列化不能报错，也不能为它找或声明前缀。`<r xmlns="urn:b" a="2"/>` 必须能写出并 round-trip。
#[test]
fn ser_unprefixed_attr_under_default_namespace() {
    let (mut doc, r) = new_doc_with_root_ns(Name::qualified(B, None, "r"), &[(None, B)]);
    doc.set_attribute(r, Name::local("a"), "2").unwrap();
    assert_eq!(assert_serializes(&doc), r#"<r xmlns="urn:b" a="2"/>"#);
    let (mut doc, r) = new_doc_with_root_ns(
        Name::qualified(A, Some("p"), "r"),
        &[(Some("p"), A), (None, B)],
    );
    doc.set_attribute(r, Name::local("a"), "2").unwrap();
    let s = assert_serializes(&doc);
    // 只有用户声明的两个，序列化不能为无前缀属性另外发明前缀或声明
    assert_eq!(s.matches("xmlns").count(), 2, "{s}");
    assert!(s.contains(" a=\"2\""), "{s}");
    // 子元素上的无前缀属性同理：不能为它发明前缀声明，属性保持无前缀
    // （子元素上重复写出的 xmlns="urn:b" 是另一回事，见 v10.7 观察项 O-C2-5，这里不锁）
    let c = add_el(&mut doc, r, Name::qualified(B, None, "c"));
    doc.set_attribute(c, Name::local("b"), "3").unwrap();
    let s = assert_serializes(&doc);
    assert_eq!(s.matches("xmlns:").count(), 1, "{s}");
    assert!(s.contains(" b=\"3\""), "{s}");
}
