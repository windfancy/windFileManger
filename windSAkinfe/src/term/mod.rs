//! 终端输出层：颜色、进度条、确认交互、JSON 双通道（架构 D9）。
//!
//! 设计要点：
//! - 所有颜色都过 `paint()`，非 TTY / `NO_COLOR` / `--no-color` 时自动降级为纯文本。
//! - `Out` 同时承担「人读」与「机读（JSON）」两种模式，命令层只调用语义方法，
//!   不关心最终怎么落地。
//! - 进度条只写 stderr，绝不污染 stdout 的管道输出。

use std::io::{self, IsTerminal, Write};
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::OnceLock;
use std::time::Instant;

use crate::util::format_size;

// ---------------------------------------------------------------- 全局开关

#[derive(Debug, Clone)]
#[derive(Default)]
pub struct TermCfg {
    /// 是否启用 ANSI 颜色
    pub color: bool,
    /// JSON 模式（stdout 只出 JSON）
    pub json: bool,
    /// 静默：只出错误
    pub quiet: bool,
    /// 啰嗦：额外输出处理细节
    pub verbose: bool,
    /// 全局自动确认（等价于 --yes）
    pub assume_yes: bool,
    /// 交互式确认的开关（--no-confirm 或非 TTY 时关闭）
    pub interactive: bool,
}


static CFG: OnceLock<TermCfg> = OnceLock::new();

/// 初始化终端配置（只能成功一次，重复调用忽略）。
pub fn init(cfg: TermCfg) {
    let _ = CFG.set(cfg);
}

pub fn cfg() -> &'static TermCfg {
    CFG.get_or_init(TermCfg::default)
}

pub fn color_on() -> bool {
    cfg().color
}

pub fn json_on() -> bool {
    cfg().json
}

// ---------------------------------------------------------------- ANSI 颜色

/// 是否值得尝试开启 ANSI。Windows 10+ 的控制台支持 VT 序列，但要先置位。
pub fn detect_color(force_no: bool) -> bool {
    if force_no {
        return false;
    }
    if std::env::var_os("NO_COLOR").is_some() {
        return false;
    }
    if let Some(t) = std::env::var_os("TERM") {
        if t == "dumb" {
            return false;
        }
    }
    if !io::stdout().is_terminal() {
        return false;
    }
    #[cfg(windows)]
    {
        enable_vt()
    }
    #[cfg(not(windows))]
    {
        true
    }
}

#[cfg(windows)]
fn enable_vt() -> bool {
    #[link(name = "kernel32")]
    extern "system" {
        fn GetStdHandle(which: u32) -> *mut core::ffi::c_void;
        fn GetConsoleMode(h: *mut core::ffi::c_void, mode: *mut u32) -> i32;
        fn SetConsoleMode(h: *mut core::ffi::c_void, mode: u32) -> i32;
    }
    const STD_OUTPUT_HANDLE: u32 = -11i32 as u32;
    const ENABLE_VIRTUAL_TERMINAL_PROCESSING: u32 = 0x0004;
    let mut ok = true;
    for which in [STD_OUTPUT_HANDLE, -12i32 as u32] {
        unsafe {
            let h = GetStdHandle(which);
            if h.is_null() {
                ok = false;
                continue;
            }
            let mut mode: u32 = 0;
            if GetConsoleMode(h, &mut mode) == 0 {
                ok = false;
                continue;
            }
            if mode & ENABLE_VIRTUAL_TERMINAL_PROCESSING == 0
                && SetConsoleMode(h, mode | ENABLE_VIRTUAL_TERMINAL_PROCESSING) == 0
            {
                ok = false;
            }
        }
    }
    ok
}

/// 用 ANSI 颜色包裹文本；未启用颜色时原样返回。
pub fn paint(s: &str, code: &str) -> String {
    if color_on() && !code.is_empty() {
        format!("\x1b[{}m{}\x1b[0m", code, s)
    } else {
        s.to_string()
    }
}

