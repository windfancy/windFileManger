//! 命令实现。每个文件一组相关命令，全部走「先建计划、再执行」的同一路径。

pub mod archive_ops;
pub mod codec_ops;
pub mod config_ops;
pub mod fsops;
pub mod inspect;

use std::path::{Path, PathBuf};

use crate::algo::pattern::Pattern;
use crate::cli::{Cli, Cmd, SelectArgs};
use crate::plan::{expand_many, Fail, Summary};
use crate::term::{self, Out};

/// 全局运行上下文（从 CLI 全局参数派生）。
#[derive(Debug, Clone)]
pub struct Global {
    pub dry_run: bool,
    pub jobs: usize,
    pub verbose: bool,
    pub assume_yes: bool,
}

impl Global {
    fn from_cli(cli: &Cli) -> Global {
        Global {
            dry_run: cli.dry_run,
            jobs: cli.jobs.unwrap_or_else(default_jobs),
            verbose: cli.verbose,
            assume_yes: cli.yes,
        }
    }
}

pub fn default_jobs() -> usize {
    std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4).clamp(1, 64)
}

/// 入口：解析后统一分发，返回进程退出码。
pub fn dispatch(cli: Cli) -> i32 {
    let g = Global::from_cli(&cli);
    // 终端初始化
    let color = term::detect_color(cli.no_color);
    term::init(term::TermCfg {
        color,
        json: cli.json,
        quiet: cli.quiet,
        verbose: cli.verbose,
        assume_yes: cli.yes,
        interactive: std::io::IsTerminal::is_terminal(&std::io::stdin()) && !cli.yes,
    });

    let mut out = Out::new();
    let sum = match &cli.cmd {
        Cmd::Rm(a) => fsops::rm(a, &g, &mut out),
        Cmd::Cp(a) => fsops::cp(a, &g, &mut out),
        Cmd::Mv(a) => fsops::mv(a, &g, &mut out),
        Cmd::Rn(a) => fsops::rn(a, &g, &mut out),
        Cmd::Touch(a) => fsops::touch(a, &g, &mut out),
        Cmd::Link(a) => fsops::link(a, &g, &mut out),
        Cmd::Find(a) => inspect::find(a, &g, &mut out),
        Cmd::Stat(a) => inspect::stat(a, &g, &mut out),
        Cmd::Tree(a) => inspect::tree(a, &g, &mut out),
        Cmd::Wc(a) => inspect::wc(a, &g, &mut out),
        Cmd::Dedup(a) => inspect::dedup(a, &g, &mut out),
        Cmd::Pack(a) => archive_ops::pack(a, &g, &mut out),
        Cmd::Unpack(a) => archive_ops::unpack(a, &g, &mut out),
        Cmd::Split(a) => archive_ops::split(a, &g, &mut out),
        Cmd::Join(a) => archive_ops::join(a, &g, &mut out),
        Cmd::B64(a) => codec_ops::b64(a, &g, &mut out),
        Cmd::Hash(a) => codec_ops::hash(a, &g, &mut out),
        Cmd::Replace(a) => codec_ops::replace(a, &g, &mut out),
        Cmd::Cfg(a) => config_ops::cfg(a, &g, &mut out),
    };
    sum.report(&mut out);
    out.flush();
    if !sum.is_ok() && !cli.json {
        eprintln!("{}", term::dim(&sum.line()));
    }
    sum.exit_code()
}

// ---------------------------------------------------------------- 选择器

/// 名称/路径过滤器（对应 `SelectArgs`）。
pub struct Filter {
    patterns: Vec<Pattern>,
    include: Vec<Pattern>,
    exclude: Vec<Pattern>,
    files_only: bool,
    dirs_only: bool,
    depth: Option<usize>,
}

impl Filter {
    pub fn from_args(a: &SelectArgs) -> Result<Filter, Fail> {
        let ci = !(a.case_sensitive || a.cs_alias);
        let build = |v: &Vec<String>| -> Result<Vec<Pattern>, Fail> {
            let mut out = Vec::with_capacity(v.len());
            for p in v {
                out.push(Pattern::new(p, ci).map_err(Fail::usage)?);
            }
            Ok(out)
        };
        Ok(Filter {
            patterns: build(&a.pattern)?,
            include: build(&a.include)?,
            exclude: build(&a.exclude)?,
            files_only: a.files_only,
            dirs_only: a.dirs_only,
            depth: a.depth,
        })
    }

