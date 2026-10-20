//! M1 安全默认测试（S1–S11）。样本与期望见 tests/data/attacks/README.md。
//!
//! 每个样本：独立线程 + 超时（小样本 2 秒，大样本 10 秒；建议 `--release` 跑）、
//! 峰值内存 < 64 MiB（超过 512 MiB 直接中止进程），不允许 panic，按错误类别断言。
//! 推荐运行：`tests/run-security.sh`（额外加 ulimit -v 兜底）。

mod common;

use common::{run_guarded, ErrorClass, Failure, Guarded, Limits, Summary, SOFT_CAP};
use std::io::ErrorKind as IoErrorKind;
use std::net::TcpListener;
use std::time::Duration;

use ErrorClass::{Encoding, ExternalEntityForbidden as Eef, SecurityLimit, Syntax};

const T_SMALL: Duration = Duration::from_secs(2);
const T_LARGE: Duration = Duration::from_secs(10);
const KIB: usize = 1024;
const MIB: usize = 1024 * 1024;

/// 期望结果
#[derive(Debug, Clone, Copy)]
enum Expect {
    Ok,
    /// 允许的错误类别集合 + 可选的限额种类（Debug 名）
    Err(&'static [ErrorClass], Option<&'static str>),
}

fn check(id: &str, input: Vec<u8>, timeout: Duration, expect: Expect) -> Result<Summary, Failure> {
    check_with(id, input, timeout, expect, Limits::default())
}

fn check_with(
    id: &str,
    input: Vec<u8>,
    timeout: Duration,
    expect: Expect,
    limits: Limits,
) -> Result<Summary, Failure> {
    let len = input.len();
    let r = run_guarded(id, timeout, move || common::parse_with(&input, limits));
    let (result, elapsed, peak) = match r {
        Guarded::Done {
            value,
            elapsed,
            peak_bytes,
        } => (value, elapsed, peak_bytes),
        Guarded::Panicked(m) => panic!("[{id}] 解析器 panic（不允许）：{m}"),
        Guarded::TimedOut => panic!("[{id}] 超时 {timeout:?}（疑似挂起或指数展开）"),
    };
    eprintln!(
        "[{id}] 输入 {len} B，耗时 {elapsed:?}，峰值 {} KiB",
        peak / 1024
    );
    assert!(
        peak < SOFT_CAP,
        "[{id}] 峰值内存 {} MiB 超过 {} MiB",
        peak / MIB,
        SOFT_CAP / MIB
    );
    match (expect, &result) {
        (Expect::Ok, Ok(_)) => {}
        (Expect::Ok, Err(f)) => panic!("[{id}] 期望 Ok，实际 {f:?}"),
        (Expect::Err(classes, _), Ok(_)) => panic!("[{id}] 期望 {classes:?}，实际 Ok（被接受）"),
        (Expect::Err(classes, limit), Err(f)) => {
            assert!(
                classes.contains(&f.class),
                "[{id}] 期望 {classes:?}，实际 {f:?}"
            );
            if f.class == SecurityLimit {
                assert!(f.line >= 1, "[{id}] SecurityLimit 必须带行列号：{f:?}");
                if let Some(k) = limit {
                    assert_eq!(f.limit.as_deref(), Some(k), "[{id}] 限额种类不符：{f:?}");
                }
            }
        }
    }
    result
}

fn ok() -> Expect {
    Expect::Ok
}
fn err(c: &'static [ErrorClass]) -> Expect {
    Expect::Err(c, None)
}
fn limit(kind: &'static str) -> Expect {
    Expect::Err(&[SecurityLimit], Some(kind))
}

macro_rules! sample {
    ($file:literal) => {
        include_bytes!(concat!("data/attacks/", $file)).to_vec()
    };
}

/// 普通样本：只按类别断言。
macro_rules! attack {
    ($name:ident, $file:literal, $expect:expr) => {
        #[test]
        fn $name() {
            check(stringify!($name), sample!($file), T_SMALL, $expect).ok();
        }
    };
}

// ───────────── S1/S2/S3：外部访问必须为零 ─────────────

/// 外部访问检查用的环境：金丝雀文件 + 本地随机监听端口。
struct IoTrap {
    canary: String,
    path: std::path::PathBuf,
    listener: TcpListener,
    port: u16,
}