pub fn green(s: &str) -> String {
    paint(s, "32")
}
pub fn red(s: &str) -> String {
    paint(s, "31")
}
pub fn yellow(s: &str) -> String {
    paint(s, "33")
}
pub fn cyan(s: &str) -> String {
    paint(s, "36")
}
pub fn dim(s: &str) -> String {
    paint(s, "2")
}
pub fn bold(s: &str) -> String {
    paint(s, "1")
}

// ---------------------------------------------------------------- JSON 输出

/// 极简 JSON 字符串转义（不引 serde，保持零依赖红线）。
pub fn json_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 8);
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{08}' => out.push_str("\\b"),
            '\u{0c}' => out.push_str("\\f"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out
}

/// 构造一个 JSON 对象：`obj(&[("k", JsonVal::Str("v")), ...])`
pub enum JVal {
    Str(String),
    Num(i64),
    Real(f64),
    Bool(bool),
    Null,
    Arr(Vec<JVal>),
    Obj(Vec<(String, JVal)>),
}

impl JVal {
    pub fn s(v: impl Into<String>) -> JVal {
        JVal::Str(v.into())
    }
    pub fn n(v: impl Into<i64>) -> JVal {
        JVal::Num(v.into())
    }

    pub fn render(&self) -> String {
        match self {
            JVal::Str(s) => format!("\"{}\"", json_escape(s)),
            JVal::Num(n) => n.to_string(),
            JVal::Real(f) => {
                if f.is_finite() {
                    format!("{f}")
                } else {
                    "null".into()
                }
            }
            JVal::Bool(b) => b.to_string(),
            JVal::Null => "null".into(),
            JVal::Arr(items) => {
                let inner: Vec<String> = items.iter().map(|v| v.render()).collect();
                format!("[{}]", inner.join(","))
            }
            JVal::Obj(kv) => {
                let inner: Vec<String> = kv
                    .iter()
                    .map(|(k, v)| format!("\"{}\":{}", json_escape(k), v.render()))
                    .collect();
                format!("{{{}}}", inner.join(","))
            }
        }
    }
}

// ---------------------------------------------------------------- 进度条

static SPIN: AtomicU64 = AtomicU64::new(0);

pub struct Progress {
    label: String,
    total: u64,
    done: u64,
    started: Instant,
    last: Instant,
    enabled: bool,
    /// 已知总量的比例模式
    pct: bool,
    drew: bool,
}

impl Progress {
    pub fn new(label: impl Into<String>, total: u64) -> Progress {
        let enabled = !cfg().json && !cfg().quiet && io::stderr().is_terminal();
        Progress {
            label: label.into(),
            total,
            done: 0,
            started: Instant::now(),
            last: Instant::now() - std::time::Duration::from_secs(1),
            enabled,
            pct: total > 0,
            drew: false,
        }
    }

    /// 未知总量模式（螺旋指示）
    pub fn spinner(label: impl Into<String>) -> Progress {
        Progress::new(label, 0)
    }

    pub fn set_total(&mut self, total: u64) {
        self.total = total;
        self.pct = total > 0;
    }

    pub fn inc(&mut self, n: u64) {
        self.done += n;
        self.maybe_paint(false);
    }

    pub fn set(&mut self, done: u64) {
        self.done = done;
        self.maybe_paint(false);
    }

    pub fn set_label(&mut self, label: impl Into<String>) {
        self.label = label.into();
    }

    fn maybe_paint(&mut self, force: bool) {
        if !self.enabled {
            return;
        }
        // 节流到 ~15 fps，避免刷屏拖慢 IO
        if !force && self.last.elapsed().as_millis() < 66 {
            return;
        }
        self.last = Instant::now();
        self.drew = true;
        let secs = self.started.elapsed().as_secs_f64();
        let mut err = io::stderr();
        let line = if self.pct {
            let ratio = (self.done as f64 / self.total as f64).clamp(0.0, 1.0);
            let width = 28usize;
            let fill = (ratio * width as f64).round() as usize;
            let bar: String = std::iter::repeat_n('#', fill)
                .chain(std::iter::repeat_n('-', width - fill))
                .collect();
            let speed = if secs > 0.0 { self.done as f64 / secs } else { 0.0 };
            format!(
                "\r{} [{}] {:5.1}%  {}/{}  {}/s   ",
                self.label,
                bar,
                ratio * 100.0,
                format_size(self.done),
                format_size(self.total),
                format_size(speed as u64)
            )
        } else {
            let n = SPIN.fetch_add(1, Ordering::Relaxed);
            let frames = ['|', '/', '-', '\\'];
            format!(
                "\r{} {}  {}  {:.1}s   ",
                self.label,
                frames[(n as usize) % 4],
                format_size(self.done),
                secs
            )
        };
        let _ = err.write_all(line.as_bytes());
        let _ = err.flush();
    }