    pub fn is_active(&self) -> bool {
        !self.patterns.is_empty()
            || !self.include.is_empty()
            || !self.exclude.is_empty()
            || self.files_only
            || self.dirs_only
            || self.depth.is_some()
    }

    pub fn depth_limit(&self) -> usize {
        self.depth.unwrap_or(usize::MAX)
    }

    /// 只按「文件/目录」类型判断，忽略名称类条件。
    ///
    /// 目录常常只是容器（例如 `pack --include '*.txt' proj` 里的 `proj`），
    /// 用完整 `accepts` 会把容器目录一起筛掉，导致归档为空。
    pub fn accepts_kind(&self, is_dir: bool) -> bool {
        !(self.files_only && is_dir) && !(self.dirs_only && !is_dir)
    }

    /// `rel` 是相对根目录的路径（`/` 分隔），用于 include/exclude 与名称模式。
    pub fn accepts(&self, name: &str, rel: &str, is_dir: bool, depth: usize) -> bool {
        if self.files_only && is_dir {
            return false;
        }
        if self.dirs_only && !is_dir {
            return false;
        }
        if let Some(d) = self.depth {
            if depth > d {
                return false;
            }
        }
        if !self.patterns.is_empty() && !self.patterns.iter().any(|p| p.is_match(name)) {
            return false;
        }
        if !self.include.is_empty()
            && !self.include.iter().any(|p| p.is_match(rel) || p.is_match(name))
        {
            return false;
        }
        if self.exclude.iter().any(|p| p.is_match(rel) || p.is_match(name)) {
            return false;
        }
        true
    }
}

/// 展开路径参数（含通配符），去重并保持顺序。
pub fn expand_targets(specs: &[PathBuf], case_sensitive: bool) -> Result<Vec<PathBuf>, Fail> {
    let ci = !case_sensitive;
    let mut out = Vec::new();
    for s in specs {
        let got = expand_many(std::slice::from_ref(s), ci)
            .map_err(|e| Fail::usage(format!("{}: {e}", crate::util::path_to_display(s))))?;
        if got.is_empty() {
            out.push(s.clone()); // 保持「路径不存在」由后续逻辑报错
        } else {
            out.extend(got);
        }
    }
    Ok(out)
}

pub fn ci_of(case_sensitive: bool) -> bool {
    !case_sensitive
}

/// 判定冲突并给出最终目标路径。
pub fn resolve_dst(
    target: &Path,
    policy: crate::plan::ConflictPolicy,
) -> Result<Option<PathBuf>, Fail> {
    crate::plan::resolve_conflict(target, policy)
}

/// 从 `--on-conflict` / `--force` 组合出策略。
pub fn policy_of(force: bool, on_conflict: &str) -> Result<crate::plan::ConflictPolicy, Fail> {
    if force {
        return Ok(crate::plan::ConflictPolicy::Overwrite);
    }
    crate::plan::ConflictPolicy::parse(on_conflict)
        .ok_or_else(|| Fail::usage(format!("未知的冲突策略：{on_conflict}")))
}

/// 收集目录下所有条目（前序），返回 (路径, 相对路径, 深度, 是否目录)。
pub fn walk_relative(
    root: &Path,
    max_depth: usize,
    follow_links: bool,
) -> std::io::Result<Vec<(PathBuf, String, usize, bool)>> {
    let opts = crate::fsx::WalkOpts { max_depth, follow_links, sort: true };
    let mut out = Vec::new();
    for (p, depth) in crate::fsx::walk_dir(root, &opts)? {
        let rel = p
            .strip_prefix(root)
            .unwrap_or(&p)
            .to_string_lossy()
            .replace('\\', "/");
        let is_dir = std::fs::symlink_metadata(&p).map(|m| m.is_dir()).unwrap_or(false);
        out.push((p, rel, depth, is_dir));
    }
    Ok(out)
}

/// 打一行汇总。
pub fn finish(sum: &mut Summary, out: &mut Out) {
    out.note(term::dim(&sum.line()));
}

/// 把 `Fail` 收进汇总的快捷方式。
pub fn push_fail(sum: &mut Summary, f: Fail) {
    sum.fail(f);
}