impl IoTrap {
    fn new(id: &str) -> Self {
        let canary = format!(
            "PUREXML_CANARY_{}_{}",
            std::process::id(),
            id.replace(|c: char| !c.is_ascii_alphanumeric(), "_")
        );
        let path = std::env::temp_dir().join(format!("{canary}.txt"));
        std::fs::write(&path, &canary).expect("write canary");
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        listener.set_nonblocking(true).expect("nonblocking");
        let port = listener.local_addr().unwrap().port();
        IoTrap {
            canary,
            path,
            listener,
            port,
        }
    }

    /// 把样本中的 /etc/passwd、/etc/hostname、127.0.0.1:18080 指向金丝雀文件和监听端口。
    fn rewrite(&self, raw: &[u8]) -> Vec<u8> {
        let text = String::from_utf8(raw.to_vec()).expect("S1-S3/S9 样本应为 UTF-8");
        let p = self.path.to_str().unwrap();
        text.replace("/etc/passwd", p)
            .replace("/etc/hostname", p)
            .replace("127.0.0.1:18080", &format!("127.0.0.1:{}", self.port))
            .into_bytes()
    }

    /// 断言：0 连接、金丝雀内容未泄漏。
    fn assert_clean(&self, id: &str, r: &Result<Summary, Failure>) {
        // 给可能的异步连接一点时间
        std::thread::sleep(Duration::from_millis(100));
        match self.listener.accept() {
            Err(e) if e.kind() == IoErrorKind::WouldBlock => {}
            Ok((_, peer)) => panic!("[{id}] 解析器发起了网络连接：{peer}"),
            Err(e) => panic!("[{id}] accept 出错：{e}"),
        }
        let leaked = match r {
            Ok(s) => s.text.contains(&self.canary),
            Err(f) => f.detail.contains(&self.canary),
        };
        assert!(!leaked, "[{id}] 金丝雀文件内容泄漏到输出");
    }
}

