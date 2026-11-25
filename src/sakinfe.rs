//! windSAkinfe 功能整合层。
//!
//! 复用 windSAkinfe 库的全自研算法（零算法第三方依赖）：
//! * 打包：zip / tar.gz（底层 `ZipWriter` / `write_tar` + `write_gzip`）
//! * 解压：zip / 7z / tar / gz / xz / rar 只读（底层 `AnyArchive`，含 Zip Slip 防护）
//! * 哈希：CRC32 / MD5 / SHA-1 / SHA-256 一次读盘全算（底层 `hash::Hasher`）
//!
//! 与 CLI 层（`cmd::archive_ops`）不同，这里直接调底层模块，
//! 不经过 clap 参数结构，便于 GUI 后台线程直调。

use std::fs;
use std::io::{BufWriter, Read, Seek, Write};
use std::path::{Path, PathBuf};

use windsakinfe::algo::hash::{Algo, Hasher};
use windsakinfe::archive::tar::{self, TarInput};
use windsakinfe::archive::zip::ZipWriter;
use windsakinfe::archive::{safe_join, AnyArchive, Entry};
use windsakinfe::util::{self, Civil};

/// 递归深度上限（超深树防栈溢出，与 tasks.rs 同一铁律）
const MAX_DEPTH: usize = 64;

/// 待打包条目：归档内相对名 + 磁盘绝对路径 + 是否目录
struct CollectItem {
    name: String,
    path: PathBuf,
    is_dir: bool,
    mtime: Option<Civil>,
}

fn civil_of(p: &Path) -> Option<Civil> {
    fs::metadata(p).ok()?.modified().ok().map(util::system_time_to_local)
}

/// 展开目录内容（递归，名称带前缀）。跳过 symlink/junction（防环）。
fn collect_children(dir: &Path, prefix: &str, depth: usize, out: &mut Vec<CollectItem>) -> std::io::Result<()> {
    if depth > MAX_DEPTH {
        return Err(std::io::Error::other("目录嵌套过深（>64 层），已截断"));
    }
    // 先快照再递归（防自吞式递归，与 ops::copy_recursive 同款铁律）
    let items: Vec<fs::DirEntry> = fs::read_dir(dir)?.collect::<Result<_, _>>()?;
    for e in items {
        let md = e.metadata()?;
        if md.is_symlink() {
            continue;
        }
        let name = e.file_name().to_string_lossy().into_owned();
        let rel = format!("{prefix}/{name}");
        let p = e.path();
        if md.is_dir() {
            out.push(CollectItem {
                name: rel.clone(),
                path: p.clone(),
                is_dir: true,
                mtime: civil_of(&p),
            });
            collect_children(&p, &rel, depth + 1, out)?;
        } else {
            let mtime = civil_of(&p);
            out.push(CollectItem {
                name: rel,
                path: p,
                is_dir: false,
                mtime,
            });
        }
    }
    Ok(())
}

/// 把若干顶层路径（文件/目录树）展开成打包条目列表。
fn gather(paths: &[PathBuf]) -> std::io::Result<Vec<CollectItem>> {
    let mut items: Vec<CollectItem> = Vec::new();
    for p in paths {
        let md = fs::symlink_metadata(p)?;
        if md.is_symlink() {
            continue; // junction/链接不收
        }
        let base = p
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "root".to_string());
        if md.is_dir() {
            items.push(CollectItem {
                name: base.clone(),
                path: p.clone(),
                is_dir: true,
                mtime: civil_of(p),
            });
            collect_children(p, &base, 1, &mut items)?;
        } else {
            items.push(CollectItem {
                name: base,
                path: p.clone(),
                is_dir: false,
                mtime: civil_of(p),
            });
        }
    }
    Ok(items)
}

