//! 只读检查类命令：find / stat / tree / wc / dedup。

use std::collections::HashMap;
use std::fs;
use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};

use crate::algo::hash::{self, Algo};
use crate::algo::pattern::Pattern;
use crate::cli::{DedupArgs, FindArgs, StatArgs, TreeArgs, WcArgs};
use crate::fsx;
use crate::plan::{Fail, Summary};
use crate::term::{self, JVal, Out};
use crate::util::{format_size, local_to_unix_secs, path_to_display, system_time_to_local, Civil};

use super::{walk_relative, Global};

// ---------------------------------------------------------------- find

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum KindFilter {
    File,
    Dir,
    Link,
    Empty,
}

pub fn find(a: &FindArgs, _g: &Global, out: &mut Out) -> Summary {
    let mut sum = Summary::new();
    let roots: Vec<PathBuf> = if a.roots.is_empty() { vec![PathBuf::from(".")] } else { a.roots.clone() };

    let kind = match a.kind.as_deref() {
        None => None,
        Some(s) => match s.to_ascii_lowercase().as_str() {
            "file" | "f" => Some(KindFilter::File),
            "dir" | "d" => Some(KindFilter::Dir),
            "link" | "l" => Some(KindFilter::Link),
            "empty" | "e" => Some(KindFilter::Empty),
            other => {
                sum.fail(Fail::usage(format!("未知的 --type：{other}")));
                return sum;
            }
        },
    };
    let min_size = match a.min_size.as_deref().map(crate::util::parse_size).transpose() {
        Ok(v) => v,
        Err(e) => {
            sum.fail(Fail::usage(format!("--min-size: {e}")));
            return sum;
        }
    };
    let max_size = match a.max_size.as_deref().map(crate::util::parse_size).transpose() {
        Ok(v) => v,
        Err(e) => {
            sum.fail(Fail::usage(format!("--max-size: {e}")));
            return sum;
        }
    };
    let newer = match a.newer.as_deref().map(parse_time_bound).transpose() {
        Ok(v) => v,
        Err(e) => {
            sum.fail(Fail::usage(format!("--newer: {e}")));
            return sum;
        }
    };
    let older = match a.older.as_deref().map(parse_time_bound).transpose() {
        Ok(v) => v,
        Err(e) => {
            sum.fail(Fail::usage(format!("--older: {e}")));
            return sum;
        }
    };
    let ci = cfg!(windows);
    let mut patterns = Vec::new();
    for p in &a.pattern {
        match Pattern::new(p, ci) {
            Ok(x) => patterns.push(x),
            Err(e) => {
                sum.fail(Fail::usage(e));
                return sum;
            }
        }
    }
    let (mut inc, mut exc) = (Vec::new(), Vec::new());
    for p in &a.include {
        if let Ok(x) = Pattern::new(p, ci) {
            inc.push(x);
        }
    }
    for p in &a.exclude {
        if let Ok(x) = Pattern::new(p, ci) {
            exc.push(x);
        }
    }

    let mut rows: Vec<(PathBuf, u64, Option<u64>, bool)> = Vec::new();
    for root in &roots {
        let md = match fs::symlink_metadata(root) {
            Ok(m) => m,
            Err(e) => {
                sum.fail(Fail::io(root.clone(), e));
                continue;
            }
        };
        // 根自身也参与匹配，之后的后裔一次性从 walk_entries 取回
        // （类型/大小/时间都来自 readdir，不再逐个 stat）
        let mut items: Vec<fsx::WalkItem> = vec![fsx::WalkItem {
            path: root.clone(),
            rel: root.file_name().map(|s| s.to_string_lossy().to_string()).unwrap_or_default(),
            depth: 0,
            is_dir: md.is_dir(),
            is_symlink: md.file_type().is_symlink(),
            len: md.len(),
            mtime: md
                .modified()
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_secs()),
        }];
        if md.is_dir() {
            let opts = fsx::WalkOpts {
                max_depth: a.depth.map(|d| d.max(1)).unwrap_or(usize::MAX),
                follow_links: false,
                sort: true,
            };
            match fsx::walk_entries(root, &opts) {
                Ok(v) => items.extend(v),
                Err(e) => sum.fail(Fail::io(root.clone(), e)),
            }
        }
        for it in items {
            let (p, rel, is_dir, is_link) = (it.path, it.rel, it.is_dir, it.is_symlink);
            if let Some(d) = a.depth {
                if it.depth > d {
                    continue;
                }
            }
            let name = p.file_name().map(|s| s.to_string_lossy().to_string()).unwrap_or_default();
            if !patterns.is_empty() && !patterns.iter().any(|x| x.is_match(&name)) {
                continue;
            }
            if !inc.is_empty() && !inc.iter().any(|x| x.is_match(&rel) || x.is_match(&name)) {
                continue;
            }
            if exc.iter().any(|x| x.is_match(&rel) || x.is_match(&name)) {
                continue;
            }
            // 「空」目录需要真的看一眼；文件看大小即可（已在 WalkItem 里）
            let empty = if is_dir {
                if matches!(kind, Some(KindFilter::Empty)) {
                    fs::read_dir(&p).map(|mut d| d.next().is_none()).unwrap_or(false)
                } else {
                    false
                }
            } else {
                it.len == 0
            };
            let ok_kind = match kind {
                None => true,
                Some(KindFilter::File) => !is_dir && !is_link,
                Some(KindFilter::Dir) => is_dir && !is_link,
                Some(KindFilter::Link) => is_link,
                Some(KindFilter::Empty) => empty,
            };
            if !ok_kind {
                continue;
            }
            let size = if is_dir { 0 } else { it.len };
            if let Some(mn) = min_size {
                if size < mn {
                    continue;
                }
            }
            if let Some(mx) = max_size {
                if size > mx {
                    continue;
                }
            }
            if let Some(bound) = newer {
                if it.mtime.unwrap_or(0) < bound {
                    continue;
                }
            }
            if let Some(bound) = older {
                if it.mtime.unwrap_or(0) >= bound {
                    continue;
                }
            }
            rows.push((p, size, it.mtime, is_dir));
        }
    }

    if a.sort_size {
        rows.sort_by_key(|x| std::cmp::Reverse(x.1));
    }
    for (p, size, mtime, is_dir) in &rows {
        let disp = path_to_display(p);
        let line = if a.long {
            let t = mtime
                .map(|s| {
                    system_time_to_local(std::time::UNIX_EPOCH + std::time::Duration::from_secs(s))
                        .format("YYYY-MM-DD HH:mm:ss")
                })
                .unwrap_or_else(|| "-".repeat(19));
            format!("{:>10}  {}  {}", if *is_dir { "DIR".to_string() } else { format_size(*size) }, t, disp)
        } else {
            disp.clone()
        };
        out.raw(&line);
        let mut kv = vec![
            ("status".to_string(), JVal::s("ok")),
            ("action".to_string(), JVal::s("found")),
            ("path".to_string(), JVal::s(disp)),
            ("is_dir".to_string(), JVal::Bool(*is_dir)),
            ("size".to_string(), JVal::n(*size as i64)),
        ];
        if let Some(t) = mtime {
            kv.push(("mtime".to_string(), JVal::n(*t as i64)));
        }
        out.json_item(JVal::Obj(kv));
        sum.ok += 1;
    }
    if !term::json_on() && !term::cfg().quiet {
        out.note(term::dim(&format!("共 {} 项", sum.ok)));
    }
    sum
}

