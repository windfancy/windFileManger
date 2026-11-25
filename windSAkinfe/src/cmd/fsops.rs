//! 文件操作四件套 + 时间戳 + 链接：rm / cp / mv / rn / touch / link。

use std::fs;
use std::path::{Path, PathBuf};

use crate::algo::template::{RenderCtx, Template};
use crate::cli::{CpArgs, LinkArgs, MvArgs, RmArgs, RnArgs, TouchArgs};
use crate::fsx;
use crate::plan::{Action, ConflictPolicy, Fail, Plan, Summary};
use crate::term::{self, Out};
use crate::util::{format_size, now_local, path_to_display, system_time_to_local};

use super::{expand_targets, policy_of, resolve_dst, Global, Filter};

// ---------------------------------------------------------------- rm

pub fn rm(a: &RmArgs, g: &Global, out: &mut Out) -> Summary {
    let mut sum = Summary::new();
    let filter = match Filter::from_args(&a.select) {
        Ok(f) => f,
        Err(f) => {
            sum.fail(f);
            return sum;
        }
    };
    let targets = match expand_targets(&a.paths, a.select.case_sensitive || a.select.cs_alias) {
        Ok(t) => t,
        Err(f) => {
            sum.fail(f);
            return sum;
        }
    };

    let mut plan = Plan::new();
    for t in &targets {
        let md = match fs::symlink_metadata(t) {
            Ok(m) => m,
            Err(e) => {
                sum.fail(Fail::io(t.clone(), e));
                continue;
            }
        };
        // 安全护栏：根路径/主目录/系统目录一律拒绝
        if let Err(f) = crate::plan::guard_delete(t) {
            sum.fail(f);
            sum.aborted = true;
            return sum;
        }
        let is_dir = md.is_dir();
        let bytes = if is_dir {
            fsx::dir_stats(t).1
        } else {
            md.len()
        };

        if !filter.is_active() {
            if a.trash {
                // 回收站优先：文件与目录都走 Trash
                plan.push(Action::Trash { path: t.clone(), bytes });
            } else if a.no_recursive && is_dir {
                plan.push(Action::RmdirIfEmpty { path: t.clone() });
            } else {
                plan.push(Action::Delete { path: t.clone(), is_dir, bytes });
            }
            continue;
        }

        // 有过滤条件：逐项判定，后序删除
        let mut hits: Vec<(PathBuf, usize, bool)> = Vec::new();
        let name = t.file_name().map(|s| s.to_string_lossy().to_string()).unwrap_or_default();
        if filter.accepts(&name, &name, is_dir, 0) {
            hits.push((t.clone(), 0, is_dir));
        }
        if is_dir && !a.no_recursive {
            match super::walk_relative(t, filter.depth_limit(), false) {
                Ok(items) => {
                    for (p, rel, depth, is_dir) in items {
                        let nm = p
                            .file_name()
                            .map(|s| s.to_string_lossy().to_string())
                            .unwrap_or_default();
                        if filter.accepts(&nm, &rel, is_dir, depth) {
                            hits.push((p, depth, is_dir));
                        }
                    }
                }
                Err(e) => sum.fail(Fail::io(t.clone(), e)),
            }
        }
        // 深者先删，避免父目录先消失导致子项失败
        hits.sort_by_key(|x| std::cmp::Reverse(x.1));
        for (p, _, is_dir) in hits {
            let b = if is_dir { fsx::dir_stats(&p).1 } else { fs::metadata(&p).map(|m| m.len()).unwrap_or(0) };
            if a.trash {
                plan.push(Action::Trash { path: p, bytes: b });
            } else {
                plan.push(Action::Delete { path: p, is_dir, bytes: b });
            }
        }
    }

    if plan.is_empty() {
        out.note("没有匹配的项");
        return sum;
    }
    if !g.dry_run && plan.needs_confirm() && !confirm_plan(&plan, out) {
        sum.aborted = true;
        sum.fail(Fail::usage("用户取消了操作"));
        return sum;
    }
    if let Err(e) = plan.execute(g.dry_run, out, &mut sum) {
        sum.fail(Fail::usage(e.to_string()));
    }
    if !g.dry_run {
        out.note(term::dim(&sum.line()));
    } else {
        out.note(term::dim(&plan.preview()));
    }
    sum
}