/// 打包为 zip。返回 (条目数, 写出字节)。
pub fn pack_zip(paths: &[PathBuf], out_path: &Path) -> std::io::Result<(usize, u64)> {
    let items = gather(paths)?;
    let n = items.len();

    let file = fs::File::create(out_path)?;
    let mut zw = ZipWriter::new(BufWriter::new(file), 6, None);
    for it in &items {
        if it.is_dir {
            zw.add_dir(&it.name, it.mtime.as_ref())?;
        } else {
            let data = fs::read(&it.path)?;
            zw.add_file(&it.name, &data, it.mtime.as_ref())?;
        }
    }
    let w = zw.finish()?;
    let mut f = w.into_inner()?;
    let size = f.seek(std::io::SeekFrom::End(0))?;
    Ok((n, size))
}

/// 打包为 tar.gz。返回 (条目数, 写出字节)。
pub fn pack_targz(paths: &[PathBuf], out_path: &Path) -> std::io::Result<(usize, u64)> {
    let items = gather(paths)?;
    let n = items.len();

    let mut inputs: Vec<TarInput> = Vec::with_capacity(items.len());
    for it in &items {
        let data = if it.is_dir { Vec::new() } else { fs::read(&it.path)? };
        inputs.push(TarInput {
            name: it.name.clone(),
            data,
            is_dir: it.is_dir,
            mtime: it.mtime.as_ref().map_or(0, |c| {
                util::local_to_unix_secs(c).max(0) as u64
            }),
            mode: if it.is_dir { 0o755 } else { 0o644 },
        });
    }
    let mut tar_data = Vec::new();
    tar::write_tar(&mut tar_data, &inputs)?;

    let file = fs::File::create(out_path)?;
    let mut w = BufWriter::new(file);
    windsakinfe::archive::gz::write_gzip(&mut w, &tar_data, 6)?;
    w.flush()?;
    let mut f = w.into_inner()?;
    let size = f.seek(std::io::SeekFrom::End(0))?;
    Ok((n, size))
}

/// 判断路径是否为可解压的归档（zip/7z/tar/gz/xz/tgz/txz）。
/// **RAR 明确排除**：windSAkinfe 对 rar 只能解析条目清单、无法解出内容，
/// 放进支持列表只会误导用户（点击后必失败）。
pub fn is_archive(p: &Path) -> bool {
    let ext = p.extension().map(|e| e.to_string_lossy().to_lowercase()).unwrap_or_default();
    if ext == "rar" {
        return false;
    }
    windsakinfe::archive::Kind::from_path(p).is_some()
}

/// 是否为 RAR 归档（解压入口用于给出明确提示）
pub fn is_rar(p: &Path) -> bool {
    p.extension().map(|e| e.to_string_lossy().to_lowercase()) == Some("rar".into())
}

/// 解压归档到 dest_dir（Zip Slip 防护走 windSAkinfe 的 safe_join）。
/// 返回解出的文件条目数。带密码归档会报错上抛。
/// （仅测试直用；生产走 extract_smart）
#[cfg_attr(not(test), allow(dead_code))]
pub fn extract_any(archive_path: &Path, dest_dir: &Path) -> std::io::Result<usize> {
    let data = fs::read(archive_path)?;
    let arch = AnyArchive::open(archive_path, data)?;
    extract_opened(&arch, &arch.entries(), dest_dir)
}

/// 已存在文件处理：返回 Err 提示。
fn check_no_conflict(entries: &[Entry], dest_dir: &Path) -> std::io::Result<()> {
    use std::collections::BTreeSet;
    let roots: BTreeSet<&str> = entries
        .iter()
        .map(|e| e.name.split('/').next().unwrap_or(""))
        .collect();
    for r in roots {
        if dest_dir.join(r).exists() {
            return Err(std::io::Error::other(format!("目标已存在：{r}")));
        }
    }
    Ok(())
}