fn parse_time_bound(s: &str) -> Result<u64, String> {
    if let Ok(secs) = crate::util::parse_duration(s) {
        let now = crate::util::local_to_unix_secs(&crate::util::now_local()).max(0) as u64;
        return Ok(now.saturating_sub(secs));
    }
    let c = crate::util::parse_datetime(s)?;
    Ok(local_to_unix_secs(&c).max(0) as u64)
}

// ---------------------------------------------------------------- stat

pub fn stat(a: &StatArgs, _g: &Global, out: &mut Out) -> Summary {
    let mut sum = Summary::new();
    let algo = match &a.hash {
        Some(s) => match Algo::parse(s) {
            Some(x) => Some(x),
            None => {
                sum.fail(Fail::usage(format!("未知算法：{s}")));
                return sum;
            }
        },
        None => None,
    };
    for p in &a.paths {
        let md = match fs::symlink_metadata(p) {
            Ok(m) => m,
            Err(e) => {
                sum.fail(Fail::io(p.clone(), e));
                continue;
            }
        };
        let is_link = md.file_type().is_symlink();
        let abs = crate::plan::absolute(p);
        let (files, bytes) = if md.is_dir() && a.recursive {
            fsx::dir_stats(p)
        } else {
            (if md.is_file() { 1 } else { 0 }, md.len())
        };
        let created = md.created().ok();
        let modified = md.modified().ok();
        let accessed = md.accessed().ok();

        let mut kv = vec![
            ("status".to_string(), JVal::s("ok")),
            ("action".to_string(), JVal::s("stat")),
            ("path".to_string(), JVal::s(path_to_display(p))),
            ("absolute".to_string(), JVal::s(path_to_display(&abs))),
            ("is_dir".to_string(), JVal::Bool(md.is_dir())),
            ("is_symlink".to_string(), JVal::Bool(is_link)),
            ("size".to_string(), JVal::n(md.len() as i64)),
            ("readonly".to_string(), JVal::Bool(md.permissions().readonly())),
        ];

        if !term::json_on() {
            out.raw(&term::bold(&path_to_display(p)));
            out.raw(&format!("  类型      : {}", kind_name(&md)));
            out.raw(&format!("  绝对路径  : {}", path_to_display(&abs)));
            out.raw(&format!(
                "  大小      : {} 字节 ({})",
                md.len(),
                format_size(md.len())
            ));
            if md.is_dir() {
                out.raw(&format!("  内含      : {files} 个文件，{}", format_size(bytes)));
            }
            out.raw(&format!("  创建时间  : {}", fmt_time(created)));
            out.raw(&format!("  修改时间  : {}", fmt_time(modified)));
            out.raw(&format!("  访问时间  : {}", fmt_time(accessed)));
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                out.raw(&format!("  权限      : {:o}", md.permissions().mode()));
            }
            out.raw(&format!("  只读      : {}", md.permissions().readonly()));
            if is_link {
                if let Ok(t) = fs::read_link(p) {
                    out.raw(&format!("  链接指向  : {}", path_to_display(&t)));
                }
            }
            if let Some(free) = fsx::available_space(p) {
                out.raw(&format!("  卷剩余    : {}", format_size(free)));
            }
        }

        if let Some(al) = algo {
            if md.is_file() {
                match hash::hash_file(al, p) {
                    Ok(v) => {
                        let hexv = crate::util::hex(&v);
                        kv.push(("algo".to_string(), JVal::s(al.name())));
                        kv.push(("hash".to_string(), JVal::s(hexv.clone())));
                        if !term::json_on() {
                            out.raw(&format!("  {}  : {}", al.name(), hexv));
                        }
                    }
                    Err(e) => sum.fail(Fail::io(p.clone(), e)),
                }
            }
        }
        out.json_item(JVal::Obj(kv));
        sum.ok += 1;
    }
    sum
}

