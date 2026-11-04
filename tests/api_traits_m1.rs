//! M1 第三轮：公开 API 的线程安全、XmlText 行为和借用路径零拷贝（按 Lead 的 XmlText / Arc 决定编写）。
//!
//! - 编译期断言：Event、XmlText、QName、Attribute、Error、ParserConfig 以及各事件结构体都是 Send + Sync；
//!   `Reader<&[u8]>`、`Reader<Vec<u8>>` 和 `Writer<Vec<u8>>` 也是 Send + Sync。
//!   公开类型里只要还有 `Rc`，这个文件就会整体编译失败（在 6b29230 上就是这样），所以单独放一个文件。
//! - 运行期：用 `std::thread::scope` 跨线程借用传递事件；转成 `'static` 后移到 `std::thread::spawn` 线程。
//! - XmlText：as_str、Deref、AsRef、is_borrowed、into_owned、into_cow、`From<XmlText> for Cow`、
//!   Display、Debug、Clone、PartialEq（与 str / &str / String，双向）、Eq、Hash。
//! - 零拷贝：没有实体、不需要规范化的文本和属性值必须 `is_borrowed()`，指针落在输入缓冲内，
//!   读这个事件时分配的字节数与值的长度无关（`from_bytes` 和 `from_str` 两条路径）。
//!
//! 本文件自带计数分配器，不引用 `tests/common`（一个测试二进制只能有一个全局分配器）。

use std::alloc::{GlobalAlloc, Layout, System};
use std::borrow::Cow;
use std::cell::Cell;
use std::collections::hash_map::DefaultHasher;
use std::collections::HashSet;
use std::hash::{Hash, Hasher};
use std::sync::Arc;

use purexml::{
    Attribute, Doctype, EmptyElement, EndElement, Error, Event, Namespace, ParserConfig,
    ProcessingInstruction, QName, Reader, StartElement, Writer, XmlDecl, XmlText,
};

// ───────────── 编译期断言 ─────────────

const _: () = {
    const fn send_sync<T: Send + Sync>() {}
    send_sync::<Event<'static>>();
    send_sync::<XmlText<'static>>();
    send_sync::<QName<'static>>();
    send_sync::<Attribute<'static>>();
    send_sync::<Namespace<'static>>();
    send_sync::<StartElement<'static>>();
    send_sync::<EmptyElement<'static>>();
    send_sync::<EndElement<'static>>();
    send_sync::<ProcessingInstruction<'static>>();
    send_sync::<XmlDecl<'static>>();
    send_sync::<Doctype<'static>>();
    send_sync::<Error>();
    send_sync::<ParserConfig>();
    send_sync::<purexml::WriteError>();
    // 内部满足条件时（输入和 sink 本身是 Send + Sync）
    send_sync::<Reader<&'static [u8]>>();
    send_sync::<Reader<Vec<u8>>>();
    send_sync::<Writer<Vec<u8>>>();
};

// 借用了非 'static 输入的事件也必须能跨线程（scope 线程）
fn assert_send_sync_val<T: Send + Sync>(_: &T) {}

// ───────────── 计数分配器（只统计开启了计数的线程） ─────────────

thread_local! {
    static ON: Cell<bool> = const { Cell::new(false) };
    static BYTES: Cell<usize> = const { Cell::new(0) };
}

struct Counting;

// SAFETY: 全部委托给 System，只在线程本地做计数；thread_local 是 const 初始化、没有析构。
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, l: Layout) -> *mut u8 {
        count(l.size());
        unsafe { System.alloc(l) }
    }
    unsafe fn alloc_zeroed(&self, l: Layout) -> *mut u8 {
        count(l.size());
        unsafe { System.alloc_zeroed(l) }
    }
    unsafe fn realloc(&self, p: *mut u8, l: Layout, n: usize) -> *mut u8 {
        count(n);
        unsafe { System.realloc(p, l, n) }
    }
    unsafe fn dealloc(&self, p: *mut u8, l: Layout) {
        unsafe { System.dealloc(p, l) }
    }
}

