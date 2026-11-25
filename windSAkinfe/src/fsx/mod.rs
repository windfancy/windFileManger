//! 文件系统原语：遍历、复制、删除、改名、时间戳、剩余空间。
//!
//! 这一层只做「安全、可复用、跨平台」的基础动作，不含业务策略
//! （冲突怎么处理、要不要预演，全部由 `plan` 层决定）。

use std::fs;
use std::io::{self, Read, Write};
use std::path::{Component, Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::algo::natsort::{natural_cmp, natural_cmp_path};

// ---------------------------------------------------------------- 遍历

#[derive(Debug, Clone)]
pub struct WalkOpts {
    /// 最大深度，0 = 只看根的直接子项，usize::MAX = 不限
    pub max_depth: usize,
    /// 是否跟随符号链接（默认否，防环）
    pub follow_links: bool,
    /// 是否按自然序排序（默认是，输出稳定）
    pub sort: bool,
}

impl Default for WalkOpts {
    fn default() -> Self {
        WalkOpts { max_depth: usize::MAX, follow_links: false, sort: true }
    }
}

/// 一次遍历拿到的条目快照。
///
/// Windows 上 `DirEntry::metadata()` 直接复用 readdir 的 `WIN32_FIND_DATA`，
/// 不再发起系统调用；而 `fs::metadata(path)` 每个文件约 40~100µs（杀软实时
/// 扫描 + NTFS 元数据）。10 万文件时这个差别是「1 秒」和「10 秒」。
#[derive(Debug, Clone)]
pub struct WalkItem {
    pub path: PathBuf,
    /// 相对根目录的路径，统一 `/` 分隔
    pub rel: String,
    pub depth: usize,
    pub is_dir: bool,
    pub is_symlink: bool,
    pub len: u64,
    /// unix 秒；取不到为 `None`
    pub mtime: Option<u64>,
}

/// 前序遍历并带回类型/大小/时间，一次 readdir 拿全。
pub fn walk_entries(root: &Path, opts: &WalkOpts) -> io::Result<Vec<WalkItem>> {
    let mut out = Vec::new();
    let meta = fs::symlink_metadata(root)?;
    if !meta.is_dir() {
        return Ok(out);
    }
    walk_entries_inner(root, root, 1, opts, &mut out)?;
    Ok(out)
}

fn walk_entries_inner(
    root: &Path,
    dir: &Path,
    depth: usize,
    opts: &WalkOpts,
    out: &mut Vec<WalkItem>,
) -> io::Result<()> {
    let rd = match fs::read_dir(dir) {
        Ok(r) => r,
        Err(e) => {
            if e.kind() == io::ErrorKind::PermissionDenied {
                return Ok(());
            }
            return Err(e);
        }
    };
    let mut ents: Vec<fs::DirEntry> = Vec::new();
    for e in rd.flatten() {
        ents.push(e);
    }
    if opts.sort {
        // 同一目录下的条目前缀一致，按文件名自然排序与按全路径排序等价。
        // 名称只转换一次（而不是每次比较都 to_string_lossy）。
        let mut keyed: Vec<(String, fs::DirEntry)> = ents
            .into_iter()
            .map(|e| (e.file_name().to_string_lossy().to_string(), e))
            .collect();
        keyed.sort_by(|a, b| natural_cmp(&a.0, &b.0));
        ents = keyed.into_iter().map(|(_, e)| e).collect();
    }
    for e in ents {
        let p = e.path();
        // 注意：DirEntry::metadata 不跟随符号链接，与 symlink_metadata 语义一致
        let md = e.metadata().ok();
        let (is_symlink, len, mtime) = match &md {
            Some(m) => (
                m.file_type().is_symlink(),
                m.len(),
                m.modified().ok().and_then(|t| t.duration_since(UNIX_EPOCH).ok()).map(|d| d.as_secs()),
            ),
            None => (false, 0, None),
        };
        let is_dir = match &md {
            Some(m) if is_symlink => {
                if opts.follow_links {
                    fs::metadata(&p).map(|x| x.is_dir()).unwrap_or(false)
                } else {
                    false
                }
            }
            Some(m) => m.is_dir(),
            None => false,
        };
        let rel = p.strip_prefix(root).unwrap_or(&p).to_string_lossy().replace('\\', "/");
        let descend = is_dir && depth < opts.max_depth;
        // 先备份一份路径给递归用，避免克隆整个结构体
        let child = if descend { Some(p.clone()) } else { None };
        out.push(WalkItem { path: p, rel, depth, is_dir, is_symlink, len, mtime });
        if let Some(c) = child {
            walk_entries_inner(root, &c, depth + 1, opts, out)?;
        }
    }
    Ok(())
}

/// 前序遍历（父目录先于其子项），返回 (路径, 深度)。不含 `root` 本身。
pub fn walk_dir(root: &Path, opts: &WalkOpts) -> io::Result<Vec<(PathBuf, usize)>> {
    Ok(walk_entries(root, opts)?
        .into_iter()
        .map(|i| (i.path, i.depth))
        .collect())
}

/// 后序遍历（子项先于父项），用于安全删除与体积统计。
pub fn walk_post_order(root: &Path) -> io::Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    post_inner(root, &mut out)?;
    Ok(out)
}