fn kind_name(md: &fs::Metadata) -> &'static str {
    if md.file_type().is_symlink() {
        "符号链接"
    } else if md.is_dir() {
        "目录"
    } else if md.is_file() {
        "文件"
    } else {
        "特殊文件"
    }
}

fn fmt_time(t: Option<std::time::SystemTime>) -> String {
    match t {
        Some(t) => system_time_to_local(t).format("YYYY-MM-DD HH:mm:ss"),
        None => "-".to_string(),
    }
}

// ---------------------------------------------------------------- tree

pub fn tree(a: &TreeArgs, _g: &Global, out: &mut Out) -> Summary {
    let mut sum = Summary::new();
    let root = &a.root;
    if !root.is_dir() {
        sum.fail(Fail::usage(format!("{} 不是目录", path_to_display(root))));
        return sum;
    }
    let ci = cfg!(windows);
    let exc: Vec<Pattern> = a
        .exclude
        .iter()
        .filter_map(|p| Pattern::new(p, ci).ok())
        .collect();

    out.raw(&term::bold(&path_to_display(root)));
    let mut acc = TreeAcc { dirs: 0, files: 0, bytes: 0 };
    render_tree(
        &TreeCursor { dir: root, depth: 1, max_depth: a.depth, prefix: String::new() },
        &TreeCfg { dirs_only: a.dirs_only, no_size: a.no_size, sort_size: a.sort == "size" },
        &exc,
        out,
        &mut acc,
    );

    out.raw(&term::dim(&format!(
        "合计 {} 个文件，{} 个目录，{}",
        acc.files,
        acc.dirs,
        format_size(acc.bytes)
    )));
    out.json_item(JVal::Obj(vec![
        ("status".to_string(), JVal::s("ok")),
        ("action".to_string(), JVal::s("tree")),
        ("path".to_string(), JVal::s(path_to_display(root))),
        ("files".to_string(), JVal::n(acc.files as i64)),
        ("dirs".to_string(), JVal::n(acc.dirs as i64)),
        ("size".to_string(), JVal::n(acc.bytes as i64)),
    ]));
    sum.ok = acc.files + acc.dirs;
    sum
}