/// 大型破坏性操作的确认闸门。
pub fn confirm_plan(plan: &Plan, out: &mut Out) -> bool {
    out.note(term::yellow(&plan.preview()));
    term::confirm("确认执行？", false)
}

// ---------------------------------------------------------------- cp

pub fn cp(a: &CpArgs, g: &Global, out: &mut Out) -> Summary {
    let mut sum = Summary::new();
    let policy = match policy_of(a.conflict.force, &a.conflict.on_conflict) {
        Ok(p) => p,
        Err(f) => {
            sum.fail(f);
            return sum;
        }
    };
    let filter = match Filter::from_args(&a.select) {
        Ok(f) => f,
        Err(f) => {
            sum.fail(f);
            return sum;
        }
    };
    let srcs = match expand_targets(&a.src, a.select.case_sensitive || a.select.cs_alias) {
        Ok(t) => t,
        Err(f) => {
            sum.fail(f);
            return sum;
        }
    };

    // 目标是否为目录：多源、已存在目录、或以分隔符结尾
    let dst_is_dir = srcs.len() > 1
        || a.dst.is_dir()
        || a.dst.to_string_lossy().ends_with('/')
        || a.dst.to_string_lossy().ends_with('\\');
    if srcs.len() > 1 && !dst_is_dir {
        sum.fail(Fail::usage("复制多个源时，目标必须是已存在的目录"));
        return sum;
    }

    // 空间检查（需求 F2.6）
    if !a.no_space_check && !g.dry_run {
        let mut need = 0u64;
        for s in &srcs {
            match fs::symlink_metadata(s) {
                Ok(m) if m.is_dir() => need += fsx::dir_stats(s).1,
                Ok(m) => need += m.len(),
                Err(_) => {}
            }
        }
        if let Some(free) = fsx::available_space(&a.dst) {
            if need > free {
                sum.fail(Fail::resource(format!(
                    "目标卷剩余空间不足：需要 {}，可用 {}",
                    format_size(need),
                    format_size(free)
                )));
                return sum;
            }
        }
    }

    // 复制选项：保留元数据 / 链接处理 / 条目筛选（需求 F2.4、F2.5）
    let ci_pat = super::ci_of(a.select.case_sensitive || a.select.cs_alias);
    let build_pats = |v: &Vec<String>| -> Result<Vec<crate::algo::pattern::Pattern>, Fail> {
        v.iter()
            .map(|p| crate::algo::pattern::Pattern::new(p, ci_pat).map_err(Fail::usage))
            .collect()
    };
    let copts = fsx::CopyOpts {
        preserve_mtime: !a.no_preserve,
        preserve_perms: a.preserve_perms,
        mode: if a.copy_links { fsx::CopyMode::LinkItself } else { fsx::CopyMode::Normal },
        overwrite: policy == ConflictPolicy::Overwrite,
        include: {
            // `-p` 与 `--include` 都是「只收匹配的」，合并成一份
            let mut v = a.select.pattern.clone();
            v.extend(a.select.include.iter().cloned());
            match build_pats(&v) {
                Ok(x) => x,
                Err(f) => {
                    sum.fail(f);
                    return sum;
                }
            }
        },
        exclude: match build_pats(&a.select.exclude) {
            Ok(x) => x,
            Err(f) => {
                sum.fail(f);
                return sum;
            }
        },
    };

    let mut plan = Plan::new();
    for s in &srcs {
        let md = match fs::symlink_metadata(s) {
            Ok(m) => m,
            Err(e) => {
                sum.fail(Fail::io(s.clone(), e));
                continue;
            }
        };
        // 顶层：目录只是容器，只受 --files-only/--dirs-only 约束；
        // 名称类条件作用在目录内部（由 CopyOpts 的 include/exclude 处理）。
        let is_dir_top = md.is_dir() && !md.file_type().is_symlink();
        let top_ok = if is_dir_top {
            filter.accepts_kind(true)
        } else {
            let name = s.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
            filter.accepts(&name, &name, false, 0)
        };
        if !top_ok {
            sum.skipped += 1;
            out.skip("skip", s, "未通过选择条件");
            continue;
        }
        let base_name = s.file_name().map(|n| n.to_os_string()).unwrap_or_default();
        let dst = if dst_is_dir { a.dst.join(&base_name) } else { a.dst.clone() };

        // 目标是源自身或其内部：直接拒绝
        if fsx::is_ancestor(s, &dst) {
            sum.fail(Fail::usage(format!(
                "不能把 {} 复制到它自己的子目录里",
                path_to_display(s)
            )));
            continue;
        }

        // --flat：把目录里的文件逐个平铺进目标目录（不保留目录层级）。
        // 注意这一支不能走 CopyTree，否则还是会把 base_name 当一层。
        if a.flat && is_dir_top {
            let target_dir = &a.dst;
            let opts = fsx::WalkOpts {
                max_depth: a.select.depth.map(|d| d.max(1)).unwrap_or(usize::MAX),
                follow_links: false,
                sort: true,
            };
            match fsx::walk_entries(s, &opts) {
                Ok(items) => {
                    for it in items {
                        if it.is_dir {
                            continue;
                        }
                        let name = it
                            .path
                            .file_name()
                            .map(|x| x.to_string_lossy().to_string())
                            .unwrap_or_default();
                        if !copts.accepts(&it.rel, &name, false) {
                            continue;
                        }
                        let want = target_dir.join(&name);
                        match resolve_dst(&want, policy) {
                            Ok(Some(p)) => plan.push(Action::CopyFile {
                                src: it.path,
                                dst: p,
                                bytes: it.len,
                                opts: copts.clone(),
                            }),
                            Ok(None) => {
                                sum.skipped += 1;
                                out.skip("skip", &want, "已存在");
                            }
                            Err(f) => sum.fail(f),
                        }
                    }
                }
                Err(e) => sum.fail(Fail::io(s.clone(), e)),
            }
            continue;
        }

        let final_dst = match resolve_dst(&dst, policy) {
            Ok(Some(p)) => p,
            Ok(None) => {
                sum.skipped += 1;
                out.skip("skip", &dst, "已存在");
                continue;
            }
            Err(f) => {
                sum.fail(f);
                continue;
            }
        };
        let bytes = if md.is_dir() { fsx::dir_stats(s).1 } else { md.len() };
        if md.is_dir() && !md.file_type().is_symlink() {
            plan.push(Action::CopyTree {
                src: s.clone(),
                dst: final_dst,
                bytes,
                opts: copts.clone(),
            });
        } else {
            plan.push(Action::CopyFile {
                src: s.clone(),
                dst: final_dst,
                bytes,
                opts: copts.clone(),
            });
        }
    }

    if plan.is_empty() {
        if sum.skipped > 0 {
            out.note(term::dim(&sum.line()));
        }
        return sum;
    }
    if !g.dry_run && plan.needs_confirm() && !confirm_plan(&plan, out) {
        sum.aborted = true;
        sum.fail(Fail::usage("用户取消了操作"));
        return sum;
    }
    if let Err(e) = plan.execute(g.dry_run, out, &mut sum) {
        sum.fail(Fail::usage(e.to_string()));
    }
    out.note(term::dim(&sum.line()));
    sum
}

