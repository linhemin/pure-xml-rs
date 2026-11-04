//! 差分测试：purexml 对照 roxmltree（DOM，allow_dtd）与 quick-xml（流式，宽松）。
//!
//! 比较两件事：是否良构；以及（都良构时）元素 / 属性 / 文本序列。
//! 分歧以 W3C 规范判定：W3C 用例按清单 TYPE；自建样本按 README 期望。
//! 报告写到 target/differential-report.txt，并打印前 20 条分歧。
//!
//! 断言（保持开启）：W3C valid/sa 中三方都接受的用例，purexml 与 roxmltree 的结构序列必须一致。
//! 运行：cargo test --release --test differential -- --nocapture

mod common;

use common::{push_text, Node};
use std::path::{Path, PathBuf};

fn rox_structure(input: &[u8]) -> Result<Vec<Node>, String> {
    let text = std::str::from_utf8(input).map_err(|e| format!("utf8: {e}"))?;
    let opt = roxmltree::ParsingOptions {
        allow_dtd: true,
        ..roxmltree::ParsingOptions::default()
    };
    let doc = roxmltree::Document::parse_with_options(text, opt).map_err(|e| e.to_string())?;
    let mut out = Vec::new();
    fn walk(n: roxmltree::Node, out: &mut Vec<Node>) {
        if n.is_element() {
            let mut attrs: Vec<_> = n
                .attributes()
                .map(|a| {
                    (
                        (a.namespace().map(str::to_string), a.name().to_string()),
                        a.value().to_string(),
                    )
                })
                .collect();
            attrs.sort();
            out.push(Node::Start {
                name: (
                    n.tag_name()
                        .namespace()
                        .filter(|u| !u.is_empty())
                        .map(str::to_string),
                    n.tag_name().name().to_string(),
                ),
                attrs,
            });
            for c in n.children() {
                walk(c, out);
            }
            out.push(Node::End);
        } else if n.is_text() {
            push_text(out, n.text().unwrap_or(""));
        }
    }
    walk(doc.root_element(), &mut out);
    Ok(out)
}

fn quick_ok(input: &[u8]) -> Result<(), String> {
    let mut r = quick_xml::Reader::from_reader(input);
    r.config_mut().check_end_names = true;
    r.config_mut().check_comments = true;
    let mut buf = Vec::new();
    loop {
        match r.read_event_into(&mut buf) {
            Ok(quick_xml::events::Event::Eof) => return Ok(()),
            Ok(_) => {}
            Err(e) => return Err(e.to_string()),
        }
        buf.clear();
    }
}

/// purexml 的结构序列只取根元素内部（与 roxmltree 的 root_element 对齐）。
fn pure(input: &[u8]) -> Result<Vec<Node>, String> {
    common::structure(input).map_err(|f| format!("{:?}: {}", f.class, f.detail))
}

/// M2：同一份字节走 from_bytes、from_vec、from_reader（逐字节、7 字节块、8 KiB 默认），
/// 事件序列和错误（kind、position）必须完全相同。TotalInput 按位置判定，不再是例外（计划 §0.1 v7）。
macro_rules! snap_events {
    ($reader:expr) => {{
        let reader = &mut $reader;
        let mut ev = Vec::new();
        let end = loop {
            match reader.read_event() {
                Ok(Some(e)) => ev.push(format!("{e:?}")),
                Ok(None) => break Ok(()),
                Err(e) => break Err((e.kind.clone(), e.position)),
            }
        };
        (ev, end)
    }};
}

struct Chunks<'a>(&'a [u8], usize);
impl std::io::Read for Chunks<'_> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let n = self.1.min(buf.len()).min(self.0.len());
        buf[..n].copy_from_slice(&self.0[..n]);
        self.0 = &self.0[n..];
        Ok(n)
    }
}

/// 返回不一致的路径名（空 = 一致）。
fn path_mismatches(input: &[u8]) -> Vec<String> {
    let mut r = purexml::Reader::from_bytes(input);
    let base = snap_events!(r);
    let same = |label: &str,
                got: (
        Vec<String>,
        Result<(), (purexml::ErrorKind, purexml::Position)>,
    )| {
        let ok = got == base;
        (!ok).then(|| label.to_string())
    };
    let mut out = Vec::new();
    let mut v = purexml::Reader::from_vec(input.to_vec());
    out.extend(same("from_vec", snap_events!(v)));
    for (label, n) in [
        ("from_reader(1)", 1usize),
        ("from_reader(7)", 7),
        ("from_reader(8K)", 8192),
    ] {
        let mut r =
            purexml::Reader::from_reader(std::io::BufReader::with_capacity(1, Chunks(input, n)))
                .expect("from_reader");
        out.extend(same(label, snap_events!(r)));
    }
    out
}