struct TreeCfg {
    dirs_only: bool,
    no_size: bool,
    sort_size: bool,
}

#[derive(Default)]
struct TreeAcc {
    dirs: usize,
    files: usize,
    bytes: u64,
}

/// 一次递归的位置信息（把「目录 + 深度 + 缩进前缀」打包，避免参数爆炸）。
struct TreeCursor<'a> {
    dir: &'a Path,
    depth: usize,
    max_depth: usize,
    prefix: String,
}

/// 递归画树：用 `├─` / `└─` 与延续竖线，保证缩进与顺序都稳定。
fn render_tree(cur: &TreeCursor<'_>, cfg: &TreeCfg, exc: &[Pattern], out: &mut Out, acc: &mut TreeAcc) {
    let (dir, depth, max_depth, prefix) = (cur.dir, cur.depth, cur.max_depth, cur.prefix.as_str());
    let rd = match fs::read_dir(dir) {
        Ok(r) => r,
        Err(_) => return,
    };
    let mut items: Vec<(PathBuf, bool, u64, String)> = Vec::new();
    for ent in rd.flatten() {
        let p = ent.path();
        let name = p.file_name().map(|s| s.to_string_lossy().to_string()).unwrap_or_default();
        if exc.iter().any(|x| x.is_match(&name)) {
            continue;
        }
        // 类型与文件大小都取自 DirEntry（Windows 上零系统调用）
        let md = ent.metadata().ok();
        let is_dir = md
            .as_ref()
            .map(|m| m.is_dir() && !m.file_type().is_symlink())
            .unwrap_or(false);
        if cfg.dirs_only && !is_dir {
            continue;
        }
        // 体积用于显示与排序；目录要递归求和（--no-size 且不排序时省掉）
        let size = if is_dir {
            if cfg.no_size && !cfg.sort_size {
                0
            } else {
                fsx::dir_stats(&p).1
            }
        } else {
            match &md {
                Some(m) => m.len(),
                None => 0,
            }
        };
        items.push((p, is_dir, size, name));
    }
    if cfg.sort_size {
        items.sort_by_key(|x| std::cmp::Reverse(x.2));
    } else {
        items.sort_by(|a, b| crate::algo::natsort::natural_cmp(&a.3, &b.3));
    }

    let n = items.len();
    for (idx, (p, is_dir, size, name)) in items.into_iter().enumerate() {
        let last = idx + 1 == n;
        let branch = if last { "└─ " } else { "├─ " };
        let shown = if is_dir {
            term::cyan(&format!("{name}/"))
        } else {
            name.clone()
        };
        let tail = if cfg.no_size {
            String::new()
        } else {
            format!("  {}", term::dim(&format_size(size)))
        };
        out.raw(&format!("{prefix}{branch}{shown}{tail}"));
        if is_dir {
            acc.dirs += 1;
        } else {
            acc.files += 1;
            acc.bytes += size;
        }
        if is_dir && depth < max_depth {
            let next = format!("{prefix}{}", if last { "   " } else { "│  " });
            render_tree(
                &TreeCursor { dir: &p, depth: depth + 1, max_depth, prefix: next },
                cfg,
                exc,
                out,
                acc,
            );
        }
    }
}

