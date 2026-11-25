//! 计划与执行层（架构 D4/D5/D7）。
//!
//! 核心约定：**任何破坏性命令都先建计划、再执行**。`--dry-run` 只是把
//! `execute` 的最后一个参数置为 true，走的完全是同一条代码路径 —— 这样
//! 预演结果与真实执行不会漂移。

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use crate::algo::pattern::Pattern;
use crate::fsx;
use crate::term::Out;

// ---------------------------------------------------------------- 错误与汇总

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailKind {
    Io,
    Unsupported,
    Guard,
    Usage,
    Resource,
    Corrupt,
}

impl FailKind {
    pub fn tag(self) -> &'static str {
        match self {
            FailKind::Io => "io",
            FailKind::Unsupported => "unsupported",
            FailKind::Guard => "guard",
            FailKind::Usage => "usage",
            FailKind::Resource => "resource",
            FailKind::Corrupt => "corrupt",
        }
    }
}

#[derive(Debug, Clone)]
pub struct Fail {
    pub path: Option<PathBuf>,
    pub kind: FailKind,
    pub msg: String,
}

impl Fail {
    pub fn io(p: impl Into<PathBuf>, e: io::Error) -> Fail {
        Fail { path: Some(p.into()), kind: FailKind::Io, msg: e.to_string() }
    }
    pub fn usage(msg: impl Into<String>) -> Fail {
        Fail { path: None, kind: FailKind::Usage, msg: msg.into() }
    }
    pub fn guard(msg: impl Into<String>) -> Fail {
        Fail { path: None, kind: FailKind::Guard, msg: msg.into() }
    }
    pub fn unsupported(msg: impl Into<String>) -> Fail {
        Fail { path: None, kind: FailKind::Unsupported, msg: msg.into() }
    }
    pub fn resource(msg: impl Into<String>) -> Fail {
        Fail { path: None, kind: FailKind::Resource, msg: msg.into() }
    }
    pub fn corrupt(p: impl Into<PathBuf>, msg: impl Into<String>) -> Fail {
        Fail { path: Some(p.into()), kind: FailKind::Corrupt, msg: msg.into() }
    }

    pub fn render(&self) -> String {
        let p = self.path.as_ref().map(|x| crate::util::path_to_display(x));
        match p {
            Some(p) => format!("{p}: {}", self.msg),
            None => self.msg.clone(),
        }
    }
}

/// 命令执行汇总。退出码由它决定。
#[derive(Debug, Default)]
pub struct Summary {
    pub ok: usize,
    pub skipped: usize,
    pub failed: Vec<Fail>,
    /// 计划阶段就失败（尚未动磁盘）
    pub aborted: bool,
}

impl Summary {
    pub fn new() -> Summary {
        Summary::default()
    }

    pub fn fail(&mut self, f: Fail) {
        self.failed.push(f);
    }

    /// 0 全成功 / 1 有条目失败 / 2 用法 / 3 资源 / 4 护栏 / 5 不支持
    pub fn exit_code(&self) -> i32 {
        if self.failed.is_empty() {
            return 0;
        }
        // 严重程度取最高
        let mut code = 1;
        for f in &self.failed {
            let c = match f.kind {
                FailKind::Io | FailKind::Corrupt => 1,
                FailKind::Usage => 2,
                FailKind::Resource => 3,
                FailKind::Guard => 4,
                FailKind::Unsupported => 5,
            };
            code = code.max(c);
        }
        if self.aborted {
            return code.max(2);
        }
        code
    }

    pub fn is_ok(&self) -> bool {
        self.failed.is_empty()
    }

    /// 打印失败列表到 stderr。
    pub fn report(&self, out: &mut Out) {
        for f in &self.failed {
            out.error(format!("[{}] {}", f.kind.tag(), f.render()));
        }
    }

    /// 一行人类可读总结。
    pub fn line(&self) -> String {
        let mut s = format!("完成 {} 项", self.ok);
        if self.skipped > 0 {
            s.push_str(&format!("，跳过 {}", self.skipped));
        }
        if !self.failed.is_empty() {
            s.push_str(&format!("，失败 {}", self.failed.len()));
        }
        s
    }
}