fn post_inner(p: &Path, out: &mut Vec<PathBuf>) -> io::Result<()> {
    let meta = match fs::symlink_metadata(p) {
        Ok(m) => m,
        Err(_) => return Ok(()),
    };
    if meta.is_dir() && !meta.file_type().is_symlink() {
        if let Ok(rd) = fs::read_dir(p) {
            let mut names: Vec<PathBuf> = rd.flatten().map(|e| e.path()).collect();
            names.sort_by(|a, b| natural_cmp_path(a, b));
            for c in names {
                post_inner(&c, out)?;
            }
        }
    }
    out.push(p.to_path_buf());
    Ok(())
}

/// 递归统计目录内文件数与总字节数（不含符号链接目标）。
pub fn dir_stats(root: &Path) -> (u64, u64) {
    let mut files = 0u64;
    let mut bytes = 0u64;
    let mut stack = vec![root.to_path_buf()];
    while let Some(d) = stack.pop() {
        let rd = match fs::read_dir(&d) {
            Ok(r) => r,
            Err(_) => continue,
        };
        for ent in rd.flatten() {
            // 用 DirEntry::metadata（Windows 复用 readdir 数据，零系统调用），
            // 而不是 fs::symlink_metadata(&path)——后者每个文件约 40~100µs。
            let md = match ent.metadata() {
                Ok(m) => m,
                Err(_) => continue,
            };
            if md.file_type().is_symlink() {
                continue; // 不跟随、也不计入
            }
            if md.is_dir() {
                stack.push(ent.path());
            } else if md.is_file() {
                files += 1;
                bytes += md.len();
            }
        }
    }
    (files, bytes)
}

// ---------------------------------------------------------------- 元数据

pub fn mtime_of(p: &Path) -> Option<SystemTime> {
    fs::metadata(p).ok().and_then(|m| m.modified().ok())
}

pub fn mtime_secs(p: &Path) -> Option<u64> {
    mtime_of(p)?.duration_since(UNIX_EPOCH).ok().map(|d| d.as_secs())
}

pub fn is_symlink(p: &Path) -> bool {
    fs::symlink_metadata(p).map(|m| m.file_type().is_symlink()).unwrap_or(false)
}

/// 设置修改时间（同时也设置访问时间，避免某些平台联动异常）。
pub fn set_mtime(p: &Path, secs: u64) -> io::Result<()> {
    set_times(p, Some(secs), Some(secs))
}

/// 分别设置 mtime / atime：传 `None` 的那一项保持不变（对应 `touch -m` / `-a`）。
pub fn set_times(p: &Path, mtime: Option<u64>, atime: Option<u64>) -> io::Result<()> {
    if mtime.is_none() && atime.is_none() {
        return Ok(());
    }
    let f = open_for_time(p)?;
    let mut times = fs::FileTimes::new();
    if let Some(s) = mtime {
        times = times.set_modified(UNIX_EPOCH + Duration::from_secs(s));
    }
    if let Some(s) = atime {
        times = times.set_accessed(UNIX_EPOCH + Duration::from_secs(s));
    }
    f.set_times(times)
}

/// 以可写方式打开（改时间戳需要写句柄）；只读文件临时清位后恢复。
fn open_for_time(p: &Path) -> io::Result<fs::File> {
    // Rust 1.75+ 提供 File::set_times；文件必须以可写方式打开
    fs::OpenOptions::new().write(true).open(p).or_else(|_| {
        let md = fs::metadata(p)?;
        let mut perm = md.permissions();
        let was_ro = perm.readonly();
        if was_ro {
            #[allow(clippy::permissions_set_readonly_false)]
            perm.set_readonly(false);
            fs::set_permissions(p, perm.clone())?;
        }
        let f = fs::OpenOptions::new().write(true).open(p);
        if was_ro {
            let mut restore = md.permissions();
            restore.set_readonly(true);
            let _ = fs::set_permissions(p, restore);
        }
        f
    })
}

