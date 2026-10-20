//! 集成测试公共层：purexml 真实 API 适配 + 超时/内存保护。
//!
//! 放在仓库根目录的 `tests/common/mod.rs`（门面 crate `purexml` 的集成测试）。
//! 测试只按 [`ErrorClass`] 与限额种类断言，不比对错误字符串。
#![allow(dead_code)]

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use purexml::{ErrorKind, Event, ParserConfig, Reader, XmlText};

// ───────────────────────────── 测试侧统一模型 ─────────────────────────────

/// 测试只按这几类断言。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum ErrorClass {
    SecurityLimit,
    ExternalEntityForbidden,
    Syntax,
    Encoding,
    Io,
}

#[derive(Debug, Clone)]
pub struct Failure {
    pub class: ErrorClass,
    /// 限额种类（仅 SecurityLimit），Debug 名，例如 "EntityDepth"
    pub limit: Option<String>,
    /// SecurityLimit 的 limit / actual 数值
    pub limit_value: Option<u64>,
    pub actual: Option<u64>,
    pub line: u64,
    pub column: u64,
    pub offset: u64,
    pub detail: String,
}

/// 成功解析后的摘要。
#[derive(Debug, Default, Clone)]
pub struct Summary {
    pub events: usize,
    pub max_depth: usize,
    /// 所有文本、CDATA、属性值拼接（最多保留 1 MiB），用于检查金丝雀泄漏
    pub text: String,
    /// Doctype 事件：(name, public_id, system_id)
    pub doctype: Option<(String, Option<String>, Option<String>)>,
}

const TEXT_KEEP: usize = 1 << 20;

fn keep(dst: &mut String, s: &str) {
    if dst.len() < TEXT_KEEP {
        let room = TEXT_KEEP - dst.len();
        let mut end = s.len().min(room);
        while !s.is_char_boundary(end) {
            end -= 1;
        }
        dst.push_str(&s[..end]);
    }
}

fn classify(e: purexml::Error) -> Failure {
    let (class, limit, limit_value, actual) = match &e.kind {
        ErrorKind::SecurityLimit {
            kind,
            limit,
            actual,
        } => (
            ErrorClass::SecurityLimit,
            Some(format!("{kind:?}")),
            Some(*limit),
            Some(*actual),
        ),
        ErrorKind::ExternalEntityForbidden => {
            (ErrorClass::ExternalEntityForbidden, None, None, None)
        }
        ErrorKind::Syntax => (ErrorClass::Syntax, None, None, None),
        ErrorKind::Encoding => (ErrorClass::Encoding, None, None, None),
        ErrorKind::Io => (ErrorClass::Io, None, None, None),
    };
    Failure {
        class,
        limit,
        limit_value,
        actual,
        line: e.position.line,
        column: e.position.column,
        offset: e.position.offset,
        detail: e.message,
    }
}

/// 可调整的限额（None = 用默认值）。
#[derive(Debug, Clone, Copy, Default)]
pub struct Limits {
    pub max_attributes: Option<usize>,
    pub max_name_len: Option<usize>,
    pub max_text_len: Option<usize>,
    pub max_total_input: Option<usize>,
}

impl Limits {
    pub fn config(&self) -> ParserConfig {
        let mut cfg = ParserConfig::default();
        if let Some(v) = self.max_attributes {
            cfg.max_attrs = v;
        }
        if let Some(v) = self.max_name_len {
            cfg.max_name_len = v;
        }
        if let Some(v) = self.max_text_len {
            cfg.max_text_len = v as u64;
        }
        if let Some(v) = self.max_total_input {
            cfg.max_total_input = v as u64;
        }
        cfg
    }
}

/// 用默认配置把整个文档拉完。
pub fn parse_default(input: &[u8]) -> Result<Summary, Failure> {
    parse_with(input, Limits::default())
}