// ---------------------------------------------------------------- mv

pub fn mv(a: &MvArgs, g: &Global, out: &mut Out) -> Summary {
    let mut sum = Summary::new();
    let policy = match policy_of(a.conflict.force, &a.conflict.on_conflict) {
        Ok(p) => p,
        Err(f) => {
            sum.fail(f);
            return sum;
        }
    };
    let srcs = match expand_targets(&a.src, a.select.case_sensitive || a.select.cs_alias) {
        Ok(t) => t,
        Err(f) => {
            sum.fail(f);
            return sum;
        }
    };
    let dst_is_dir = srcs.len() > 1 || a.dst.is_dir();
    if srcs.len() > 1 && !dst_is_dir {
        sum.fail(Fail::usage("移动多个源时，目标必须是已存在的目录"));
        return sum;
    }

    let mut plan = Plan::new();
    for s in &srcs {
        let md = match fs::symlink_metadata(s) {
            Ok(m) => m,
            Err(e) => {
                sum.fail(Fail::io(s.clone(), e));
                continue;
            }
        };
        let base_name = s.file_name().map(|n| n.to_os_string()).unwrap_or_default();
        let dst = if dst_is_dir { a.dst.join(&base_name) } else { a.dst.clone() };

        // 需求 F3.3：目标在源内部 → 拒绝（防无限递归）
        if fsx::is_ancestor(s, &dst) || fsx::is_ancestor(s, &crate::plan::absolute(&dst)) {
            sum.fail(Fail::usage(format!(
                "不能把目录移动到它自己的子目录里：{} → {}",
                path_to_display(s),
                path_to_display(&dst)
            )));
            continue;
        }
        if crate::plan::absolute(s) == crate::plan::absolute(&dst) {
            sum.skipped += 1;
            out.skip("skip", s, "源与目标相同");
            continue;
        }
        let final_dst = match resolve_dst(&dst, policy) {
            Ok(Some(p)) => p,
            Ok(None) => {
                sum.skipped += 1;
                out.skip("skip", &dst, "已存在");
                continue;
            }
            Err(f) => {
                sum.fail(f);
                continue;
            }
        };
        let bytes = if md.is_dir() { fsx::dir_stats(s).1 } else { md.len() };
        plan.push(Action::Move { src: s.clone(), dst: final_dst, bytes });
    }

    if plan.is_empty() {
        return sum;
    }
    if !g.dry_run && plan.needs_confirm() && !confirm_plan(&plan, out) {
        sum.aborted = true;
        sum.fail(Fail::usage("用户取消了操作"));
        return sum;
    }
    if let Err(e) = plan.execute(g.dry_run, out, &mut sum) {
        sum.fail(Fail::usage(e.to_string()));
    }
    out.note(term::dim(&sum.line()));
    sum
}