pub fn clear_readonly(p: &Path) -> io::Result<()> {
    let md = fs::symlink_metadata(p)?;
    if md.permissions().readonly() {
        let mut perm = md.permissions();
        #[allow(clippy::permissions_set_readonly_false)]
        perm.set_readonly(false);
        fs::set_permissions(p, perm)?;
    }
    Ok(())
}

// ---------------------------------------------------------------- 删除

/// 递归删除（后序、容错、清只读位）。返回成功删除的条目数。
pub fn remove_tree(p: &Path, keep_readonly: bool) -> io::Result<u64> {
    let mut n = 0u64;
    for item in walk_post_order(p)? {
        let meta = match fs::symlink_metadata(&item) {
            Ok(m) => m,
            Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
            Err(e) => return Err(e),
        };
        let res = if meta.is_dir() && !meta.file_type().is_symlink() {
            fs::remove_dir(&item)
        } else {
            fs::remove_file(&item)
        };
        match res {
            Ok(()) => n += 1,
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => {
                if !keep_readonly {
                    // Windows 只读文件删不掉：清位重试
                    let _ = clear_readonly(&item);
                    let retry = if meta.is_dir() && !meta.file_type().is_symlink() {
                        fs::remove_dir(&item)
                    } else {
                        fs::remove_file(&item)
                    };
                    match retry {
                        Ok(()) => n += 1,
                        Err(e2) if e2.kind() == io::ErrorKind::NotFound => {}
                        Err(e2) => return Err(e2),
                    }
                } else {
                    return Err(e);
                }
            }
        }
    }
    Ok(n)
}

// ---------------------------------------------------------------- 复制

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CopyMode {
    /// 普通读写
    Normal,
    /// 只复制链接本身
    LinkItself,
}

#[derive(Debug, Clone)]
pub struct CopyOpts {
    pub preserve_mtime: bool,
    pub preserve_perms: bool,
    pub mode: CopyMode,
    /// 覆盖已存在文件
    pub overwrite: bool,
    /// 只复制匹配这些模式的条目（空 = 全收）。同时比对相对路径与文件名。
    pub include: Vec<crate::algo::pattern::Pattern>,
    /// 排除匹配这些模式的条目
    pub exclude: Vec<crate::algo::pattern::Pattern>,
}

impl CopyOpts {
    /// 目录内部条目的筛选取舍。
    ///
    /// 目录默认放行（否则里面的命中文件无处安放），除非它自己被排除；
    /// `include` 只约束文件。
    pub fn accepts(&self, rel: &str, name: &str, is_dir: bool) -> bool {
        if self.exclude.iter().any(|p| p.is_match(rel) || p.is_match(name)) {
            return false;
        }
        is_dir || self.include.is_empty() || self.include.iter().any(|p| p.is_match(rel) || p.is_match(name))
    }
}

impl Default for CopyOpts {
    fn default() -> Self {
        CopyOpts {
            preserve_mtime: true,
            preserve_perms: false,
            mode: CopyMode::Normal,
            overwrite: true,
            include: Vec::new(),
            exclude: Vec::new(),
        }
    }
}

const BUF: usize = 256 * 1024;

/// 复制单个文件（不处理目录递归）。
pub fn copy_file(src: &Path, dst: &Path, opts: &CopyOpts) -> io::Result<u64> {
    if opts.mode == CopyMode::LinkItself && is_symlink(src) {
        let target = fs::read_link(src)?;
        if dst.exists() || is_symlink(dst) {
            if opts.overwrite {
                let _ = fs::remove_file(dst);
            } else {
                return Err(io::Error::new(io::ErrorKind::AlreadyExists, "目标已存在"));
            }
        }
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(&target, dst)?;
        }
        #[cfg(windows)]
        {
            let md = fs::metadata(src);
            if md.map(|m| m.is_dir()).unwrap_or(false) {
                std::os::windows::fs::symlink_dir(&target, dst)?;
            } else {
                std::os::windows::fs::symlink_file(&target, dst)?;
            }
        }
        return Ok(0);
    }

    if let Some(parent) = dst.parent() {
        if !parent.as_os_str().is_empty() {
            fs::create_dir_all(parent)?;
        }
    }
    let mut r = fs::File::open(src)?;
    let mut w = fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(dst)?;
    let mut buf = vec![0u8; BUF];
    let mut total = 0u64;
    loop {
        let n = r.read(&mut buf)?;
        if n == 0 {
            break;
        }
        w.write_all(&buf[..n])?;
        total += n as u64;
    }
    w.flush()?;
    drop(w);
    drop(r);

    if opts.preserve_perms {
        if let Ok(md) = fs::metadata(src) {
            let _ = fs::set_permissions(dst, md.permissions());
        }
    }
    if opts.preserve_mtime {
        if let Some(t) = mtime_of(src) {
            let f = fs::OpenOptions::new().write(true).open(dst);
            if let Ok(f) = f {
                let times = fs::FileTimes::new().set_modified(t).set_accessed(t);
                let _ = f.set_times(times);
            }
        }
    }
    Ok(total)
}