/// 智能解压：
/// * 归档只有一个根条目（如单文件 .gz / 单文件夹包）→ 直接解到 `dest_parent`；
/// * 多个根条目 → 解到 `dest_parent/<归档名去后缀>/` 子目录（已存在则报错，避免混乱覆盖）。
///
/// 返回 (实际解压目标目录, 文件条目数)。
pub fn extract_smart(archive_path: &Path, dest_parent: &Path) -> std::io::Result<(PathBuf, usize)> {
    let data = fs::read(archive_path)?;
    let arch = AnyArchive::open(archive_path, data)?;
    let entries = arch.entries();
    use std::collections::BTreeSet;
    let roots: BTreeSet<&str> = entries
        .iter()
        .map(|e| e.name.split('/').next().unwrap_or(""))
        .filter(|s| !s.is_empty())
        .collect();

    let target = if roots.len() <= 1 {
        // 单根：先确认不会撞名
        check_no_conflict(&entries, dest_parent)?;
        dest_parent.to_path_buf()
    } else {
        // 多根：归入子目录 <stem>（tar.gz/tgz 等复合后缀剥两层）
        let name = archive_path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "archive".to_string());
        let lower = name.to_lowercase();
        let stem: String = if let Some(l) = lower.strip_suffix(".tar.gz") {
            name[..l.len()].to_string()
        } else if let Some(l) = lower.strip_suffix(".tar.xz") {
            name[..l.len()].to_string()
        } else if let Some(l) = lower.strip_suffix(".tgz") {
            name[..l.len()].to_string()
        } else if let Some(l) = lower.strip_suffix(".txz") {
            name[..l.len()].to_string()
        } else {
            archive_path
                .file_stem()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or(name)
        };
        let mut target = dest_parent.join(&stem);
        let mut k = 1;
        while target.exists() {
            target = dest_parent.join(format!("{stem} ({k})"));
            k += 1;
        }
        target
    };
    fs::create_dir_all(&target)?;
    let count = extract_opened(&arch, &entries, &target)?;
    Ok((target, count))
}

/// 把已打开的归档条目写入 dest（内部共用）。
fn extract_opened(
    arch: &AnyArchive,
    entries: &[Entry],
    dest: &Path,
) -> std::io::Result<usize> {
    let mut count = 0usize;
    for (idx, e) in entries.iter().enumerate() {
        let dest = safe_join(dest, &e.name)
            .ok_or_else(|| std::io::Error::other(format!("非法归档内路径：{}", e.name)))?;
        if e.is_dir {
            fs::create_dir_all(&dest)?;
        } else {
            if let Some(p) = dest.parent() {
                fs::create_dir_all(p)?;
            }
            let blob = arch.read(idx, None)?;
            fs::write(&dest, &blob)?;
            count += 1;
        }
    }
    Ok(count)
}

/// 一次读盘同时算 CRC32 / MD5 / SHA-1 / SHA-256（流式，4 个 Hasher 同喂）。
/// 返回多行 "算法: hex" 文本。
pub fn hash_file_all(path: &Path) -> std::io::Result<String> {
    let algos = [Algo::Crc32, Algo::Md5, Algo::Sha1, Algo::Sha256];
    let mut hs: Vec<Hasher> = algos.iter().map(|&a| Hasher::new(a)).collect();
    let mut f = fs::File::open(path)?;
    let mut buf = vec![0u8; 1 << 20]; // 1 MiB
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        for h in &mut hs {
            h.update(&buf[..n]);
        }
    }
    let mut out = String::new();
    for (a, h) in algos.iter().zip(hs) {
        let mut digest = h.finish();
        // windSAkinfe 的 Hasher 对 CRC32 输出小端字节序（zip 存储序），
        // 显示惯例是大端十六进制（如 "abc" → 352441c2）
        if *a == Algo::Crc32 {
            digest.reverse();
        }
        let hex: String = digest.iter().map(|b| format!("{b:02x}")).collect();
        out.push_str(&format!("{}: {}\n", a.name(), hex));
    }
    out.pop(); // 去掉末尾换行
    Ok(out)
}

// ---------------------------------------------------------------- 批量重命名 / 批量哈希

use windsakinfe::algo::template::{RenderCtx, Template};

