//! 归档命令：pack / unpack / split / join。

use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use crate::archive::{self, AnyArchive, Kind};
use crate::cli::{JoinArgs, PackArgs, SplitArgs, UnpackArgs};
use crate::fsx;
use crate::plan::{Fail, Summary};
use crate::term::{self, JVal, Out};
use crate::util::{format_size, path_to_display, system_time_to_local};

use super::Global;

// ---------------------------------------------------------------- pack

/// 待写入归档的条目。
struct Item {
    name: String,
    data: Vec<u8>,
    is_dir: bool,
    mtime: Option<u64>,
}

pub fn pack(a: &PackArgs, g: &Global, out: &mut Out) -> Summary {
    let mut sum = Summary::new();
    let kind = match resolve_kind(a) {
        Ok(k) => k,
        Err(f) => {
            sum.fail(f);
            return sum;
        }
    };
    if a.password.is_some() && kind != Kind::Zip {
        sum.fail(Fail::unsupported(
            "只有 zip 支持密码加密（7z/rar 的 AES 加密不在支持范围，见需求 §1.2）",
        ));
        return sum;
    }
    let out_path = match a.output.clone() {
        Some(p) => p,
        None => default_output(&a.paths, kind),
    };

    // 选择条件（预编译一次，条目多时避免反复编译通配）
    let exc = compile_patterns(&a.select.exclude);
    let inc = compile_patterns(&a.select.include);
    let npat = compile_patterns(&a.select.pattern);
    let top_filter = match super::Filter::from_args(&a.select) {
        Ok(f) => f,
        Err(f) => {
            sum.fail(f);
            return sum;
        }
    };
    // 文件是否要收进归档：名称模式、include、exclude 三者都要过。
    let keep_file = |full: &str, rel: &str, base: &str| -> bool {
        if !npat.is_empty() && !matches_any(&npat, full, rel, base) {
            return false;
        }
        if matches_any(&exc, full, rel, base) {
            return false;
        }
        inc.is_empty() || matches_any(&inc, full, rel, base)
    };

    // 收集条目
    let mut items: Vec<Item> = Vec::new();
    for p in &a.paths {
        // 顶层：目录只受 --files-only/--dirs-only 约束，文件才过完整条件
        let top_is_dir = fs::symlink_metadata(p).map(|m| m.is_dir()).unwrap_or(false);
        let top_base = p.file_name().map(|s| s.to_string_lossy().to_string()).unwrap_or_default();
        let top_ok = if top_is_dir {
            top_filter.accepts_kind(true)
        } else {
            top_filter.accepts(&top_base, &top_base, false, 0)
        };
        if !top_ok {
            out.skip("skip", p, "未通过选择条件");
            continue;
        }
        match fs::symlink_metadata(p) {
            Ok(md) if md.is_dir() => {
                let root_name = a
                    .root
                    .as_ref()
                    .and_then(|r| p.strip_prefix(r).ok())
                    .and_then(|r| r.file_name())
                    .or_else(|| p.file_name())
                    .map(|s| s.to_string_lossy().to_string())
                    .unwrap_or_else(|| "root".to_string());
                // 顶层目录本身被排除（如 `--exclude '*'`）：整棵跳过，别只留个空目录条目
                if matches_any(&exc, &root_name, &root_name, &root_name) {
                    out.skip("skip", p, "被排除规则跳过");
                    continue;
                }
                items.push(Item {
                    name: root_name.clone(),
                    data: Vec::new(),
                    is_dir: true,
                    mtime: fsx::mtime_secs(p),
                });
                match super::walk_relative(p, usize::MAX, false) {
                    Ok(list) => {
                        for (path, rel, _, is_dir) in list {
                            let full = format!("{root_name}/{rel}");
                            let base = path
                                .file_name()
                                .map(|s| s.to_string_lossy().to_string())
                                .unwrap_or_default();
                            if matches_any(&exc, &full, &rel, &base) {
                                continue;
                            }
                            if !is_dir && !keep_file(&full, &rel, &base) {
                                continue;
                            }
                            if is_dir {
                                items.push(Item {
                                    name: full,
                                    data: Vec::new(),
                                    is_dir: true,
                                    mtime: fsx::mtime_secs(&path),
                                });
                            } else {
                                match fs::read(&path) {
                                    Ok(d) => items.push(Item {
                                        name: full,
                                        data: d,
                                        is_dir: false,
                                        mtime: fsx::mtime_secs(&path),
                                    }),
                                    Err(e) => sum.fail(Fail::io(path, e)),
                                }
                            }
                        }
                    }
                    Err(e) => sum.fail(Fail::io(p.clone(), e)),
                }
            }
            Ok(_) => {
                let name = p
                    .file_name()
                    .map(|s| s.to_string_lossy().to_string())
                    .unwrap_or_else(|| "file".to_string());
                if !keep_file(&name, &name, &name) {
                    continue;
                }
                match fs::read(p) {
                    Ok(d) => items.push(Item { name, data: d, is_dir: false, mtime: fsx::mtime_secs(p) }),
                    Err(e) => sum.fail(Fail::io(p.clone(), e)),
                }
            }
            Err(e) => sum.fail(Fail::io(p.clone(), e)),
        }
    }

    // 一个条目都没收进来：直接报错，别写出一个 22 字节的空 EOCD 让人以为成功
    if items.is_empty() {
        sum.fail(Fail::usage("没有任何文件匹配打包条件，未写出归档"));
        return sum;
    }

    // gz / xz 是单流格式：多文件时先打成 tar 再压
    let (payload, effective) = if matches!(kind, Kind::Gz | Kind::Xz) {
        if items.iter().filter(|i| !i.is_dir).count() > 1 {
            let tar_bytes = build_tar(&items);
            (tar_bytes, kind)
        } else {
            let single = items.iter().find(|i| !i.is_dir).map(|i| i.data.clone()).unwrap_or_default();
            (single, kind)
        }
    } else {
        (Vec::new(), kind)
    };

    let total_in: u64 = items.iter().map(|i| i.data.len() as u64).sum();
    if g.dry_run {
        out.note(format!(
            "{} 将要打包 {} 个条目（{}）→ {}",
            term::dim("would"),
            items.len(),
            format_size(total_in),
            path_to_display(&out_path)
        ));
        sum.ok = items.len();
        return sum;
    }

    // 产出字节
    let bytes = match write_archive(effective, &items, &payload, a, &mut sum) {
        Some(b) => b,
        None => return sum,
    };
    if bytes.is_empty() {
        sum.fail(Fail::usage("归档内容为空，未写出文件"));
        return sum;
    }

    if let Some(parent) = out_path.parent() {
        if !parent.as_os_str().is_empty() {
            if let Err(e) = fs::create_dir_all(parent) {
                sum.fail(Fail::io(out_path.clone(), e));
                return sum;
            }
        }
    }

    match &a.split {
        None => {
            if let Err(e) = fs::write(&out_path, &bytes) {
                sum.fail(Fail::io(out_path.clone(), e));
                return sum;
            }
            sum.ok += 1;
            out.ok_with(
                "packed",
                &out_path,
                format!("{} → {}（{} 个条目）", format_size(total_in), format_size(bytes.len() as u64), items.len()),
                JVal::Obj(vec![
                    ("format".to_string(), JVal::s(kind.name())),
                    ("entries".to_string(), JVal::n(items.len() as i64)),
                    ("size".to_string(), JVal::n(bytes.len() as i64)),
                ]),
            );
        }
        Some(spec) => {
            let size = match crate::util::parse_size(spec) {
                Ok(s) if s > 0 => s,
                Ok(_) => {
                    sum.fail(Fail::usage("--split 大小必须大于 0"));
                    return sum;
                }
                Err(e) => {
                    sum.fail(Fail::usage(format!("--split: {e}")));
                    return sum;
                }
            };
            match split_bytes(&bytes, size, &out_path, 3) {
                Ok(parts) => {
                    for p in &parts {
                        out.ok("part", p, "");
                    }
                    sum.ok += parts.len();
                }
                Err(e) => sum.fail(Fail::io(out_path.clone(), e)),
            }
        }
    }
    out.note(term::dim(&sum.line()));
    sum
}