// ---------------------------------------------------------------- rn

pub fn rn(a: &RnArgs, g: &Global, out: &mut Out) -> Summary {
    let mut sum = Summary::new();
    let policy = match policy_of(a.conflict.force, &a.conflict.on_conflict) {
        Ok(p) => p,
        Err(f) => {
            sum.fail(f);
            return sum;
        }
    };
    let tpl = match Template::new(&a.template) {
        Ok(t) => t,
        Err(e) => {
            sum.fail(Fail::usage(e));
            return sum;
        }
    };
    let ci = a.select.case_sensitive || a.select.cs_alias;
    let targets = match expand_targets(&a.paths, ci) {
        Ok(t) => t,
        Err(f) => {
            sum.fail(f);
            return sum;
        }
    };

    // 供 `*` 回填用的捕获组：来自第一个名称模式
    let cap_pat = if a.select.pattern.is_empty() {
        None
    } else {
        match crate::algo::pattern::Pattern::new(&a.select.pattern[0], super::ci_of(ci)) {
            Ok(p) => Some(p),
            Err(e) => {
                sum.fail(Fail::usage(e));
                return sum;
            }
        }
    };
    let filter = match Filter::from_args(&a.select) {
        Ok(f) => f,
        Err(f) => {
            sum.fail(f);
            return sum;
        }
    };

    // 目录参数默认展开成「里面的文件」参与改名——批量改名的常见用法就是
    // `rn photos/ -t '%3d.jpg'`；想让目录自己也改名时加 --include-dirs。
    let mut expanded: Vec<PathBuf> = Vec::new();
    for t in &targets {
        let is_dir = fs::symlink_metadata(t).map(|m| m.is_dir()).unwrap_or(false);
        if !is_dir {
            expanded.push(t.clone());
            continue;
        }
        if a.include_dirs {
            expanded.push(t.clone());
        }
        let opts = fsx::WalkOpts {
            max_depth: a.select.depth.map(|d| d.max(1)).unwrap_or(usize::MAX),
            follow_links: false,
            sort: true,
        };
        match fsx::walk_entries(t, &opts) {
            Ok(items) => {
                for it in items {
                    if it.is_dir && !a.include_dirs {
                        continue;
                    }
                    let name = it
                        .path
                        .file_name()
                        .map(|s| s.to_string_lossy().to_string())
                        .unwrap_or_default();
                    if filter.is_active() && !filter.accepts(&name, &it.rel, it.is_dir, it.depth) {
                        continue;
                    }
                    expanded.push(it.path);
                }
            }
            Err(e) => sum.fail(Fail::io(t.clone(), e)),
        }
    }
    let targets = expanded;

    let now = now_local();
    let per_dir = a.number_mode.eq_ignore_ascii_case("per-dir");
    let mut per_dir_counter: std::collections::HashMap<PathBuf, u64> = std::collections::HashMap::new();

    let mut plan = Plan::new();
    let mut seq = a.start;
    for (i, src) in targets.iter().enumerate() {
        let md = match fs::symlink_metadata(src) {
            Ok(m) => m,
            Err(e) => {
                sum.fail(Fail::io(src.clone(), e));
                continue;
            }
        };
        if md.is_dir() && !a.include_dirs {
            sum.skipped += 1;
            out.skip("skip", src, "是目录（用 --include-dirs 才会改名）");
            continue;
        }
        let dir = src.parent().map(|p| p.to_path_buf()).unwrap_or_else(|| PathBuf::from("."));
        let cur = a.start + (i as u64) * a.step;
        let this_seq = if per_dir {
            let c = per_dir_counter.entry(dir.clone()).or_insert(a.start);
            let v = *c;
            *c += a.step;
            v
        } else {
            seq
        };
        if !per_dir {
            seq += a.step;
        }
        let _ = cur;

        let name = src.file_name().map(|s| s.to_string_lossy().to_string()).unwrap_or_default();
        let caps: Vec<String> = match &cap_pat {
            Some(p) => p.captures(&name).map(|c| c[1.min(c.len() - 1)..].to_vec()).unwrap_or_default(),
            None => Vec::new(),
        };
        // `%u` 每个文件取一次新的随机串（模板里可能出现多次，同一次渲染保持一致）
        let rand = crate::util::rand_hex(6);
        let rendered = tpl.render(&RenderCtx {
            captures: &caps,
            seq: this_seq,
            path: src,
            root: &dir,
            mtime: md.modified().ok().map(system_time_to_local),
            size: md.len(),
            now: &now,
            rand: &rand,
        });
        if rendered.is_empty() {
            sum.fail(Fail::usage(format!("模板对 {} 渲染出空名字", path_to_display(src))));
            continue;
        }
        let dst = dir.join(&rendered);
        if dst == *src {
            sum.skipped += 1;
            out.skip("skip", src, "名字未变化");
            continue;
        }
        match resolve_dst(&dst, policy) {
            Ok(Some(p)) => plan.push(Action::Rename { from: src.clone(), to: p }),
            Ok(None) => {
                sum.skipped += 1;
                out.skip("skip", &dst, "已存在");
            }
            Err(f) => sum.fail(f),
        }
    }

    if plan.is_empty() {
        out.note(term::dim(&sum.line()));
        return sum;
    }
    if !g.dry_run && plan.needs_confirm() && !confirm_plan(&plan, out) {
        sum.aborted = true;
        sum.fail(Fail::usage("用户取消了操作"));
        return sum;
    }
    if let Err(e) = plan.execute(g.dry_run, out, &mut sum) {
        sum.fail(Fail::usage(e.to_string()));
    }
    out.note(term::dim(&sum.line()));
    sum
}

