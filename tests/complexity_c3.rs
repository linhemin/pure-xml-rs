//! C3 复杂度硬门槛回归（Lead 2026-11-04 19:55 裁定；v10.9 按 Lead 2026-11-10 C3 设计裁定 Q3/Q5 调整）。
//!
//! **v10.9 状态：按 C3 设计签名起草，当前 main efb8be1 上无法编译**——依赖
//! `purexml::dom::Document::from_bytes_with_config(&[u8], &ParserConfig) -> Result<Document, purexml::Error>`
//! （C3-design.md §1）。C3 入口落地后应可直接编译；其余部分只用 main 上已有的公开 API。
//!
//! 门槛：以下每条“被测路径”在“深度、属性数、前缀声明数、兄弟数、长 URI”等维度上，耗时对
//! 输入字节数的 log-log 斜率（最小二乘）都必须 ≤ 1.2。每个点取中位数。只认 release。
//! - 解析路径：Reader from_bytes / from_vec / from_reader(8 KiB)
//! - 建树路径：**C3 入口 `Document::from_bytes_with_config`（判定）**；mutation builder
//!   （Reader 事件 → 公开修改 API，tests/common/dom_c2.rs）只记录、不判定，作对照（Lead Q3）
//! - 序列化路径（Lead Q5，硬门槛）：`ser[c3]` = C3 建树（不计时）后 `Document::write_to`；
//!   `ser[reader→writer]` = Reader 事件直通 `Writer::write_event`（D-C3-1 在这条路径上，M1 起就有）
//!
//! 固定前缀、只增加叶子数 L 的两组（Lead 指定的最坏输入）上，L 方向的斜率测不出
//! O(L × 祖先声明数) 这种“线性但系数被放大”的代价，所以这两组另加放大倍数门槛：
//! 叶子部分每字节的边际耗时 ≤ 10 × 前缀部分每字节的平均耗时。同一个最坏输入另有一组
//! “前缀和叶子一起放大”的版本（ns_scaled*），走斜率门槛。
//!
//! D-C3-1（Writer 按 URI 内容哈希，序列化放大）对应两组（v10.9 新增，所有路径都跑）：
//! - `long_uri_joint`：URI 长 U=64 KiB→1 MiB，叶子数 L=U/64 一起放大，走斜率门槛。
//!   O(L×U) 的实现在这里斜率≈2（main efb8be1 实测 reader→writer 1.96）。
//! - `long_uri_amp`：固定 512 KiB URI，L=0/1k/4k/16k/64k 个 `<p:e p:a=""/>` 叶子；
//!   叶子边际耗时（每叶）≤ 10 × 同样叶子在 16 字节 URI 下的边际耗时。
//!   不用“相对前缀每字节”的基线：前缀是一整块 512 KiB 属性值，每字节成本远低于叶子，
//!   正确实现也会被判放大（main 上只读解析就有约 30×）。
//!
//! 序列化输出另有大小上限：输出 ≤ 2 × 输入 + 64 KiB，超过立即判失败（防 O-C2-5 冗余 xmlns
//! 在长 URI 下把输出放大到 L×U 而撑爆内存；main 上 mutation 建树 + to_string 实测 1500×）。
//!
//! 运行（必须 release，绑核，单线程；机器空闲、没有 fuzz 在跑）：
//!   taskset -c 2-5 cargo test --release --test complexity_c3 -- --ignored --test-threads=1 --nocapture
//! 只跑解析路径：加 `parse_`；只跑建树：加 `build_`；只跑序列化：加 `ser_`。
//! mutation 对照：`build_mutation_reference`（只打印，不断言；单点超时只记录）。
//! 单点上限 PUREXML_C3_POINT_CAP_S（默认 90 秒）：任何一次运行超过上限即判不过（视为超线性）。
#![allow(dead_code)]

#[path = "common/dom_c2.rs"]
mod dom_c2;

use std::io::{BufReader, Write};
use std::time::Instant;

use purexml::dom::Document;
use purexml::{ParserConfig, Reader, Writer};