struct Case {
    id: String,
    expect_wf: Option<bool>,
    bytes: Vec<u8>,
    gated: bool,
    /// W3C 规范输出（James Clark canonical form），仅 valid/sa 有
    canonical: Option<Vec<u8>>,
}

fn w3c_cases() -> Vec<Case> {
    let root = std::env::var_os("PUREXML_W3C_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/w3c/cache/xmlconf"));
    let mut out = Vec::new();
    for (dir, wf) in [("valid/sa", true), ("not-wf/sa", false)] {
        let d = root.join("xmltest").join(dir);
        let Ok(rd) = std::fs::read_dir(&d) else {
            continue;
        };
        let mut files: Vec<_> = rd
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|x| x == "xml"))
            .collect();
        files.sort();
        for p in files {
            out.push(Case {
                id: format!("w3c:{dir}/{}", p.file_name().unwrap().to_string_lossy()),
                expect_wf: Some(wf),
                bytes: std::fs::read(&p).unwrap(),
                gated: wf,
                canonical: std::fs::read(d.join("out").join(p.file_name().unwrap())).ok(),
            });
        }
    }
    out
}

fn own_cases() -> Vec<Case> {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data/attacks");
    let mut out = Vec::new();
    let mut files: Vec<_> = std::fs::read_dir(&dir)
        .unwrap()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "xml"))
        .collect();
    files.sort();
    for p in files {
        let name = p.file_name().unwrap().to_string_lossy().to_string();
        out.push(Case {
            expect_wf: None,
            id: format!("attack:{name}"),
            bytes: std::fs::read(&p).unwrap(),
            gated: false,
            canonical: None,
        });
    }
    // 自建良构样本：命名空间、实体、CDATA、换行规范化、属性规范化
    let extra: &[(&str, &str)] = &[
        (
            "ns-default",
            r#"<a xmlns="urn:a" xmlns:b="urn:b"><b:c b:x="1" y="2"/><d xmlns=""/></a>"#,
        ),
        (
            "entity-mix",
            r#"<!DOCTYPE r [<!ENTITY e "x<i>y</i>z">]><r>1&e;2&amp;&#65;</r>"#,
        ),
        ("cdata-merge", "<r>a<![CDATA[<b>]]>c</r>"),
        ("crlf", "<r a=\"1\r\n2\">x\r\ny\rz</r>"),
        ("attr-ws", "<r a=\" \t x \n\"/>"),
        ("charref-attr", "<r a=\"&#9;&#10;&#13;\"/>"),
        (
            "xml-ns",
            r#"<r xml:lang="en"><x xmlns:p="urn:p" p:a="1"/></r>"#,
        ),
        ("pi-comment", "<?p d?><!--c--><r><?q?><!--d--></r>"),
    ];
    for (id, x) in extra {
        out.push(Case {
            id: format!("own:{id}"),
            expect_wf: Some(true),
            bytes: x.as_bytes().to_vec(),
            gated: false,
            canonical: None,
        });
    }
    out
}

/// roxmltree 解析是递归下降，debug 构建下 256 层嵌套样本会撑爆 2 MiB 的默认测试线程栈，
/// 所以整体放到 256 MiB 栈的线程里跑（purexml 自身的栈安全由 security_tests 在 8 MiB 栈下覆盖）。
#[test]
fn differential() {
    std::thread::Builder::new()
        .stack_size(256 << 20)
        .spawn(differential_body)
        .unwrap()
        .join()
        .unwrap();
}