// ---------------------------------------------------------------- touch

pub fn touch(a: &TouchArgs, g: &Global, out: &mut Out) -> Summary {
    let mut sum = Summary::new();
    let secs = match &a.time {
        Some(t) => match parse_time_arg(t) {
            Ok(v) => v,
            Err(e) => {
                sum.fail(Fail::usage(e));
                return sum;
            }
        },
        None => crate::util::local_to_unix_secs(&now_local()).max(0) as u64,
    };
    let targets = match expand_targets(&a.paths, a.select.case_sensitive || a.select.cs_alias) {
        Ok(t) => t,
        Err(f) => {
            sum.fail(f);
            return sum;
        }
    };
    // `-m` / `-a` 只改其中一项；两个都给或都不给时改两项
    if a.mtime_only && a.atime_only {
        sum.fail(Fail::usage("-m 与 -a 不能同时使用（只写一个，或都不写表示都改）"));
        return sum;
    }
    let (mt, at) = match (a.mtime_only, a.atime_only) {
        (true, false) => (Some(secs), None),
        (false, true) => (None, Some(secs)),
        _ => (Some(secs), Some(secs)),
    };
    let mut plan = Plan::new();
    for t in &targets {
        plan.push(Action::Touch { path: t.clone(), mtime: mt, atime: at, create: !a.no_create });
    }
    if let Err(e) = plan.execute(g.dry_run, out, &mut sum) {
        sum.fail(Fail::usage(e.to_string()));
    }
    out.note(term::dim(&sum.line()));
    sum
}