// ---------------------------------------------------------------- 安全护栏

/// 判断是否属于「绝对不该被批量删除」的路径（需求 F1 验收：指向 C:\ 或 $HOME 退出码 4）。
pub fn guard_delete(p: &Path) -> Result<(), Fail> {
    let abs = absolute(p);
    // 1) 根路径：整个路径里没有任何普通名字段（`C://`、`C:`、`/`）
    let has_name = abs
        .components()
        .any(|c| matches!(c, std::path::Component::Normal(_)));
    if !has_name {
        return Err(Fail::guard(format!(
            "拒绝删除根路径 {}（这会清空整个卷）",
            crate::util::path_to_display(&abs)
        )));
    }
    // 2) 用户主目录本身
    if let Some(home) = home_dir() {
        if same_path(&abs, &home) {
            return Err(Fail::guard(format!(
                "拒绝删除用户主目录 {}",
                crate::util::path_to_display(&abs)
            )));
        }
    }
    // 3) 系统关键目录
    #[cfg(windows)]
    {
        let mut sys = vec![
            std::env::var("SystemRoot").unwrap_or_else(|_| "C:\\Windows".into()),
            std::env::var("ProgramFiles").unwrap_or_else(|_| "C:\\Program Files".into()),
            std::env::var("ProgramFiles(x86)").unwrap_or_else(|_| "C:\\Program Files (x86)".into()),
            std::env::var("ProgramData").unwrap_or_else(|_| "C:\\ProgramData".into()),
        ];
        sys.dedup();
        for s in sys {
            if same_path(&abs, Path::new(&s)) {
                return Err(Fail::guard(format!(
                    "拒绝删除系统目录 {}",
                    crate::util::path_to_display(&abs)
                )));
            }
        }
    }
    #[cfg(not(windows))]
    {
        for s in ["/usr", "/etc", "/bin", "/sbin", "/lib", "/var", "/boot", "/System", "/Library"] {
            if same_path(&abs, Path::new(s)) {
                return Err(Fail::guard(format!("拒绝删除系统目录 {s}")));
            }
        }
    }
    // 4) 当前工作目录本身（批量删掉整个工程也是灾难）
    if let Ok(cwd) = std::env::current_dir() {
        if same_path(&abs, &cwd) {
            return Err(Fail::guard(format!(
                "拒绝删除当前工作目录 {}",
                crate::util::path_to_display(&abs)
            )));
        }
    }
    Ok(())
}

pub fn home_dir() -> Option<PathBuf> {
    #[cfg(windows)]
    {
        std::env::var_os("USERPROFILE")
            .map(PathBuf::from)
            .or_else(|| {
                let d = std::env::var_os("HOMEDRIVE")?;
                let p = std::env::var_os("HOMEPATH")?;
                let mut s = d;
                s.push(p);
                Some(PathBuf::from(s))
            })
    }
    #[cfg(not(windows))]
    {
        std::env::var_os("HOME").map(PathBuf::from)
    }
}

pub fn absolute(p: &Path) -> PathBuf {
    if p.is_absolute() {
        return p.to_path_buf();
    }
    std::env::current_dir().map(|d| d.join(p)).unwrap_or_else(|_| p.to_path_buf())
}

fn same_path(a: &Path, b: &Path) -> bool {
    let (a, b) = (absolute(a), absolute(b));
    #[cfg(windows)]
    {
        a.to_string_lossy().to_lowercase() == b.to_string_lossy().to_lowercase()
    }
    #[cfg(not(windows))]
    {
        a == b
    }
}

/// 破坏性操作的规模护栏：超过阈值要求显式确认。
pub const CONFIRM_COUNT: usize = 200;
pub const CONFIRM_BYTES: u64 = 1 << 30; // 1 GiB

// ---------------------------------------------------------------- 选择器展开

