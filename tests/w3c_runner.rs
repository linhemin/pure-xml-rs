//! W3C XML Conformance（xmlts20130923）M1 子集运行器。
//! 门槛子集：xmltest 的 valid/sa 与 not-wf/sa；参考子集（只统计、不计门槛）：valid/not-sa。
//!
//! 准备数据：`tests/w3c/fetch.sh`（下载到 tests/w3c/cache/，不入库）。
//! 运行：`cargo test --release --test w3c_runner -- --nocapture`
//! 环境变量：
//!   PUREXML_W3C_DIR     xmlconf 目录（默认 tests/w3c/cache/xmlconf）；不存在则跳过并提示
//!   PUREXML_W3C_UPDATE=1  重写 tests/w3c/known-failures.txt
//!   PUREXML_W3C_STRICT=1  数据缺失时判失败（本地验收时用）
//!
//! 判定：valid → 必须 Ok；not-wf → 必须 Err（任意类别）。
//! M1 实体策略（Lead 已定）：
//! - `<!DOCTYPE r SYSTEM/PUBLIC ...>` 外部 DTD 不加载、不报错，跳过外部子集继续解析。
//!   所以“引用外部 DTD 但不依赖其内容”的用例必须通过，**不归入 policy-eef**；
//!   依赖外部 DTD 里定义的实体的用例会报 Syntax（未定义实体），按普通失败记录。
//! - 带 SYSTEM/PUBLIC 的实体声明、外部参数实体声明、任何 `%x;` 引用 → ExternalEntityForbidden。
//!   valid 用例只有因为这类 EEF 失败时才归为 `policy-eef`，单独计数，不计入 95% 门槛的分母。
//!
//! 其余失败：不在 known-failures.txt 里的视为回归，测试失败（参考子集同样做回归比对）。

mod common;

use common::{ErrorClass, Guarded, run_guarded};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::time::Duration;

/// 计入 95% 门槛的子集
const GATED: [&str; 2] = ["valid/sa/", "not-wf/sa/"];
/// 只统计、做回归比对，不计门槛（外部 DTD 跳过后应能通过一部分）
const INFO: [&str; 1] = ["valid/not-sa/"];
const SUBSETS: [&str; 3] = ["valid/sa/", "not-wf/sa/", "valid/not-sa/"];
const GATE: f64 = 0.95;

#[derive(Debug)]
struct Case {
    id: String,
    ty: String,
    entities: String,
    uri: String,
    desc: String,
}