fn parse_time_arg(s: &str) -> Result<u64, String> {
    // 1) YYYYMMDDHHMM[.SS]
    let t = s.trim();
    if t.len() >= 12 && t.chars().take(12).all(|c| c.is_ascii_digit()) {
        let c = crate::util::Civil {
            year: t[0..4].parse().map_err(|_| "年份非法".to_string())?,
            month: t[4..6].parse().map_err(|_| "月份非法".to_string())?,
            day: t[6..8].parse().map_err(|_| "日期非法".to_string())?,
            hour: t[8..10].parse().map_err(|_| "小时非法".to_string())?,
            min: t[10..12].parse().map_err(|_| "分钟非法".to_string())?,
            sec: if t.len() >= 15 && t.as_bytes()[12] == b'.' {
                t[13..15].parse().map_err(|_| "秒非法".to_string())?
            } else {
                0
            },
        };
        return Ok(crate::util::local_to_unix_secs(&c).max(0) as u64);
    }
    // 2) 宽松日期时间
    let c = crate::util::parse_datetime(t)?;
    Ok(crate::util::local_to_unix_secs(&c).max(0) as u64)
}

// ---------------------------------------------------------------- link

pub fn link(a: &LinkArgs, g: &Global, out: &mut Out) -> Summary {
    let mut sum = Summary::new();
    let symbolic = !a.hard;
    if !a.target.exists() && !symbolic {
        sum.fail(Fail::io(a.target.clone(), std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "硬链接目标不存在",
        )));
        return sum;
    }
    if fsx::exists_any(&a.link) {
        if !a.force {
            sum.skipped += 1;
            out.skip("skip", &a.link, "已存在（用 --force 重建）");
            out.note(term::dim(&sum.line()));
            return sum;
        }
        if !g.dry_run {
            if let Err(e) = fsx::remove_tree(&a.link, false) {
                sum.fail(Fail::io(a.link.clone(), e));
                return sum;
            }
        }
    }
    let mut plan = Plan::new();
    plan.push(Action::Link {
        target: crate::plan::absolute(&a.target),
        link: a.link.clone(),
        symbolic,
    });
    if let Err(e) = plan.execute(g.dry_run, out, &mut sum) {
        sum.fail(Fail::usage(e.to_string()));
    }
    // execute 内部会把单条动作的失败记进 summary（不会返回 Err），
    // 所以这里事后改写：Windows 建符号链接需要开发者模式或管理员（1314）。
    if symbolic {
        let hint = "创建符号链接需要 Windows 开发者模式或管理员权限；请改用 --hard 创建硬链接（仅限同卷、不支持目录）";
        for f in sum.failed.iter_mut() {
            if f.msg.contains("1314") {
                f.kind = crate::plan::FailKind::Unsupported;
                f.msg = hint.to_string();
            }
        }
    }
    out.note(term::dim(&sum.line()));
    sum
}

/// 供 `pack` 等命令复用：把路径列表展开成文件清单。
pub fn list_files(roots: &[PathBuf], exclude: &[String]) -> Vec<(PathBuf, PathBuf)> {
    let mut out = Vec::new();
    for r in roots {
        if r.is_file() {
            out.push((r.clone(), PathBuf::from(r.file_name().unwrap_or_default())));
            continue;
        }
        let base: &Path = r;
        if let Ok(items) = super::walk_relative(base, usize::MAX, false) {
            for (p, rel, _, is_dir) in items {
                if is_dir {
                    continue;
                }
                if exclude.iter().any(|pat| {
                    crate::algo::pattern::Pattern::new(pat, cfg!(windows))
                        .map(|x| x.is_match(&rel))
                        .unwrap_or(false)
                }) {
                    continue;
                }
                out.push((p, rel.into()));
            }
        }
    }
    out
}