/// 把一个路径/通配符参数展开成真实路径列表。
///
/// - 不含通配符：原样返回（存在与否由调用方判断）
/// - 含 `* ? [ {` ：定位最长的无通配前缀做基准目录，剩余部分作为模式
///   对整条相对路径（含 `/`）匹配，因此 `**` 与 `dir/*/x` 都能工作
pub fn expand(spec: &Path, ci: bool) -> io::Result<Vec<PathBuf>> {
    let s = spec.to_string_lossy().replace('\\', "/");
    let has_wild = s.contains('*') || s.contains('?') || s.contains('[') || s.contains('{');
    if !has_wild {
        return Ok(vec![spec.to_path_buf()]);
    }
    // 拆出基准目录
    let parts: Vec<&str> = s.split('/').collect();
    let mut base = String::new();
    let mut idx = 0usize;
    for (i, part) in parts.iter().enumerate() {
        if part.contains('*')
            || part.contains('?')
            || part.contains('[')
            || part.contains('{')
        {
            idx = i;
            break;
        }
        if (i > 0 || !part.is_empty() || s.starts_with('/'))
            && !(i == 0 && part.is_empty() && !s.starts_with('/')) {
                base.push_str(part);
                base.push('/');
            }
        idx = i + 1;
    }
    let base_dir = if base.is_empty() { PathBuf::from(".") } else { PathBuf::from(base.trim_end_matches('/')) };
    let pattern_str = parts[idx..].join("/");
    let pat = Pattern::new(&pattern_str, ci)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;

    let depth_budget = if pattern_str.contains("**") {
        usize::MAX
    } else {
        pattern_str.split('/').count()
    };
    let opts = fsx::WalkOpts { max_depth: depth_budget, follow_links: false, sort: true };
    let mut out = Vec::new();
    // 基准目录自身也可能匹配（例如模式是 `**`）
    let entries = fsx::walk_dir(&base_dir, &opts).unwrap_or_default();
    for (p, _) in entries {
        let rel = p
            .strip_prefix(&base_dir)
            .unwrap_or(&p)
            .to_string_lossy()
            .replace('\\', "/");
        if pat.is_match(&rel) {
            out.push(p);
        }
    }
    Ok(out)
}

/// 批量展开 + 去重（保持顺序）。
pub fn expand_many(specs: &[PathBuf], ci: bool) -> io::Result<Vec<PathBuf>> {
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    for s in specs {
        for p in expand(s, ci)? {
            if seen.insert(p.clone()) {
                out.push(p);
            }
        }
    }
    Ok(out)
}

// ---------------------------------------------------------------- 动作

#[derive(Debug, Clone)]
pub enum Action {
    Delete { path: PathBuf, is_dir: bool, bytes: u64 },
    /// 送入系统回收站
    Trash { path: PathBuf, bytes: u64 },
    /// 只在目录为空时删除（对应 `rm --no-recursive`）
    RmdirIfEmpty { path: PathBuf },
    CopyFile { src: PathBuf, dst: PathBuf, bytes: u64, opts: fsx::CopyOpts },
    CopyTree { src: PathBuf, dst: PathBuf, bytes: u64, opts: fsx::CopyOpts },
    MkDir(PathBuf),
    Move { src: PathBuf, dst: PathBuf, bytes: u64 },
    Rename { from: PathBuf, to: PathBuf },
    Link { target: PathBuf, link: PathBuf, symbolic: bool },
    Touch { path: PathBuf, mtime: Option<u64>, atime: Option<u64>, create: bool },
    WriteFile { path: PathBuf, data: Vec<u8> },
}