    /// 结束并清掉进度行。
    pub fn finish(&mut self) {
        if self.enabled && self.drew {
            let mut err = io::stderr();
            let _ = err.write_all(b"\r\x1b[K");
            let _ = err.flush();
        }
        self.drew = false;
    }

    /// 结束并保留一行总结。
    pub fn finish_with(&mut self, msg: &str) {
        if self.enabled && self.drew {
            let mut err = io::stderr();
            let _ = err.write_all(b"\r\x1b[K");
            let _ = writeln!(err, "{msg}");
            let _ = err.flush();
        }
        self.drew = false;
    }
}

// ---------------------------------------------------------------- 输出通道

/// 一条处理结果（命令层只描述语义，怎么画由 Out 决定）。
pub struct Item {
    pub tag: &'static str,
    pub path: String,
    pub detail: String,
}

pub struct Out {
    items: Vec<Item>,
    json_items: Vec<JVal>,
    notes: usize,
}

impl Default for Out {
    fn default() -> Self {
        Out::new()
    }
}

impl Out {
    pub fn new() -> Out {
        Out { items: Vec::new(), json_items: Vec::new(), notes: 0 }
    }

    /// 一项成功操作。
    pub fn ok(&mut self, tag: &'static str, path: &Path, detail: impl Into<String>) {
        self.emit(tag, path, detail.into(), JVal::Null);
    }

    /// 一项跳过操作（冲突、过滤）。
    pub fn skip(&mut self, tag: &'static str, path: &Path, detail: impl Into<String>) {
        self.emit(tag, path, detail.into(), JVal::Null);
    }

    fn emit(&mut self, tag: &'static str, path: &Path, detail: String, extra: JVal) {
        let p = crate::util::path_to_display(path);
        if json_on() {
            let mut kv = vec![
                ("status".to_string(), JVal::s(if tag.starts_with('!') { "skip" } else { "ok" })),
                ("action".to_string(), JVal::s(tag.trim_start_matches('!'))),
                ("path".to_string(), JVal::s(p.clone())),
            ];
            if !detail.is_empty() {
                kv.push(("detail".to_string(), JVal::s(detail.clone())));
            }
            if let JVal::Obj(more) = extra {
                kv.extend(more);
            }
            self.json_items.push(JVal::Obj(kv));
        } else if !cfg().quiet {
            self.items.push(Item { tag, path: p, detail });
        }
    }

    /// 带附加字段的成功项（JSON 模式更有用）。
    pub fn ok_with(&mut self, tag: &'static str, path: &Path, detail: impl Into<String>, extra: JVal) {
        self.emit(tag, path, detail.into(), extra);
    }

    /// 一条说明性文字（不进入 JSON 数组）。
    ///
    /// 输出前先把已缓冲的条目刷出去，否则「条目 + 汇总」的顺序会颠倒。
    pub fn note(&mut self, msg: impl AsRef<str>) {
        self.notes += 1;
        if !json_on() && !cfg().quiet {
            self.flush_items();
            println!("{}", msg.as_ref());
        }
    }

    /// 只刷条目、不结束 JSON。
    fn flush_items(&mut self) {
        if json_on() {
            return;
        }
        let items = std::mem::take(&mut self.items);
        for it in items {
            let head = if it.tag.starts_with('!') {
                format!("{} {}", dim("skip"), cyan(&it.path))
            } else {
                format!("{:>7} {}", green(it.tag), cyan(&it.path))
            };
            if it.detail.is_empty() {
                println!("{head}");
            } else {
                println!("{head} {}", dim(&it.detail));
            }
        }
    }