// ---------------------------------------------------------------- wc

pub fn wc(a: &WcArgs, _g: &Global, out: &mut Out) -> Summary {
    let mut sum = Summary::new();
    let (mut tl, mut tw, mut tb, mut tc) = (0u64, 0u64, 0u64, 0u64);
    for p in &a.paths {
        let data = if p.as_os_str() == "-" {
            let mut b = Vec::new();
            match std::io::stdin().read_to_end(&mut b) {
                Ok(_) => b,
                Err(e) => {
                    sum.fail(Fail::io(p.clone(), e));
                    continue;
                }
            }
        } else {
            match fs::read(p) {
                Ok(b) => b,
                Err(e) => {
                    sum.fail(Fail::io(p.clone(), e));
                    continue;
                }
            }
        };
        // 二进制检测：前 8KB 含 NUL
        let probe = &data[..data.len().min(8192)];
        if !a.binary && probe.contains(&0) {
            sum.skipped += 1;
            out.warn(format!("{} 看起来是二进制文件，已跳过（--binary 强制统计）", path_to_display(p)));
            continue;
        }
        let (lines, words) = count_text(&data);
        let chars = String::from_utf8_lossy(&data).chars().count() as u64;
        tl += lines;
        tw += words;
        tb += data.len() as u64;
        tc += chars;

        let mut line = format!("{:>8} {:>8} {:>10}", lines, words, data.len());
        if a.chars {
            line.push_str(&format!(" {:>10}", chars));
        }
        line.push(' ');
        line.push_str(&path_to_display(p));
        out.raw(&line);
        out.json_item(JVal::Obj(vec![
            ("status".to_string(), JVal::s("ok")),
            ("action".to_string(), JVal::s("wc")),
            ("path".to_string(), JVal::s(path_to_display(p))),
            ("lines".to_string(), JVal::n(lines as i64)),
            ("words".to_string(), JVal::n(words as i64)),
            ("bytes".to_string(), JVal::n(data.len() as i64)),
            ("chars".to_string(), JVal::n(chars as i64)),
        ]));
        sum.ok += 1;
    }
    if a.total && a.paths.len() > 1 {
        let mut line = format!("{:>8} {:>8} {:>10}", tl, tw, tb);
        if a.chars {
            line.push_str(&format!(" {:>10}", tc));
        }
        line.push(' ');
        line.push_str("合计");
        out.raw(&line);
    }
    sum
}

