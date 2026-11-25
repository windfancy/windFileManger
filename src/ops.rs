//! 文件操作：新建/重命名/删除到回收站/复制移动/剪贴板互通/系统打开。
//!
//! 铁律（项目记忆）：
//! - `Path::canonicalize` 返回 `\\?\D:\...`，SHFileOperationW 不接受（返回 124），
//!   调回收站前剥前缀；
//! - 多路径列表必须双 NUL 结尾的宽字符。

use std::ffi::c_void;
use std::path::{Path, PathBuf};

use windows_sys::Win32::Foundation::{GlobalFree, HWND};
use windows_sys::Win32::System::Com::{CoInitializeEx, CoTaskMemFree, COINIT_APARTMENTTHREADED};
use windows_sys::Win32::System::DataExchange::{
    CloseClipboard, GetClipboardData, IsClipboardFormatAvailable, OpenClipboard,
    RegisterClipboardFormatW, SetClipboardData, EmptyClipboard,
};
use windows_sys::Win32::System::Memory::{
    GlobalAlloc, GlobalLock, GlobalUnlock, GMEM_MOVEABLE,
};
use windows_sys::Win32::UI::Shell::{
    DragQueryFileW, ILCreateFromPathW, SHOpenFolderAndSelectItems, ShellExecuteW,
    SHFILEOPSTRUCTW, FO_DELETE, FOF_ALLOWUNDO, FOF_SILENT,
    HDROP,
};