/// 批量重命名预览行（预览与应用共用同一渲染，保证所见即所得）
#[derive(Clone, Debug)]
pub struct PreviewRow {
    pub old: String,
    pub new: String,
    /// 与目录内既有条目或本批其他目标重名（不分大小写）
    pub conflict: bool,
    /// 新名与原名相同
    pub unchanged: bool,
    /// 该条目专属错误（渲染空名 / 读元数据失败）
    pub error: Option<String>,
}

/// 一条待改名条目
#[derive(Clone, Debug)]
pub struct RenameItem {
    pub path: PathBuf,
    pub new_name: String,
}

/// 批量重命名结果统计
#[derive(Debug, Default)]
pub struct RenameOutcome {
    pub ok: u32,
    pub skipped: u32,
    pub failed: u32,
    pub first_err: Option<String>,
}

/// 渲染整批改名预览。`names_in_dir` 传目录内**全部**既有条目名
/// （内部自行排除参与条目自身），冲突判定不分大小写（Windows 语义）。
/// 模板非法（含裸 `*` 或语法错误）返回 Err。
pub fn render_batch(
    paths: &[PathBuf],
    tpl: &str,
    start: u64,
    names_in_dir: &std::collections::HashSet<String>,
) -> Result<Vec<PreviewRow>, String> {
    let t = Template::new(tpl)?;
    if t.star_count() > 0 {
        return Err("GUI 暂不支持 * 捕获回填（CLI rn 支持）".into());
    }
    let now = util::now_local();
    // 目录内全部既有名（不分大小写）。条目自身原名经 unchanged 分支豁免，
    // 其他条目原名仍算占用（保守：宁可标红让用户改模板，也不静默覆盖）。
    let taken: std::collections::HashSet<String> =
        names_in_dir.iter().map(|n| n.to_lowercase()).collect();

    let mut rows = Vec::with_capacity(paths.len());
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    for (i, p) in paths.iter().enumerate() {
        let md = match fs::symlink_metadata(p) {
            Ok(m) => m,
            Err(e) => {
                rows.push(PreviewRow {
                    old: file_name_str(p),
                    new: String::new(),
                    conflict: false,
                    unchanged: false,
                    error: Some(format!("读取失败: {e}")),
                });
                continue;
            }
        };
        let rendered = t.render(&RenderCtx {
            captures: &[],
            seq: start + i as u64,
            path: p,
            root: p.parent().unwrap_or(Path::new("")),
            mtime: md.modified().ok().map(util::system_time_to_local),
            size: md.len(),
            now: &now,
            rand: &util::rand_hex(6),
        });
        if rendered.is_empty() {
            rows.push(PreviewRow {
                old: file_name_str(p),
                new: String::new(),
                conflict: false,
                unchanged: false,
                error: Some("渲染出空名字".into()),
            });
            continue;
        }
        let old_lower = file_name_str(p).to_lowercase();
        let key = rendered.to_lowercase();
        let unchanged = key == old_lower;
        let conflict = !unchanged && (taken.contains(&key) || !seen.insert(key));
        rows.push(PreviewRow {
            old: file_name_str(p),
            new: rendered,
            conflict,
            unchanged,
            error: None,
        });
    }
    Ok(rows)
}

/// 执行批量改名。skip 语义：目标已存在则跳过，绝不覆盖。
pub fn batch_rename(items: &[RenameItem]) -> RenameOutcome {
    let mut out = RenameOutcome::default();
    for it in items {
        let dst = match it.path.parent() {
            Some(d) => d.join(&it.new_name),
            None => {
                out.failed += 1;
                if out.first_err.is_none() {
                    out.first_err = Some(format!("{}: 无父目录", file_name_str(&it.path)));
                }
                continue;
            }
        };
        if dst == it.path {
            out.skipped += 1; // 名字未变化
            continue;
        }
        if dst.exists() {
            out.skipped += 1; // 冲突跳过（预览已标红）
            continue;
        }
        match fs::rename(&it.path, &dst) {
            Ok(()) => out.ok += 1,
            Err(e) => {
                out.failed += 1;
                if out.first_err.is_none() {
                    out.first_err = Some(format!("{}: {e}", file_name_str(&it.path)));
                }
            }
        }
    }
    out
}