/// 极简清单扫描（不依赖被测解析器，也不引第三方库）：逐个取 `<TEST ...>` 的属性。
fn parse_manifest(src: &str) -> Vec<Case> {
    let mut out = Vec::new();
    let mut rest = src;
    while let Some(i) = rest.find("<TEST ") {
        rest = &rest[i + 6..];
        let Some(gt) = rest.find('>') else { break };
        let attrs = &rest[..gt];
        let body_end = rest.find("</TEST>").unwrap_or(gt);
        let desc = rest[gt + 1..body_end.max(gt + 1)]
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");
        let get = |k: &str| -> String {
            let pat = format!("{k}=\"");
            attrs
                .find(&pat)
                .map(|p| {
                    let v = &attrs[p + pat.len()..];
                    v[..v.find('"').unwrap_or(v.len())].to_string()
                })
                .unwrap_or_default()
        };
        out.push(Case {
            id: get("ID"),
            ty: get("TYPE"),
            entities: get("ENTITIES"),
            uri: get("URI"),
            desc,
        });
        rest = &rest[gt..];
    }
    out
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
enum Outcome {
    Pass,
    /// 期望 Ok 但因 ExternalEntityForbidden 失败（M1 策略内）
    PolicyEef,
    /// 按 M1 策略或规范版本差异排除（UTF-16、命名空间约束、第 5 版名字规则）
    Policy(&'static str),
    /// 看似策略内、但经人工判定是缺陷的用例（不排除出分母）
    Defect(&'static str),
    /// 期望 Ok 但报错
    FalseReject(ErrorClass),
    /// 期望 Err 但被接受
    FalseAccept,
    Panic,
    Timeout,
}

impl Outcome {
    fn tag(&self) -> String {
        match self {
            Outcome::Pass => "pass".into(),
            Outcome::PolicyEef => "policy-eef".into(),
            Outcome::Policy(t) => format!("policy-{t}"),
            Outcome::Defect(t) => format!("defect-{t}"),
            Outcome::FalseReject(c) => format!("false-reject:{c:?}"),
            Outcome::FalseAccept => "false-accept".into(),
            Outcome::Panic => "panic".into(),
            Outcome::Timeout => "timeout".into(),
        }
    }
}

/// 人工判定表（见 M1-test-report.md）：
/// - policy-ns：XML 1.0 合法、但违反 XML Namespaces 的名字（M1 解析器启用命名空间）
/// - policy-5e：xmltest 按第 1 版名字字符表判为 not-wf，XML 1.0 第 5 版下合法
/// - defect-*：错误类别看似策略内，但实际是缺陷，计入失败
const OVERRIDES: &[(&str, Outcome)] = &[
    ("valid-sa-012", Outcome::Policy("ns")),
    ("not-wf-sa-140", Outcome::Policy("5e")),
    ("not-wf-sa-141", Outcome::Policy("5e")),
    // ATTLIST 默认值字面量里的 `%` 不是参数实体引用，不应报 EEF（缺陷 D6）
    ("valid-sa-094", Outcome::Defect("attlist-percent-eef")),
    // 实体只在外部 DTD 里定义：按 Lead 决定报 Syntax（未定义实体）
    ("valid-not-sa-031", Outcome::Policy("ext-dtd-entity")),
];

fn is_utf16(bytes: &[u8]) -> bool {
    bytes.starts_with(&[0xFF, 0xFE]) || bytes.starts_with(&[0xFE, 0xFF])
}

fn data_dir() -> PathBuf {
    std::env::var_os("PUREXML_W3C_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/w3c/cache/xmlconf"))
}

fn known_failures_path() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/w3c/known-failures.txt")
}

fn load_known() -> BTreeSet<String> {
    std::fs::read_to_string(known_failures_path())
        .unwrap_or_default()
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .filter_map(|l| l.split('\t').next().map(str::to_string))
        .collect()
}

#[test]
fn w3c_xmltest_subset() {
    let root = data_dir();
    let manifest = root.join("xmltest/xmltest.xml");
    if !manifest.is_file() {
        let msg = format!(
            "W3C 数据不存在：{}，请先运行 tests/w3c/fetch.sh",
            manifest.display()
        );
        if std::env::var_os("PUREXML_W3C_STRICT").is_some() {
            panic!("{msg}");
        }
        eprintln!("跳过：{msg}");
        return;
    }
    let cases = parse_manifest(&std::fs::read_to_string(&manifest).unwrap());
    let selected: Vec<&Case> = cases
        .iter()
        .filter(|c| SUBSETS.iter().any(|s| c.uri.starts_with(s)))
        .collect();
    assert!(!selected.is_empty(), "清单解析失败");

    // (子集, TYPE) -> 各结果计数
    let mut stats: BTreeMap<(String, String), BTreeMap<String, usize>> = BTreeMap::new();
    let mut failures: Vec<(String, String, String)> = Vec::new();

    for c in &selected {
        let path = root.join("xmltest").join(&c.uri);
        let bytes = std::fs::read(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
        let want_ok = c.ty == "valid";
        let utf16 = is_utf16(&bytes);
        let g = run_guarded(&c.id, Duration::from_secs(2), move || {
            common::parse_default(&bytes)
                .map(|_| ())
                .map_err(|f| f.class)
        });
        let outcome = match g {
            Guarded::Done { value: Ok(()), .. } if want_ok => Outcome::Pass,
            Guarded::Done { value: Err(_), .. } if !want_ok => Outcome::Pass,
            Guarded::Done { value: Ok(()), .. } => Outcome::FalseAccept,
            Guarded::Done {
                value: Err(ErrorClass::ExternalEntityForbidden),
                ..
            } => Outcome::PolicyEef,
            // M1 策略：UTF-16 报 Encoding
            Guarded::Done {
                value: Err(ErrorClass::Encoding),
                ..
            } if utf16 => Outcome::Policy("utf16"),
            Guarded::Done {
                value: Err(cls), ..
            } => Outcome::FalseReject(cls),
            Guarded::Panicked(_) => Outcome::Panic,
            Guarded::TimedOut => Outcome::Timeout,
        };
        let outcome = match OVERRIDES.iter().find(|(id, _)| *id == c.id) {
            Some((_, o)) if outcome != Outcome::Pass => o.clone(),
            _ => outcome,
        };
        let subset = SUBSETS.iter().find(|s| c.uri.starts_with(*s)).unwrap();
        *stats
            .entry((subset.trim_end_matches('/').to_string(), c.ty.clone()))
            .or_default()
            .entry(outcome.tag())
            .or_default() += 1;
        if outcome != Outcome::Pass {
            failures.push((
                c.id.clone(),
                outcome.tag(),
                format!("ENTITIES={} {}", c.entities, c.desc),
            ));
        }
    }

    // ── 报表 ──
    println!("\n==== W3C xmltest M1 子集报表 ====");
    let mut gate_fail = Vec::new();
    for ((subset, ty), m) in &stats {
        let total: usize = m.values().sum();
        let pass = m.get("pass").copied().unwrap_or(0);
        let policy: usize = m
            .iter()
            .filter(|(k, _)| k.starts_with("policy-"))
            .map(|(_, v)| *v)
            .sum();
        let denom = total - policy;
        let rate = if denom == 0 {
            1.0
        } else {
            pass as f64 / denom as f64
        };
        println!(
            "{subset:<10} TYPE={ty:<7} 总数 {total:>3}  通过 {pass:>3}  策略排除 {policy:>3}  \
             通过率(不含策略) {:.1}%  明细 {m:?}",
            rate * 100.0
        );
        let gated = GATED.iter().any(|g| g.trim_end_matches('/') == subset);
        if !gated {
            println!("  ↑ 参考子集（{INFO:?}），只统计和做回归比对，不计 95% 门槛");
        }
        if gated && rate < GATE {
            gate_fail.push(format!("{subset} {ty} {:.1}%", rate * 100.0));
        }
    }

    // ── known-failures ──
    if std::env::var_os("PUREXML_W3C_UPDATE").is_some() {
        let mut s = String::from(
            "# W3C xmltest M1 已知失败（自动生成：PUREXML_W3C_UPDATE=1）\n\
             # 格式：ID<TAB>类别<TAB>说明；policy-eef = 按 M1 安全策略拒绝（外部实体/参数实体）；policy-utf16 = M1 不支持 UTF-16；policy-ns = 违反命名空间约束；policy-5e = 第 5 版名字规则下合法；defect-* = 人工判定的缺陷\n",
        );
        for (id, tag, d) in &failures {
            s.push_str(&format!("{id}\t{tag}\t{d}\n"));
        }
        std::fs::write(known_failures_path(), s).unwrap();
        println!("已写入 {}", known_failures_path().display());
    }
    let known = load_known();
    let regressions: Vec<_> = failures
        .iter()
        .filter(|(id, ..)| !known.contains(id))
        .collect();
    let fixed: Vec<_> = known
        .iter()
        .filter(|id| !failures.iter().any(|(f, ..)| f == *id))
        .collect();
    if !fixed.is_empty() {
        println!("已修复（可从 known-failures 删除）：{fixed:?}");
    }
    let hard: Vec<_> = failures
        .iter()
        .filter(|(_, t, _)| t == "panic" || t == "timeout")
        .collect();
    assert!(hard.is_empty(), "panic/超时（永远不允许）：{hard:#?}");
    assert!(
        regressions.is_empty(),
        "新增失败（不在 known-failures.txt）：{regressions:#?}"
    );
    assert!(gate_fail.is_empty(), "未达 95% 门槛：{gate_fail:?}");
}