/// 预编译通配模式：条目可能有上万个，逐个条目重新编译太浪费。
fn compile_patterns(pats: &[String]) -> Vec<crate::algo::pattern::Pattern> {
    let ci = cfg!(windows);
    pats.iter()
        .filter_map(|p| crate::algo::pattern::Pattern::new(p, ci).ok())
        .collect()
}

/// 排除/包含匹配：同时比对「归档内全名」「相对被打包目录的路径」「文件名」。
///
/// 这样 `-e '*.o'`（按文件名）、`-e 'build/*'`（按相对路径）、
/// `-e 'proj/**'`（按归档全名）三种写法都能生效。
fn matches_any(pats: &[crate::algo::pattern::Pattern], full: &str, rel: &str, base: &str) -> bool {
    pats.iter().any(|x| x.is_match(full) || x.is_match(rel) || x.is_match(base))
}

fn resolve_kind(a: &PackArgs) -> Result<Kind, Fail> {
    if let Some(f) = &a.format {
        return Kind::from_name(f)
            .ok_or_else(|| Fail::usage(format!("未知格式：{f}（支持 zip/7z/tar/gz/xz）")));
    }
    if let Some(o) = &a.output {
        if let Some(k) = Kind::from_path(o) {
            return Ok(k);
        }
        return Err(Fail::usage(format!(
            "无法从 {} 推断格式，请用 -f 指定（zip/7z/tar/gz/xz）",
            path_to_display(o)
        )));
    }
    Err(Fail::usage("请用 -o 指定输出文件名，或用 -f 指定格式"))
}