impl Action {
    pub fn describe(&self) -> String {
        match self {
            Action::Delete { path, is_dir, .. } => {
                format!("删除{} {}", if *is_dir { "目录" } else { "" }, crate::util::path_to_display(path))
            }
            Action::Trash { path, .. } => {
                format!("移入回收站 {}", crate::util::path_to_display(path))
            }
            Action::RmdirIfEmpty { path } => {
                format!("删除空目录 {}", crate::util::path_to_display(path))
            }
            Action::CopyFile { src, dst, .. } => format!(
                "复制 {} → {}",
                crate::util::path_to_display(src),
                crate::util::path_to_display(dst)
            ),
            Action::CopyTree { src, dst, .. } => format!(
                "复制目录 {} → {}",
                crate::util::path_to_display(src),
                crate::util::path_to_display(dst)
            ),
            Action::MkDir(p) => format!("创建目录 {}", crate::util::path_to_display(p)),
            Action::Move { src, dst, .. } => format!(
                "移动 {} → {}",
                crate::util::path_to_display(src),
                crate::util::path_to_display(dst)
            ),
            Action::Rename { from, to } => format!(
                "改名 {} → {}",
                crate::util::path_to_display(from),
                crate::util::path_to_display(to)
            ),
            Action::Link { target, link, symbolic } => format!(
                "{}链接 {} → {}",
                if *symbolic { "符号" } else { "硬" },
                crate::util::path_to_display(link),
                crate::util::path_to_display(target)
            ),
            Action::Touch { path, .. } => format!("更新时间戳 {}", crate::util::path_to_display(path)),
            Action::WriteFile { path, data } => format!(
                "写入 {}（{} 字节）",
                crate::util::path_to_display(path),
                data.len()
            ),
        }
    }

    pub fn bytes(&self) -> u64 {
        match self {
            Action::Delete { bytes, .. }
            | Action::Trash { bytes, .. }
            | Action::CopyFile { bytes, .. }
            | Action::CopyTree { bytes, .. }
            | Action::Move { bytes, .. } => *bytes,
            Action::WriteFile { data, .. } => data.len() as u64,
            _ => 0,
        }
    }
}

#[derive(Debug, Default)]
pub struct Plan {
    pub actions: Vec<Action>,
    /// 计划里被跳过的条目（冲突、过滤），只计数
    pub skipped: usize,
    /// 计划阶段的告警
    pub warnings: Vec<String>,
}

impl Plan {
    pub fn new() -> Plan {
        Plan::default()
    }

    pub fn push(&mut self, a: Action) {
        self.actions.push(a);
    }

    pub fn is_empty(&self) -> bool {
        self.actions.is_empty()
    }

    pub fn total_bytes(&self) -> u64 {
        self.actions.iter().map(|a| a.bytes()).sum()
    }

    /// 需要确认吗（对象数或字节数超阈值）
    pub fn needs_confirm(&self) -> bool {
        self.actions.len() > CONFIRM_COUNT || self.total_bytes() > CONFIRM_BYTES
    }

    pub fn preview(&self) -> String {
        let mut s = format!(
            "计划：{} 项操作，共 {}",
            self.actions.len(),
            crate::util::format_size(self.total_bytes())
        );
        if self.skipped > 0 {
            s.push_str(&format!("，跳过 {} 项", self.skipped));
        }
        s
    }

    /// 执行计划。`dry_run` 为 true 时只报告不落盘。
    pub fn execute(&self, dry_run: bool, out: &mut Out, summary: &mut Summary) -> io::Result<()> {
        let total = self.actions.len();
        let mut prog = if dry_run {
            None
        } else {
            Some(crate::term::Progress::new("执行中", total as u64))
        };

        // 两阶段改名：先全部改到临时名，再改到最终名（D5）
        let renames: Vec<(PathBuf, PathBuf)> = self
            .actions
            .iter()
            .filter_map(|a| match a {
                Action::Rename { from, to } => Some((from.clone(), to.clone())),
                _ => None,
            })
            .collect();
        if !renames.is_empty() && !dry_run {
            match two_phase_rename(&renames, out, summary) {
                Ok(n) => summary.ok += n,
                Err(e) => summary.fail(Fail::usage(e.to_string())),
            }
        }

        for a in &self.actions {
            if let Some(p) = prog.as_mut() {
                p.inc(1);
            }
            if dry_run {
                out.note(format!("{} {}", crate::term::dim("would"), a.describe()));
                summary.ok += 1;
                continue;
            }
            // 真实执行时改名由 two_phase_rename 统一报告，这里跳过避免重复输出。
            // 注意：这个跳过必须放在 dry_run 之后，否则预演会一条都不显示。
            if matches!(a, Action::Rename { .. }) {
                continue;
            }
            match run_action(a) {
                Ok(()) => {
                    summary.ok += 1;
                    out.ok(action_tag(a), action_path(a), "");
                }
                Err(e) => summary.fail(Fail::io(action_path(a), e)),
            }
        }
        if let Some(p) = prog.as_mut() {
            p.finish();
        }
        summary.skipped += self.skipped;
        Ok(())
    }
}