/// 行数 / 词数统计。
///
/// 必须按 Unicode 标量值判断空白，不能按字节判断：UTF-8 的多字节序列里
/// 会出现 0xA0 等字节，它们按字节解释恰是 U+00A0（不换行空格），
/// 用字节判断会把中文词错误地拆开（实测「中文内容统计测试」被拆成 2 个词）。
fn count_text(data: &[u8]) -> (u64, u64) {
    let mut lines = 0u64;
    let mut words = 0u64;
    let mut in_word = false;
    match std::str::from_utf8(data) {
        Ok(s) => {
            for c in s.chars() {
                if c == '\n' {
                    lines += 1;
                    in_word = false;
                } else if c.is_whitespace() {
                    in_word = false;
                } else if !in_word {
                    in_word = true;
                    words += 1;
                }
            }
        }
        Err(_) => {
            // 非 UTF-8（可能是 GB18030 等）：退回按 ASCII 字节判断
            for &b in data {
                if b == b'\n' {
                    lines += 1;
                    in_word = false;
                } else if b.is_ascii_whitespace() {
                    in_word = false;
                } else if !in_word {
                    in_word = true;
                    words += 1;
                }
            }
        }
    }
    // 末行没换行也算一行
    if !data.is_empty() && !data.ends_with(b"\n") {
        lines += 1;
    }
    (lines, words)
}

// ---------------------------------------------------------------- dedup