pub fn parse_with(input: &[u8], limits: Limits) -> Result<Summary, Failure> {
    let mut reader = Reader::from_bytes(input).with_config(limits.config());
    let mut s = Summary::default();
    let mut depth = 0usize;
    loop {
        let Some(ev) = reader.read_event().map_err(classify)? else {
            return Ok(s);
        };
        s.events += 1;
        match ev {
            Event::StartElement(el) => {
                depth += 1;
                s.max_depth = s.max_depth.max(depth);
                for a in &el.attributes {
                    keep(&mut s.text, &a.value);
                }
            }
            Event::EmptyElement(el) => {
                s.max_depth = s.max_depth.max(depth + 1);
                for a in &el.attributes {
                    keep(&mut s.text, &a.value);
                }
            }
            Event::EndElement(_) => depth = depth.saturating_sub(1),
            Event::Text(t) | Event::CData(t) => keep(&mut s.text, &t),
            Event::Doctype(d) => {
                s.doctype = Some((
                    d.name.into_owned(),
                    d.public_id.map(XmlText::into_owned),
                    d.system_id.map(XmlText::into_owned),
                ));
            }
            _ => {}
        }
    }
}

/// 规范化后的结构事件，用于差分测试：注释/PI/声明不比较，相邻文本与 CDATA 合并，
/// 空元素展开为 Start + End，属性按 (命名空间, 本地名) 排序，xmlns 声明不算属性。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Node {
    Start {
        name: (Option<String>, String),
        attrs: Vec<((Option<String>, String), String)>,
    },
    End,
    Text(String),
}

pub fn push_text(out: &mut Vec<Node>, t: &str) {
    if t.is_empty() {
        return;
    }
    if let Some(Node::Text(prev)) = out.last_mut() {
        prev.push_str(t);
    } else {
        out.push(Node::Text(t.to_string()));
    }
}

pub fn structure(input: &[u8]) -> Result<Vec<Node>, Failure> {
    let mut reader = Reader::from_bytes(input);
    let mut out = Vec::new();
    loop {
        let Some(ev) = reader.read_event().map_err(classify)? else {
            return Ok(out);
        };
        match ev {
            Event::StartElement(el) => {
                let mut attrs: Vec<_> = el
                    .attributes
                    .iter()
                    .map(|a| {
                        (
                            (
                                a.name.namespace.as_deref().map(str::to_string),
                                a.name.local.to_string(),
                            ),
                            a.value.to_string(),
                        )
                    })
                    .collect();
                attrs.sort();
                out.push(Node::Start {
                    name: (
                        el.name.namespace.as_deref().map(str::to_string),
                        el.name.local.to_string(),
                    ),
                    attrs,
                });
            }
            Event::EmptyElement(el) => {
                let mut attrs: Vec<_> = el
                    .attributes
                    .iter()
                    .map(|a| {
                        (
                            (
                                a.name.namespace.as_deref().map(str::to_string),
                                a.name.local.to_string(),
                            ),
                            a.value.to_string(),
                        )
                    })
                    .collect();
                attrs.sort();
                out.push(Node::Start {
                    name: (
                        el.name.namespace.as_deref().map(str::to_string),
                        el.name.local.to_string(),
                    ),
                    attrs,
                });
                out.push(Node::End);
            }
            Event::EndElement(_) => out.push(Node::End),
            Event::Text(t) | Event::CData(t) => push_text(&mut out, &t),
            _ => {}
        }
    }
}

/// 默认配置的 Debug 输出（S11 检查字段名）。
pub fn default_config_debug() -> String {
    format!("{:?}", ParserConfig::default())
}