fn default_output(paths: &[PathBuf], kind: Kind) -> PathBuf {
    let stem = paths
        .first()
        .and_then(|p| p.file_name())
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| "archive".to_string());
    PathBuf::from(format!("{stem}.{}", kind.name()))
}

fn build_tar(items: &[Item]) -> Vec<u8> {
    let inputs: Vec<archive::tar::TarInput> = items
        .iter()
        .map(|i| archive::tar::TarInput {
            name: i.name.clone(),
            data: i.data.clone(),
            is_dir: i.is_dir,
            mtime: i.mtime.unwrap_or(0),
            mode: if i.is_dir { 0o755 } else { 0o644 },
        })
        .collect();
    let mut buf = Vec::new();
    let _ = archive::tar::write_tar(&mut buf, &inputs);
    buf
}

fn write_archive(
    kind: Kind,
    items: &[Item],
    payload: &[u8],
    a: &PackArgs,
    sum: &mut Summary,
) -> Option<Vec<u8>> {
    let mut buf: Vec<u8> = Vec::new();
    match kind {
        Kind::Zip => {
            let mut w = archive::zip::ZipWriter::new(&mut buf, a.level, a.password.as_deref().map(|s| s.as_bytes()));
            for i in items {
                let civil = i.mtime.map(|s| system_time_to_local(std::time::UNIX_EPOCH + std::time::Duration::from_secs(s)));
                let r = if i.is_dir {
                    w.add_dir(&i.name, civil.as_ref())
                } else {
                    w.add_file(&i.name, &i.data, civil.as_ref())
                };
                if let Err(e) = r {
                    sum.fail(Fail::usage(format!("写入 zip 条目 {} 失败：{e}", i.name)));
                    return None;
                }
            }
            if let Err(e) = w.finish() {
                sum.fail(Fail::usage(format!("完成 zip 失败：{e}")));
                return None;
            }
        }
        Kind::SevenZ => {
            let inputs: Vec<archive::sevenz::SevenZInput> = items
                .iter()
                .map(|i| archive::sevenz::SevenZInput {
                    name: i.name.clone(),
                    data: i.data.clone(),
                    is_dir: i.is_dir,
                    mtime_filetime: i.mtime.map(fsx::unix_to_filetime),
                })
                .collect();
            if let Err(e) = archive::sevenz::write_7z(&mut buf, &inputs, a.level) {
                sum.fail(Fail::usage(format!("写入 7z 失败：{e}")));
                return None;
            }
        }
        Kind::Tar => {
            let inputs: Vec<archive::tar::TarInput> = items
                .iter()
                .map(|i| archive::tar::TarInput {
                    name: i.name.clone(),
                    data: i.data.clone(),
                    is_dir: i.is_dir,
                    mtime: i.mtime.unwrap_or(0),
                    mode: if i.is_dir { 0o755 } else { 0o644 },
                })
                .collect();
            if let Err(e) = archive::tar::write_tar(&mut buf, &inputs) {
                sum.fail(Fail::usage(format!("写入 tar 失败：{e}")));
                return None;
            }
        }
        Kind::Gz => {
            if let Err(e) = archive::gz::write_gzip(&mut buf, payload, a.level) {
                sum.fail(Fail::usage(format!("写入 gzip 失败：{e}")));
                return None;
            }
        }
        Kind::Xz => {
            if let Err(e) = archive::xz::write_xz(&mut buf, payload, a.level) {
                sum.fail(Fail::usage(format!("写入 xz 失败：{e}")));
                return None;
            }
        }
        Kind::Rar => {
            sum.fail(Fail::unsupported("RAR 格式无法创建（算法规范未公开，见需求 §1.2）"));
            return None;
        }
    }
    Some(buf)
}