/// 递归复制目录。返回复制的文件数与字节数。
pub fn copy_tree(src: &Path, dst: &Path, opts: &CopyOpts) -> io::Result<(u64, u64)> {
    fs::create_dir_all(dst)?;
    let mut files = 0u64;
    let mut bytes = 0u64;
    // 栈元素带「相对 src 的路径」，include/exclude 才有得比
    let mut stack: Vec<(PathBuf, PathBuf, String)> =
        vec![(src.to_path_buf(), dst.to_path_buf(), String::new())];
    while let Some((s, d, rel)) = stack.pop() {
        for ent in fs::read_dir(&s)?.flatten() {
            let name = ent.file_name().to_string_lossy().to_string();
            let crel = if rel.is_empty() { name.clone() } else { format!("{rel}/{name}") };
            let sp = ent.path();
            // DirEntry::metadata 在 Windows 上零系统调用；且不跟随符号链接
            let md = match ent.metadata() {
                Ok(m) => m,
                Err(_) => continue,
            };
            let is_symlink = md.file_type().is_symlink();
            let is_dir = md.is_dir() && !is_symlink;
            if !opts.accepts(&crel, &name, is_dir) {
                continue;
            }
            let dp = d.join(&name);
            if is_symlink {
                if opts.mode == CopyMode::LinkItself {
                    copy_file(&sp, &dp, opts)?;
                } else if let Ok(target_md) = fs::metadata(&sp) {
                    if target_md.is_dir() {
                        stack.push((sp, dp, crel));
                    } else {
                        bytes += copy_file(&sp, &dp, opts)?;
                        files += 1;
                    }
                }
            } else if is_dir {
                fs::create_dir_all(&dp)?;
                stack.push((sp, dp, crel));
            } else {
                bytes += copy_file(&sp, &dp, opts)?;
                files += 1;
            }
        }
    }
    Ok((files, bytes))
}

// ---------------------------------------------------------------- 冲突命名

/// 仿资源管理器的冲突重命名：`a.txt` → `a (1).txt`。
pub fn unique_path(p: &Path) -> PathBuf {
    if !exists_any(p) {
        return p.to_path_buf();
    }
    let parent = p.parent().map(|x| x.to_path_buf()).unwrap_or_default();
    let stem = p.file_stem().map(|s| s.to_string_lossy().to_string()).unwrap_or_default();
    let ext = p.extension().map(|s| s.to_string_lossy().to_string());
    for i in 1u32..=100_000 {
        let name = match &ext {
            Some(e) => format!("{stem} ({i}).{e}"),
            None => format!("{stem} ({i})"),
        };
        let cand = parent.join(name);
        if !exists_any(&cand) {
            return cand;
        }
    }
    p.to_path_buf()
}

pub fn exists_any(p: &Path) -> bool {
    fs::symlink_metadata(p).is_ok()
}

/// 大小写不敏感平台上的「是否同一路径」判断，用于拒绝 `mv a a/b`。
pub fn is_ancestor(ancestor: &Path, child: &Path) -> bool {
    let a = normalize(ancestor);
    let c = normalize(child);
    c != a && c.starts_with(&a)
}