const SLOPE_GATE: f64 = 1.2;
const AMPLIFICATION_GATE: f64 = 10.0;
/// 序列化输出上限：2 × 输入 + 64 KiB。
const OUTPUT_FACTOR: usize = 2;
const OUTPUT_SLACK: usize = 64 << 10;

// ───────────────────────────── 可插拔的建树函数 ─────────────────────────────

/// 输入字节 → DOM。返回值只用来防止优化掉。
pub type BuildFn = fn(&[u8], &ParserConfig) -> Result<(), String>;

/// 对照（只记录、不判定，Lead Q3）：Reader 事件 → 公开修改 API。
fn build_via_mutation(data: &[u8], cfg: &ParserConfig) -> Result<(), String> {
    let events =
        dom_c2::read_bytes_with(data, cfg.clone()).map_err(|(_, e)| format!("解析失败：{e:?}"))?;
    let b = dom_c2::build_with(&events, cfg).map_err(|e| format!("建树失败：{e:?}"))?;
    std::hint::black_box(&b.doc);
    Ok(())
}

/// 判定：C3 公开解析→DOM 入口（C3-design.md §1；需要 C3，main efb8be1 上不存在）。
fn build_via_c3(data: &[u8], cfg: &ParserConfig) -> Result<(), String> {
    let doc = c3_doc(data, cfg)?;
    std::hint::black_box(&doc);
    Ok(())
}

fn c3_doc(data: &[u8], cfg: &ParserConfig) -> Result<Document, String> {
    Document::from_bytes_with_config(data, cfg).map_err(|e| format!("C3 解析失败：{e:?}"))
}

/// 判定用的建树函数固定为 C3 入口；`PUREXML_C3_BUILDER` 不再改变判定对象（v10.9）。
fn builder() -> (&'static str, BuildFn) {
    ("c3", build_via_c3)
}

// ───────────────────────────── 解析路径 ─────────────────────────────

fn parse_bytes(data: &[u8], cfg: &ParserConfig) -> Result<(), String> {
    let mut r = Reader::from_bytes(data).with_config(cfg.clone());
    while r.read_event().map_err(|e| format!("{e:?}"))?.is_some() {}
    Ok(())
}
fn parse_vec(data: &[u8], cfg: &ParserConfig) -> Result<(), String> {
    let mut r = Reader::from_vec(data.to_vec()).with_config(cfg.clone());
    while r.read_event().map_err(|e| format!("{e:?}"))?.is_some() {}
    Ok(())
}
fn parse_reader_8k(data: &[u8], cfg: &ParserConfig) -> Result<(), String> {
    let mut r = Reader::from_reader_with_config(BufReader::with_capacity(8192, data), cfg.clone())
        .map_err(|e| format!("{e:?}"))?;
    while r.read_event().map_err(|e| format!("{e:?}"))?.is_some() {}
    Ok(())
}

const PARSE_PATHS: &[(&str, BuildFn)] = &[
    ("from_bytes", parse_bytes),
    ("from_vec", parse_vec),
    ("from_reader8k", parse_reader_8k),
];

// ───────────────────────────── 序列化路径（v10.9） ─────────────────────────────

/// 有上限的计数 sink：超过上限返回 io 错误，避免输出放大撑爆内存。
struct CappedSink {
    written: usize,
    cap: usize,
}

