//! M1 性能基线（只记录，不设门槛）：1 KB / 1 MB × 配置型 / 属性密集 / 文本密集 / 命名空间密集，
//! 对照 quick-xml（流式，不解实体）与 roxmltree（DOM）。
//! 运行：cargo bench --bench baseline
//! 三方工作量尽量对齐：都遍历所有事件 / 节点，都取出全部属性值与文本（含实体解码）。
//! 命名空间：purexml 和 roxmltree 都解析元素的命名空间；quick-xml 用普通 Reader，不解析命名空间（它的 NsReader 是另一套 API）。

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

/// 命名空间密集（第三轮新增）：根上 4 个前缀加默认命名空间，每个元素用带前缀的名字和属性，
/// 每隔一个元素重新声明一个前缀，子元素放在默认命名空间里
fn ns_dense(target: usize) -> String {
    let mut s = String::from(
        "<a:feed xmlns=\"urn:example:default\" xmlns:a=\"http://www.w3.org/2005/Atom\" \
         xmlns:b=\"urn:example:b\" xmlns:c=\"urn:example:c\" xmlns:d=\"urn:example:d\">",
    );
    let mut i = 0;
    while s.len() < target {
        if i % 2 == 0 {
            s.push_str(&format!(
                "<a:entry xmlns:c=\"urn:example:c{}\" b:id=\"e{i}\" c:rank=\"{}\">",
                i % 8,
                i % 100
            ));
        } else {
            s.push_str(&format!("<a:entry b:id=\"e{i}\" c:rank=\"{}\">", i % 100));
        }
        s.push_str(&format!(
            "<title>t{i}</title><d:meta b:k=\"v\" d:v=\"{i}\"/></a:entry>"
        ));
        i += 1;
    }
    s.push_str("</a:feed>");
    s
}

/// 读完全部事件，并读取属性值、命名空间 URI 和文本长度（三方工作量对齐）。
/// 用宏而不是泛型函数，这样不依赖 `Reader` 的类型参数（M2 PR-B 会把 from_reader 的返回类型
/// 改成 `Reader<IoInput<R>>`，宏不用跟着改）。
macro_rules! purexml_drain_reader {
    ($r:expr) => {{
        use purexml::Event;
        let mut r = $r;
        let mut n = 0usize;
        while let Some(ev) = r.read_event().expect("purexml") {
            match ev {
                // 命名空间解析结果也计入，三方都要做的工作量对齐
                Event::StartElement(e) => {
                    n += e.attributes.iter().map(|a| a.value.len()).sum::<usize>()
                        + e.name.namespace.as_deref().map_or(0, str::len)
                        + 1
                }
                Event::EmptyElement(e) => {
                    n += e.attributes.iter().map(|a| a.value.len()).sum::<usize>()
                        + e.name.namespace.as_deref().map_or(0, str::len)
                        + 1
                }
                Event::Text(t) => n += t.len(),
                _ => n += 1,
            }
        }
        n
    }};
}

fn purexml_drain(x: &[u8]) -> usize {
    purexml_drain_reader!(purexml::Reader::from_bytes(x))
}

/// from_reader 路径：`BufReader` 默认容量（8 KiB），与 M2 设计稿 §4.2 的口径一致。
/// M2 只记录，不作门槛；目标是不超过同时段 from_bytes 的 1.3 倍（Lead 08:45 裁定）。
fn purexml_reader_drain(x: &[u8]) -> usize {
    let src = std::io::BufReader::new(x);
    purexml_drain_reader!(purexml::Reader::from_reader(src).expect("purexml from_reader"))
}

/// 测量参数可以用环境变量覆盖，默认值与 M1 相同（30 样本，预热 1 s，测量 2 s / 5 s）。
/// M2 门槛测量用 `PXB_SAMPLES=50 PXB_WARMUP=2 PXB_MEAS_SMALL=5 PXB_MEAS_LARGE=10`。
fn env_u64(key: &str, default: u64) -> u64 {
    std::env::var(key)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
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
        n += node.tag_name().namespace().map_or(0, str::len);
        n += node.text().map_or(1, str::len);
    }
    n
}

fn bench(c: &mut Criterion) {
    type Make = fn(usize) -> String;
    let shapes: [(&str, Make); 4] = [
        ("config", config_like),
        ("attr-dense", attr_dense),
        ("text-dense", text_dense),
        ("ns-dense", ns_dense),
    ];
    for (size_name, size) in [("1KB", 1usize << 10), ("1MB", 1usize << 20)] {
        for (shape, make) in shapes {
            let input = make(size);
            let mut g = c.benchmark_group(format!("{shape}/{size_name}"));
            g.throughput(Throughput::Bytes(input.len() as u64));
            g.warm_up_time(Duration::from_secs(env_u64("PXB_WARMUP", 1)));
            g.measurement_time(Duration::from_secs(if size > 4096 {
                env_u64("PXB_MEAS_LARGE", 5)
            } else {
                env_u64("PXB_MEAS_SMALL", 2)
            }));
            g.sample_size(env_u64("PXB_SAMPLES", 30) as usize);
            g.bench_with_input(BenchmarkId::new("purexml", input.len()), &input, |b, x| {
                b.iter(|| purexml_drain(black_box(x.as_bytes())))
            });
            // from_reader 只测 1 MB 档（M2 只记录，F-2）
            if size > 4096 {
                g.bench_with_input(
                    BenchmarkId::new("purexml-from_reader", input.len()),
                    &input,
                    |b, x| b.iter(|| purexml_reader_drain(black_box(x.as_bytes()))),
                );
            }
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