fn differential_body() {
    let mut cases = w3c_cases();
    let w3c_n = cases.len();
    cases.extend(own_cases());
    let mut lines = Vec::new();
    let mut struct_mismatch_gated = Vec::new();
    let (mut agree, mut total, mut quick_only) = (0usize, 0usize, 0usize);
    let mut canon = (0usize, 0usize, Vec::new());
    let mut path_bad = Vec::new();
    for c in &cases {
        total += 1;
        let bad = path_mismatches(&c.bytes);
        if !bad.is_empty() {
            path_bad.push(format!("{}：{}", c.id, bad.join("、")));
        }
        let p = pure(&c.bytes);
        let x = rox_structure(&c.bytes);
        let q = quick_ok(&c.bytes);
        let (pw, xw, qw) = (p.is_ok(), x.is_ok(), q.is_ok());
        let verdict = |w: bool| if w { "WF" } else { "拒绝" };
        let mut note = String::new();
        if pw == xw && pw != qw {
            // quick-xml 是刻意宽松的流式解析器（不查 DTD / 实体 / 名字字符），只计数不列出
            quick_only += 1;
        } else if pw != xw {
            let judged = match c.expect_wf {
                Some(true) => "规范：良构",
                Some(false) => "规范：非良构",
                None => "规范：见 attacks/README",
            };
            // M1 策略：外部实体 / 参数实体一律 EEF，UTF-16 一律 Encoding，不算 purexml 判错
            let by_policy = matches!(&p, Err(e) if e.starts_with("ExternalEntityForbidden")
                || e.starts_with("Encoding") || e.starts_with("SecurityLimit"));
            let purexml_right = if by_policy && !pw {
                None
            } else {
                c.expect_wf.map(|e| e == pw)
            };
            note = format!(
                "良构判定分歧 purexml={} roxmltree={} quick-xml={}；{judged}；purexml {}",
                verdict(pw),
                verdict(xw),
                verdict(qw),
                match purexml_right {
                    Some(true) => "正确",
                    Some(false) => "错误",
                    None => "按安全策略",
                }
            );
            if let Err(e) = &p {
                note.push_str(&format!(
                    "；purexml: {}",
                    e.chars().take(80).collect::<String>()
                ));
            }
            if let Err(e) = &x {
                note.push_str(&format!(
                    "；roxmltree: {}",
                    e.chars().take(80).collect::<String>()
                ));
            }
        } else if let (Ok(a), Ok(b)) = (&p, &x) {
            if let Some(out) = &c.canonical {
                // 以 W3C 规范输出（xmltest/valid/sa/out/*.xml）为准
                let want = rox_structure(out).expect("canonical output must parse");
                canon.0 += 1;
                if *a == want {
                    canon.1 += 1;
                } else {
                    canon.2.push(c.id.clone());
                }
            }
            if a != b {
                let i = a
                    .iter()
                    .zip(b.iter())
                    .position(|(l, r)| l != r)
                    .unwrap_or(a.len().min(b.len()));
                note = format!(
                    "结构分歧（第 {i} 项）purexml={:?} roxmltree={:?}",
                    a.get(i),
                    b.get(i)
                );
                let judge = c.canonical.as_ref().map(|o| rox_structure(o).unwrap());
                match judge {
                    Some(w) if w == *a => note.push_str("；按 W3C 规范输出 purexml 正确"),
                    Some(w) if w == *b => {
                        note.push_str("；按 W3C 规范输出 roxmltree 正确");
                        if c.gated {
                            struct_mismatch_gated.push(c.id.clone());
                        }
                    }
                    Some(_) => note.push_str("；两者都与 W3C 规范输出不同"),
                    None => {
                        if c.gated {
                            struct_mismatch_gated.push(c.id.clone());
                        }
                    }
                }
            }
        }
        if pw == xw && pw != qw {
            // 已计入 quick_only
        } else if note.is_empty() {
            agree += 1;
        } else {
            lines.push(format!("{}\t{}", c.id, note));
        }
    }
    let report = format!(
        "差分测试：共 {total} 例（W3C {w3c_n}，自建 {}），一致 {agree}，\
         purexml↔roxmltree 分歧 {}，仅 quick-xml 不同（其宽松接受）{quick_only}；\
         purexml 对照 W3C 规范输出 {}/{} 一致，不一致：{:?}；\
         from_bytes/from_vec/from_reader(1、7、8K) 四条路径不一致 {} 例：{:?}\n{}\n",
        total - w3c_n,
        lines.len(),
        canon.1,
        canon.0,
        canon.2,
        path_bad.len(),
        path_bad,
        lines.join("\n")
    );
    let out = Path::new(env!("CARGO_MANIFEST_DIR")).join("target/differential-report.txt");
    let _ = std::fs::create_dir_all(out.parent().unwrap());
    std::fs::write(&out, &report).unwrap();
    println!("{}", report.lines().take(21).collect::<Vec<_>>().join("\n"));
    println!("完整报告：{}", out.display());
    assert!(
        path_bad.is_empty(),
        "from_bytes、from_vec、from_reader 结果不一致：{path_bad:?}"
    );
    assert!(
        struct_mismatch_gated.is_empty(),
        "W3C valid/sa 中三方都接受、但结构序列不一致：{struct_mismatch_gated:?}"
    );
}