fn action_tag(a: &Action) -> &'static str {
    match a {
        Action::Delete { .. } => "deleted",
        Action::Trash { .. } => "trashed",
        Action::RmdirIfEmpty { .. } => "rmdir",
        Action::CopyFile { .. } | Action::CopyTree { .. } => "copied",
        Action::MkDir(_) => "mkdir",
        Action::Move { .. } => "moved",
        Action::Rename { .. } => "renamed",
        Action::Link { .. } => "linked",
        Action::Touch { .. } => "touch",
        Action::WriteFile { .. } => "wrote",
    }
}

fn action_path(a: &Action) -> &Path {
    match a {
        Action::Delete { path, .. } => path,
        Action::Trash { path, .. } => path,
        Action::RmdirIfEmpty { path } => path,
        Action::CopyFile { dst, .. } | Action::CopyTree { dst, .. } => dst,
        Action::MkDir(p) => p,
        Action::Move { dst, .. } => dst,
        Action::Rename { to, .. } => to,
        Action::Link { link, .. } => link,
        Action::Touch { path, .. } => path,
        Action::WriteFile { path, .. } => path,
    }
}

fn run_action(a: &Action) -> io::Result<()> {
    match a {
        Action::Delete { path, .. } => {
            if !fsx::exists_any(path) {
                return Ok(());
            }
            fsx::remove_tree(path, false).map(|_| ())
        }
        Action::CopyFile { src, dst, opts, .. } => fsx::copy_file(src, dst, opts).map(|_| ()),
        Action::CopyTree { src, dst, opts, .. } => fsx::copy_tree(src, dst, opts).map(|_| ()),
        Action::Trash { path, .. } => {
            if !fsx::exists_any(path) {
                return Ok(());
            }
            fsx::trash(path)
        }
        Action::RmdirIfEmpty { path } => match fs::remove_dir(path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e),
        },
        Action::MkDir(p) => fs::create_dir_all(p),
        Action::Move { src, dst, .. } => {
            if let Some(parent) = dst.parent() {
                if !parent.as_os_str().is_empty() {
                    fs::create_dir_all(parent)?;
                }
            }
            match fs::rename(src, dst) {
                Ok(()) => Ok(()),
                Err(_) => {
                    // 跨卷：复制 + 删除
                    let opts = fsx::CopyOpts::default();
                    if src.is_dir() {
                        fsx::copy_tree(src, dst, &opts)?;
                    } else {
                        fsx::copy_file(src, dst, &opts)?;
                    }
                    fsx::remove_tree(src, false).map(|_| ())
                }
            }
        }
        Action::Rename { from, to } => fs::rename(from, to),
        Action::Link { target, link, symbolic } => {
            if *symbolic {
                #[cfg(unix)]
                {
                    std::os::unix::fs::symlink(target, link)
                }
                #[cfg(windows)]
                {
                    if target.is_dir() {
                        std::os::windows::fs::symlink_dir(target, link)
                    } else {
                        std::os::windows::fs::symlink_file(target, link)
                    }
                }
            } else {
                fs::hard_link(target, link)
            }
        }
        Action::Touch { path, mtime, atime, create } => {
            if !fsx::exists_any(path) {
                if *create {
                    if let Some(parent) = path.parent() {
                        if !parent.as_os_str().is_empty() {
                            fs::create_dir_all(parent)?;
                        }
                    }
                    fs::write(path, b"")?;
                } else {
                    return Ok(());
                }
            }
            fsx::set_times(path, *mtime, *atime)
        }
        Action::WriteFile { path, data } => {
            if let Some(parent) = path.parent() {
                if !parent.as_os_str().is_empty() {
                    fs::create_dir_all(parent)?;
                }
            }
            fs::write(path, data)
        }
    }
}

