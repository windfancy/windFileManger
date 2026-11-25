//! 后台任务：目录大小统计（复刻 NanoFile get_dir_stats）与当前目录递归搜索
//! （复刻 search_directory / cancel_all_searches）。
//!
//! 线程模型：std::thread + generation 计数取消 + 事件循环回调。

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// 每 N 毫秒向上层刷一次增量结果
const FLUSH_INTERVAL_MS: u128 = 250;

/// 搜索结果批：(显示名, 全路径)
pub type SearchBatch = Vec<(String, String)>;

// ---- 目录统计 ----

pub struct StatsHandle {
    cancel: Mutex<Arc<AtomicBool>>,
}

impl StatsHandle {
    pub fn new() -> Self {
        Self { cancel: Mutex::new(Arc::new(AtomicBool::new(false))) }
    }

    /// 启动一轮统计。`on_result(path, size)` 在 UI 线程回调。
    /// 新一轮启动会自动取消上一轮。
    pub fn start<F>(&self, dirs: Vec<PathBuf>, on_result: F) -> bool
    where
        F: Fn(String, i64) + Send + 'static,
    {
        let dirs: Vec<PathBuf> = dirs.into_iter().take(512).collect();
        if dirs.is_empty() {
            return false;
        }
        // 换入新的取消标志并取消上一轮
        let cancel_local = Arc::new(AtomicBool::new(false));
        {
            let mut guard = self.cancel.lock().unwrap();
            guard.store(true, Ordering::SeqCst);
            *guard = Arc::clone(&cancel_local);
        }
        let cancel_watch = Arc::clone(&cancel_local);
        std::thread::spawn(move || {
            let mut pending: Vec<(String, i64)> = Vec::new();
            let mut last_flush = Instant::now();
            for d in dirs {
                if cancel_watch.load(Ordering::Relaxed) {
                    break;
                }
                let size = dir_size(&d, &cancel_watch);
                pending.push((d.to_string_lossy().into_owned(), size as i64));
                if last_flush.elapsed() >= Duration::from_millis(FLUSH_INTERVAL_MS as u64) {
                    for (p, s) in pending.drain(..) {
                        on_result(p, s);
                    }
                    last_flush = Instant::now();
                }
            }
            for (p, s) in pending.drain(..) {
                on_result(p, s);
            }
        });
        true
    }

    pub fn cancel_current(&self) {
        self.cancel.lock().unwrap().store(true, Ordering::SeqCst);
    }
}

/// 递归求目录大小；`cancel` 置位时提前放弃（返回当前累计值）。
/// 跳过 junction/symlink（reparse point），否则
/// `AppData\Local\Application Data` 这类自引用 junction 会无限递归 → 栈溢出。
/// 深度超过 MAX_WALK_DEPTH 停止（磁盘上真实存在近千层深树，2MB 线程栈扛不住）。
fn dir_size(dir: &Path, cancel: &AtomicBool) -> u64 {
    dir_size_at(dir, cancel, 0)
}

const MAX_WALK_DEPTH: usize = 64;

fn dir_size_at(dir: &Path, cancel: &AtomicBool, depth: usize) -> u64 {
    if depth > MAX_WALK_DEPTH {
        return 0;
    }
    let mut total = 0u64;
    let Ok(rd) = std::fs::read_dir(dir) else { return 0 };
    for e in rd.flatten() {
        if cancel.load(Ordering::Relaxed) {
            return total;
        }
        // 只用 DirEntry::file_type()/metadata()（零 stat），避免 Windows 逐文件 stat 的 100µs 坑
        let Ok(ft) = e.file_type() else { continue };
        if ft.is_symlink() {
            continue; // junction/符号链接不跟进
        }
        let Ok(md) = e.metadata() else { continue };
        if md.is_dir() {
            total += dir_size_at(&e.path(), cancel, depth + 1);
        } else {
            total = total.saturating_add(md.len());
        }
    }
    total
}

// ---- 递归搜索 ----

pub struct SearchHandle {
    cancel: Arc<AtomicBool>,
}

pub const MAX_SEARCH_RESULTS: usize = 5000;

/// 启动递归搜索（当前目录整棵子树，名称包含匹配，大小写不敏感）。
/// `on_batch(names_with_paths)` 与 `on_done(cancelled)` 在 UI 线程回调。
pub fn start_search<F, D>(
    root: &str,
    pattern: String,
    on_batch: F,
    on_done: D,
) -> SearchHandle
where
    F: Fn(Vec<(String, String)>) + Send + 'static, // (显示名, 全路径)
    D: Fn(bool) + Send + 'static,
{
    let cancel = Arc::new(AtomicBool::new(false));
    let cancel_local = Arc::clone(&cancel);
    let root = root.to_string();
    std::thread::spawn(move || {
        let mut found: Vec<(String, String)> = Vec::new();
        let mut last_flush = Instant::now();
        let pat = pattern.to_lowercase();
        walk(&root, &pat, &mut found, &mut last_flush, &cancel_local, &mut |batch| {
            on_batch(std::mem::take(batch));
        }, 0);
        // 兜底：不足一个刷屏周期的尾部结果也要送出
        if !found.is_empty() {
            on_batch(std::mem::take(&mut found));
        }
        on_done(cancel_local.load(Ordering::Relaxed));
    });
    SearchHandle { cancel }
}