    /// 原样输出一行（JSON 模式下不输出），供管道友好的命令使用。
    pub fn raw(&mut self, line: &str) {
        if !json_on() && !cfg().quiet {
            println!("{line}");
        }
    }

    /// 追加一个 JSON 结果对象（非 JSON 模式忽略）。
    pub fn json_item(&mut self, v: JVal) {
        if json_on() {
            self.json_items.push(v);
        }
    }

    /// 告警，走 stderr。
    pub fn warn(&mut self, msg: impl AsRef<str>) {
        if !cfg().quiet {
            eprintln!("{} {}", yellow("warning:"), msg.as_ref());
        }
    }

    /// 错误，走 stderr。
    pub fn error(&mut self, msg: impl AsRef<str>) {
        eprintln!("{} {}", red("error:"), msg.as_ref());
    }

    /// 是否已经有任何输出（用于决定要不要打汇总头）。
    pub fn is_empty(&self) -> bool {
        self.items.is_empty() && self.json_items.is_empty()
    }

    /// 把缓冲的条目刷出去。`json` 模式下这是唯一一次 stdout 写入。
    pub fn flush(&mut self) {
        if json_on() {
            let arr = JVal::Arr(std::mem::take(&mut self.json_items));
            println!("{}", arr.render());
            self.items.clear();
            return;
        }
        self.flush_items();
    }
}

// ---------------------------------------------------------------- 确认交互

static YES_ALL: AtomicBool = AtomicBool::new(false);

pub fn set_yes_all(v: bool) {
    YES_ALL.store(v, Ordering::Relaxed);
}

pub fn yes_all() -> bool {
    YES_ALL.load(Ordering::Relaxed)
}

/// 询问 y/N。非交互场景返回 `default_yes`。
pub fn confirm(question: &str, default_yes: bool) -> bool {
    if cfg().assume_yes || yes_all() {
        return true;
    }
    if !cfg().interactive || !io::stdin().is_terminal() {
        return default_yes;
    }
    let hint = if default_yes { "[Y/n]" } else { "[y/N]" };
    let mut err = io::stderr();
    let _ = write!(err, "{} {} ", yellow("?"), question);
    let _ = write!(err, "{hint} ");
    let _ = err.flush();
    let mut line = String::new();
    if io::stdin().read_line(&mut line).is_err() {
        return default_yes;
    }
    match line.trim().to_ascii_lowercase().as_str() {
        "y" | "yes" => true,
        "a" | "all" => {
            set_yes_all(true);
            true
        }
        "n" | "no" => false,
        "" => default_yes,
        _ => false,
    }
}

/// 询问 y/N，并支持 a(ll)。
pub fn confirm_all(question: &str) -> bool {
    confirm(question, false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_escape_basics() {
        assert_eq!(json_escape("a\"b"), "a\\\"b");
        assert_eq!(json_escape("a\\b"), "a\\\\b");
        assert_eq!(json_escape("a\nb"), "a\\nb");
        assert_eq!(json_escape("\u{1}"), "\\u0001");
        assert_eq!(json_escape("正常"), "正常");
    }

    #[test]
    fn jval_render() {
        let v = JVal::Obj(vec![
            ("a".into(), JVal::n(1)),
            ("b".into(), JVal::s("x\"y")),
            ("c".into(), JVal::Bool(true)),
            ("d".into(), JVal::Null),
            ("e".into(), JVal::Arr(vec![JVal::n(1), JVal::n(2)])),
        ]);
        assert_eq!(v.render(), r#"{"a":1,"b":"x\"y","c":true,"d":null,"e":[1,2]}"#);
    }

    #[test]
    fn paint_disabled_is_plain() {
        // 测试环境默认不启用颜色
        assert_eq!(paint("abc", "32"), "abc");
    }
}