/// 两阶段改名：先统一挪到临时名，再落到最终名。
///
/// 这样 `a→b, b→c` 这类链式改名不会互相踩踏（D5 / 需求 F4.3）。
fn two_phase_rename(
    pairs: &[(PathBuf, PathBuf)],
    out: &mut Out,
    summary: &mut Summary,
) -> io::Result<usize> {
    // 只对「源是别人目标」的链条才需要中转；但统一走临时名更简单也更安全
    let mut staged: Vec<(PathBuf, PathBuf)> = Vec::with_capacity(pairs.len());
    for (i, (from, _)) in pairs.iter().enumerate() {
        let dir = from.parent().map(|p| p.to_path_buf()).unwrap_or_else(|| PathBuf::from("."));
        let tmp = fsx::temp_name(&dir, i);
        match fs::rename(from, &tmp) {
            Ok(()) => staged.push((tmp, from.clone())),
            Err(e) => {
                summary.fail(Fail::io(from.clone(), e));
            }
        }
    }
    // 第二段：临时名 → 最终名。中途失败的，尽量还原回原名
    let mut done = 0usize;
    for (idx, (from, to)) in pairs.iter().enumerate() {
        // 找到对应的 staged 项（可能因第一段失败而缺失）
        let Some(pos) = staged.iter().position(|(_, orig)| orig == from) else {
            continue;
        };
        let tmp = staged[pos].0.clone();
        if fsx::exists_any(to) {
            let _ = fs::remove_file(to);
        }
        match fs::rename(&tmp, to) {
            Ok(()) => {
                done += 1;
                out.ok("renamed", to, "");
            }
            Err(e) => {
                summary.fail(Fail::io(to.clone(), e));
                // 还原
                let _ = fs::rename(&tmp, from);
            }
        }
        let _ = idx;
    }
    Ok(done)
}