/// 按固定大小切分字节流。
fn split_bytes(data: &[u8], size: u64, base: &Path, digits: usize) -> std::io::Result<Vec<PathBuf>> {
    let mut parts = Vec::new();
    let mut off = 0usize;
    let mut n = 0usize;
    while off < data.len() {
        let end = (off + size as usize).min(data.len());
        let name = format!("{}.{:0width$}", base.to_string_lossy(), n + 1, width = digits);
        let p = PathBuf::from(name);
        fs::write(&p, &data[off..end])?;
        parts.push(p);
        off = end;
        n += 1;
    }
    Ok(parts)
}

// ---------------------------------------------------------------- unpack

pub fn unpack(a: &UnpackArgs, g: &Global, out: &mut Out) -> Summary {
    let mut sum = Summary::new();
    let policy = match super::policy_of(a.conflict.force, &a.conflict.on_conflict) {
        Ok(p) => p,
        Err(f) => {
            sum.fail(f);
            return sum;
        }
    };
    let ci = cfg!(windows);
    let mut inc = Vec::new();
    for p in &a.include {
        if let Ok(x) = crate::algo::pattern::Pattern::new(p, ci) {
            inc.push(x);
        }
    }
    let mut exc = Vec::new();
    for p in &a.exclude {
        if let Ok(x) = crate::algo::pattern::Pattern::new(p, ci) {
            exc.push(x);
        }
    }

    for arc_path in &a.archives {
        let data = match fs::read(arc_path) {
            Ok(d) => d,
            Err(e) => {
                sum.fail(Fail::io(arc_path.clone(), e));
                continue;
            }
        };
        let arch = match AnyArchive::open(arc_path, data) {
            Ok(x) => x,
            Err(e) => {
                let kind = if e.kind() == std::io::ErrorKind::Unsupported {
                    crate::plan::FailKind::Unsupported
                } else {
                    crate::plan::FailKind::Corrupt
                };
                sum.fail(Fail { path: Some(arc_path.clone()), kind, msg: e.to_string() });
                continue;
            }
        };
        let entries = arch.entries();

        if a.list {
            out.raw(&term::bold(&path_to_display(arc_path)));
            for e in &entries {
                let kind = if e.is_dir { "dir " } else { "file" };
                let enc = if e.encrypted { " [加密]" } else { "" };
                let line = format!(
                    "  {:>10}  {}  {}{}",
                    if e.is_dir { "-".to_string() } else { format_size(e.size) },
                    kind,
                    e.name,
                    enc
                );
                out.raw(&line);
            }
            out.raw(&term::dim(&format!("共 {} 个条目", entries.len())));
            sum.ok += entries.len();
            continue;
        }

        let dest_root = a.output.clone().unwrap_or_else(|| {
            let stem = arc_path
                .file_stem()
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or_else(|| "unpacked".to_string());
            PathBuf::from(stem)
        });

        if !g.dry_run {
            if let Err(e) = fs::create_dir_all(&dest_root) {
                sum.fail(Fail::io(dest_root.clone(), e));
                continue;
            }
        }

        for (idx, e) in entries.iter().enumerate() {
            if e.is_dir {
                continue;
            }
            let base = e.name.rsplit('/').next().unwrap_or(&e.name);
            if !inc.is_empty()
                && !inc.iter().any(|x| x.is_match(&e.name) || x.is_match(base))
            {
                continue;
            }
            if exc.iter().any(|x| x.is_match(&e.name) || x.is_match(base)) {
                continue;
            }
            let rel = if a.flat {
                PathBuf::from(e.name.rsplit('/').next().unwrap_or(&e.name))
            } else {
                match archive::safe_join(&dest_root, &e.name) {
                    Some(p) => p,
                    None => {
                        out.warn(format!("条目路径不安全，已拒绝：{}", e.name));
                        sum.skipped += 1;
                        continue;
                    }
                }
            };
            let target = if a.flat { dest_root.join(rel) } else { rel };
            // 二次确认仍在目标目录内
            if !a.flat && !target.starts_with(&dest_root) {
                out.warn(format!("条目逃逸目标目录，已拒绝：{}", e.name));
                sum.skipped += 1;
                continue;
            }
            let final_target = match crate::plan::resolve_conflict(&target, policy) {
                Ok(Some(p)) => p,
                Ok(None) => {
                    sum.skipped += 1;
                    continue;
                }
                Err(f) => {
                    sum.fail(f);
                    continue;
                }
            };
            if g.dry_run {
                out.note(format!("{} 解出 {}", term::dim("would"), path_to_display(&final_target)));
                sum.ok += 1;
                continue;
            }
            if e.encrypted && a.password.is_none() {
                sum.fail(Fail::unsupported(format!(
                    "“{}”是加密条目，请用 --password 提供密码",
                    e.name
                )));
                continue;
            }
            match arch.read(idx, a.password.as_deref()) {
                Ok(d) => {
                    if let Some(parent) = final_target.parent() {
                        if !parent.as_os_str().is_empty() {
                            if let Err(e) = fs::create_dir_all(parent) {
                                sum.fail(Fail::io(final_target.clone(), e));
                                continue;
                            }
                        }
                    }
                    match fs::write(&final_target, &d) {
                        Ok(()) => {
                            if let Some(t) = e.mtime {
                                let _ = fsx::set_mtime(&final_target, t);
                            }
                            out.ok("extracted", &final_target, format_size(d.len() as u64));
                            sum.ok += 1;
                        }
                        Err(err) => sum.fail(Fail::io(final_target.clone(), err)),
                    }
                }
                Err(err) => {
                    let kind = if err.kind() == std::io::ErrorKind::Unsupported {
                        crate::plan::FailKind::Unsupported
                    } else {
                        crate::plan::FailKind::Corrupt
                    };
                    sum.fail(Fail { path: Some(final_target), kind, msg: err.to_string() });
                }
            }
        }
    }
    out.note(term::dim(&sum.line()));
    sum
}