impl Write for CappedSink {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.written += buf.len();
        if self.written > self.cap {
            return Err(std::io::Error::other("输出超过上限"));
        }
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn out_cap(input_len: usize) -> usize {
    input_len * OUTPUT_FACTOR + OUTPUT_SLACK
}

/// 一次被测运行，返回输出字节数（解析/建树路径返回 0）。
type Job = Box<dyn FnMut() -> Result<usize, String>>;
/// 准备一条被测路径：准备工作（如建树）在这里做、不计时，返回只含被测部分的闭包。
type PrepFn = fn(&[u8], &ParserConfig) -> Result<Job, String>;
/// 被测路径的通用形式（fn 指针或闭包）。
type Prep<'a> = &'a dyn Fn(&[u8], &ParserConfig) -> Result<Job, String>;

fn ser_c3(data: &[u8], cfg: &ParserConfig) -> Result<Job, String> {
    let doc = c3_doc(data, cfg)?;
    let cap = out_cap(data.len());
    Ok(Box::new(move || {
        let mut w = Writer::new(CappedSink { written: 0, cap });
        doc.write_to(&mut w)
            .map_err(|e| format!("序列化失败（上限 {cap} B）：{e}"))?;
        Ok(w.into_inner().written)
    }))
}

fn ser_reader_writer(data: &[u8], cfg: &ParserConfig) -> Result<Job, String> {
    let data = data.to_vec();
    let cfg = cfg.clone();
    let cap = out_cap(data.len());
    Ok(Box::new(move || {
        let mut r = Reader::from_bytes(&data).with_config(cfg.clone());
        let mut w = Writer::new(CappedSink { written: 0, cap });
        while let Some(ev) = r.read_event().map_err(|e| format!("{e:?}"))? {
            w.write_event(&ev)
                .map_err(|e| format!("写出失败（上限 {cap} B）：{e}"))?;
        }
        Ok(w.into_inner().written)
    }))
}

const SER_PATHS: &[(&str, PrepFn)] = &[
    ("ser[c3]", ser_c3),
    ("ser[reader→writer]", ser_reader_writer),
];

/// 把解析/建树函数包成被测路径。
fn job_of(f: BuildFn, data: &[u8], cfg: &ParserConfig) -> Job {
    let data = data.to_vec();
    let cfg = cfg.clone();
    Box::new(move || f(&data, &cfg).map(|()| 0))
}

// ───────────────────────────── 计时与拟合 ─────────────────────────────

fn point_cap() -> f64 {
    std::env::var("PUREXML_C3_POINT_CAP_S")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(90.0)
}

fn median(mut v: Vec<f64>) -> f64 {
    v.sort_by(|a, b| a.partial_cmp(b).unwrap());
    v[v.len() / 2]
}

/// 单次耗时的中位数（秒）和输出字节数。短点内循环到 ≥ 20 ms、取 7 次；> 1 s 的点取 3 次
/// （首次兼作预热）。任何一次超过单点上限返回 Err。
fn measure(job: &mut Job) -> Result<(f64, usize), String> {
    let cap = point_cap();
    let t = Instant::now();
    let out = job()?;
    let one = t.elapsed().as_secs_f64();
    if one > cap {
        return Err(format!("单次 {one:.1} s 超过上限 {cap} s"));
    }
    let (inner, reps) = if one > 1.0 {
        (1, 3)
    } else {
        job()?; // 预热
        (
            ((0.02 / one.max(1e-9)).ceil() as usize).clamp(1, 100_000),
            7,
        )
    };
    let mut v = Vec::with_capacity(reps);
    for _ in 0..reps {
        let t = Instant::now();
        for _ in 0..inner {
            job()?;
        }
        let dt = t.elapsed().as_secs_f64();
        if dt / inner as f64 > cap {
            return Err(format!("单次 {:.1} s 超过上限 {cap} s", dt / inner as f64));
        }
        v.push(dt / inner as f64);
    }
    Ok((median(v), out))
}

/// 最小二乘 log-log 斜率。
pub fn slope(xs: &[f64], ys: &[f64]) -> f64 {
    let n = xs.len() as f64;
    let lx: Vec<f64> = xs.iter().map(|x| x.ln()).collect();
    let ly: Vec<f64> = ys.iter().map(|y| y.ln()).collect();
    let mx = lx.iter().sum::<f64>() / n;
    let my = ly.iter().sum::<f64>() / n;
    let num: f64 = lx.iter().zip(&ly).map(|(a, b)| (a - mx) * (b - my)).sum();
    let den: f64 = lx.iter().map(|a| (a - mx).powi(2)).sum();
    num / den
}

// ───────────────────────────── 输入生成 ─────────────────────────────

fn chain(d: usize) -> Vec<u8> {
    let mut s = String::with_capacity(7 * d);
    for _ in 0..d {
        s.push_str("<a>");
    }
    for _ in 0..d {
        s.push_str("</a>");
    }
    s.into_bytes()
}

fn siblings(n: usize) -> Vec<u8> {
    let mut s = String::from("<r>");
    for _ in 0..n {
        s.push_str("<e/>");
    }
    s.push_str("</r>");
    s.into_bytes()
}

fn attrs_one(a: usize) -> Vec<u8> {
    let mut s = String::from("<r");
    for i in 0..a {
        s.push_str(&format!(" a{i}=\"v\""));
    }
    s.push_str("/>");
    s.into_bytes()
}

/// d 层链，每层声明 k 个前缀，最底层放 leaves 个 `<x a0="" …/>`（每个 a 个无前缀属性）。
/// 无前缀的叶子和属性，命名空间查找要走完整条链。
pub fn ns_deep(d: usize, k: usize, leaves: usize, a: usize) -> Vec<u8> {
    let mut s = String::new();
    for lvl in 0..d {
        s.push_str("<a");
        for j in 0..k {
            s.push_str(&format!(" xmlns:p{lvl}_{j}=\"u\""));
        }
        s.push('>');
    }
    let mut leaf = String::from("<x");
    for i in 0..a {
        leaf.push_str(&format!(" a{i}=\"\""));
    }
    leaf.push_str("/>");
    for _ in 0..leaves {
        s.push_str(&leaf);
    }
    for _ in 0..d {
        s.push_str("</a>");
    }
    s.into_bytes()
}

/// D-C3-1：`<r xmlns:p="U">` + L × `<p:e p:a=""/>`，U 为 `u` 重复 uri_len 次。
pub fn long_uri(uri_len: usize, leaves: usize) -> Vec<u8> {
    let mut s = String::with_capacity(uri_len + 16 * leaves + 32);
    s.push_str("<r xmlns:p=\"");
    s.extend(std::iter::repeat_n('u', uri_len));
    s.push_str("\">");
    for _ in 0..leaves {
        s.push_str("<p:e p:a=\"\"/>");
    }
    s.push_str("</r>");
    s.into_bytes()
}

// ───────────────────────────── 门槛 ─────────────────────────────

struct Family {
    name: &'static str,
    cfg: ParserConfig,
    points: Vec<(f64, Vec<u8>)>,
}

fn unlimited_depth() -> ParserConfig {
    ParserConfig::default().with_max_depth(1 << 20)
}

fn families() -> Vec<Family> {
    let d = ParserConfig::default;
    vec![
        Family {
            name: "chain_default(depth 16→256)",
            cfg: d(),
            points: [16, 32, 64, 128, 256]
                .iter()
                .map(|&n| (n as f64, chain(n)))
                .collect(),
        },
        Family {
            name: "chain_unlimited(depth 1k→64k)",
            cfg: unlimited_depth(),
            points: [1024, 2048, 4096, 8192, 16384, 32768, 65536]
                .iter()
                .map(|&n| (n as f64, chain(n)))
                .collect(),
        },
        Family {
            name: "attrs_one(16→1024)",
            cfg: d(),
            points: [16, 32, 64, 128, 256, 512, 1024]
                .iter()
                .map(|&n| (n as f64, attrs_one(n)))
                .collect(),
        },
        Family {
            name: "siblings(1k→1M)",
            cfg: d(),
            points: [1_000, 10_000, 100_000, 1_000_000]
                .iter()
                .map(|&n| (n as f64, siblings(n)))
                .collect(),
        },
        // 每层前缀声明数 k（255 层），叶子数随 k 一起放大：L = 64k
        Family {
            name: "ns_scaled(255层×k前缀, L=64k)",
            cfg: d(),
            points: [64, 128, 256, 512, 1023]
                .iter()
                .map(|&k| (k as f64, ns_deep(255, k, 64 * k, 0)))
                .collect(),
        },
        // 同上，叶子各带 1024 个无前缀属性：L = k/16
        Family {
            name: "ns_scaled_attrs(255层×k前缀, L=k/16, 每叶1024属性)",
            cfg: d(),
            points: [64, 128, 256, 512, 1023]
                .iter()
                .map(|&k| (k as f64, ns_deep(255, k, (k / 16).max(1), 1024)))
                .collect(),
        },
        // D-C3-1：URI 长度与叶子数一起放大，L = U/64
        Family {
            name: "long_uri_joint(U=64K→1M, L=U/64)",
            cfg: d(),
            points: [64usize << 10, 128 << 10, 256 << 10, 512 << 10, 1 << 20]
                .iter()
                .map(|&u| (u as f64, long_uri(u, u / 64)))
                .collect(),
        },
    ]
}

/// 固定前缀（255 层 × 1023 前缀）、只增加叶子数的最坏输入。第一个点 L=0 作基线。
fn worst_families() -> Vec<Family> {
    vec![
        Family {
            name: "worst(255×1023, L=0/1k/4k/16k/64k)",
            cfg: ParserConfig::default(),
            points: [0, 1000, 4000, 16000, 64000]
                .iter()
                .map(|&l| (l as f64, ns_deep(255, 1023, l, 0)))
                .collect(),
        },
        // Lead 要求 L 取 1k…64k；每叶 1024 属性时当前 builder 单叶约 0.3 s，64k 叶要数小时，
        // 所以这一组取 L=0/1/4/16/64（叶子部分 0→约 570 KB）。
        Family {
            name: "worst_attrs(255×1023, 每叶1024属性, L=0/1/4/16/64)",
            cfg: ParserConfig::default(),
            points: [0, 1, 4, 16, 64]
                .iter()
                .map(|&l| (l as f64, ns_deep(255, 1023, l, 1024)))
                .collect(),
        },
    ]
}

fn require_release() {
    let debug = cfg!(debug_assertions);
    assert!(
        !debug,
        "复杂度门槛必须用 --release 跑（taskset -c 2-5 … --test-threads=1）"
    );
}

/// 一族、一条路径：逐点测量，返回 (x, 字节数, 秒)；超时/失败返回 Err。
fn run_family(prep: Prep<'_>, fam: &Family, path: &str) -> Result<Vec<(f64, usize, f64)>, String> {
    let mut rows = Vec::new();
    for (x, data) in &fam.points {
        let ctx = |e: String| format!("{} {path} x={x}: {e}", fam.name);
        let mut job = prep(data, &fam.cfg).map_err(ctx)?;
        let (t, out) = measure(&mut job).map_err(ctx)?;
        println!(
            "C3PERF\t{}\t{path}\t{x}\t{}\t{t:.3e}\tout={out}",
            fam.name,
            data.len()
        );
        rows.push((*x, data.len(), t));
    }
    Ok(rows)
}

/// 门槛结果：Ok(()) 或失败原因。`gate=false` 时只打印（对照）。
fn slope_gate(prep: Prep<'_>, path: &str, gate: bool) -> Vec<String> {
    let mut fails = Vec::new();
    for fam in families() {
        match run_family(prep, &fam, path) {
            Ok(rows) => {
                let xs: Vec<f64> = rows.iter().map(|r| r.1 as f64).collect();
                let ys: Vec<f64> = rows.iter().map(|r| r.2).collect();
                let k = slope(&xs, &ys);
                let verdict = match (gate, k <= SLOPE_GATE) {
                    (false, _) => "REF",
                    (true, true) => "PASS",
                    (true, false) => "FAIL",
                };
                println!(
                    "C3SLOPE\t{}\t{path}\tslope(时间~字节)={k:.2}\t{verdict}",
                    fam.name
                );
                if gate && k > SLOPE_GATE {
                    fails.push(format!("{} {path}: 斜率 {k:.2} > {SLOPE_GATE}", fam.name));
                }
            }
            Err(e) => {
                println!(
                    "C3SLOPE\t{}\t{path}\t失败/超时：{e}\t{}",
                    fam.name,
                    if gate { "FAIL" } else { "REF" }
                );
                if gate {
                    fails.push(e);
                }
            }
        }
    }
    fails
}

fn amplification_gate(prep: Prep<'_>, path: &str, gate: bool) -> Vec<String> {
    let mut fails = Vec::new();
    let tag = |pass: bool| match (gate, pass) {
        (false, _) => "REF",
        (true, true) => "PASS",
        (true, false) => "FAIL",
    };
    for fam in worst_families() {
        match run_family(prep, &fam, path) {
            Ok(rows) => {
                let (_, b0, t0) = rows[0];
                let (_, bn, tn) = *rows.last().unwrap();
                let base = t0 / b0 as f64;
                let marginal = (tn - t0).max(0.0) / (bn - b0) as f64;
                let ratio = marginal / base;
                // 参考：叶子方向的斜率（边际耗时 ~ L），不作门槛
                let tail: Vec<_> = rows[1..].iter().filter(|r| r.2 > t0).collect();
                let k = if tail.len() >= 2 {
                    slope(
                        &tail.iter().map(|r| r.0).collect::<Vec<_>>(),
                        &tail.iter().map(|r| r.2 - t0).collect::<Vec<_>>(),
                    )
                } else {
                    f64::NAN
                };
                println!(
                    "C3AMP\t{}\t{path}\t基线 {:.1} ns/B\t叶子边际 {:.1} ns/B\t放大 {ratio:.1}×\t(参考 L 向斜率 {k:.2})\t{}",
                    fam.name,
                    base * 1e9,
                    marginal * 1e9,
                    tag(ratio <= AMPLIFICATION_GATE)
                );
                if gate && ratio > AMPLIFICATION_GATE {
                    fails.push(format!(
                        "{} {path}: 放大 {ratio:.1}× > {AMPLIFICATION_GATE}×",
                        fam.name
                    ));
                }
            }
            Err(e) => {
                println!(
                    "C3AMP\t{}\t{path}\t失败/超时：{e}\t{}",
                    fam.name,
                    tag(false)
                );
                if gate {
                    fails.push(e);
                }
            }
        }
    }
    // D-C3-1：固定 512 KiB URI，叶子边际（每叶）对照 16 字节 URI 下的同样叶子
    let leaves = [0usize, 1000, 4000, 16000, 64000];
    let mut marg = Vec::new();
    for uri_len in [16usize, 512 << 10] {
        let fam = Family {
            name: if uri_len == 16 {
                "long_uri_amp(U=16, L=0…64k)"
            } else {
                "long_uri_amp(U=512K, L=0…64k)"
            },
            cfg: ParserConfig::default(),
            points: leaves
                .iter()
                .map(|&l| (l as f64, long_uri(uri_len, l)))
                .collect(),
        };
        match run_family(prep, &fam, path) {
            Ok(rows) => {
                let (l0, _, t0) = rows[0];
                let (ln, _, tn) = *rows.last().unwrap();
                marg.push((tn - t0).max(0.0) / (ln - l0));
            }
            Err(e) => {
                println!(
                    "C3AMP\t{}\t{path}\t失败/超时：{e}\t{}",
                    fam.name,
                    tag(false)
                );
                if gate {
                    fails.push(e);
                }
                return fails;
            }
        }
    }
    let ratio = marg[1] / marg[0].max(1e-12);
    println!(
        "C3AMP\tlong_uri_amp(512K vs 16B)\t{path}\t每叶边际 {:.1} ns vs {:.1} ns\t放大 {ratio:.1}×\t{}",
        marg[1] * 1e9,
        marg[0] * 1e9,
        tag(ratio <= AMPLIFICATION_GATE)
    );
    if gate && ratio > AMPLIFICATION_GATE {
        fails.push(format!(
            "long_uri_amp {path}: 每叶边际放大 {ratio:.1}× > {AMPLIFICATION_GATE}×（D-C3-1）"
        ));
    }
    fails
}

fn as_prep(f: BuildFn) -> impl Fn(&[u8], &ParserConfig) -> Result<Job, String> {
    move |d: &[u8], c: &ParserConfig| Ok(job_of(f, d, c))
}

#[test]
#[ignore = "复杂度门槛：release + taskset 跑，见文件头"]
fn parse_paths_linear() {
    require_release();
    let mut fails = Vec::new();
    for (name, f) in PARSE_PATHS {
        fails.extend(slope_gate(&as_prep(*f), name, true));
    }
    assert!(fails.is_empty(), "解析路径超过门槛：\n{}", fails.join("\n"));
}

#[test]
#[ignore = "复杂度门槛：release + taskset 跑，见文件头"]
fn parse_paths_no_amplification() {
    require_release();
    let mut fails = Vec::new();
    for (name, f) in PARSE_PATHS {
        fails.extend(amplification_gate(&as_prep(*f), name, true));
    }
    assert!(
        fails.is_empty(),
        "解析路径放大超过门槛：\n{}",
        fails.join("\n")
    );
}

#[test]
#[ignore = "复杂度门槛：release + taskset 跑，见文件头"]
fn build_linear() {
    require_release();
    let (name, f) = builder();
    let fails = slope_gate(&as_prep(f), &format!("build[{name}]"), true);
    assert!(fails.is_empty(), "建树超过门槛：\n{}", fails.join("\n"));
}

#[test]
#[ignore = "复杂度门槛：release + taskset 跑，见文件头"]
fn build_no_amplification() {
    require_release();
    let (name, f) = builder();
    let fails = amplification_gate(&as_prep(f), &format!("build[{name}]"), true);
    assert!(fails.is_empty(), "建树放大超过门槛：\n{}", fails.join("\n"));
}

/// 对照（Lead Q3）：mutation builder 只记录、不判定。worst_attrs 等点会超时，超时只记录。
#[test]
#[ignore = "对照：release + taskset 跑，只打印 REF，不断言"]
fn build_mutation_reference() {
    require_release();
    let _ = slope_gate(&as_prep(build_via_mutation), "build[mutation]", false);
    let _ = amplification_gate(&as_prep(build_via_mutation), "build[mutation]", false);
}

/// 序列化路径（Lead Q5，硬门槛）。
#[test]
#[ignore = "复杂度门槛：release + taskset 跑，见文件头"]
fn ser_paths_linear() {
    require_release();
    let mut fails = Vec::new();
    for (name, p) in SER_PATHS {
        fails.extend(slope_gate(p, name, true));
    }
    assert!(
        fails.is_empty(),
        "序列化路径超过门槛：\n{}",
        fails.join("\n")
    );
}

#[test]
#[ignore = "复杂度门槛：release + taskset 跑，见文件头"]
fn ser_paths_no_amplification() {
    require_release();
    let mut fails = Vec::new();
    for (name, p) in SER_PATHS {
        fails.extend(amplification_gate(p, name, true));
    }
    assert!(
        fails.is_empty(),
        "序列化路径放大超过门槛：\n{}",
        fails.join("\n")
    );
}

/// 输入生成器自检（不需要 release）：long_uri 是合法 XML，前缀绑定到长 URI。
#[test]
fn long_uri_generator_selftest() {
    let data = long_uri(32, 3);
    let mut r = Reader::from_bytes(&data);
    let mut leaves = 0;
    while let Some(ev) = r.read_event().unwrap() {
        if let purexml::Event::EmptyElement(e) = ev {
            assert_eq!(e.name.namespace.as_deref().map(str::len), Some(32));
            assert_eq!(
                e.attributes[0].name.namespace.as_deref().map(str::len),
                Some(32)
            );
            leaves += 1;
        }
    }
    assert_eq!(leaves, 3);
}

/// 拟合函数自检（不需要 release）。
#[test]
fn slope_fit_selftest() {
    let xs = [1.0, 2.0, 4.0, 8.0];
    assert!((slope(&xs, &xs.map(|x| 3.0 * x)) - 1.0).abs() < 1e-9);
    assert!((slope(&xs, &xs.map(|x| x * x)) - 2.0).abs() < 1e-9);
}
