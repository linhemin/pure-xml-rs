//! fuzz 目标 writer_roundtrip：解析成功的输入，用 `Writer::write_event` 逐个写出，
//! 再解析写出的结果，两次的规范化事件序列必须相同。
//! - 写出过程不允许出错：解析器产生的事件一定能写成良构 XML
//! - 写出的结果必须能解析成功
//!
//! 规范化规则：相邻的 Text 合并，相邻的 CData 合并（Writer 会把含 `]]>` 的 CDATA 拆段）；
//! Doctype 只比较 name、system_id、public_id（内部子集不会被写出，实体已经展开，属性默认值已补到开始标签上）。
//!
//! 运行：cargo +nightly fuzz run writer_roundtrip -- -max_total_time=600 -timeout=600 -rss_limit_mb=1024
#![no_main]

use libfuzzer_sys::fuzz_target;
use purexml::{Event, Reader, Writer};

#[derive(Debug, PartialEq)]
enum N {
    Open(String, bool),
    Close(String),
    Text(String),
    CData(String),
    Comment(String),
    Pi(String, Option<String>),
    Decl(String, Option<String>, Option<bool>),
    Dtd(String, Option<String>, Option<String>),
}

fn qn(q: &purexml::QName<'_>) -> String {
    // 只比较文本内容：XmlText 的 Debug 会带出 Borrowed/Shared 内部表示，不能参与比较
    format!(
        "{:?}|{}|{:?}",
        q.prefix.as_deref(),
        &*q.local,
        q.namespace.as_deref()
    )
}

fn open(
    name: &purexml::QName<'_>,
    attrs: &[purexml::Attribute<'_>],
    nss: &[purexml::Namespace<'_>],
) -> String {
    let mut a: Vec<String> = attrs
        .iter()
        .map(|x| format!("{}={:?}", qn(&x.name), &*x.value))
        .collect();
    a.sort();
    let mut n: Vec<String> = nss
        .iter()
        .map(|x| format!("{:?}={:?}", x.prefix.as_deref(), &*x.uri))
        .collect();
    n.sort();
    format!("{} {:?} {:?}", qn(name), a, n)
}

fn norm(ev: &Event<'_>, out: &mut Vec<N>) {
    let n = match ev {
        Event::StartElement(e) => N::Open(open(&e.name, &e.attributes, &e.namespaces), false),
        Event::EmptyElement(e) => N::Open(open(&e.name, &e.attributes, &e.namespaces), true),
        Event::EndElement(e) => N::Close(qn(&e.name)),
        Event::Text(t) => {
            if let Some(N::Text(p)) = out.last_mut() {
                p.push_str(t);
                return;
            }
            N::Text(t.to_string())
        }
        Event::CData(t) => {
            if let Some(N::CData(p)) = out.last_mut() {
                p.push_str(t);
                return;
            }
            N::CData(t.to_string())
        }
        Event::Comment(t) => N::Comment(t.to_string()),
        Event::PI(p) => N::Pi(p.target.to_string(), p.data.as_deref().map(str::to_string)),
        Event::XmlDecl(d) => N::Decl(
            d.version.to_string(),
            d.encoding.as_deref().map(str::to_string),
            d.standalone,
        ),
        Event::Doctype(d) => N::Dtd(
            d.name.to_string(),
            d.system_id.as_deref().map(str::to_string),
            d.public_id.as_deref().map(str::to_string),
        ),
    };
    out.push(n);
}

fuzz_target!(|data: &[u8]| {
    let mut r = Reader::from_bytes(data);
    let mut w = Writer::new(Vec::new());
    let mut first = Vec::new();
    let mut write_err = None;
    loop {
        match r.read_event() {
            Ok(Some(ev)) => {
                norm(&ev, &mut first);
                if write_err.is_none() {
                    if let Err(e) = w.write_event(&ev) {
                        write_err = Some(format!("{e:?} 写 {ev:?}"));
                    }
                }
            }
            Ok(None) => break,
            Err(_) => return, // 只检查解析成功的输入
        }
    }
    if let Some(e) = write_err {
        panic!("解析成功的事件写不出去：{e}");
    }
    let out = w.into_inner();
    let mut r2 = Reader::from_bytes(&out);
    let mut second = Vec::new();
    loop {
        match r2.read_event() {
            Ok(Some(ev)) => norm(&ev, &mut second),
            Ok(None) => break,
            Err(e) => panic!(
                "Writer 输出无法重新解析：{e:?}\n输出：{:?}",
                String::from_utf8_lossy(&out)
            ),
        }
    }
    assert_eq!(
        first,
        second,
        "往返后事件不等价\n输出：{:?}",
        String::from_utf8_lossy(&out)
    );
});