// ---------------------------------------------------------------- split / join

pub fn split(a: &SplitArgs, g: &Global, out: &mut Out) -> Summary {
    let mut sum = Summary::new();
    let size = match crate::util::parse_size(&a.size) {
        Ok(s) if s > 0 => s,
        Ok(_) => {
            sum.fail(Fail::usage("分卷大小必须大于 0"));
            return sum;
        }
        Err(e) => {
            sum.fail(Fail::usage(format!("-s/--size: {e}")));
            return sum;
        }
    };
    if a.digits == 0 || a.digits > 9 {
        sum.fail(Fail::usage("--digits 必须在 1..9 之间"));
        return sum;
    }
    let base = a.output.clone().unwrap_or_else(|| a.file.clone());
    let mut src = match fs::File::open(&a.file) {
        Ok(f) => f,
        Err(e) => {
            sum.fail(Fail::io(a.file.clone(), e));
            return sum;
        }
    };
    let total = src.metadata().map(|m| m.len()).unwrap_or(0);
    if g.dry_run {
        let parts = total.div_ceil(size);
        out.note(format!(
            "{} 将切成 {parts} 卷（每卷 {}）",
            term::dim("would"),
            format_size(size)
        ));
        sum.ok = parts as usize;
        return sum;
    }

    // 流式：内存占用不超过一个分卷
    let mut buf = vec![0u8; size as usize];
    let mut n = 0usize;
    loop {
        let mut filled = 0usize;
        while filled < buf.len() {
            match src.read(&mut buf[filled..]) {
                Ok(0) => break,
                Ok(k) => filled += k,
                Err(e) => {
                    sum.fail(Fail::io(a.file.clone(), e));
                    return sum;
                }
            }
        }
        if filled == 0 {
            break;
        }
        n += 1;
        let p = PathBuf::from(format!(
            "{}.{:0width$}",
            base.to_string_lossy(),
            n,
            width = a.digits
        ));
        if let Err(e) = fs::write(&p, &buf[..filled]) {
            sum.fail(Fail::io(p, e));
            return sum;
        }
        out.ok("part", &p, format_size(filled as u64));
        sum.ok += 1;
        if filled < buf.len() {
            break;
        }
    }
    out.note(term::dim(&format!("共 {n} 卷，源 {}", format_size(total))));
    out.note(term::dim(&sum.line()));
    sum
}