fn normalize(p: &Path) -> PathBuf {
    let abs = if p.is_absolute() {
        p.to_path_buf()
    } else {
        std::env::current_dir().map(|d| d.join(p)).unwrap_or_else(|_| p.to_path_buf())
    };
    let mut out = PathBuf::new();
    for comp in abs.components() {
        match comp {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    // Windows 盘符/大小写归一
    #[cfg(windows)]
    {
        let s = out.to_string_lossy().to_lowercase();
        PathBuf::from(s)
    }
    #[cfg(not(windows))]
    {
        out
    }
}

// ---------------------------------------------------------------- 剩余空间

#[cfg(windows)]
pub fn available_space(p: &Path) -> Option<u64> {
    #[link(name = "kernel32")]
    extern "system" {
        fn GetDiskFreeSpaceExW(
            dir: *const u16,
            free_for_user: *mut u64,
            total: *mut u64,
            total_free: *mut u64,
        ) -> i32;
    }
    use std::os::windows::ffi::OsStrExt;
    // 取已存在的最高层祖先，盘符必须存在才能查询
    let mut probe = p.to_path_buf();
    while !probe.exists() {
        match probe.parent() {
            Some(x) if !x.as_os_str().is_empty() => probe = x.to_path_buf(),
            _ => return None,
        }
    }
    let wide: Vec<u16> = probe.as_os_str().encode_wide().chain(std::iter::once(0)).collect();
    let mut free_for_user: u64 = 0;
    let mut total: u64 = 0;
    let mut total_free: u64 = 0;
    let ok = unsafe {
        GetDiskFreeSpaceExW(wide.as_ptr(), &mut free_for_user, &mut total, &mut total_free)
    };
    if ok == 0 {
        None
    } else {
        Some(free_for_user)
    }
}

#[cfg(not(windows))]
pub fn available_space(_p: &Path) -> Option<u64> {
    // 无额外依赖时用 statvfs
    #[cfg(target_os = "linux")]
    {
        #[repr(C)]
        struct StatVfs {
            f_bsize: u64,
            f_frsize: u64,
            f_blocks: u64,
            f_bfree: u64,
            f_bavail: u64,
            f_files: u64,
            f_ffree: u64,
            f_favail: u64,
            f_fsid: u64,
            f_flag: u64,
            f_namemax: u64,
            _pad: [u64; 4],
        }
        extern "C" {
            fn statvfs(path: *const std::os::raw::c_char, buf: *mut StatVfs) -> i32;
        }
        use std::ffi::CString;
        let cs = CString::new(_p.to_string_lossy().as_bytes()).ok()?;
        let mut buf = std::mem::MaybeUninit::<StatVfs>::zeroed();
        let r = unsafe { statvfs(cs.as_ptr(), buf.as_mut_ptr()) };
        if r != 0 {
            return None;
        }
        let b = unsafe { buf.assume_init() };
        Some(b.f_bavail * b.f_frsize)
    }
    #[cfg(not(target_os = "linux"))]
    {
        None
    }
}

// ---------------------------------------------------------------- 回收站

/// 把文件/目录送入系统回收站。
///
/// - Windows：`SHFileOperationW(FO_DELETE | FOF_ALLOWUNDO)`，进资源管理器回收站。
/// - 其它平台：按 FreeDesktop Trash 规范搬到 `~/.local/share/Trash/files`
///   并写 `.trashinfo`。
#[cfg(windows)]
pub fn trash(p: &Path) -> io::Result<()> {
    #[repr(C)]
    struct ShFileOpStructW {
        hwnd: *mut core::ffi::c_void,
        w_func: u32,
        p_from: *const u16,
        p_to: *const u16,
        f_flags: u16,
        f_any_operations_aborted: i32,
        h_name_mappings: *mut core::ffi::c_void,
        lpsz_progress_title: *const u16,
    }
    #[link(name = "shell32")]
    extern "system" {
        fn SHFileOperationW(op: *mut ShFileOpStructW) -> i32;
    }
    const FO_DELETE: u32 = 3;
    const FOF_SILENT: u16 = 0x0004;
    const FOF_NOCONFIRMATION: u16 = 0x0010;
    const FOF_ALLOWUNDO: u16 = 0x0040;
    const FOF_NOERRORUI: u16 = 0x0400;

    use std::os::windows::ffi::OsStrExt;
    // 注意：canonicalize 在 Windows 上返回 `\\?\D://...` 形式的长路径前缀，
    // SHFileOperationW 不接受（会返回 124 = DE_INVALIDFILES），必须剥掉前缀。
    let raw = std::fs::canonicalize(p).unwrap_or_else(|_| crate::plan::absolute(p));
    let s = raw.to_string_lossy().to_string();
    let stripped = s.strip_prefix(r"\\?\").map(|x| x.to_string()).unwrap_or(s);
    let abs = std::path::PathBuf::from(stripped);
    // pFrom 必须以双 NUL 结尾，且是绝对路径
    let mut from: Vec<u16> = abs.as_os_str().encode_wide().collect();
    from.push(0);
    from.push(0);
    let mut op = ShFileOpStructW {
        hwnd: std::ptr::null_mut(),
        w_func: FO_DELETE,
        p_from: from.as_ptr(),
        p_to: std::ptr::null(),
        f_flags: FOF_ALLOWUNDO | FOF_NOCONFIRMATION | FOF_SILENT | FOF_NOERRORUI,
        f_any_operations_aborted: 0,
        h_name_mappings: std::ptr::null_mut(),
        lpsz_progress_title: std::ptr::null(),
    };
    let rc = unsafe { SHFileOperationW(&mut op) };
    if rc != 0 {
        return Err(io::Error::other(format!("移入回收站失败（SHFileOperation 返回 {rc}）")));
    }
    if op.f_any_operations_aborted != 0 {
        return Err(io::Error::other("移入回收站被用户取消"));
    }
    Ok(())
}

#[cfg(not(windows))]
pub fn trash(p: &Path) -> io::Result<()> {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .ok_or_else(|| io::Error::other("找不到 HOME，无法定位回收站"))?;
    let root = home.join(".local/share/Trash");
    let files = root.join("files");
    let info = root.join("info");
    fs::create_dir_all(&files)?;
    fs::create_dir_all(&info)?;
    let name = p.file_name().map(|s| s.to_os_string()).unwrap_or_default();
    let mut target = files.join(&name);
    let mut i = 1;
    while exists_any(&target) {
        let n = format!("{}.{i}", String::from_utf8_lossy(name.as_encoded_bytes()));
        target = files.join(n);
        i += 1;
    }
    fs::rename(p, &target).or_else(|_| {
        let opts = CopyOpts::default();
        copy_tree(p, &target, &opts)?;
        remove_tree(p, false).map(|_| ())
    })?;
    let when = crate::util::now_local().format("YYYY-MM-DD HH:mm:ss");
    let mut body = String::from("[Trash Info]\nPath=");
    body.push_str(&absolute(p).to_string_lossy().replace(' ', "%20"));
    body.push_str("\nDeletionDate=");
    body.push_str(&when);
    body.push('\n');
    fs::write(info.join(format!("{}.trashinfo", target.file_name().unwrap().to_string_lossy())), body)?;
    Ok(())
}

// ---------------------------------------------------------------- 临时名

/// 两阶段改名用的临时名（同目录，隐藏前缀 + pid + 序号）。
pub fn temp_name(dir: &Path, seq: usize) -> PathBuf {
    dir.join(format!(".wsak_tmp_{}_{}", std::process::id(), seq))
}

pub fn filetime_to_unix(ft: u64) -> u64 {
    // Windows FILETIME：1601-01-01 起的 100ns 数
    const EPOCH_DIFF: u64 = 11_644_473_600;
    if ft < EPOCH_DIFF * 10_000_000 {
        return 0;
    }
    ft / 10_000_000 - EPOCH_DIFF
}

pub fn unix_to_filetime(secs: u64) -> u64 {
    const EPOCH_DIFF: u64 = 11_644_473_600;
    (secs + EPOCH_DIFF) * 10_000_000
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmpdir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("wsak_fsx_{}_{}", tag, std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn walk_pre_and_post_order() {
        let d = tmpdir("walk");
        fs::create_dir_all(d.join("a/b")).unwrap();
        fs::write(d.join("a/f1.txt"), b"1").unwrap();
        fs::write(d.join("a/b/f2.txt"), b"22").unwrap();
        fs::write(d.join("top.txt"), b"333").unwrap();

        let pre = walk_dir(&d, &WalkOpts::default()).unwrap();
        assert_eq!(pre.len(), 5);
        // 第一个应该是 a/（排序后 a 在 top.txt 前）
        assert_eq!(pre[0].0, d.join("a"));

        let post = walk_post_order(&d).unwrap();
        assert_eq!(post.len(), 6); // 含根
        assert_eq!(post.last().unwrap(), &d);
        // 子项必须先于父项出现
        let ia = post.iter().position(|p| p == &d.join("a")).unwrap();
        let ib = post.iter().position(|p| p == &d.join("a/b")).unwrap();
        assert!(ib < ia);

        let (files, bytes) = dir_stats(&d);
        assert_eq!(files, 3);
        assert_eq!(bytes, 6);
        fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn copy_and_remove() {
        let d = tmpdir("copy");
        fs::create_dir_all(d.join("src/sub")).unwrap();
        fs::write(d.join("src/a.txt"), b"hello").unwrap();
        fs::write(d.join("src/sub/b.txt"), b"world!").unwrap();
        let (n, b) = copy_tree(&d.join("src"), &d.join("dst"), &CopyOpts::default()).unwrap();
        assert_eq!(n, 2);
        assert_eq!(b, 11);
        assert_eq!(fs::read(d.join("dst/sub/b.txt")).unwrap(), b"world!");
        let removed = remove_tree(&d.join("src"), false).unwrap();
        assert_eq!(removed, 4); // src + sub + 2 files
        assert!(!d.join("src").exists());
        fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn unique_path_numbers() {
        let d = tmpdir("uniq");
        fs::write(d.join("a.txt"), b"").unwrap();
        fs::write(d.join("a (1).txt"), b"").unwrap();
        let p = unique_path(&d.join("a.txt"));
        assert_eq!(p.file_name().unwrap(), "a (2).txt");
        let q = unique_path(&d.join("none.txt"));
        assert_eq!(q.file_name().unwrap(), "none.txt");
        fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn ancestor_detection() {
        assert!(is_ancestor(Path::new("a"), Path::new("a/b")));
        assert!(!is_ancestor(Path::new("a"), Path::new("a")));
        assert!(!is_ancestor(Path::new("a/b"), Path::new("a")));
    }

    #[test]
    fn mtime_roundtrip() {
        let d = tmpdir("mtime");
        let f = d.join("x.txt");
        fs::write(&f, b"x").unwrap();
        set_mtime(&f, 1_700_000_000).unwrap();
        assert_eq!(mtime_secs(&f), Some(1_700_000_000));
        fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn readonly_delete_retry() {
        let d = tmpdir("ro");
        let f = d.join("ro.txt");
        fs::write(&f, b"x").unwrap();
        let mut perm = fs::metadata(&f).unwrap().permissions();
        perm.set_readonly(true);
        fs::set_permissions(&f, perm).unwrap();
        remove_tree(&f, false).unwrap();
        assert!(!f.exists());
        fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn filetime_conversion() {
        assert_eq!(unix_to_filetime(0), 116444736000000000);
        assert_eq!(filetime_to_unix(116444736000000000), 0);
        assert_eq!(filetime_to_unix(unix_to_filetime(1_700_000_000)), 1_700_000_000);
    }

    #[test]
    fn space_query_is_sane() {
        if let Some(s) = available_space(&std::env::temp_dir()) {
            assert!(s > 0);
        }
    }
}

#[cfg(test)]
mod copy_opts_tests {
    use super::*;

    fn tmpdir(tag: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!(
            "wsak_cp_{tag}_{}_{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = fs::remove_dir_all(&p);
        fs::create_dir_all(&p).unwrap();
        p
    }

    /// 在 Windows 上没有 SeCreateSymbolicLinkPrivilege 时会失败，
    /// 这时直接跳过（不把环境限制当作代码缺陷）。
    fn try_symlink_file(target: &Path, link: &Path) -> bool {
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(target, link).is_ok()
        }
        #[cfg(windows)]
        {
            std::os::windows::fs::symlink_file(target, link).is_ok()
        }
    }

    #[test]
    fn copy_links_keeps_link_itself() {
        let d = tmpdir("link");
        let src = d.join("real.txt");
        fs::write(&src, b"payload").unwrap();
        let link = d.join("link.txt");
        if !try_symlink_file(&src, &link) {
            fs::remove_dir_all(&d).ok();
            return; // 无权限，跳过
        }

        // --copy-links：结果是链接本身（read_link 能读回）
        let dst_link = d.join("copied_link.txt");
        let opts = CopyOpts { mode: CopyMode::LinkItself, ..CopyOpts::default() };
        copy_file(&link, &dst_link, &opts).unwrap();
        assert!(is_symlink(&dst_link), "LinkItself 应产出符号链接");
        assert_eq!(fs::read_link(&dst_link).unwrap(), src);

        // 默认（Normal）：跟随链接，产出普通文件
        let dst_file = d.join("copied_file.txt");
        copy_file(&link, &dst_file, &CopyOpts::default()).unwrap();
        assert!(!is_symlink(&dst_file), "默认应跟随链接产出普通文件");
        assert_eq!(fs::read(&dst_file).unwrap(), b"payload");

        fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn copy_tree_respects_copy_links() {
        let d = tmpdir("treelink");
        let sub = d.join("src");
        fs::create_dir_all(&sub).unwrap();
        let real = sub.join("real.txt");
        fs::write(&real, b"abc").unwrap();
        let link = sub.join("link.txt");
        if !try_symlink_file(&real, &link) {
            fs::remove_dir_all(&d).ok();
            return;
        }

        let follow = d.join("out_follow");
        copy_tree(&sub, &follow, &CopyOpts::default()).unwrap();
        assert!(!is_symlink(&follow.join("link.txt")));

        let keep = d.join("out_keep");
        let opts = CopyOpts { mode: CopyMode::LinkItself, ..CopyOpts::default() };
        copy_tree(&sub, &keep, &opts).unwrap();
        assert!(is_symlink(&keep.join("link.txt")));

        fs::remove_dir_all(&d).ok();
    }

    /// 保留权限位：源为只读时，只有 `preserve_perms` 才让目标也只读。
    #[test]
    fn preserve_perms_only_when_asked() {
        let d = tmpdir("perm");
        let src = d.join("ro.txt");
        fs::write(&src, b"x").unwrap();
        let mut perm = fs::metadata(&src).unwrap().permissions();
        perm.set_readonly(true);
        fs::set_permissions(&src, perm).unwrap();

        let plain = d.join("plain.txt");
        copy_file(&src, &plain, &CopyOpts { preserve_perms: false, ..CopyOpts::default() }).unwrap();
        assert!(
            !fs::metadata(&plain).unwrap().permissions().readonly(),
            "未开启 preserve_perms 时目标不应该是只读"
        );

        let kept = d.join("kept.txt");
        copy_file(&src, &kept, &CopyOpts { preserve_perms: true, ..CopyOpts::default() }).unwrap();
        assert!(
            fs::metadata(&kept).unwrap().permissions().readonly(),
            "开启 preserve_perms 后目标应保留只读位"
        );

        // 清位后才能删
        for f in [&src, &plain, &kept] {
            let mut p = fs::metadata(f).unwrap().permissions();
            #[allow(clippy::permissions_set_readonly_false)]
            p.set_readonly(false);
            let _ = fs::set_permissions(f, p);
        }
        fs::remove_dir_all(&d).ok();
    }

    /// mtime 保留开关：`preserve_mtime=false` 时目标取当前时间。
    #[test]
    fn mtime_preserve_switch() {
        let d = tmpdir("mtime");
        let src = d.join("a.txt");
        fs::write(&src, b"x").unwrap();
        // 把源改到 2020-01-02 03:04:05 UTC 附近
        let old = 1_577_934_245u64;
        set_mtime(&src, old).unwrap();

        let keep = d.join("keep.txt");
        copy_file(&src, &keep, &CopyOpts::default()).unwrap();
        assert_eq!(mtime_secs(&keep), Some(old), "默认应保留 mtime");

        let fresh = d.join("fresh.txt");
        copy_file(&src, &fresh, &CopyOpts { preserve_mtime: false, ..CopyOpts::default() }).unwrap();
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let got = mtime_secs(&fresh).expect("目标应有 mtime");
        assert!(got.abs_diff(now) < 120, "未开启时应是当前时间");

        fs::remove_dir_all(&d).ok();
    }
}

#[cfg(test)]
mod copy_filter_tests {
    use super::*;
    use crate::algo::pattern::Pattern;

    fn pat(s: &str) -> Pattern {
        Pattern::new(s, true).unwrap()
    }

    #[test]
    fn copy_tree_include_only_matching_files() {
        let d = std::env::temp_dir().join(format!("wsak_cpf_{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        let src = d.join("src");
        fs::create_dir_all(src.join("sub")).unwrap();
        fs::write(src.join("a.jpg"), b"1").unwrap();
        fs::write(src.join("b.txt"), b"2").unwrap();
        fs::write(src.join("sub/c.jpg"), b"3").unwrap();

        let dst = d.join("out");
        let opts = CopyOpts { include: vec![pat("*.jpg")], ..CopyOpts::default() };
        let (files, _) = copy_tree(&src, &dst, &opts).unwrap();
        assert_eq!(files, 2, "只应复制两个 jpg");
        assert!(dst.join("a.jpg").exists());
        assert!(!dst.join("b.txt").exists());
        assert!(dst.join("sub/c.jpg").exists(), "子目录里的命中文件也要复制");

        fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn copy_tree_exclude_skips_whole_dir() {
        let d = std::env::temp_dir().join(format!("wsak_cpe_{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        let src = d.join("src");
        fs::create_dir_all(src.join("build")).unwrap();
        fs::create_dir_all(src.join("keep")).unwrap();
        fs::write(src.join("build/x.o"), b"1").unwrap();
        fs::write(src.join("keep/y.txt"), b"2").unwrap();

        let dst = d.join("out");
        let opts = CopyOpts { exclude: vec![pat("build")], ..CopyOpts::default() };
        let (files, _) = copy_tree(&src, &dst, &opts).unwrap();
        assert_eq!(files, 1);
        assert!(!dst.join("build").exists(), "被排除的目录不应出现");
        assert!(dst.join("keep/y.txt").exists());

        fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn copy_tree_without_filters_copies_everything() {
        let d = std::env::temp_dir().join(format!("wsak_cpa_{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        let src = d.join("src");
        fs::create_dir_all(src.join("sub")).unwrap();
        fs::write(src.join("a.jpg"), b"1").unwrap();
        fs::write(src.join("sub/b.txt"), b"2").unwrap();

        let dst = d.join("out");
        let (files, bytes) = copy_tree(&src, &dst, &CopyOpts::default()).unwrap();
        assert_eq!((files, bytes), (2, 2));
        assert!(dst.join("sub/b.txt").exists());

        fs::remove_dir_all(&d).ok();
    }
}