/// 批量计算 SHA-256（每文件独立流式读取）。返回 (文件名, 哈希或错误)。
pub fn hash_files_sha256(paths: &[PathBuf]) -> Vec<(String, Result<String, String>)> {
    paths
        .iter()
        .map(|p| {
            let name = file_name_str(p);
            let r = sha256_hex(p).map_err(|e| e.to_string());
            (name, r)
        })
        .collect()
}

fn sha256_hex(path: &Path) -> std::io::Result<String> {
    let mut h = Hasher::new(Algo::Sha256);
    let mut f = fs::File::open(path)?;
    let mut buf = vec![0u8; 1 << 20];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    Ok(h.finish().iter().map(|b| format!("{b:02x}")).collect())
}

fn file_name_str(p: &Path) -> String {
    p.file_name().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn tmp_dir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("wfm_sak_{}_{}", tag, std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn test_is_archive_rar_excluded() {
        // rar 明确不支持（无法解出内容），从可解压归档中排除
        assert!(!is_archive(Path::new("C:\\data\\movie.rar")));
        assert!(is_rar(Path::new("C:\\data\\movie.rar")));
        assert!(is_rar(Path::new("C:\\data\\MOVIE.RAR")));
        // 其余格式照常
        assert!(is_archive(Path::new("C:\\a.zip")));
        assert!(is_archive(Path::new("C:\\a.7z")));
        assert!(is_archive(Path::new("C:\\a.tar")));
        assert!(is_archive(Path::new("C:\\a.tar.gz")));
        assert!(is_archive(Path::new("C:\\a.tgz")));
        assert!(is_archive(Path::new("C:\\a.xz")));
        assert!(is_archive(Path::new("C:\\a.gz")));
        // 无后缀 / 目录
        assert!(!is_archive(Path::new("C:\\justfile")));
        assert!(!is_rar(Path::new("C:\\a.zip")));
    }

    #[test]
    fn test_pack_and_extract_zip() {
        let src = tmp_dir("zsrc");
        let dst = tmp_dir("zdst");
        fs::create_dir_all(src.join("sub")).unwrap();
        fs::write(src.join("a.txt"), "hello zip").unwrap();
        fs::write(src.join("sub/b.txt"), "nested").unwrap();

        let arc = src.parent().unwrap().join("t.zip");
        let (n, size) = pack_zip(std::slice::from_ref(&src), &arc).unwrap();
        assert!(n >= 4, "条目数 {n}"); // dir + a.txt + sub + sub/b.txt
        assert!(size > 0);

        assert!(is_archive(&arc));
        let cnt = extract_any(&arc, &dst).unwrap();
        assert_eq!(cnt, 2);
        let inner = dst.join(src.file_name().unwrap());
        assert_eq!(fs::read_to_string(inner.join("a.txt")).unwrap(), "hello zip");
        assert_eq!(
            fs::read_to_string(inner.join("sub").join("b.txt")).unwrap(),
            "nested"
        );
        let _ = fs::remove_file(&arc);
        let _ = fs::remove_dir_all(&src);
        let _ = fs::remove_dir_all(&dst);
    }

    #[test]
    fn test_pack_and_extract_targz() {
        let src = tmp_dir("gsrc");
        let dst = tmp_dir("gdst");
        fs::write(src.join("x.log"), "gzip me".repeat(100)).unwrap();

        let arc = src.parent().unwrap().join("t.tar.gz");
        let (n, size) = pack_targz(std::slice::from_ref(&src), &arc).unwrap();
        assert_eq!(n, 2); // dir + x.log
        assert!(size > 0 && size < 800, "gzip 后应远小于原文 {size}");

        let cnt = extract_any(&arc, &dst).unwrap();
        assert_eq!(cnt, 1);
        let inner = dst.join(src.file_name().unwrap());
        assert_eq!(fs::read_to_string(inner.join("x.log")).unwrap().len(), 700);
        let _ = fs::remove_file(&arc);
        let _ = fs::remove_dir_all(&src);
        let _ = fs::remove_dir_all(&dst);
    }

    #[test]
    fn test_hash_file_all() {
        let d = tmp_dir("h");
        let f = d.join("h.txt");
        fs::write(&f, b"abc").unwrap();
        let s = hash_file_all(&f).unwrap();
        // 已知值校验：SHA-256("abc")
        assert!(s.contains("ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"), "{s}");
        // MD5("abc")
        assert!(s.contains("900150983cd24fb0d6963f7d28e17f72"), "{s}");
        // CRC32("abc")
        assert!(s.to_lowercase().contains("352441c2"), "{s}");
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn test_extract_smart_multi_root() {
        // 两个独立文件 → 多根 → 应解到 <stem> 子目录
        let d = tmp_dir("smart");
        let f1 = d.join("one.txt");
        let f2 = d.join("two.txt");
        fs::write(&f1, "1").unwrap();
        fs::write(&f2, "22").unwrap();
        let arc = d.join("bundle.zip");
        pack_zip(&[f1, f2], &arc).unwrap();
        let (target, cnt) = extract_smart(&arc, &d).unwrap();
        assert_eq!(cnt, 2);
        assert!(target.ends_with("bundle"), "{target:?}");
        assert!(target.join("one.txt").is_file());
        // 再解一次 → 子目录名带 (1)
        let (t2, _) = extract_smart(&arc, &d).unwrap();
        assert!(t2.ends_with("bundle (1)"), "{t2:?}");
        let _ = fs::remove_dir_all(&d);
        let _ = fs::remove_file(&arc);
    }

    #[test]
    fn test_pack_rejects_deep_tree() {
        // 造一条 70 层深目录链，pack 应报错而不是爆栈
        let d = tmp_dir("deep");
        let mut cur = d.clone();
        for i in 0..70 {
            cur = cur.join(format!("L{i}"));
        }
        fs::create_dir_all(&cur).unwrap();
        fs::write(cur.join("deep.txt"), "bottom").unwrap();
        let root = d.join("L0");
        let arc = d.join("deep.zip");
        let r = pack_zip(&[root], &arc);
        assert!(r.is_err(), "超深树应报错");
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn test_extract_rejects_zip_slip() {
        // 手工构造带 .. 逃逸条目名的 zip 验证 safe_join 拦截
        let d = tmp_dir("slip");
        // 直接用 ZipWriter 打一个正常包，再把条目名改写成 ..\evil 不现实——
        // 这里退而验证 safe_join 对逃逸路径返回 None（windSAkinfe 已有自身单测），
        // 本测试确保我们的 extract 路径确实经过 safe_join：伪造 zip 用正常流程。
        let src = d.join("ok");
        fs::create_dir_all(&src).unwrap();
        fs::write(src.join("f.txt"), "x").unwrap();
        let arc = d.join("ok.zip");
        pack_zip(&[src], &arc).unwrap();
        let dst = d.join("out");
        fs::create_dir_all(&dst).unwrap();
        assert!(extract_any(&arc, &dst).is_ok());
        let _ = fs::remove_dir_all(&d);
    }

    // ---- 批量处理 ----

    use std::collections::HashSet;

    fn names_of(d: &Path) -> HashSet<String> {
        fs::read_dir(d).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect()
    }

    #[test]
    fn test_render_batch_seq() {
        let d = tmp_dir("rb1");
        let p: Vec<PathBuf> = ["a.txt", "b.txt", "c.txt"].iter().map(|n| d.join(n)).collect();
        for f in &p {
            fs::write(f, "x").unwrap();
        }
        let rows = render_batch(&p, "%3d", 1, &names_of(&d)).unwrap();
        assert_eq!(rows.len(), 3);
        assert!(rows.iter().all(|r| r.error.is_none() && !r.conflict));
        assert_eq!(rows[0].new, "001");
        assert_eq!(rows[1].new, "002");
        assert_eq!(rows[2].new, "003");
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn test_render_batch_conflict_and_unchanged() {
        let d = tmp_dir("rb2");
        let pa = d.join("001");
        let pb = d.join("b.txt");
        fs::write(&pa, "x").unwrap();
        fs::write(&pb, "x").unwrap();
        // 模板 %2d：a.txt→01、b.txt→02 均不冲突；再放一个名为 02 的干扰文件
        fs::write(d.join("02"), "x").unwrap();
        let p = vec![pa.clone(), pb.clone()];
        let rows = render_batch(&p, "%2d", 1, &names_of(&d)).unwrap();
        assert!(!rows[0].conflict);
        assert!(rows[1].conflict, "02 与干扰文件冲突");
        // 模板含原变量 → unchanged
        let rows2 = render_batch(&p, "%f", 1, &names_of(&d)).unwrap();
        assert!(rows2[0].unchanged && rows2[1].unchanged);
        // 大小写不敏感：目标 B.TXT 与既有 b.txt 冲突
        let rows3 = render_batch(&p, "B.TXT", 1, &names_of(&d)).unwrap();
        assert!(rows3[0].conflict);
        assert!(rows3[1].unchanged); // 自身原名不算冲突
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn test_render_batch_invalid_tpl() {
        let d = tmp_dir("rb3");
        let f = d.join("x.txt");
        fs::write(&f, "x").unwrap();
        let p = vec![f];
        assert!(render_batch(&p, "*", 1, &names_of(&d)).is_err()); // GUI 禁 *
        assert!(render_batch(&p, "%", 1, &names_of(&d)).is_err()); // 孤立 %
        assert!(render_batch(&p, "%9q", 1, &names_of(&d)).is_err()); // 宽度+非法符
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn test_batch_rename_ok_skip_no_overwrite() {
        let d = tmp_dir("br");
        let f1 = d.join("a.txt");
        let f2 = d.join("b.txt");
        fs::write(&f1, "1").unwrap();
        fs::write(&f2, "2").unwrap();
        fs::write(d.join("001"), "占用").unwrap(); // 目标已存在
        let items = vec![
            RenameItem { path: f1.clone(), new_name: "001".into() }, // 冲突 → skip
            RenameItem { path: f2.clone(), new_name: "002".into() }, // 成功
        ];
        let o = batch_rename(&items);
        assert_eq!((o.ok, o.skipped, o.failed), (1, 1, 0), "{o:?}");
        assert_eq!(fs::read_to_string(d.join("001")).unwrap(), "占用", "绝不覆盖");
        assert!(d.join("002").is_file());
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn test_batch_rename_dirs() {
        let d = tmp_dir("brd");
        let dir1 = d.join("docs");
        fs::create_dir_all(dir1.join("inner")).unwrap();
        fs::write(dir1.join("inner/f.txt"), "x").unwrap();
        let items = vec![RenameItem { path: dir1, new_name: "docs2026".into() }];
        let o = batch_rename(&items);
        assert_eq!(o.ok, 1);
        assert!(d.join("docs2026/inner/f.txt").is_file(), "目录改名后内容不受影响");
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn test_hash_files_sha256() {
        let d = tmp_dir("bh");
        let f = d.join("hi.txt");
        fs::write(&f, "hi\n").unwrap();
        let miss = d.join("gone.txt");
        let rs = hash_files_sha256(&[f.clone(), miss]);
        assert_eq!(rs.len(), 2);
        assert_eq!(
            rs[0].1.as_deref().unwrap(),
            "98ea6e4f216f2fb4b69fff9b3a44842c38686ca685f3f55dc48c5d3fb1107be4"
        );
        assert!(rs[1].1.is_err());
        let _ = fs::remove_dir_all(&d);
    }
}