/// 剥离 `\\?\` 前缀（SHFileOperationW 不接受 verbatim 路径）
pub fn strip_verbatim(p: &Path) -> String {
    let s = p.to_string_lossy();
    s.strip_prefix(r"\\?\").unwrap_or(&s).to_string()
}

fn to_double_nul(paths: &[PathBuf]) -> Vec<u16> {
    let mut v = Vec::new();
    for p in paths {
        v.extend(strip_verbatim(p).encode_utf16());
        v.push(0);
    }
    v.push(0); // 双 NUL 结尾
    v
}

/// 删除到回收站。让系统弹确认框（不传 FOF_NOCONFIRMATION）。
pub fn delete_to_recycle_bin(paths: &[PathBuf]) -> Result<(), String> {
    if paths.is_empty() {
        return Ok(());
    }
    let from = to_double_nul(paths);
    let mut op: SHFILEOPSTRUCTW = unsafe { std::mem::zeroed() };
    op.hwnd = 0 as HWND;
    op.wFunc = FO_DELETE;
    op.pFrom = from.as_ptr();
    op.fFlags = (FOF_ALLOWUNDO | FOF_SILENT) as u16;
    let code = unsafe { windows_sys::Win32::UI::Shell::SHFileOperationW(&mut op) };
    if code != 0 {
        return Err(format!("删除失败（系统错误码 {code}）"));
    }
    Ok(())
}

/// 系统默认程序打开文件/目录
pub fn open_native(path: &Path) -> Result<(), String> {
    let p = strip_verbatim(path);
    let verb = wide("open");
    let file = wide(&p);
    let r = unsafe {
        ShellExecuteW(0 as HWND, verb.as_ptr(), file.as_ptr(), std::ptr::null(), std::ptr::null(), 5 /*SW_SHOW*/)
    };
    if r as isize <= 32 {
        return Err("打开失败：没有关联的程序或路径不可访问".to_string());
    }
    Ok(())
}

/// 在资源管理器中显示并选中
pub fn reveal_in_explorer(path: &Path) -> Result<(), String> {
    let hr = unsafe { CoInitializeEx(std::ptr::null(), COINIT_APARTMENTTHREADED as u32) };
    let p = strip_verbatim(path);
    let wide_p: Vec<u16> = p.encode_utf16().chain(std::iter::once(0)).collect();
    let pidl = unsafe { ILCreateFromPathW(wide_p.as_ptr()) };
    if pidl.is_null() {
        return Err("无法定位路径".to_string());
    }
    let code = unsafe { SHOpenFolderAndSelectItems(pidl as *const _, 0, std::ptr::null(), 0) };
    unsafe { CoTaskMemFree(pidl as *mut c_void) };
    let _ = hr; // 不显式 CoUninitialize（线程生命周期内保持 COM）
    if code != 0 {
        return Err(format!("资源管理器打开失败（{code}）"));
    }
    Ok(())
}

/// 用系统默认处理器打开 shell: 虚拟路径或 URL（ftp://、http(s)://、dlna://）
pub fn open_shell_uri(uri: &str) -> Result<(), String> {
    let verb = wide("open");
    let file = wide(uri);
    let r = unsafe {
        ShellExecuteW(0 as HWND, verb.as_ptr(), file.as_ptr(), std::ptr::null(), std::ptr::null(), 5 /*SW_SHOW*/)
    };
    if r as isize <= 32 {
        return Err(format!("打开失败：系统没有处理 {uri} 的程序"));
    }
    Ok(())
}

/// 在 Windows 终端（cmd）打开并进入目录
pub fn open_terminal(dir: &Path) -> Result<(), String> {
    let d = strip_verbatim(dir);
    let verb = wide("open");
    let app = wide("cmd.exe");
    let args = wide(&terminal_args(&d));
    let r = unsafe {
        ShellExecuteW(0 as HWND, verb.as_ptr(), app.as_ptr(), args.as_ptr(), std::ptr::null(), 5 /*SW_SHOW*/)
    };
    if r as isize <= 32 {
        return Err("打开终端失败".to_string());
    }
    Ok(())
}

/// cmd 参数构造（独立纯函数便于单测）：/k cd /d "<path>"，引号保护含空格路径
fn terminal_args(dir: &str) -> String {
    format!("/k cd /d \"{}\"", dir)
}

// ---- 剪贴板 ----

const CF_HDROP: u32 = 15;
const CF_UNICODETEXT: u32 = 13;
/// Preferred DropEffect：DROPEFFECT_COPY=1 / DROPEFFECT_MOVE=2
const DROPEFFECT_COPY: u32 = 1;
const DROPEFFECT_MOVE: u32 = 2;

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// 把文件路径写入系统剪贴板（CF_HDROP + Preferred DropEffect）。
/// `cut=true` 时 Explorer 粘贴表现为移动。
pub fn write_file_clipboard(paths: &[PathBuf], cut: bool) -> Result<(), String> {
    if paths.is_empty() {
        return Ok(());
    }
    if unsafe { OpenClipboard(0 as HWND) } == 0 {
        return Err("剪贴板打开失败".to_string());
    }
    let ok = (|| {
        unsafe { EmptyClipboard() };
        // DROPFILES 头（20 字节）+ 双 NUL 结尾的宽字符路径列表
        //   DWORD pFiles=20; POINT pt=(0,0); BOOL fNC=FALSE; BOOL fWide=TRUE
        let mut buf: Vec<u16> = vec![0u16; 10];
        buf[0] = 20; // pFiles 偏移
        buf[8] = 0; // fNC = FALSE
        buf[9] = 1; // fWide = TRUE
        for p in paths {
            buf.extend(strip_verbatim(p).encode_utf16());
            buf.push(0);
        }
        buf.push(0); // 双 NUL
        let bytes_len = buf.len() * 2;
        let h = unsafe { GlobalAlloc(GMEM_MOVEABLE, bytes_len) };
        if h.is_null() {
            return Err("内存分配失败".to_string());
        }
        let dst = unsafe { GlobalLock(h) };
        if dst.is_null() {
            unsafe { GlobalFree(h) };
            return Err("内存锁定失败".to_string());
        }
        unsafe {
            std::ptr::copy_nonoverlapping(buf.as_ptr() as *const u8, dst as *mut u8, bytes_len);
            GlobalUnlock(h);
            if SetClipboardData(CF_HDROP, h).is_null() {
                GlobalFree(h);
                return Err("写入剪贴板失败".to_string());
            }
        }
        // Preferred DropEffect
        let fmt_name = wide("Preferred DropEffect");
        let fmt = unsafe { RegisterClipboardFormatW(fmt_name.as_ptr()) };
        if fmt != 0 {
            let effect = if cut { DROPEFFECT_MOVE } else { DROPEFFECT_COPY };
            let h2 = unsafe { GlobalAlloc(GMEM_MOVEABLE, 4) };
            if !h2.is_null() {
                let dst2 = unsafe { GlobalLock(h2) };
                if !dst2.is_null() {
                    unsafe {
                        std::ptr::copy_nonoverlapping(
                            effect.to_ne_bytes().as_ptr(),
                            dst2 as *mut u8,
                            4,
                        );
                        GlobalUnlock(h2);
                        SetClipboardData(fmt, h2);
                    }
                }
            }
        }
        Ok(())
    })();
    unsafe { CloseClipboard() };
    ok
}

/// 把纯文本写入系统剪贴板（CF_UNICODETEXT）。供「计算哈希」复制结果。
pub fn write_text_clipboard(text: &str) -> Result<(), String> {
    if unsafe { OpenClipboard(0 as HWND) } == 0 {
        return Err("剪贴板打开失败".to_string());
    }
    let ok = (|| {
        unsafe { EmptyClipboard() };
        let wide: Vec<u16> = text.encode_utf16().chain(std::iter::once(0)).collect();
        let bytes_len = wide.len() * 2;
        let h = unsafe { GlobalAlloc(GMEM_MOVEABLE, bytes_len) };
        if h.is_null() {
            return Err("内存分配失败".to_string());
        }
        let dst = unsafe { GlobalLock(h) };
        if dst.is_null() {
            unsafe { GlobalFree(h) };
            return Err("内存锁定失败".to_string());
        }
        unsafe {
            std::ptr::copy_nonoverlapping(wide.as_ptr() as *const u8, dst as *mut u8, bytes_len);
            GlobalUnlock(h);
            if SetClipboardData(CF_UNICODETEXT, h).is_null() {
                GlobalFree(h);
                return Err("写入剪贴板失败".to_string());
            }
        }
        Ok(())
    })();
    unsafe { CloseClipboard() };
    ok
}

#[repr(C)]
#[allow(dead_code)]
struct DropFilesHeader {
    p_files: u32,
    pt_x: i32,
    pt_y: i32,
    f_nc: i32,
    f_wide: i32,
}

/// 读取系统剪贴板里的文件列表（来自 Explorer 或本程序）
pub fn read_file_clipboard() -> Option<Vec<PathBuf>> {
    if unsafe { IsClipboardFormatAvailable(CF_HDROP) } == 0 {
        return None;
    }
    if unsafe { OpenClipboard(0 as HWND) } == 0 {
        return None;
    }
    let out = (|| {
        let h = unsafe { GetClipboardData(CF_HDROP) } as HDROP;
        if h.is_null() {
            return None;
        }
        let n = unsafe { DragQueryFileW(h, u32::MAX, std::ptr::null_mut(), 0) };
        let mut paths = Vec::new();
        for i in 0..n {
            let len = unsafe { DragQueryFileW(h, i, std::ptr::null_mut(), 0) };
            let mut buf = vec![0u16; len as usize + 1];
            unsafe { DragQueryFileW(h, i, buf.as_mut_ptr(), buf.len() as u32) };
            while buf.last() == Some(&0) {
                buf.pop();
            }
            paths.push(PathBuf::from(String::from_utf16_lossy(&buf)));
        }
        Some(paths)
    })();
    unsafe { CloseClipboard() };
    out
}

/// 判断剪贴板是否是移动（读 Preferred DropEffect）
pub fn clipboard_is_cut() -> bool {
    if unsafe { OpenClipboard(0 as HWND) } == 0 {
        return false;
    }
    let fmt_name = wide("Preferred DropEffect");
    let fmt = unsafe { RegisterClipboardFormatW(fmt_name.as_ptr()) };
    let cut = if fmt != 0 && unsafe { IsClipboardFormatAvailable(fmt) } != 0 {
        let h = unsafe { GetClipboardData(fmt) };
        let mut result = false;
        if !h.is_null() {
            let p = unsafe { GlobalLock(h as _) };
            if !p.is_null() {
                let v = unsafe { *(p as *const u32) };
                result = v == DROPEFFECT_MOVE;
                unsafe { GlobalUnlock(h as _) };
            }
        }
        result
    } else {
        false
    };
    unsafe { CloseClipboard() };
    cut
}

// ---- 复制 / 移动（自实现，带唯一名处理）----

pub fn copy_recursive(src: &Path, dst_root: &Path, cancel: &std::sync::atomic::AtomicBool) -> std::io::Result<u64> {
    copy_recursive_at(src, dst_root, cancel, 0)
}

const MAX_COPY_DEPTH: usize = 64;

fn copy_recursive_at(
    src: &Path,
    dst_root: &Path,
    cancel: &std::sync::atomic::AtomicBool,
    depth: usize,
) -> std::io::Result<u64> {
    use std::sync::atomic::Ordering;
    if cancel.load(Ordering::Relaxed) {
        return Err(std::io::Error::other("已取消"));
    }
    if depth > MAX_COPY_DEPTH {
        return Err(std::io::Error::other("目录嵌套过深，已跳过"));
    }
    // 自嵌套防护：目标落在源目录内部会无限递归（实测栈溢出崩溃）
    if src.is_dir() && dst_root.starts_with(src) {
        return Err(std::io::Error::other("不能将文件夹复制到其自身内部"));
    }
    let md = std::fs::metadata(src)?;
    if md.is_dir() {
        std::fs::create_dir_all(dst_root)?;
        let mut total = 0u64;
        // 关键：先快照再复制。read_dir 是惰性迭代器，边读边写会把刚生成的
        // dst 子目录再枚举出来，造成自吞式无限递归。
        let items: Vec<std::fs::DirEntry> = std::fs::read_dir(src)?.collect::<Result<_, _>>()?;
        for e in items {
            let d = dst_root.join(e.file_name());
            total += copy_recursive_at(&e.path(), &d, cancel, depth + 1)?;
        }
        Ok(total)
    } else {
        if let Some(parent) = dst_root.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::copy(src, dst_root)
    }
}

pub fn move_one(src: &Path, dst_root: &Path, cancel: &std::sync::atomic::AtomicBool) -> std::io::Result<u64> {
    let md = std::fs::metadata(src)?;
    // 同卷 rename，跨卷退化为复制+删除
    if std::fs::rename(src, dst_root).is_ok() {
        return Ok(md.len());
    }
    let n = copy_recursive(src, dst_root, cancel)?;
    if md.is_dir() {
        std::fs::remove_dir_all(src)?;
    } else {
        std::fs::remove_file(src)?;
    }
    Ok(n)
}

/// 目标重名时生成 `name (1)` / `name (2)` …
pub fn unique_dst(dst_dir: &Path, name: &str) -> PathBuf {
    let candidate = dst_dir.join(name);
    if !candidate.exists() {
        return candidate;
    }
    let stem = Path::new(name).file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
    let ext = Path::new(name).extension().map(|s| format!(".{}", s.to_string_lossy())).unwrap_or_default();
    let pid = std::process::id();
    for i in 1..1000u32 {
        let cand = dst_dir.join(format!("{stem} ({i}){ext}"));
        if !cand.exists() {
            return cand;
        }
    }
    dst_dir.join(format!("{stem}-{pid}{ext}"))
}

/// 新建文件夹/文件（重名报错）
pub fn create_new(dst_dir: &Path, name: &str, is_dir: bool) -> Result<PathBuf, String> {
    let p = dst_dir.join(name);
    if p.exists() {
        return Err(format!("「{name}」已存在"));
    }
    let r = if is_dir {
        std::fs::create_dir(&p)
    } else {
        std::fs::File::create(&p).map(|_| ())
    };
    r.map_err(|e| format!("创建失败：{e}"))?;
    Ok(p)
}

pub fn rename_item(old: &Path, new_name: &str) -> Result<PathBuf, String> {
    if new_name.is_empty() || new_name.contains(['\\', '/', ':', '*', '?', '"', '<', '>', '|']) {
        return Err("名称包含非法字符".to_string());
    }
    let dst = old.parent().ok_or("无父目录")?.join(new_name);
    if dst.exists() {
        return Err(format!("「{new_name}」已存在"));
    }
    std::fs::rename(old, &dst).map_err(|e| format!("重命名失败：{e}"))?;
    Ok(dst)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_strip_verbatim() {
        assert_eq!(strip_verbatim(Path::new(r"\\?\D:\a\b")), r"D:\a\b");
        assert_eq!(strip_verbatim(Path::new(r"D:\a\b")), r"D:\a\b");
        assert_eq!(strip_verbatim(Path::new(r"\\?\C:\")), r"C:\");
    }

    #[test]
    fn test_terminal_args() {
        assert_eq!(terminal_args(r"D:\a"), "/k cd /d \"D:\\a\"");
        assert_eq!(terminal_args(r"D:\a b\c"), "/k cd /d \"D:\\a b\\c\"");
    }

    #[test]
    fn test_unique_dst() {
        let tmp = std::env::temp_dir().join(format!("wfm_test_{}", std::process::id()));
        std::fs::create_dir_all(&tmp).unwrap();
        let f = tmp.join("a.txt");
        std::fs::write(&f, "x").unwrap();
        let u = unique_dst(&tmp, "a.txt");
        assert_eq!(u.file_name().unwrap().to_string_lossy(), "a (1).txt");
        std::fs::write(&u, "x").unwrap();
        let u2 = unique_dst(&tmp, "a.txt");
        assert_eq!(u2.file_name().unwrap().to_string_lossy(), "a (2).txt");
        std::fs::remove_dir_all(&tmp).unwrap();
    }

    #[test]
    fn test_create_and_rename() {
        let tmp = std::env::temp_dir().join(format!("wfm_test2_{}", std::process::id()));
        std::fs::create_dir_all(&tmp).unwrap();
        let p = create_new(&tmp, "新文件夹", true).unwrap();
        assert!(p.is_dir());
        assert!(create_new(&tmp, "新文件夹", true).is_err());
        let p2 = rename_item(&p, "改名").unwrap();
        assert!(p2.is_dir());
        std::fs::remove_dir_all(&tmp).unwrap();
    }

    #[test]
    fn test_illegal_rename() {
        let tmp = std::env::temp_dir().join(format!("wfm_test3_{}", std::process::id()));
        std::fs::create_dir_all(&tmp).unwrap();
        let f = tmp.join("x.txt");
        std::fs::write(&f, "").unwrap();
        assert!(rename_item(&f, "a<b").is_err());
        std::fs::remove_dir_all(&tmp).unwrap();
    }

    /// 回归：目录粘贴到自身内部必须被拒绝（此前边 read_dir 边写入导致
    /// 自吞式无限递归、栈溢出崩溃，exit 0xC0000005）。
    #[test]
    fn test_copy_into_self_rejected() {
        let tmp = std::env::temp_dir().join(format!("wfm_selfcp_{}", std::process::id()));
        std::fs::create_dir_all(&tmp).unwrap();
        let cancel = std::sync::atomic::AtomicBool::new(false);
        let dst = tmp.join(".agents (1)");
        let r = copy_recursive(&tmp, &dst, &cancel);
        assert!(r.is_err(), "粘贴到自身内部必须报错而不是递归");
        std::fs::remove_dir_all(&tmp).unwrap();
    }

    /// 回归：正常复制后快照枚举不得把目标目录卷进来（源=目标父目录时
    /// dst 恰好落在 src 内部被防线拦截的场景之外，这里是普通子目录复制）。
    #[test]
    fn test_copy_recursive_snapshot() {
        let tmp = std::env::temp_dir().join(format!("wfm_snap_{}", std::process::id()));
        let src = tmp.join("src");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::write(src.join("f.txt"), "hello").unwrap();
        let cancel = std::sync::atomic::AtomicBool::new(false);
        let dst = tmp.join("copy_of_src");
        let n = copy_recursive(&src, &dst, &cancel).unwrap();
        assert_eq!(n, 5);
        assert_eq!(std::fs::read_to_string(dst.join("f.txt")).unwrap(), "hello");
        std::fs::remove_dir_all(&tmp).unwrap();
    }
}