impl Drop for IoTrap {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

fn check_no_io(id: &str, raw: &[u8], expect: Expect) -> Result<Summary, Failure> {
    let trap = IoTrap::new(id);
    let r = check(id, trap.rewrite(raw), T_SMALL, expect);
    trap.assert_clean(id, &r);
    r
}

macro_rules! attack_io {
    ($name:ident, $file:literal, $expect:expr) => {
        #[test]
        fn $name() {
            check_no_io(stringify!($name), &sample!($file), $expect).ok();
        }
    };
}

/// 外部 DTD：必须解析成功、零 IO，并且 Doctype 事件如实带出 PUBLIC/SYSTEM 标识。
fn check_external_dtd(id: &str, raw: &[u8], public: Option<&str>, system_suffix: &str) -> Summary {
    let trap = IoTrap::new(id);
    let r = check(id, trap.rewrite(raw), T_SMALL, ok());
    trap.assert_clean(id, &r);
    let s = r.unwrap();
    let (name, pub_id, sys_id) = s
        .doctype
        .clone()
        .unwrap_or_else(|| panic!("[{id}] 缺少 Doctype 事件"));
    assert_eq!(name, "r", "[{id}] Doctype 名称");
    assert_eq!(pub_id.as_deref(), public, "[{id}] PUBLIC 标识");
    let sys_id = sys_id.unwrap_or_else(|| panic!("[{id}] 缺少 SYSTEM 标识"));
    assert!(
        sys_id.ends_with(system_suffix),
        "[{id}] SYSTEM 标识 {sys_id:?} 应以 {system_suffix:?} 结尾"
    );
    s
}

// S1 XXE file://（带 SYSTEM/PUBLIC 的实体声明 → EEF，声明即拒绝）
attack_io!(s1a_xxe_file, "S1a-xxe-file.xml", err(&[Eef]));
attack_io!(
    s1b_xxe_file_relative,
    "S1b-xxe-file-relative.xml",
    err(&[Eef])
);
attack_io!(s1c_xxe_public, "S1c-xxe-public.xml", err(&[Eef]));
attack_io!(
    s1d_xxe_declared_unused,
    "S1d-xxe-declared-unused.xml",
    err(&[Eef])
);
attack_io!(
    s1e_xxe_in_attribute,
    "S1e-xxe-in-attribute.xml",
    err(&[Eef])
);
attack_io!(
    s1f_xxe_unparsed_ndata,
    "S1f-xxe-unparsed-ndata.xml",
    err(&[Eef])
);
// S2 XXE http://
attack_io!(s2a_xxe_http, "S2a-xxe-http.xml", err(&[Eef]));
attack_io!(s2b_xxe_https, "S2b-xxe-https.xml", err(&[Eef]));
// S3 外部参数实体（声明即 EEF）
attack_io!(s3a_external_pe, "S3a-external-pe.xml", err(&[Eef]));
attack_io!(
    s3h_external_pe_declared_unused,
    "S3h-external-pe-declared-unused.xml",
    err(&[Eef])
);
attack_io!(s3f_oob_exfil, "S3f-oob-exfil.xml", err(&[Eef]));

// S3 外部 DTD：不加载、不报错、跳过外部子集继续解析
#[test]
fn s3b_external_dtd_http() {
    let id = "s3b_external_dtd_http";
    check_external_dtd(id, &sample!("S3b-external-dtd-http.xml"), None, "/ext.dtd");
}

#[test]
fn s3c_external_dtd_file() {
    let id = "s3c_external_dtd_file";
    // SYSTEM 指向金丝雀文件：标识如实给出，但文件内容不得被读入
    check_external_dtd(id, &sample!("S3c-external-dtd-file.xml"), None, ".txt");
}

#[test]
fn s3d_external_dtd_public() {
    let id = "s3d_external_dtd_public";
    check_external_dtd(
        id,
        &sample!("S3d-external-dtd-public.xml"),
        Some("-//EVIL//DTD"),
        "/p.dtd",
    );
}

#[test]
fn s3e_external_dtd_plus_subset() {
    let id = "s3e_external_dtd_plus_subset";
    let s = check_external_dtd(
        id,
        &sample!("S3e-external-dtd-plus-subset.xml"),
        None,
        "/ext.dtd",
    );
    assert!(s.text.contains("fine"), "[{id}] 内部子集实体应正常展开");
}

// 引用只在外部 DTD 里定义的实体 → Syntax（未定义实体），且零 IO
attack_io!(
    s3g_entity_only_in_external_dtd,
    "S3g-entity-only-in-external-dtd.xml",
    err(&[Syntax])
);

// ───────────── S4/S5 展开攻击 ─────────────
attack!(
    s4a_billion_laughs,
    "S4a-billion-laughs.xml",
    err(&[SecurityLimit])
);
attack!(
    s4b_laughs_shallow_wide,
    "S4b-laughs-shallow-wide.xml",
    err(&[SecurityLimit])
);
attack!(
    s4c_laughs_in_attribute,
    "S4c-laughs-in-attribute.xml",
    err(&[SecurityLimit])
);
attack!(
    s5a_quadratic_small,
    "S5a-quadratic-small.xml",
    err(&[SecurityLimit])
);

// ───────────── S6 递归 / 压线边界 ─────────────
attack!(s6a_recursive_ab, "S6a-recursive-ab.xml", err(&[Syntax]));
attack!(s6b_self_ref, "S6b-self-ref.xml", err(&[Syntax]));
attack!(
    s6c_recursive_in_attr,
    "S6c-recursive-in-attr.xml",
    err(&[Syntax])
);
// 嵌套深度：内容里的顶层引用算第 1 层
attack!(s6d_entity_depth_8_ok, "S6d-entity-depth-8-ok.xml", ok());
attack!(
    s6e_entity_depth_9,
    "S6e-entity-depth-9.xml",
    limit("EntityDepth")
);
// 展开次数：每次展开都算
attack!(s6f_expansions_10000_ok, "S6f-expansions-10000-ok.xml", ok());
attack!(
    s6g_expansions_10001,
    "S6g-expansions-10001.xml",
    limit("EntityExpansions")
);
attack!(
    s6h_unbalanced_markup,
    "S6h-unbalanced-markup-in-entity.xml",
    err(&[Syntax])
);
attack!(
    s6i_undeclared_entity,
    "S6i-undeclared-entity.xml",
    err(&[Syntax])
);
// 比例：展开总量 ≤ 64 KiB 不查比例；> 64 KiB 时要求 展开总量 ≤ 输入 × 10
attack!(s6j_ratio_exact_64k_ok, "S6j-ratio-exact-64k-ok.xml", ok());
attack!(
    s6k_ratio_64k_plus1,
    "S6k-ratio-64k-plus1.xml",
    limit("ExpansionRatio")
);
attack!(s6l_ratio_exact_10x_ok, "S6l-ratio-exact-10x-ok.xml", ok());
attack!(
    s6m_ratio_10x_plus,
    "S6m-ratio-10x-plus.xml",
    limit("ExpansionRatio")
);
// 只有流式比例检查才能拦下：前部 ≈92 倍，全文 ≈6.35 倍
attack!(
    s6n_ratio_streaming_only,
    "S6n-ratio-streaming-only.xml",
    limit("ExpansionRatio")
);

/// 流式比例的精确数值：limit = 已消费字节 × 10（已消费 = 最后一个引用 `;` 之后的偏移），
/// actual = 累计替换字节。
fn ratio_numbers(id: &str, raw: Vec<u8>, consumed: u64, actual: u64) {
    let f = check(id, raw, T_SMALL, limit("ExpansionRatio")).unwrap_err();
    assert_eq!(
        f.actual,
        Some(actual),
        "[{id}] actual 应为累计替换字节：{f:?}"
    );
    assert_eq!(
        f.limit_value,
        Some(consumed * 10),
        "[{id}] limit 应为 已消费×10：{f:?}"
    );
}

#[test]
fn s6k_ratio_numbers() {
    let raw = sample!("S6k-ratio-64k-plus1.xml");
    let consumed = raw.windows(3).rposition(|w| w == b"&b;").unwrap() as u64 + 3;
    ratio_numbers("s6k_ratio_numbers", raw, consumed, 65_537);
}

#[test]
fn s6m_ratio_numbers() {
    ratio_numbers(
        "s6m_ratio_numbers",
        sample!("S6m-ratio-10x-plus.xml"),
        20_479,
        204_800,
    );
}

#[test]
fn s6n_ratio_numbers() {
    // 流式口径在第一次“超 64 KiB 且超 10 倍”时就停：第 17 次展开（69632 字节）
    let raw = sample!("S6n-ratio-streaming-only.xml");
    let first_ref = raw.windows(3).position(|w| w == b"&a;").unwrap() as u64;
    ratio_numbers("s6n_ratio_numbers", raw, first_ref + 17 * 3, 17 * 4096);
}

// 预定义实体与字符引用不计入展开次数：各 10_001 次仍应通过
attack!(
    s6o_predefined_and_charrefs_not_counted,
    "S6o-predefined-charrefs-not-counted-ok.xml",
    ok()
);

// ───────────── S7 元素深度 ─────────────
attack!(s7a_depth_256_ok, "S7a-depth-256-ok.xml", ok());
attack!(s7b_depth_257, "S7b-depth-257.xml", limit("ElementDepth"));

// ───────────── S8 名称 / 属性 ─────────────
attack!(s8a_duplicate_attr, "S8a-duplicate-attr.xml", err(&[Syntax]));
attack!(
    s8b_duplicate_attr_ns,
    "S8b-duplicate-attr-ns.xml",
    err(&[Syntax])
);
attack!(s8c_unbound_prefix, "S8c-unbound-prefix.xml", err(&[Syntax]));
attack!(s8d_xmlns_rebind, "S8d-xmlns-rebind.xml", err(&[Syntax]));
attack!(s8e_lt_in_attr, "S8e-lt-in-attr.xml", err(&[Syntax]));
attack!(s8k_attrs_1024_ok, "S8k-attrs-1024-ok.xml", ok());
attack!(s8l_attrs_1025, "S8l-attrs-1025.xml", limit("Attributes"));
attack!(s8m_elem_name_1024_ok, "S8m-elem-name-1024-ok.xml", ok());
attack!(
    s8n_elem_name_1025,
    "S8n-elem-name-1025.xml",
    limit("NameLength")
);
attack!(s8o_attr_name_1024_ok, "S8o-attr-name-1024-ok.xml", ok());
attack!(
    s8p_attr_name_1025,
    "S8p-attr-name-1025.xml",
    limit("NameLength")
);
attack!(
    s8q_name_1024_bytes_multibyte_ok,
    "S8q-name-1024-bytes-multibyte-ok.xml",
    ok()
);
attack!(
    s8r_name_1025_bytes_multibyte,
    "S8r-name-1025-bytes-multibyte.xml",
    limit("NameLength")
);
// 结束标签名超长：限额或标签不匹配都可以，关键是不能绕过、不能崩
attack!(
    s8s_end_name_1025,
    "S8s-end-name-1025.xml",
    err(&[SecurityLimit, Syntax])
);

// ───────────── S9 参数实体（声明不引用 OK；引用 %x; → EEF）─────────────
attack_io!(
    s9a_pe_builds_external,
    "S9a-pe-builds-external.xml",
    err(&[Eef])
);
attack!(
    s9b_pe_in_markup_decl,
    "S9b-pe-in-markup-decl.xml",
    err(&[Eef, Syntax])
);
attack!(
    s9c_pe_internal_simple,
    "S9c-pe-internal-simple.xml",
    err(&[Eef])
);
attack!(s9d_pe_laughs, "S9d-pe-laughs.xml", err(&[Eef]));
attack!(
    s9e_pe_declared_unused_ok,
    "S9e-pe-declared-unused-ok.xml",
    ok()
);

// ───────────── S10 编码 ─────────────
attack!(
    s10a_bad_continuation,
    "S10a-utf8-bad-continuation.xml",
    err(&[Encoding])
);
attack!(
    s10b_lone_continuation,
    "S10b-utf8-lone-continuation.xml",
    err(&[Encoding])
);
attack!(
    s10c_overlong_2,
    "S10c-utf8-overlong-2byte.xml",
    err(&[Encoding])
);
attack!(
    s10d_overlong_3,
    "S10d-utf8-overlong-3byte.xml",
    err(&[Encoding])
);
attack!(
    s10e_overlong_4,
    "S10e-utf8-overlong-4byte.xml",
    err(&[Encoding])
);
attack!(
    s10f_surrogate_high,
    "S10f-utf8-surrogate-high.xml",
    err(&[Encoding])
);
attack!(
    s10g_surrogate_low,
    "S10g-utf8-surrogate-low.xml",
    err(&[Encoding])
);
attack!(
    s10h_above_10ffff,
    "S10h-utf8-above-10ffff.xml",
    err(&[Encoding])
);
attack!(s10i_ff_fe, "S10i-utf8-invalid-ff-fe.xml", err(&[Encoding]));
attack!(
    s10j_truncated_eof,
    "S10j-utf8-truncated-eof.xml",
    err(&[Encoding])
);
attack!(
    s10k_bad_in_name,
    "S10k-utf8-bad-in-name.xml",
    err(&[Encoding])
);
attack!(
    s10l_bad_in_attr,
    "S10l-utf8-bad-in-attr.xml",
    err(&[Encoding])
);
attack!(
    s10m_bad_in_comment,
    "S10m-utf8-bad-in-comment.xml",
    err(&[Encoding])
);
attack!(
    s10n_bad_in_cdata,
    "S10n-utf8-bad-in-cdata.xml",
    err(&[Encoding])
);
attack!(s10o_5byte_seq, "S10o-utf8-5byte-seq.xml", err(&[Encoding]));
attack!(s10p_utf16le_bom, "S10p-utf16le-bom.xml", err(&[Encoding]));
attack!(s10q_utf16be_bom, "S10q-utf16be-bom.xml", err(&[Encoding]));
attack!(
    s10r_utf16_truncated,
    "S10r-utf16-truncated.xml",
    err(&[Encoding])
);
attack!(
    s10s_utf16_declared,
    "S10s-utf16-declared-no-bom.xml",
    err(&[Encoding])
);
attack!(
    s10t_bom8_decl16,
    "S10t-utf8-bom-declares-utf16.xml",
    err(&[Encoding])
);
attack!(
    s10u_bom16_decl8,
    "S10u-utf16le-bom-declares-utf8.xml",
    err(&[Encoding])
);
attack!(s10v_utf32_bom, "S10v-utf32le-bom.xml", err(&[Encoding]));
attack!(
    s10w_unknown_encoding,
    "S10w-unsupported-encoding.xml",
    err(&[Encoding])
);
attack!(
    s10x_illegal_ctrl,
    "S10x-xml-illegal-char-ctrl.xml",
    err(&[Syntax])
);
attack!(s10y_illegal_nul, "S10y-xml-illegal-nul.xml", err(&[Syntax]));
attack!(
    s10z_illegal_fffe,
    "S10z-xml-illegal-fffe.xml",
    err(&[Syntax])
);
attack!(
    s10za_charref_surrogate,
    "S10za-charref-surrogate.xml",
    err(&[Syntax])
);
attack!(s10zb_charref_zero, "S10zb-charref-zero.xml", err(&[Syntax]));
attack!(
    s10zc_charref_too_big,
    "S10zc-charref-too-big.xml",
    err(&[Syntax])
);

// ───────────── 大样本与可调限额（内存中生成，不入库）─────────────

fn doc(subset: &str, body: &str) -> String {
    format!("<?xml version=\"1.0\"?>\n<!DOCTYPE r [\n{subset}\n]>\n{body}\n")
}

/// 在 body 前插入注释填充，使整个文档恰好 `total` 字节。
fn sized(subset: Option<&str>, body: &str, total: usize) -> Vec<u8> {
    let build = |k: usize| {
        let pad = format!("<!--{}-->", "p".repeat(k));
        match subset {
            Some(s) => doc(s, &format!("{pad}{body}")),
            None => format!("{pad}{body}\n"),
        }
    };
    let base = build(0).len();
    assert!(total >= base, "目标长度 {total} 小于最小文档 {base}");
    let out = build(total - base).into_bytes();
    assert_eq!(out.len(), total);
    out
}

/// 实体 a（ent_len 字节）引用 refs 次 + 实体 b（1 字节）引用 extra 次；展开总量 = ent_len*refs + extra。
fn ratio_doc(ent_len: usize, refs: usize, extra: usize, total: Option<usize>) -> Vec<u8> {
    let subset = format!("<!ENTITY a \"{}\">\n<!ENTITY b \"x\">", "A".repeat(ent_len));
    let body = format!("<r>{}{}</r>", "&a;".repeat(refs), "&b;".repeat(extra));
    match total {
        Some(t) => sized(Some(&subset), &body, t),
        None => doc(&subset, &body).into_bytes(),
    }
}

#[test]
fn s5b_quadratic_50k_x_50k() {
    check(
        "s5b",
        ratio_doc(50 * KIB, 50_000, 0, None),
        T_LARGE,
        err(&[SecurityLimit]),
    )
    .ok();
}

#[test]
fn s5c_abs_1mib_exact_ok() {
    // 展开总量恰好 1 MiB；输入 110000 字节，10 倍 = 1.1 MB，不触发比例
    check(
        "s5c",
        ratio_doc(64 * KIB, 16, 0, Some(110_000)),
        T_LARGE,
        ok(),
    )
    .ok();
}

#[test]
fn s5d_abs_1mib_plus1() {
    check(
        "s5d",
        ratio_doc(64 * KIB, 16, 1, Some(110_000)),
        T_LARGE,
        limit("ExpandedBytes"),
    )
    .ok();
}

#[test]
fn s7c_depth_100000() {
    let n = 100_000;
    let s = format!("{}{}", "<a>".repeat(n), "</a>".repeat(n));
    check("s7c", s.into_bytes(), T_LARGE, limit("ElementDepth")).ok();
}

#[test]
fn s7d_depth_100000_unclosed() {
    check(
        "s7d",
        "<a>".repeat(100_000).into_bytes(),
        T_LARGE,
        limit("ElementDepth"),
    )
    .ok();
}

#[test]
fn s8f_long_element_name() {
    let s = format!("<{}/>", "n".repeat(MIB));
    check("s8f", s.into_bytes(), T_LARGE, limit("NameLength")).ok();
}

#[test]
fn s8g_long_attr_name() {
    let s = format!("<r {}=\"1\"/>", "a".repeat(MIB));
    check("s8g", s.into_bytes(), T_LARGE, limit("NameLength")).ok();
}

#[test]
fn s8h_long_attr_value_16mib_plus1() {
    // Implementer 已定：属性值超长报 AttrValueLength，上限取 max_text_len（16 MiB）
    let s = format!("<r a=\"{}\"/>", "v".repeat(16 * MIB + 1));
    check("s8h", s.into_bytes(), T_LARGE, limit("AttrValueLength")).ok();
}

#[test]
fn s8y_attr_value_16mib_ok() {
    let s = format!("<r a=\"{}\"/>", "v".repeat(16 * MIB));
    check("s8y", s.into_bytes(), T_LARGE, ok()).ok();
}

#[test]
fn s8i_many_attrs_100k() {
    let attrs: Vec<String> = (0..100_000).map(|i| format!("a{i}=\"1\"")).collect();
    let s = format!("<r {}/>", attrs.join(" "));
    check("s8i", s.into_bytes(), T_LARGE, limit("Attributes")).ok();
}

#[test]
fn s8j_long_text_64mib() {
    let s = format!("<r>{}</r>", "t".repeat(64 * MIB));
    check("s8j", s.into_bytes(), T_LARGE, limit("TextLength")).ok();
}

#[test]
fn s8t_text_16mib_ok() {
    let s = format!("<r>{}</r>", "t".repeat(16 * MIB));
    check("s8t", s.into_bytes(), T_LARGE, ok()).ok();
}

#[test]
fn s8u_text_16mib_plus1() {
    let s = format!("<r>{}</r>", "t".repeat(16 * MIB + 1));
    check("s8u", s.into_bytes(), T_LARGE, limit("TextLength")).ok();
}

// 注释 / CDATA / PI 也受 max_text_len（TextLength）约束
#[test]
fn s8z_comment_cdata_pi_text_length() {
    let n = 16 * MIB + 1;
    let comment = format!("<r><!--{}--></r>", "c".repeat(n)).into_bytes();
    check("s8z-comment", comment, T_LARGE, limit("TextLength")).ok();
    let cdata = format!("<r><![CDATA[{}]]></r>", "c".repeat(n)).into_bytes();
    check("s8z-cdata", cdata, T_LARGE, limit("TextLength")).ok();
    let pi = format!("<r><?p {}?></r>", "c".repeat(n)).into_bytes();
    check("s8z-pi", pi, T_LARGE, limit("TextLength")).ok();
    // 恰好 16 MiB 的注释通过
    let ok16 = format!("<r><!--{}--></r>", "c".repeat(16 * MIB)).into_bytes();
    check("s8z-comment-ok", ok16, T_LARGE, ok()).ok();
}

// 属性数上限把 xmlns 声明计入：1000 属性 + 24 声明 = 1024 通过；+25 声明 = 1025 报错
#[test]
fn s8za_xmlns_counted_in_attrs() {
    let build = |decls: usize| {
        let mut s = String::from("<r");
        for i in 0..decls {
            s.push_str(&format!(" xmlns:p{i}=\"urn:{i}\""));
        }
        for i in 0..1000 {
            s.push_str(&format!(" a{i}=\"1\""));
        }
        s.push_str("/>");
        s.into_bytes()
    };
    check("s8za-1024", build(24), T_SMALL, ok()).ok();
    check("s8za-1025", build(25), T_SMALL, limit("Attributes")).ok();
}

// 总输入 1 GiB 不真生成：调低 max_total_input 验证同一条代码路径
#[test]
fn s8v_total_input_lowered_exact_ok() {
    let lim = Limits {
        max_total_input: Some(4096),
        ..Limits::default()
    };
    check_with("s8v", sized(None, "<r/>", 4096), T_SMALL, ok(), lim).ok();
}

#[test]
fn s8w_total_input_lowered_plus1() {
    let lim = Limits {
        max_total_input: Some(4096),
        ..Limits::default()
    };
    check_with(
        "s8w",
        sized(None, "<r/>", 4097),
        T_SMALL,
        limit("TotalInput"),
        lim,
    )
    .ok();
}

// “均可通过 ParserConfig 调整”：调高后原本超限的样本应通过，调低后应报错
#[test]
fn s8x_limits_adjustable() {
    let raise_attrs = Limits {
        max_attributes: Some(2048),
        ..Limits::default()
    };
    check_with(
        "s8x-attrs",
        sample!("S8l-attrs-1025.xml"),
        T_SMALL,
        ok(),
        raise_attrs,
    )
    .ok();
    let raise_name = Limits {
        max_name_len: Some(2048),
        ..Limits::default()
    };
    check_with(
        "s8x-name",
        sample!("S8n-elem-name-1025.xml"),
        T_SMALL,
        ok(),
        raise_name,
    )
    .ok();
    let low_text = Limits {
        max_text_len: Some(1024),
        ..Limits::default()
    };
    let t1024 = format!("<r>{}</r>", "t".repeat(1024)).into_bytes();
    let t1025 = format!("<r>{}</r>", "t".repeat(1025)).into_bytes();
    check_with("s8x-text-ok", t1024, T_SMALL, ok(), low_text).ok();
    check_with("s8x-text", t1025, T_SMALL, limit("TextLength"), low_text).ok();
    // 属性值同样跟随 max_text_len，但限额种类是 AttrValueLength
    let v1024 = format!("<r a=\"{}\"/>", "v".repeat(1024)).into_bytes();
    let v1025 = format!("<r a=\"{}\"/>", "v".repeat(1025)).into_bytes();
    check_with("s8x-attr-ok", v1024, T_SMALL, ok(), low_text).ok();
    check_with(
        "s8x-attr",
        v1025,
        T_SMALL,
        limit("AttrValueLength"),
        low_text,
    )
    .ok();
    let low_attrs = Limits {
        max_attributes: Some(2),
        ..Limits::default()
    };
    let three = b"<r a=\"1\" b=\"2\" c=\"3\"/>".to_vec();
    check_with(
        "s8x-attrs-low",
        three,
        T_SMALL,
        limit("Attributes"),
        low_attrs,
    )
    .ok();
}

// ───────────── S11 无外部实体开关 + 默认值 ─────────────

#[test]
fn s11_default_limits_match_lead_decision() {
    let expected = [
        ("max_entity_depth", 8u64),
        ("max_entity_expansions", 10_000),
        ("max_expanded_bytes", MIB as u64),
        ("max_expansion_ratio", 10),
        ("ratio_check_threshold", 64 * KIB as u64),
        ("max_depth", 256),
        ("max_attributes", 1024),
        ("max_name_len", 1024),
        ("max_text_len", 16 * MIB as u64),
        ("max_total_input", 1024 * MIB as u64),
    ];
    for ((name, got), (ename, want)) in common::default_limits().iter().zip(expected) {
        assert_eq!(*name, ename);
        assert_eq!(*got, want, "默认值 {name} 应为 {want}");
    }
}

#[test]
fn s11_no_external_entity_switch() {
    // 1) 配置的 Debug 输出里不应出现任何外部加载相关字段
    let dbg = common::default_config_debug().to_lowercase();
    for bad in [
        "external",
        "resolver",
        "load_dtd",
        "network",
        "allow_dtd_load",
    ] {
        assert!(
            !dbg.contains(bad),
            "ParserConfig 出现可疑字段 `{bad}`：{dbg}"
        );
    }
    // 2) 源码扫描：库代码（非测试）不得引用文件系统/网络 API 或 resolver 概念
    //    路径按 M1 workspace 布局：facade 在 src/，实现在 crates/purexml-core/src/
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let dirs = [root.join("src"), root.join("crates/purexml-core/src")];
    let forbidden = [
        "std::fs",
        "std::net",
        "File::open",
        "TcpStream",
        "UdpSocket",
        "std::process",
        "EntityResolver",
        "resolve_external",
        "external_entities",
        "load_external_dtd",
    ];
    let mut scanned = 0;
    for d in dirs.iter().filter(|d| d.is_dir()) {
        for path in walk_rs(d) {
            let src = std::fs::read_to_string(&path).unwrap();
            // 粗略地截掉 #[cfg(test)] 之后的单元测试部分
            let lib_part = src.split("#[cfg(test)]").next().unwrap_or("");
            for f in forbidden {
                assert!(
                    !lib_part.contains(f),
                    "{} 含禁止的 `{f}`（外部实体/IO 不得存在）",
                    path.display()
                );
            }
            scanned += 1;
        }
    }
    assert!(scanned > 0, "没扫描到任何源码文件，检查路径");
}

fn walk_rs(dir: &std::path::Path) -> Vec<std::path::PathBuf> {
    let mut out = Vec::new();
    for e in std::fs::read_dir(dir).unwrap().flatten() {
        let p = e.path();
        if p.is_dir() {
            out.extend(walk_rs(&p));
        } else if p.extension().is_some_and(|x| x == "rs") {
            out.push(p);
        }
    }
    out
}