#[global_allocator]
static GLOBAL: Counting = Counting;

fn count(n: usize) {
    let _ = ON.try_with(|on| {
        if on.get() {
            BYTES.with(|b| b.set(b.get() + n));
        }
    });
}

/// 运行 f，返回 (结果, 期间本线程分配的总字节数)
fn measure<T>(f: impl FnOnce() -> T) -> (T, usize) {
    BYTES.with(|b| b.set(0));
    ON.with(|o| o.set(true));
    let v = f();
    ON.with(|o| o.set(false));
    (v, BYTES.with(|b| b.get()))
}

fn inside(s: &str, buf: &[u8]) -> bool {
    let p = s.as_ptr() as usize;
    let b = buf.as_ptr() as usize;
    p >= b && p + s.len() <= b + buf.len()
}

// ───────────── 'static 转换（只用公开 API） ─────────────

fn st(t: XmlText<'_>) -> XmlText<'static> {
    XmlText::shared(Arc::from(t.into_owned()))
}

fn st_name(q: QName<'_>) -> QName<'static> {
    QName {
        prefix: q.prefix.map(st),
        local: st(q.local),
        namespace: q.namespace,
    }
}

fn st_attrs(a: Vec<Attribute<'_>>) -> Vec<Attribute<'static>> {
    a.into_iter()
        .map(|a| Attribute {
            name: st_name(a.name),
            value: st(a.value),
        })
        .collect()
}

fn st_nss(n: Vec<Namespace<'_>>) -> Vec<Namespace<'static>> {
    n.into_iter()
        .map(|n| Namespace {
            prefix: n.prefix.map(st),
            uri: st(n.uri),
        })
        .collect()
}

fn into_static(ev: Event<'_>) -> Event<'static> {
    match ev {
        Event::StartElement(e) => Event::StartElement(StartElement {
            name: st_name(e.name),
            attributes: st_attrs(e.attributes),
            namespaces: st_nss(e.namespaces),
        }),
        Event::EmptyElement(e) => Event::EmptyElement(EmptyElement {
            name: st_name(e.name),
            attributes: st_attrs(e.attributes),
            namespaces: st_nss(e.namespaces),
        }),
        Event::EndElement(e) => Event::EndElement(EndElement {
            name: st_name(e.name),
        }),
        Event::Text(t) => Event::Text(st(t)),
        Event::CData(t) => Event::CData(st(t)),
        Event::Comment(t) => Event::Comment(st(t)),
        Event::PI(p) => Event::PI(ProcessingInstruction {
            target: st(p.target),
            data: p.data.map(st),
        }),
        Event::XmlDecl(d) => Event::XmlDecl(XmlDecl {
            version: st(d.version),
            encoding: d.encoding.map(st),
            standalone: d.standalone,
        }),
        Event::Doctype(d) => Event::Doctype(Doctype {
            name: st(d.name),
            system_id: d.system_id.map(st),
            public_id: d.public_id.map(st),
            has_internal_subset: d.has_internal_subset,
        }),
    }
}

const DOC: &str = r#"<?xml version="1.0" encoding="UTF-8"?><!DOCTYPE r [<!ENTITY e "ent"><!ATTLIST r d CDATA "dflt">]><r xmlns="urn:a" xmlns:p="urn:p" p:x="1" y="a&amp;b"><!--c--><?pi data?>text &e;<![CDATA[cd]]><p:k/></r>"#;

/// `read_event` 返回的事件借用 `&mut Reader`（流式 API），下一次读之前必须用完，所以这里逐个转成 'static
fn collect_static(input: &str) -> Vec<Event<'static>> {
    let mut r = Reader::from_str(input);
    let mut v = Vec::new();
    while let Some(ev) = r.read_event().unwrap() {
        v.push(into_static(ev));
    }
    v
}

fn describe(evs: &[Event<'_>]) -> Vec<String> {
    let mut w = Writer::new(Vec::new());
    let mut out: Vec<String> = evs
        .iter()
        .map(|e| {
            w.write_event(e).unwrap();
            format!("{e:?}")
        })
        .collect();
    out.push(String::from_utf8(w.into_inner()).unwrap());
    out
}

// ───────────── 跨线程 ─────────────

#[test]
fn events_cross_scoped_threads_while_borrowing_input() {
    let input = String::from(DOC); // 非 'static 输入
    let mut r = Reader::from_str(&input);
    let mut n = 0;
    while let Some(ev) = r.read_event().unwrap() {
        assert_send_sync_val(&ev);
        let here = describe(std::slice::from_ref(&ev));
        // 借用中的事件：共享给两个 scope 线程（Sync），再把一个 clone 移进 scope 线程（Send）
        let there = std::thread::scope(|s| {
            let a = s.spawn(|| describe(std::slice::from_ref(&ev)));
            let b = s.spawn(|| describe(std::slice::from_ref(&ev)));
            assert_eq!(a.join().unwrap(), b.join().unwrap());
            let owned = ev.clone();
            s.spawn(move || describe(std::slice::from_ref(&owned)))
                .join()
                .unwrap()
        });
        assert_eq!(here, there);
        n += 1;
    }
    assert!(n >= 8);
}

#[test]
fn owned_events_move_to_static_threads() {
    let input = String::from(DOC);
    let events = collect_static(&input);
    drop(input); // 输入释放后，'static 事件仍然有效
    let shared = Arc::new(events);
    let handles: Vec<_> = (0..4)
        .map(|_| {
            let s = Arc::clone(&shared);
            std::thread::spawn(move || describe(&s))
        })
        .collect();
    let outs: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();
    assert!(outs.windows(2).all(|w| w[0] == w[1]));
    let xml = outs[0].last().unwrap();
    assert!(xml.contains("text ent"), "{xml}");
    assert!(xml.contains("d=\"dflt\""), "{xml}");
    // 单个事件也能移到 'static 线程
    let one = shared[2].clone();
    let back = std::thread::spawn(move || one).join().unwrap();
    assert_eq!(back, shared[2]);
}

#[test]
fn reader_and_writer_move_across_threads() {
    let r = Reader::from_reader(std::io::Cursor::new(DOC.as_bytes().to_vec())).unwrap();
    let n = std::thread::spawn(move || {
        let mut r = r;
        let mut n = 0;
        while r.read_event().unwrap().is_some() {
            n += 1;
        }
        n
    })
    .join()
    .unwrap();
    assert!(n >= 8);
    let w = Writer::new(Vec::new());
    let out = std::thread::spawn(move || {
        let mut w = w;
        w.write_empty("a", &[("b", "c")]).unwrap();
        w.into_inner()
    })
    .join()
    .unwrap();
    assert_eq!(out, b"<a b=\"c\"/>");
    let mut r = Reader::from_str("<a></b>");
    let err = loop {
        match r.read_event() {
            Ok(Some(_)) => {}
            Ok(None) => panic!("应报错"),
            Err(e) => break e,
        }
    };
    let msg = std::thread::spawn(move || err.to_string()).join().unwrap();
    assert!(!msg.is_empty());
}

// ───────────── XmlText 行为 ─────────────

fn hash_of<T: Hash + ?Sized>(t: &T) -> u64 {
    let mut h = DefaultHasher::new();
    t.hash(&mut h);
    h.finish()
}

#[test]
#[allow(clippy::cmp_owned)] // 这里就是要测与 String 的 PartialEq
fn xmltext_methods_and_traits() {
    let buf = String::from("abc");
    let b = XmlText::borrowed(&buf);
    let s = XmlText::shared(Arc::from("abc"));
    // as_str / Deref / AsRef
    assert_eq!(b.as_str(), "abc");
    assert_eq!(&*s, "abc");
    assert_eq!(b.len(), 3); // 通过 Deref 调用 str 方法
    assert!(s.starts_with("ab"));
    let r: &str = b.as_ref();
    assert_eq!(r, "abc");
    // is_borrowed
    assert!(b.is_borrowed());
    assert!(!s.is_borrowed());
    assert_eq!(b.as_str().as_ptr(), buf.as_ptr(), "borrowed 必须指向原缓冲");
    // Clone 保持变体，clone 借用值不分配
    let (bc, n) = measure(|| b.clone());
    assert!(bc.is_borrowed());
    assert_eq!(n, 0, "clone 借用值不应分配");
    let (sc, n) = measure(|| s.clone());
    assert!(!sc.is_borrowed());
    assert_eq!(n, 0, "clone 共享值只应增加引用计数");
    // PartialEq：与另一个变体、str、&str、String 双向
    assert_eq!(b, s);
    assert!(b == *"abc");
    assert!(b == "abc");
    assert!(s == String::from("abc"));
    assert!(*"abc" == b);
    assert!("abc" == s);
    assert!(String::from("abc") == b);
    assert!(b != "abd");
    // Eq + Hash：按内容
    assert_eq!(hash_of(&b), hash_of(&s));
    let mut set = HashSet::new();
    set.insert(b.clone());
    assert!(
        !set.insert(s.clone()),
        "内容相同的两个变体在 HashSet 里应视为同一个键"
    );
    // Display
    assert_eq!(format!("{b}|{s}"), "abc|abc");
    // Display 的宽度和对齐（D24）拆到 xmltext_display_respects_width，M2 处理
    // into_owned / into_cow / From<XmlText> for Cow：借用值转 Cow 时保持借用、不分配
    let (cow, n) = measure(|| b.clone().into_cow());
    assert!(matches!(cow, Cow::Borrowed("abc")));
    assert_eq!(n, 0);
    let cow: Cow<'_, str> = Cow::from(b.clone());
    assert!(matches!(cow, Cow::Borrowed(_)));
    let cow: Cow<'_, str> = s.clone().into();
    assert_eq!(cow, "abc");
    assert_eq!(b.clone().into_owned(), String::from("abc"));
    assert_eq!(s.clone().into_owned(), String::from("abc"));
}

/// D24（M2）：Display 应遵守格式化宽度和对齐（从 xmltext_methods_and_traits 原样拆出，断言不变）
#[test]
fn xmltext_display_respects_width() {
    let buf = String::from("abc");
    let b = XmlText::borrowed(&buf);
    assert_eq!(format!("{b:>5}"), "  abc", "Display 应遵守格式化宽度");
}

/// Debug 和 Eq、Hash 一样只取决于内容：两个变体的 Debug 输出必须相同，并且与 `str` 的 Debug 一致。
/// 否则 assert_eq! 的报错、日志和快照测试会随内部表示变化（第三轮 writer_roundtrip 的 27 个假崩溃就是因此产生的）。
#[test]
fn xmltext_debug_is_content_based() {
    let b = XmlText::borrowed("a\"b");
    let s = XmlText::shared(Arc::from("a\"b"));
    assert_eq!(format!("{b:?}"), format!("{s:?}"));
    assert_eq!(format!("{b:?}"), format!("{:?}", "a\"b"));
}

// ───────────── 零拷贝 ─────────────

/// 零拷贝扫描：from_bytes 和 from_str 两条路径，返回没有借用输入（或不在输入缓冲内）的字段。
/// 每个事件的分配量上限（与 1 MiB 值长度无关）和检查点数量在扫描内直接断言。
fn zero_copy_violations() -> Vec<String> {
    let mut all_bad: Vec<String> = Vec::new();
    let big = "v".repeat(1 << 20);
    let doc = format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?><!DOCTYPE r SYSTEM \"s.dtd\"><r xmlns:p=\"urn:p\" a=\"{big}\" p:b=\"plain\">{big}<![CDATA[{big}]]><!--{big}--><?t {big}?><p:k/></r>"
    );
    for mode in ["from_bytes", "from_str"] {
        let mut r = if mode == "from_bytes" {
            Reader::from_bytes(doc.as_bytes())
        } else {
            Reader::from_str(&doc)
        };
        let buf = doc.as_bytes();
        let mut seen = 0;
        let mut bad: Vec<String> = Vec::new();
        loop {
            let (ev, bytes) = measure(|| r.read_event().unwrap());
            let Some(ev) = ev else { break };
            // 1 MiB 的值不能被复制：每个事件的分配量与值长度无关（只允许名字、Vec 之类的小开销）
            assert!(bytes < 16 << 10, "{mode}: {ev:?} 分配了 {bytes} 字节");
            let mut check = |t: &XmlText<'_>, what: &str| {
                if !t.is_borrowed() || !inside(t, buf) {
                    bad.push(format!("{mode}: {what}"));
                }
                seen += 1;
            };
            match &ev {
                Event::XmlDecl(d) => {
                    check(&d.version, "version");
                    check(d.encoding.as_ref().unwrap(), "encoding");
                }
                Event::Doctype(d) => {
                    check(&d.name, "doctype name");
                    check(d.system_id.as_ref().unwrap(), "system id");
                }
                Event::StartElement(e) => {
                    check(&e.name.local, "element name");
                    for a in &e.attributes {
                        check(&a.name.local, "attr name");
                        check(&a.value, "attr value");
                    }
                    for n in &e.namespaces {
                        check(&n.uri, "ns uri");
                        check(n.prefix.as_ref().unwrap(), "ns prefix");
                    }
                }
                Event::EmptyElement(e) => {
                    check(e.name.prefix.as_ref().unwrap(), "prefix");
                    check(&e.name.local, "local");
                }
                Event::EndElement(e) => check(&e.name.local, "end name"),
                Event::Text(t) => check(t, "text"),
                Event::CData(t) => check(t, "cdata"),
                Event::Comment(t) => check(t, "comment"),
                Event::PI(p) => {
                    check(&p.target, "pi target");
                    check(p.data.as_ref().unwrap(), "pi data");
                }
            }
        }
        assert!(seen >= 16, "{mode}: 只检查了 {seen} 处");
        all_bad.extend(bad);
    }
    all_bad
}

/// 除 Namespace.prefix 外的全部字段（与下面的完整断言重叠；保留它是为了在 D25 未修时仍能单独看出其余字段是否回退）
#[test]
fn borrowed_paths_are_zero_copy_m1() {
    let bad: Vec<String> = zero_copy_violations()
        .into_iter()
        .filter(|x| !x.ends_with(": ns prefix"))
        .collect();
    assert!(
        bad.is_empty(),
        "没有借用输入（或不在输入缓冲内）的字段：{bad:?}"
    );
}

/// 完整断言（含 Namespace.prefix）。Lead 已定 D25 在 M1 修，保持开启
#[test]
fn borrowed_paths_are_zero_copy() {
    let bad = zero_copy_violations();
    assert!(
        bad.is_empty(),
        "没有借用输入（或不在输入缓冲内）的字段：{bad:?}"
    );
}

/// 反面对照：需要实体展开或规范化的值必须是自有的，内容正确
#[test]
fn transformed_values_are_owned() {
    let doc = "<!DOCTYPE r [<!ENTITY e \"x\">]><r a=\"1&amp;2\" b=\"t\tu\">a&e;b\r\nc</r>";
    let mut r = Reader::from_str(doc);
    let mut texts = Vec::new();
    while let Some(ev) = r.read_event().unwrap() {
        match ev {
            Event::StartElement(e) => {
                let a = e.attribute("a").unwrap();
                assert!(!a.value.is_borrowed());
                assert_eq!(a.value, "1&2");
                let b = e.attribute("b").unwrap();
                assert!(!b.value.is_borrowed());
                assert_eq!(b.value, "t u");
            }
            Event::Text(t) => texts.push(t.into_owned()),
            _ => {}
        }
    }
    assert_eq!(texts.concat(), "axb\nc");
}
