//! M1 性能基线（只记录，不设门槛）：1 KB / 1 MB × 配置型 / 属性密集 / 文本密集，
//! 对照 quick-xml（流式，不解实体）与 roxmltree（DOM）。
//! 运行：cargo bench --bench baseline
//! 三方工作量尽量对齐：都遍历所有事件 / 节点，都取出全部属性值与文本（含实体解码）。

use criterion::{black_box, criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};
use std::time::Duration;

/// 配置型：中等嵌套、短文本、少量属性
fn config_like(target: usize) -> String {
    let mut s = String::from("<?xml version=\"1.0\"?>\n<config version=\"1\">\n");
    let mut i = 0;
    while s.len() < target {
        s.push_str(&format!(
            "  <service name=\"svc{i}\" enabled=\"true\">\n    <host>10.0.{}.{}</host>\n    \
             <port>{}</port>\n    <timeout unit=\"ms\">{}</timeout>\n    \
             <!-- comment {i} -->\n    <path>/var/lib/svc{i} &amp; data</path>\n  </service>\n",
            i % 256,
            i % 200,
            8000 + i % 1000,
            100 * (i % 50)
        ));
        i += 1;
    }
    s.push_str("</config>\n");
    s
}

/// 属性密集：每个元素 16 个属性，几乎无文本
fn attr_dense(target: usize) -> String {
    let mut s = String::from("<rows>");
    let mut i = 0;
    while s.len() < target {
        s.push_str("<row");
        for a in 0..16 {
            s.push_str(&format!(" a{a}=\"v{i}_{a}&lt;x\""));
        }
        s.push_str("/>");
        i += 1;
    }
    s.push_str("</rows>");
    s
}

/// 文本密集：长文本节点
fn text_dense(target: usize) -> String {
    let para = "Lorem ipsum dolor sit amet, consectetur adipiscing elit &amp; sed do eiusmod \
                tempor incididunt ut labore et dolore magna aliqua. 中文文本也混在里面。";
    let mut s = String::from("<doc>");
    while s.len() < target {
        s.push_str("<p>");
        for _ in 0..8 {
            s.push_str(para);
        }
        s.push_str("</p>");
    }
    s.push_str("</doc>");
    s
}

fn purexml_drain(x: &[u8]) -> usize {
    use purexml::{Event, Reader};
    let mut r = Reader::from_bytes(x);
    let mut n = 0;
    while let Some(ev) = r.read_event().expect("purexml") {
        match ev {
            Event::StartElement(e) => {
                n += e.attributes.iter().map(|a| a.value.len()).sum::<usize>() + 1
            }
            Event::EmptyElement(e) => {
                n += e.attributes.iter().map(|a| a.value.len()).sum::<usize>() + 1
            }
            Event::Text(t) => n += t.len(),
            _ => n += 1,
        }
    }
    n
}

fn quick_drain(x: &[u8]) -> usize {
    use quick_xml::events::Event;
    let mut r = quick_xml::Reader::from_reader(x);
    let mut buf = Vec::new();
    let mut n = 0;
    loop {
        match r.read_event_into(&mut buf).expect("quick-xml") {
            Event::Start(e) | Event::Empty(e) => {
                for a in e.attributes() {
                    n += a
                        .unwrap()
                        .normalized_value(quick_xml::XmlVersion::Explicit1_0)
                        .unwrap()
                        .len();
                }
                n += 1;
            }
            Event::Text(t) => n += t.xml10_content().len(),
            Event::GeneralRef(_) => n += 1,
            Event::Eof => break,
            _ => n += 1,
        }
        buf.clear();
    }
    n
}

fn rox_drain(x: &str) -> usize {
    let doc = roxmltree::Document::parse(x).expect("roxmltree");
    let mut n = 0;
    for node in doc.descendants() {
        n += node.attributes().map(|a| a.value().len()).sum::<usize>();
        n += node.text().map_or(1, str::len);
    }
    n
}

fn bench(c: &mut Criterion) {
    type Make = fn(usize) -> String;
    let shapes: [(&str, Make); 3] = [
        ("config", config_like),
        ("attr-dense", attr_dense),
        ("text-dense", text_dense),
    ];
    for (size_name, size) in [("1KB", 1usize << 10), ("1MB", 1usize << 20)] {
        for (shape, make) in shapes {
            let input = make(size);
            let mut g = c.benchmark_group(format!("{shape}/{size_name}"));
            g.throughput(Throughput::Bytes(input.len() as u64));
            g.warm_up_time(Duration::from_secs(1));
            g.measurement_time(Duration::from_secs(if size > 4096 { 5 } else { 2 }));
            g.sample_size(30);
            g.bench_with_input(BenchmarkId::new("purexml", input.len()), &input, |b, x| {
                b.iter(|| purexml_drain(black_box(x.as_bytes())))
            });
            g.bench_with_input(
                BenchmarkId::new("quick-xml", input.len()),
                &input,
                |b, x| b.iter(|| quick_drain(black_box(x.as_bytes()))),
            );
            g.bench_with_input(
                BenchmarkId::new("roxmltree", input.len()),
                &input,
                |b, x| b.iter(|| rox_drain(black_box(x))),
            );
            g.finish();
        }
    }
}

criterion_group!(benches, bench);
criterion_main!(benches);