pub fn dedup(a: &DedupArgs, g: &Global, out: &mut Out) -> Summary {
    let mut sum = Summary::new();
    let min_size = match crate::util::parse_size(&a.min_size) {
        Ok(v) => v,
        Err(e) => {
            sum.fail(Fail::usage(format!("--min-size: {e}")));
            return sum;
        }
    };
    let ci = cfg!(windows);
    let exc: Vec<Pattern> = a.exclude.iter().filter_map(|p| Pattern::new(p, ci).ok()).collect();

    // 1) 收集所有文件并按其大小分组
    let mut by_size: HashMap<u64, Vec<PathBuf>> = HashMap::new();
    for root in &a.roots {
        let items = if root.is_file() {
            vec![(root.clone(), String::new(), 0usize, false)]
        } else {
            match walk_relative(root, usize::MAX, false) {
                Ok(v) => v,
                Err(e) => {
                    sum.fail(Fail::io(root.clone(), e));
                    continue;
                }
            }
        };
        for (p, rel, _, is_dir) in items {
            if is_dir {
                continue;
            }
            if exc.iter().any(|x| x.is_match(&rel)) {
                continue;
            }
            let md = match fs::symlink_metadata(&p) {
                Ok(m) => m,
                Err(_) => continue,
            };
            if md.len() < min_size.max(1) {
                continue;
            }
            by_size.entry(md.len()).or_default().push(p);
        }
    }

    // 2) 头部 4KB + 尾部 4KB 再筛
    let mut groups: Vec<Vec<PathBuf>> = Vec::new();
    for (_, paths) in by_size {
        if paths.len() < 2 {
            continue;
        }
        let mut by_head: HashMap<Vec<u8>, Vec<PathBuf>> = HashMap::new();
        for p in paths {
            if let Ok(sig) = head_tail_sig(&p) {
                by_head.entry(sig).or_default().push(p);
            }
        }
        for (_, v) in by_head {
            if v.len() >= 2 {
                groups.push(v);
            }
        }
    }

    // 3) 全文件哈希确认
    let mut dup_groups: Vec<Vec<PathBuf>> = Vec::new();
    for cand in groups {
        let mut by_hash: HashMap<Vec<u8>, Vec<PathBuf>> = HashMap::new();
        for p in cand {
            match hash::hash_file(Algo::Sha256, &p) {
                Ok(h) => by_hash.entry(h).or_default().push(p),
                Err(e) => sum.fail(Fail::io(p, e)),
            }
        }
        for (_, v) in by_hash {
            if v.len() >= 2 {
                dup_groups.push(v);
            }
        }
    }

    if dup_groups.is_empty() {
        out.raw("没有发现重复文件");
        return sum;
    }

    let mut waste = 0u64;
    let mut plan = crate::plan::Plan::new();
    let do_delete = a.delete.is_some();
    let keep_first = a.delete.as_deref().map(|s| s.eq_ignore_ascii_case("keep-first")).unwrap_or(true);

    for g in &dup_groups {
        let size = fs::metadata(&g[0]).map(|m| m.len()).unwrap_or(0);
        let keep = if keep_first { 1 } else { 0 };
        let victims: Vec<&PathBuf> = if keep_first { g.iter().skip(1).collect() } else { g.iter().collect() };
        out.raw(&term::bold(&format!("{} × {} (每组省 {})", g.len(), format_size(size), format_size(size * victims.len() as u64))));
        for (i, p) in g.iter().enumerate() {
            let mark = if i < keep { term::dim("[保留]") } else { term::yellow("[重复]") };
            out.raw(&format!("  {} {}", mark, path_to_display(p)));
        }
        waste += size * victims.len() as u64;
        for v in victims {
            if do_delete {
                if a.hardlink {
                    // 硬链接替代删除：保留路径，节省空间
                    plan.push(crate::plan::Action::Link {
                        target: g[0].clone(),
                        link: v.clone(),
                        symbolic: false,
                    });
                } else {
                    plan.push(crate::plan::Action::Delete { path: v.clone(), is_dir: false, bytes: size });
                }
            }
        }
        out.json_item(JVal::Obj(vec![
            ("status".to_string(), JVal::s("ok")),
            ("action".to_string(), JVal::s("dup-group")),
            ("size".to_string(), JVal::n(size as i64)),
            ("count".to_string(), JVal::n(g.len() as i64)),
            (
                "paths".to_string(),
                JVal::Arr(g.iter().map(|p| JVal::s(path_to_display(p))).collect()),
            ),
        ]));
        sum.ok += 1;
    }

    out.raw(&term::dim(&format!(
        "可回收 {}（{} 组，{} 个文件）",
        format_size(waste),
        dup_groups.len(),
        dup_groups.iter().map(|g| g.len()).sum::<usize>()
    )));

    if do_delete && !plan.is_empty() {
        if a.hardlink {
            // 硬链接需要先删后建
            let mut del = crate::plan::Plan::new();
            for act in &plan.actions {
                if let crate::plan::Action::Link { link, .. } = act {
                    del.push(crate::plan::Action::Delete { path: link.clone(), is_dir: false, bytes: 0 });
                }
            }
            let mut merged = del;
            for act in &plan.actions {
                merged.push(act.clone());
            }
            plan = merged;
        }
        if !g.dry_run && plan.needs_confirm() && !super::fsops::confirm_plan(&plan, out) {
            sum.aborted = true;
            sum.fail(Fail::usage("用户取消了操作"));
            return sum;
        }
        if let Err(e) = plan.execute(g.dry_run, out, &mut sum) {
            sum.fail(Fail::usage(e.to_string()));
        }
    } else if do_delete {
        out.raw("没有需要清理的文件");
    }
    sum
}

fn head_tail_sig(p: &Path) -> std::io::Result<Vec<u8>> {
    const N: usize = 4096;
    let f = fs::File::open(p)?;
    let len = f.metadata()?.len() as usize;
    let mut head = vec![0u8; N.min(len)];
    let mut r = BufReader::new(f);
    r.read_exact(&mut head).or_else(|e| {
        if e.kind() == std::io::ErrorKind::UnexpectedEof {
            Ok(())
        } else {
            Err(e)
        }
    })?;
    let mut sig = head.clone();
    if len > N * 2 {
        use std::io::{Seek, SeekFrom};
        r.seek(SeekFrom::Start((len - N) as u64))?;
        let mut tail = vec![0u8; N];
        r.read_exact(&mut tail)?;
        sig.extend_from_slice(&tail);
    }
    Ok(sig)
}

/// 供上面 `find` 使用时区换算（保持编译期无未用告警）。
#[allow(dead_code)]
fn _unused_civil(c: &Civil) -> i64 {
    local_to_unix_secs(c)
}

#[allow(dead_code)]
fn _drain_lines(data: &[u8]) -> usize {
    let mut n = 0;
    for _ in BufReader::new(data).lines() {
        n += 1;
    }
    n
}