pub fn join(a: &JoinArgs, g: &Global, out: &mut Out) -> Summary {
    let mut sum = Summary::new();
    let base = strip_part_suffix(&a.part);
    let digits = part_digits(&a.part);
    let out_path = a.output.clone().unwrap_or_else(|| PathBuf::from(base.clone()));

    // 收集连续分卷
    let mut parts: Vec<PathBuf> = Vec::new();
    let mut i = 1usize;
    loop {
        let cand = PathBuf::from(format!("{}.{:0width$}", base, i, width = digits));
        if !cand.exists() {
            break;
        }
        parts.push(cand);
        i += 1;
        if i > 100_000 {
            break;
        }
    }
    if parts.is_empty() {
        sum.fail(Fail::io(
            a.part.clone(),
            std::io::Error::new(std::io::ErrorKind::NotFound, "找不到连续分卷"),
        ));
        return sum;
    }
    if g.dry_run {
        out.note(format!("{} 将合并 {} 个分卷 → {}", term::dim("would"), parts.len(), path_to_display(&out_path)));
        sum.ok = parts.len();
        return sum;
    }
    let mut w = match fs::File::create(&out_path) {
        Ok(f) => std::io::BufWriter::new(f),
        Err(e) => {
            sum.fail(Fail::io(out_path.clone(), e));
            return sum;
        }
    };
    let mut total = 0u64;
    let mut buf = vec![0u8; 1 << 20];
    for p in &parts {
        let mut r = match fs::File::open(p) {
            Ok(f) => f,
            Err(e) => {
                sum.fail(Fail::io(p.clone(), e));
                return sum;
            }
        };
        loop {
            match r.read(&mut buf) {
                Ok(0) => break,
                Ok(k) => {
                    if let Err(e) = w.write_all(&buf[..k]) {
                        sum.fail(Fail::io(out_path.clone(), e));
                        return sum;
                    }
                    total += k as u64;
                }
                Err(e) => {
                    sum.fail(Fail::io(p.clone(), e));
                    return sum;
                }
            }
        }
        sum.ok += 1;
    }
    if let Err(e) = w.flush() {
        sum.fail(Fail::io(out_path.clone(), e));
        return sum;
    }
    out.ok("joined", &out_path, format!("{}（{} 卷）", format_size(total), parts.len()));
    out.note(term::dim(&sum.line()));
    sum
}

fn strip_part_suffix(p: &Path) -> String {
    let s = p.to_string_lossy().to_string();
    match s.rfind('.') {
        Some(i) if s[i + 1..].chars().all(|c| c.is_ascii_digit()) && !s[i + 1..].is_empty() => {
            s[..i].to_string()
        }
        _ => s,
    }
}

fn part_digits(p: &Path) -> usize {
    let s = p.to_string_lossy().to_string();
    match s.rfind('.') {
        Some(i) if s[i + 1..].chars().all(|c| c.is_ascii_digit()) && !s[i + 1..].is_empty() => {
            s.len() - i - 1
        }
        _ => 3,
    }
}