/// 供 `rn` 命令使用：检查目标名冲突。
pub fn resolve_conflict(
    target: &Path,
    policy: ConflictPolicy,
) -> Result<Option<PathBuf>, Fail> {
    if !fsx::exists_any(target) {
        return Ok(Some(target.to_path_buf()));
    }
    match policy {
        ConflictPolicy::Skip => Ok(None),
        ConflictPolicy::Overwrite => Ok(Some(target.to_path_buf())),
        ConflictPolicy::Rename => Ok(Some(fsx::unique_path(target))),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConflictPolicy {
    Skip,
    Overwrite,
    Rename,
}

impl ConflictPolicy {
    pub fn parse(s: &str) -> Option<ConflictPolicy> {
        match s.to_ascii_lowercase().as_str() {
            "skip" => Some(ConflictPolicy::Skip),
            "overwrite" | "force" => Some(ConflictPolicy::Overwrite),
            "rename" => Some(ConflictPolicy::Rename),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmpdir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("wsak_plan_{}_{}", tag, std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn guard_rejects_roots() {
        #[cfg(windows)]
        {
            assert!(guard_delete(Path::new("C:\\")).is_err());
        }
        #[cfg(not(windows))]
        {
            assert!(guard_delete(Path::new("/")).is_err());
        }
        let d = tmpdir("guard");
        assert!(guard_delete(&d).is_ok());
        fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn guard_rejects_home() {
        if let Some(h) = home_dir() {
            assert!(guard_delete(&h).is_err());
        }
    }

    #[test]
    fn expand_wildcards() {
        let d = tmpdir("expand");
        fs::create_dir_all(d.join("a/sub")).unwrap();
        fs::write(d.join("a/1.txt"), b"").unwrap();
        fs::write(d.join("a/2.txt"), b"").unwrap();
        fs::write(d.join("a/3.md"), b"").unwrap();
        fs::write(d.join("a/sub/4.txt"), b"").unwrap();

        let spec = d.join("a").join("*.txt");
        let got = expand(&spec, false).unwrap();
        assert_eq!(got.len(), 2, "{got:?}");

        let spec = d.join("a").join("**").join("*.txt");
        let got = expand(&spec, false).unwrap();
        assert_eq!(got.len(), 3, "{got:?}");

        let spec = d.join("a").join("*").join("4.txt");
        let got = expand(&spec, false).unwrap();
        assert_eq!(got.len(), 1, "{got:?}");
        fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn expand_plain_path_is_identity() {
        let p = PathBuf::from("no/such/file.txt");
        assert_eq!(expand(&p, false).unwrap(), vec![p]);
    }

    #[test]
    fn plan_delete_and_execute() {
        let d = tmpdir("exec");
        let f = d.join("x.txt");
        fs::write(&f, b"12345").unwrap();
        let mut plan = Plan::new();
        plan.push(Action::Delete { path: f.clone(), is_dir: false, bytes: 5 });
        assert_eq!(plan.total_bytes(), 5);

        let mut out = Out::new();
        let mut sum = Summary::new();
        plan.execute(true, &mut out, &mut sum).unwrap();
        assert!(f.exists(), "dry-run 不应删文件");

        let mut sum2 = Summary::new();
        plan.execute(false, &mut out, &mut sum2).unwrap();
        assert!(!f.exists());
        assert_eq!(sum2.ok, 1);
        assert_eq!(sum2.exit_code(), 0);
        fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn two_phase_rename_handles_chain() {
        // a→b, b→c 必须两个文件都在
        let d = tmpdir("chain");
        let a = d.join("a.txt");
        let b = d.join("b.txt");
        fs::write(&a, b"A").unwrap();
        fs::write(&b, b"B").unwrap();
        let mut plan = Plan::new();
        plan.push(Action::Rename { from: a.clone(), to: b.clone() });
        plan.push(Action::Rename { from: b.clone(), to: d.join("c.txt") });
        let mut out = Out::new();
        let mut sum = Summary::new();
        plan.execute(false, &mut out, &mut sum).unwrap();
        assert!(d.join("b.txt").exists());
        assert!(d.join("c.txt").exists());
        assert_eq!(fs::read(d.join("b.txt")).unwrap(), b"A");
        assert_eq!(fs::read(d.join("c.txt")).unwrap(), b"B");
        // 没有临时文件残留
        let leftovers: Vec<_> = fs::read_dir(&d)
            .unwrap()
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().starts_with(".wsak_tmp_"))
            .collect();
        assert!(leftovers.is_empty());
        fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn summary_exit_codes() {
        let mut s = Summary::new();
        assert_eq!(s.exit_code(), 0);
        s.fail(Fail::unsupported("x"));
        assert_eq!(s.exit_code(), 5);
        let mut s = Summary::new();
        s.fail(Fail::guard("x"));
        assert_eq!(s.exit_code(), 4);
        let mut s = Summary::new();
        s.fail(Fail::resource("x"));
        assert_eq!(s.exit_code(), 3);
        let mut s = Summary::new();
        s.fail(Fail::usage("x"));
        assert_eq!(s.exit_code(), 2);
        let mut s = Summary::new();
        s.fail(Fail::io("p", io::Error::other("boom")));
        assert_eq!(s.exit_code(), 1);
    }

    #[test]
    fn conflict_policy_parse() {
        assert_eq!(ConflictPolicy::parse("SKIP"), Some(ConflictPolicy::Skip));
        assert_eq!(ConflictPolicy::parse("force"), Some(ConflictPolicy::Overwrite));
        assert_eq!(ConflictPolicy::parse("rename"), Some(ConflictPolicy::Rename));
        assert_eq!(ConflictPolicy::parse("wat"), None);
    }
}
