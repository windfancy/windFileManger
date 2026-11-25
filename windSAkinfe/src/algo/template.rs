//! 目标模板：`%` 占位符渲染 + `*` 捕获回填。

use std::path::Path;

use crate::util::Civil;

#[derive(Clone, Debug, PartialEq)]
enum Part {
    Lit(String),
    /// 第 N 个 `*` → 第 N 个捕获组（0 基）
    Star(usize),
    /// 序号：width=0 不补零；letter=true 时用字母序号
    Seq { width: usize, letter: bool },
    Var(char),
    /// `%{...}` 形式：日期时间字段，避免与 `%d`（序号）撞车
    Var2(String),
}

pub struct Template {
    parts: Vec<Part>,
    stars: usize,
    source: String,
}

pub struct RenderCtx<'a> {
    pub captures: &'a [String],
    pub seq: u64,
    pub path: &'a Path,
    pub root: &'a Path,
    pub mtime: Option<Civil>,
    pub size: u64,
    pub now: &'a Civil,
    pub rand: &'a str,
}

impl Template {
    pub fn new(tpl: &str) -> Result<Template, String> {
        let chars: Vec<char> = tpl.chars().collect();
        let mut parts = Vec::new();
        let mut lit = String::new();
        let mut stars = 0usize;
        let mut i = 0usize;
        while i < chars.len() {
            let c = chars[i];
            match c {
                '\\' if i + 1 < chars.len() => {
                    lit.push(chars[i + 1]);
                    i += 2;
                }
                '*' => {
                    if !lit.is_empty() {
                        parts.push(Part::Lit(std::mem::take(&mut lit)));
                    }
                    parts.push(Part::Star(stars));
                    stars += 1;
                    i += 1;
                }
                '%' => {
                    if !lit.is_empty() {
                        parts.push(Part::Lit(std::mem::take(&mut lit)));
                    }
                    i += 1;
                    if i >= chars.len() {
                        return Err("模板以孤立的 `%` 结尾".into());
                    }
                    if chars[i] == '%' {
                        lit.push('%');
                        i += 1;
                        continue;
                    }
                    // `%{Y}-%{m}-%{d}`：花括号形式，语义清晰且不会和 `%d`(序号) 冲突
                    if chars[i] == '{' {
                        let mut j = i + 1;
                        while j < chars.len() && chars[j] != '}' {
                            j += 1;
                        }
                        if j >= chars.len() {
                            return Err("模板中 `%{` 缺少对应的 `}`".into());
                        }
                        let name: String = chars[i + 1..j].iter().collect();
                        if name.is_empty() {
                            return Err("模板中 `%{}` 名字为空".into());
                        }
                        parts.push(Part::Var2(name));
                        i = j + 1;
                        continue;
                    }
                    let mut width = 0usize;
                    let mut has_width = false;
                    while i < chars.len() && chars[i].is_ascii_digit() {
                        width = width * 10 + chars[i].to_digit(10).unwrap_or(0) as usize;
                        has_width = true;
                        i += 1;
                        if width > 32 {
                            return Err("序号宽度过大（上限 32）".into());
                        }
                    }
                    if i >= chars.len() {
                        return Err("模板中 `%` 后缺字符".into());
                    }
                    let c2 = chars[i];
                    i += 1;
                    match c2 {
                        'd' => parts.push(Part::Seq { width, letter: false }),
                        'c' => parts.push(Part::Seq { width, letter: true }),
                        _ if has_width => {
                            return Err(format!("非法占位符 `%{width}{c2}`（宽度只能用于 d/c）"));
                        }
                        _ => parts.push(Part::Var(c2)),
                    }
                }
                _ => {
                    lit.push(c);
                    i += 1;
                }
            }
        }
        if !lit.is_empty() {
            parts.push(Part::Lit(lit));
        }
        Ok(Template { parts, stars, source: tpl.to_string() })
    }

    pub fn star_count(&self) -> usize {
        self.stars
    }

    pub fn source(&self) -> &str {
        &self.source
    }

    pub fn render(&self, ctx: &RenderCtx<'_>) -> String {
        let mut out = String::new();
        for p in &self.parts {
            match p {
                Part::Lit(s) => out.push_str(s),
                Part::Star(idx) => match ctx.captures.get(*idx) {
                    Some(c) => out.push_str(c),
                    // 捕获不足时保留 `*`，由调用方告警
                    None => out.push('*'),
                },
                Part::Seq { width, letter } => {
                    out.push_str(&fmt_seq(ctx.seq, *width, *letter));
                }
                Part::Var(v) => out.push_str(&render_var(*v, ctx)),
                Part::Var2(name) => out.push_str(&render_var2(name, ctx)),
            }
        }
        out
    }