/// 默认限额（与 Lead 决定逐项比对）。
pub fn default_limits() -> [(&'static str, u64); 10] {
    let c = ParserConfig::default();
    [
        ("max_entity_depth", c.max_entity_depth as u64),
        ("max_entity_expansions", c.max_entity_expansions),
        ("max_expanded_bytes", c.max_expanded_bytes),
        ("max_expansion_ratio", c.max_expansion_ratio),
        (
            "ratio_check_threshold",
            purexml::EXPANSION_RATIO_FLOOR_BYTES,
        ),
        ("max_depth", c.max_depth as u64),
        ("max_attributes", c.max_attrs as u64),
        ("max_name_len", c.max_name_len as u64),
        ("max_text_len", c.max_text_len),
        ("max_total_input", c.max_total_input),
    ]
}

// ───────────────────────────── 内存保护 ─────────────────────────────
//
// 计数分配器：只统计“被保护线程”的分配。超过 HARD_CAP 直接返回空指针，
// 触发 handle_alloc_error 中止测试进程——宁可测试失败，也不能把机器拖进 OOM。
// 注意：本 unsafe 只在测试二进制里，不影响库的 #![forbid(unsafe_code)]。
//
// 额外保险：建议用 tests/run-security.sh（ulimit -v）跑安全套件。

pub const SOFT_CAP: usize = 64 << 20; // 断言：峰值 < 64 MiB
pub const HARD_CAP: usize = 512 << 20; // 超过即中止

thread_local! {
    static TRACKED: Cell<bool> = const { Cell::new(false) };
    static CURRENT: Cell<usize> = const { Cell::new(0) };
    static PEAK: Cell<usize> = const { Cell::new(0) };
}

pub struct GuardAlloc;

// SAFETY: 全部委托给 System，仅额外做线程本地计数；thread_local 为 const 初始化、
// 无析构，不会在分配器中递归分配。
unsafe impl GlobalAlloc for GuardAlloc {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        if !on_alloc(layout.size()) {
            return std::ptr::null_mut();
        }
        unsafe { System.alloc(layout) }
    }
    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        if !on_alloc(layout.size()) {
            return std::ptr::null_mut();
        }
        unsafe { System.alloc_zeroed(layout) }
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        on_dealloc(layout.size());
        unsafe { System.dealloc(ptr, layout) }
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        if new_size > layout.size() {
            if !on_alloc(new_size - layout.size()) {
                return std::ptr::null_mut();
            }
        } else {
            on_dealloc(layout.size() - new_size);
        }
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static GLOBAL: GuardAlloc = GuardAlloc;

fn on_alloc(n: usize) -> bool {
    TRACKED
        .try_with(|t| {
            if !t.get() {
                return true;
            }
            let cur = CURRENT.with(|c| {
                let v = c.get().saturating_add(n);
                c.set(v);
                v
            });
            PEAK.with(|p| p.set(p.get().max(cur)));
            cur <= HARD_CAP
        })
        .unwrap_or(true)
}

fn on_dealloc(n: usize) {
    let _ = TRACKED.try_with(|t| {
        if t.get() {
            CURRENT.with(|c| c.set(c.get().saturating_sub(n)));
        }
    });
}

// ───────────────────────────── 超时保护 ─────────────────────────────

#[derive(Debug)]
pub enum Guarded<T> {
    Done {
        value: T,
        elapsed: Duration,
        peak_bytes: usize,
    },
    Panicked(String),
    TimedOut,
}

/// 在独立线程里运行 `f`，带超时与内存统计。超时后线程被遗弃（测试进程结束时回收）。
/// 线程栈固定 8 MiB：深层嵌套若导致递归栈溢出，会直接暴露为崩溃。
pub fn run_guarded<T, F>(name: &str, timeout: Duration, f: F) -> Guarded<T>
where
    T: Send + 'static,
    F: FnOnce() -> T + Send + 'static,
{
    let (tx, rx) = mpsc::channel();
    let handle = thread::Builder::new()
        .name(format!("guard-{name}"))
        .stack_size(8 << 20)
        .spawn(move || {
            TRACKED.with(|t| t.set(true));
            let start = Instant::now();
            let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(f));
            let elapsed = start.elapsed();
            let peak = PEAK.with(|p| p.get());
            TRACKED.with(|t| t.set(false));
            let _ = tx.send(match r {
                Ok(value) => Guarded::Done {
                    value,
                    elapsed,
                    peak_bytes: peak,
                },
                Err(p) => Guarded::Panicked(panic_msg(&p)),
            });
        })
        .expect("spawn guard thread");
    match rx.recv_timeout(timeout) {
        Ok(g) => {
            let _ = handle.join();
            g
        }
        Err(mpsc::RecvTimeoutError::Timeout) => Guarded::TimedOut,
        Err(mpsc::RecvTimeoutError::Disconnected) => Guarded::Panicked("guard thread died".into()),
    }
}

fn panic_msg(p: &Box<dyn std::any::Any + Send>) -> String {
    if let Some(s) = p.downcast_ref::<&str>() {
        (*s).to_string()
    } else if let Some(s) = p.downcast_ref::<String>() {
        s.clone()
    } else {
        "non-string panic".into()
    }
}