impl SearchHandle {
    pub fn cancel(&self) {
        self.cancel.store(true, Ordering::Relaxed);
    }
}

#[allow(clippy::too_many_arguments)]
fn walk(
    dir: &str,
    pat: &str,
    found: &mut SearchBatch,
    last_flush: &mut Instant,
    cancel: &AtomicBool,
    flush: &mut dyn FnMut(&mut SearchBatch),
    depth: usize,
) {
    if cancel.load(Ordering::Relaxed) || found.len() >= MAX_SEARCH_RESULTS || depth > MAX_WALK_DEPTH {
        return;
    }
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    let mut subdirs = Vec::new();
    for e in rd.flatten() {
        if cancel.load(Ordering::Relaxed) || found.len() >= MAX_SEARCH_RESULTS {
            return;
        }
        // junction/symlink 不跟进（防自引用环）
        if e.file_type().is_ok_and(|ft| ft.is_symlink()) {
            continue;
        }
        let name = e.file_name().to_string_lossy().into_owned();
        let path = e.path();
        let Ok(ft) = e.file_type() else { continue };
        if ft.is_dir() {
            if name.to_lowercase().contains(pat) {
                found.push((name.clone(), path.to_string_lossy().into_owned()));
                if last_flush.elapsed() >= Duration::from_millis(FLUSH_INTERVAL_MS as u64) {
                    flush(found);
                    *last_flush = Instant::now();
                }
            }
            subdirs.push((name, path.to_string_lossy().into_owned()));
        } else if name.to_lowercase().contains(pat) {
            found.push((name, path.to_string_lossy().into_owned()));
            if last_flush.elapsed() >= Duration::from_millis(FLUSH_INTERVAL_MS as u64) {
                flush(found);
                *last_flush = Instant::now();
            }
        }
    }
    // 目录后序（先收集当层文件，再深入子目录，浅层结果优先出现）
    for (_name, p) in subdirs {
        if cancel.load(Ordering::Relaxed) || found.len() >= MAX_SEARCH_RESULTS {
            return;
        }
        walk(&p, pat, found, last_flush, cancel, flush, depth + 1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_dir_size_junction_cycle() {
        // junction 自引用不能导致栈溢出（回归：曾让进程静默死亡）
        let tmp = std::env::temp_dir().join(format!("wfm_cycle_{}", std::process::id()));
        let deep = tmp.join("deep");
        std::fs::create_dir_all(&deep).unwrap();
        std::fs::write(deep.join("f.txt"), vec![0u8; 10]).unwrap();
        let jc = tmp.join("cycle");
        let st = std::process::Command::new("cmd")
            .args(["/C", "mklink", "/J"])
            .arg(&jc)
            .arg(&deep)
            .status()
            .expect("mklink failed");
        assert!(st.success());
        let c = AtomicBool::new(false);
        // junction 被跳过，只统计真实文件；关键是必须终止
        let n = dir_size(&tmp, &c);
        assert_eq!(n, 10);
        let _ = std::fs::remove_dir(&jc);
        std::fs::remove_dir_all(&tmp).unwrap();
    }

    /// 回归：近千层真实深树不能爆栈（粘贴自嵌套事故曾造出 996 层，
    /// stats 线程 dir_size 递归耗尽 2MB 栈，进程 0xC0000005 死亡）。
    #[test]
    fn test_dir_size_deep_tree() {
        let tmp = std::env::temp_dir().join(format!("wfm_deep_{}", std::process::id()));
        let mut p = tmp.clone();
        for _ in 0..200 {
            p = p.join("x");
        }
        std::fs::create_dir_all(&p).unwrap();
        std::fs::write(p.join("f.txt"), vec![0u8; 7]).unwrap();
        let c = AtomicBool::new(false);
        let n = dir_size(&tmp, &c); // 深度截断后必须正常返回，不得溢出
        let _ = n;
        std::fs::remove_dir_all(&tmp).unwrap();
    }

    #[test]
    fn test_dir_size_and_search() {
        let tmp = std::env::temp_dir().join(format!("wfm_tasks_{}", std::process::id()));
        let sub = tmp.join("sub");
        std::fs::create_dir_all(&sub).unwrap();
        std::fs::write(tmp.join("hello.txt"), vec![0u8; 100]).unwrap();
        std::fs::write(sub.join("hello2.txt"), vec![0u8; 50]).unwrap();
        let c = AtomicBool::new(false);
        let n = dir_size(&tmp, &c);
        assert_eq!(n, 150);
        // 搜索 "hello"
        let found = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let f2 = Arc::clone(&found);
        let done = std::sync::Arc::new(std::sync::Mutex::new(false));
        let d2 = Arc::clone(&done);
        let h = start_search(
            tmp.to_str().unwrap(),
            "hello".into(),
            move |b| f2.lock().unwrap().extend(b),
            move |_| *d2.lock().unwrap() = true,
        );
        // 等待完成（最多 5s）
        for _ in 0..100 {
            if *done.lock().unwrap() {
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        assert!(*done.lock().unwrap(), "search did not finish");
        let f = found.lock().unwrap();
        assert_eq!(f.len(), 2, "found: {f:?}");
        h.cancel();
        std::fs::remove_dir_all(&tmp).unwrap();
    }
}