    /// 模板是否显式给出了扩展名（用于决定是否补原扩展名）。
    pub fn has_explicit_ext(&self) -> bool {
        let s = &self.source;
        let last_dot = s.rfind('.');
        let last_sep = s.rfind(['/', '\\']);
        match (last_dot, last_sep) {
            (Some(d), Some(sp)) => d > sp && d + 1 < s.len(),
            (Some(d), None) => d + 1 < s.len(),
            _ => false,
        }
    }
}

fn fmt_seq(n: u64, width: usize, letter: bool) -> String {
    if letter {
        let s = to_alpha(n);
        if width > 0 {
            return format!("{s:0>width$}");
        }
        return s;
    }
    if width > 0 {
        format!("{n:0width$}")
    } else {
        n.to_string()
    }
}

/// 1 → a, 26 → z, 27 → aa（Excel 列式，1 基）
pub fn to_alpha(mut n: u64) -> String {
    if n == 0 {
        return "0".into();
    }
    let mut buf = Vec::new();
    while n > 0 {
        let r = ((n - 1) % 26) as u8;
        buf.push(b'a' + r);
        n = (n - 1) / 26;
    }
    buf.reverse();
    String::from_utf8(buf).unwrap_or_default()
}

fn render_var(v: char, ctx: &RenderCtx<'_>) -> String {
    let p = ctx.path;
    match v {
        's' => p
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default(),
        'e' => p
            .extension()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default(),
        'f' => p
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default(),
        'p' => p
            .parent()
            .and_then(|d| d.file_name())
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default(),
        'P' => {
            let parent = p.parent().unwrap_or(Path::new(""));
            let rel = parent.strip_prefix(ctx.root).unwrap_or(parent);
            let s = rel.to_string_lossy().replace('\\', "/");
            s.trim_matches('/').to_string()
        }
        'm' => ctx.mtime.map(|c| c.format("YYYYMMDD")).unwrap_or_else(|| "00000000".into()),
        'H' => ctx.mtime.map(|c| c.format("HH")).unwrap_or_else(|| "00".into()),
        'M' => ctx.mtime.map(|c| c.format("mm")).unwrap_or_else(|| "00".into()),
        'S' => ctx.mtime.map(|c| c.format("ss")).unwrap_or_else(|| "00".into()),
        'i' => ctx.size.to_string(),
        't' => ctx.now.compact(),
        'u' => ctx.rand.to_string(),
        other => format!("%{other}"),
    }
}

/// `%{...}` 占位符：默认取文件 mtime，`now:` 前缀取当前时间。
///
/// 之所以另起一套写法：`%d` 已经是「序号」，若再按 strftime 解释成「日」
/// 会产生歧义，所以日期字段统一走花括号。
fn render_var2(name: &str, ctx: &RenderCtx<'_>) -> String {
    let (use_now, key) = match name.split_once(':') {
        Some(("now", k)) => (true, k),
        _ => (false, name),
    };
    let civil = if use_now { Some(ctx.now) } else { ctx.mtime.as_ref() };
    let c = match civil {
        Some(c) => c,
        None => {
            // 没有时间可用时用等长的 0 占位，保持名字结构稳定
            return match zero_len(key) {
                Some(n) => "0".repeat(n),
                None => format!("%{{{name}}}"),
            };
        }
    };
    match key {
        "Y" => format!("{:04}", c.year),
        "y" => format!("{:02}", c.year.rem_euclid(100)),
        "m" => format!("{:02}", c.month),
        "d" => format!("{:02}", c.day),
        "H" => format!("{:02}", c.hour),
        "M" => format!("{:02}", c.min),
        "S" => format!("{:02}", c.sec),
        "D" => format!("{:04}{:02}{:02}", c.year, c.month, c.day),
        "T" => format!("{:02}{:02}{:02}", c.hour, c.min, c.sec),
        _ => format!("%{{{name}}}"),
    }
}

fn zero_len(key: &str) -> Option<usize> {
    Some(match key {
        "Y" => 4,
        "D" => 8,
        "T" => 6,
        "y" | "m" | "d" | "H" | "M" | "S" => 2,
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::util::Civil;
    use std::path::PathBuf;

    fn ctx<'a>(caps: &'a [String], seq: u64, p: &'a Path, now: &'a Civil) -> RenderCtx<'a> {
        RenderCtx { captures: caps, seq, path: p, root: Path::new(""), mtime: None, size: 0, now, rand: "a3f19c" }
    }

    fn now() -> Civil {
        Civil { year: 2026, month: 11, day: 8, hour: 20, min: 43, sec: 12 }
    }

    #[test]
    fn seq_placeholders() {
        let n = now();
        let p = PathBuf::from("/tmp/x/photo.JPG");
        let caps = vec![];
        let t = Template::new("%d-%3d-%2c").unwrap();
        let out = t.render(&ctx(&caps, 5, &p, &n));
        assert_eq!(out, "5-005-0e");
    }

    #[test]
    fn var_placeholders() {
        let n = now();
        let p = PathBuf::from("/tmp/x/photo.JPG");
        let caps = vec![];
        let t = Template::new("%s_%e_%p_%f").unwrap();
        assert_eq!(t.render(&ctx(&caps, 1, &p, &n)), "photo_JPG_x_photo.JPG");
    }

    #[test]
    fn star_backfill() {
        let n = now();
        let p = PathBuf::from("/tmp/x/第03话-封面.jpg");
        let caps = vec!["03".to_string(), "封面".to_string()];
        let t = Template::new("[%3d]*.jpg").unwrap();
        assert_eq!(t.render(&ctx(&caps, 1, &p, &n)), "[001]03.jpg");
        let t2 = Template::new("*-*.jpg").unwrap();
        assert_eq!(t2.render(&ctx(&caps, 1, &p, &n)), "03-封面.jpg");
    }

    #[test]
    fn missing_capture_keeps_star() {
        let n = now();
        let p = PathBuf::from("a.txt");
        let caps = vec!["only".to_string()];
        let t = Template::new("*-*").unwrap();
        assert_eq!(t.render(&ctx(&caps, 1, &p, &n)), "only-*");
    }

    #[test]
    fn literal_percent() {
        let n = now();
        let p = PathBuf::from("a.txt");
        let t = Template::new("100%%_%3d").unwrap();
        assert_eq!(t.render(&ctx(&[], 7, &p, &n)), "100%_007");
    }

    #[test]
    fn alpha_sequence() {
        assert_eq!(to_alpha(1), "a");
        assert_eq!(to_alpha(26), "z");
        assert_eq!(to_alpha(27), "aa");
        assert_eq!(to_alpha(52), "az");
        assert_eq!(to_alpha(53), "ba");
        assert_eq!(to_alpha(702), "zz");
        assert_eq!(to_alpha(703), "aaa");
    }

    #[test]
    fn explicit_ext_detection() {
        assert!(Template::new("%3d.jpg").unwrap().has_explicit_ext());
        assert!(!Template::new("%3d").unwrap().has_explicit_ext());
        assert!(Template::new("%s.%e").unwrap().has_explicit_ext());
        assert!(Template::new("%d.tar.gz").unwrap().has_explicit_ext());
    }

    #[test]
    fn time_and_size_vars() {
        let n = now();
        let p = PathBuf::from("a.bin");
        let c = Civil { year: 2024, month: 1, day: 31, hour: 9, min: 8, sec: 7 };
        let cx = RenderCtx {
            captures: &[],
            seq: 1,
            path: &p,
            root: Path::new(""),
            mtime: Some(c),
            size: 1234,
            now: &n,
            rand: "zz",
        };
        let t = Template::new("%m-%H%M%S-%i").unwrap();
        assert_eq!(t.render(&cx), "20240131-090807-1234");
    }

    #[test]
    fn invalid_templates() {
        assert!(Template::new("abc%").is_err());
        assert!(Template::new("%3x").is_err());
        assert!(Template::new("%{Y").is_err());
        assert!(Template::new("%{}").is_err());
    }

    /// `%{...}` 是刻意另起的写法：`%d` 是序号，不能再当「日」。
    #[test]
    fn brace_datetime_vars() {
        let n = Civil { year: 2000, month: 2, day: 3, hour: 4, min: 5, sec: 6 };
        let p = PathBuf::from("a.bin");
        let mt = Civil { year: 2024, month: 1, day: 31, hour: 9, min: 8, sec: 7 };
        let cx = RenderCtx {
            captures: &[],
            seq: 7,
            path: &p,
            root: Path::new(""),
            mtime: Some(mt),
            size: 0,
            now: &n,
            rand: "zz",
        };
        let t = Template::new("%{Y}-%{m}-%{d}_%3d").unwrap();
        assert_eq!(t.render(&cx), "2024-01-31_007");
        // 序号仍由 %d 负责，不受 %{d} 影响
        let t2 = Template::new("%{D}%{T}_%d_%d").unwrap();
        assert_eq!(t2.render(&cx), "20240131090807_7_7");
        // now: 前缀取当前时间而不是 mtime
        let t3 = Template::new("%{now:Y}%{y}%{now:S}").unwrap();
        assert_eq!(t3.render(&cx), "20002406");
        // 没有 mtime 时用等长 0 占位
        let cx2 = RenderCtx { mtime: None, ..cx };
        let t4 = Template::new("%{Y}%{d}%{T}").unwrap();
        assert_eq!(t4.render(&cx2), "000000000000");
        // 未知名字原样保留，方便发现拼错
        let t5 = Template::new("%{Q}").unwrap();
        assert_eq!(t5.render(&cx), "%{Q}");
    }
}
