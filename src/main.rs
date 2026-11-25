//! windFileManger 主程序：状态管理、后台线程调度、Slint 回调接线。
//!
//! 线程模型：
//! - 所有状态变更都在 UI 线程；状态本体放 thread_local（Rc<RefCell<AppState>>）；
//! - IO 在 std::thread，只捕获 Send 数据（Weak + 路径 + 结果），
//!   经 `Weak::upgrade_in_event_loop` 回 UI 线程后再从 thread_local 取状态。

// GUI 子系统：启动不弹控制台黑窗
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod config;
mod fs;
mod instant;
mod mft;
mod ops;
mod preview;
mod remote;
mod sakinfe;
mod tasks;

slint::include_modules!();

use slint::{Model, ModelRc, SharedString, VecModel};
use std::cell::RefCell;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::{Arc, Mutex};

const GRID_COLS: usize = 6;

struct Tab {
    id: usize,
    history: Vec<PathBuf>,
    hpos: usize,
    // Tab 级搜索：搜索框与该标签当前路径绑定，切标签各自恢复
    search_query: String,
    search_ext: i32,
    search_active: bool,
    search_hits: Vec<EntryData>,
    search_root: String,
}

impl Tab {
    fn new(id: usize, path: PathBuf) -> Self {
        Self {
            id,
            history: vec![path],
            hpos: 0,
            search_query: String::new(),
            search_ext: 0,
            search_active: false,
            search_hits: Vec::new(),
            search_root: String::new(),
        }
    }
    fn current(&self) -> &PathBuf {
        &self.history[self.hpos]
    }
}

struct AppState {
    entries_model: Rc<VecModel<EntryData>>,
    grid_model: Rc<VecModel<GridRow>>,
    search_model: Rc<VecModel<EntryData>>,
    tabs_model: Rc<VecModel<TabData>>,
    side_model: Rc<VecModel<SideItem>>,
    crumbs_model: Rc<VecModel<Crumb>>,
    tabs: Vec<Tab>,
    next_tab_id: usize,
    active: usize,
    entries: Vec<EntryData>,
    /// 内部剪贴板：(cut, paths)
    clip: Option<(bool, Vec<PathBuf>)>,
    stats: tasks::StatsHandle,
    search: Option<tasks::SearchHandle>,
    load_gen: u64,
    /// 搜索代次：启动搜索/切标签/取消时自增，使飞行中的结果作废（防跨标签串扰）
    search_gen: u64,
    cfg: config::Config,
    /// 目录大小缓存（统计线程写，UI 线程读）
    sizes: Arc<Mutex<HashMap<String, i64>>>,
}

impl AppState {
    fn active_tab(&self) -> &Tab {
        self.tabs.iter().find(|t| t.id == self.active).expect("active tab exists")
    }
    fn active_tab_mut(&mut self) -> &mut Tab {
        self.tabs.iter_mut().find(|t| t.id == self.active).expect("active tab exists")
    }
}

thread_local! {
    static ST: RefCell<Option<Rc<RefCell<AppState>>>> = const { RefCell::new(None) };
}

/// UI 线程内取全局状态（upgrade_in_event_loop 闭包在主线程执行，可用）
fn st_global() -> Rc<RefCell<AppState>> {
    ST.with(|c| Rc::clone(c.borrow().as_ref().expect("app state initialized")))
}

/// 按目标 URL 的身份键（scheme://host:port 或 \\host）匹配配置里的凭证
fn cred_for(url: &str) -> Option<remote::Cred> {
    let stl = st_global();
    let s = stl.borrow();
    let want = remote::host_key(url);
    s.cfg
        .remotes
        .iter()
        .find(|r| !r.user.is_empty() && remote::host_key(&r.url) == want)
        .map(|r| remote::Cred { user: r.user.clone(), pass: r.pass.clone() })
}

/// 当前标签是否为纯远程（非 SMB UNC）目录：SMB 走系统 FS 多数写操作天然可用，
/// FTP/WebDAV/DLNA 只读，写操作统一拦截。
fn guard_local(app: &AppWindow) -> bool {
    let cur = st_global()
        .borrow()
        .active_tab()
        .current()
        .to_string_lossy()
        .into_owned();
    if remote::is_remote(&cur) && !cur.starts_with("\\\\") {
        show_error(app, "远程目录暂只支持浏览，写操作请在本地目录使用".into());
        return false;
    }
    true
}

/// 键名规范化（对齐 Slint 端 hotkey-name 的组装序 Ctrl+Alt+Shift+主键）：
/// - Windows 上 Ctrl(+Shift)+字母的 KeyEvent.text 是控制字符 \u{1}..\u{1A}（= Ctrl+A..Z），
///   Slint 端会组装出 "Ctrl+\u{3}" 这样的键名，换算回小写字母
/// - 字母主键统一小写（捕获 Shift+C 得 "C"、普通按下得 "c"，避免同键两种写法）
/// - 多字符键名（F2 / Delete / Left / Escape ...）原样保留
fn normalize_key(key: &str) -> String {
    let (cap, rest) = if let Some(r) = key.strip_prefix("__cap:") {
        ("__cap:", r)
    } else {
        ("", key)
    };
    let mut ctrl = false;
    let mut alt = false;
    let mut shift = false;
    let mut main = rest.to_string();
    loop {
        if let Some(r) = main.strip_prefix("Ctrl+") {
            ctrl = true;
            main = r.to_string();
        } else if let Some(r) = main.strip_prefix("Alt+") {
            alt = true;
            main = r.to_string();
        } else if let Some(r) = main.strip_prefix("Shift+") {
            shift = true;
            main = r.to_string();
        } else {
            break;
        }
    }
    if main.chars().count() == 1 {
        let c = main.chars().next().unwrap();
        if ('\u{1}'..='\u{1a}').contains(&c) {
            // 控制字符只在 Ctrl 按下时产生（winit/Windows），换算回对应小写字母；
            // Shift 修饰不再单独保留（真实组合总含 Ctrl）
            main = ((c as u8 - 1 + b'a') as char).to_string();
            shift = false;
        } else if c.is_ascii_alphabetic() {
            main = c.to_ascii_lowercase().to_string();
        }
    }
    let mut out = String::from(cap);
    if ctrl {
        out.push_str("Ctrl+");
    }
    if alt {
        out.push_str("Alt+");
    }
    if shift {
        out.push_str("Shift+");
    }
    out.push_str(&main);
    out
}

/// 错误提示（红色 toast，3 秒消失）
fn show_error(app: &AppWindow, msg: String) {
    let l = app.global::<Logic>();
    l.set_toast(msg.into());
    l.set_toast_error(true);
    l.set_has_toast(true);
    // 3 秒后自动消失
    let weak = app.as_weak();
    std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_secs(3));
        let _ = weak.upgrade_in_event_loop(move |w| {
            w.global::<Logic>().set_has_toast(false);
        });
    });
}

/// 切标签前：把当前搜索 UI 状态（输入框文本 / 筛选下拉 / 视图激活 / 结果快照）写回当前标签
fn stash_tab_search(s: &mut AppState, l: &Logic) {
    let active = s.active;
    let query = l.get_search_input().to_string();
    let ext = l.get_search_ext_filter();
    let view_active = l.get_search_active();
    let searching = l.get_searching();
    let root = l.get_search_root().to_string();
    let hits: Vec<EntryData> = (0..s.search_model.row_count())
        .filter_map(|i| s.search_model.row_data(i))
        .collect();
    if let Some(t) = s.tabs.iter_mut().find(|t| t.id == active) {
        t.search_query = query;
        t.search_ext = ext;
        t.search_active = view_active;
        t.search_hits = if searching { Vec::new() } else { hits };
        t.search_root = root;
    }
}

/// 切标签后：恢复目标标签的搜索视图（含状态栏计数与黄条根目录）
fn restore_tab_search(s: &RefCell<AppState>, l: &Logic) {
    let (q, ext, act, hits, root) = {
        let stl = s.borrow();
        let t = stl.active_tab();
        (
            t.search_query.clone(),
            t.search_ext,
            t.search_active,
            t.search_hits.clone(),
            t.search_root.clone(),
        )
    };
    s.borrow().search_model.set_vec(hits);
    l.set_search_input(q.into());
    l.set_search_ext_filter(ext);
    l.set_search_active(act);
    l.set_searching(false);
    l.set_search_root(root.into());
    let (n, sel) = {
        let stl = s.borrow();
        let n = stl.search_model.row_count();
        let sel = (0..n)
            .filter_map(|i| stl.search_model.row_data(i))
            .filter(|e| e.selected)
            .count();
        (n, sel)
    };
    l.set_status_count(n as i32);
    l.set_status_selected(sel as i32);
}

/// 清空目录视图与搜索视图的全部选中态（退出多选模式时调用）
fn clear_all_selections(app: &AppWindow) {    let l = app.global::<Logic>();
    let stl = st_global();
    {
        let mut s = stl.borrow_mut();
        for e in s.entries.iter_mut() {
            e.selected = false;
        }
    }
    {
        let s = stl.borrow();
        for i in 0..s.entries_model.row_count() {
            if let Some(mut row) = s.entries_model.row_data(i) {
                row.selected = false;
                s.entries_model.set_row_data(i, row);
            }
        }
    }
    {
        let sm = stl.borrow().search_model.clone();
        for i in 0..sm.row_count() {
            if let Some(mut row) = sm.row_data(i)
                && row.selected
            {
                row.selected = false;
                sm.set_row_data(i, row);
            }
        }
    }
    l.set_status_selected(0);
}

/// 秒搜结果批量推送（UI 线程调用；与旧搜索共用 search_model，界面零改动）。
/// gen 校验：搜索已被取消/切换标签时代次不匹配，丢弃过期结果。
fn push_hits_to_ui(weak: slint::Weak<AppWindow>, hits: Vec<instant::Hit>, sgen: u64) {
    weak.upgrade_in_event_loop(move |_w| {
        let st = st_global();
        {
            let s = st.borrow();
            if s.search_gen != sgen {
                return;
            }
        }
        let rows: Vec<EntryData> = hits
            .into_iter()
            .map(|h| {
                let ext = if h.dir { String::new() } else { fs::ext_of(&h.name) };
                let category =
                    if h.dir { fs::CAT_FOLDER } else { fs::category_for_ext(&ext) };
                let (size_str, dir_size) = if h.dir {
                    ("--".to_string(), -1.0)
                } else {
                    (fs::format_size(h.size), 0.0)
                };
                EntryData {
                    name: h.name.into(),
                    path: h.path.into(),
                    is_dir: h.dir,
                    size: h.size as f32,
                    dir_size,
                    size_str: size_str.into(),
                    mtime: if h.mtime > 0 {
                        fs::format_mtime(h.mtime).into()
                    } else {
                        String::new().into()
                    },
                    mtime_sort: h.mtime as i32,
                    ext: ext.into(),
                    category,
                    hidden: false,
                    selected: false,
                }
            })
            .collect();
        let l = _w.global::<Logic>();
        // 搜索视图自己的计数（status-count 被目录视图复用，不刷新会让全选框状态失真）
        l.set_status_count(rows.len() as i32);
        l.set_status_selected(0);
        let s = st.borrow();
        for r in rows {
            s.search_model.push(r);
        }
    })
    .ok();
}

/// 秒搜完成标记（消除转圈状态）
fn search_done_ui(weak: slint::Weak<AppWindow>) {
    weak.upgrade_in_event_loop(move |w| {
        w.global::<Logic>().set_searching(false);
    })
    .ok();
}


/// 成功提示（绿色 toast，同 3 秒消失）
fn show_toast(app: &AppWindow, msg: String) {
    let l = app.global::<Logic>();
    l.set_toast(msg.into());
    l.set_toast_error(false);
    l.set_has_toast(true);
    let weak = app.as_weak();
    std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_secs(3));
        let _ = weak.upgrade_in_event_loop(move |w| {
            w.global::<Logic>().set_has_toast(false);
        });
    });
}

/// 强制窗口可缩放/可最大化。
///
/// 背景：Window 显式 width/height + 裸弹性元素（min==max 约束）时，slint-winit
/// 会以 `window_is_resizable(min,max)==false` 创建窗口并剥掉 WS_THICKFRAME /
/// WS_MAXIMIZEBOX，用户无法拖边调整大小、最大化按钮被禁。Slint 布局侧给
/// 弹性元素加显式 min 约束在复杂结构下不可靠，因此启动后直接改回窗口 style。
/// winit 只在布局约束变化时重调 set_resizable，而本应用约束是静态的，一次设置即稳定。
#[cfg(windows)]
fn force_resizable() {
    use windows_sys::Win32::Foundation::HWND;
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        FindWindowW, GetWindowLongW, SetWindowLongW, SetWindowPos, GWL_STYLE, SWP_FRAMECHANGED,
        SWP_NOZORDER, WS_MAXIMIZEBOX, WS_THICKFRAME,
    };
    let title: Vec<u16> = "windFileManger\0".encode_utf16().collect();
    let hwnd = unsafe { FindWindowW(std::ptr::null(), title.as_ptr()) };
    if hwnd == 0 as HWND {
        return;
    }
    unsafe {
        let style = GetWindowLongW(hwnd, GWL_STYLE);
        SetWindowLongW(hwnd, GWL_STYLE, style | (WS_THICKFRAME | WS_MAXIMIZEBOX) as i32);
        // 通知系统重算非客户区
        SetWindowPos(hwnd, 0 as HWND, 0, 0, 0, 0, SWP_FRAMECHANGED | SWP_NOZORDER | 0x0001 | 0x0002);
    }
}

// ---------- 管理员模式（MFT 直读需要 \\.\X: 的 GENERIC_READ） ----------

#[cfg(windows)]
fn is_admin() -> bool {
    use windows_sys::Win32::UI::Shell::IsUserAnAdmin;
    unsafe { IsUserAnAdmin() != 0 }
}

/// 非管理员时弹原生对话框询问提权；用户确认且 UAC 通过 → 以管理员重启自身并返回 true
#[cfg(windows)]
fn relaunch_as_admin_if_requested() -> bool {
    use windows_sys::Win32::UI::Shell::ShellExecuteW;
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        IDYES, MB_ICONQUESTION, MB_SETFOREGROUND, MB_YESNO, SW_SHOWNORMAL, MessageBoxW,
    };
    let exe = match std::env::current_exe() {
        Ok(p) => p,
        Err(_) => return false,
    };
    let exe_w: Vec<u16> = exe
        .as_os_str()
        .to_string_lossy()
        .encode_utf16()
        .chain([0])
        .collect();
    let caption: Vec<u16> = "windFileManger\0".encode_utf16().collect();
    let text: Vec<u16> = "windFileManger 需要管理员权限才能直读 NTFS 主文件表（MFT），建立完整的全盘秒搜索引；普通权限下索引范围受限。\n\n是否以管理员身份重新启动？\0"
        .encode_utf16()
        .collect();
    let ret = unsafe {
        MessageBoxW(
            0 as _,
            text.as_ptr(),
            caption.as_ptr(),
            MB_YESNO | MB_ICONQUESTION | MB_SETFOREGROUND,
        )
    };
    if ret != IDYES {
        return false;
    }
    let verb: Vec<u16> = "runas\0".encode_utf16().collect();
    let r = unsafe {
        ShellExecuteW(
            0 as _,
            verb.as_ptr(),
            exe_w.as_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            SW_SHOWNORMAL as _,
        )
    };
    // ShellExecuteW 成功时返回值 > 32
    (r as isize) > 32
}

// ---------- 主题 ----------

/// 跟随系统：读「应用主题」注册表（AppsUseLightTheme：0 = 深色）
#[cfg(windows)]
fn sys_prefers_dark() -> bool {
    use windows_sys::Win32::System::Registry::{
        HKEY_CURRENT_USER, RRF_RT_REG_DWORD, RegGetValueW,
    };
    let subkey: Vec<u16> =
        "Software\\Microsoft\\Windows\\CurrentVersion\\Themes\\Personalize\0".encode_utf16().collect();
    let value: Vec<u16> = "AppsUseLightTheme\0".encode_utf16().collect();
    let mut data: u32 = 1;
    let mut size = 4u32;
    let r = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            subkey.as_ptr(),
            value.as_ptr(),
            RRF_RT_REG_DWORD,
            std::ptr::null_mut(),
            (&mut data as *mut u32).cast(),
            &mut size,
        )
    };
    r == 0 && data == 0
}

/// 应用色调：theme 0 浅 / 1 深 / 2 跟随系统（换算成 0/1 写入 Theme 与 Palette）
fn apply_theme(app: &AppWindow, theme: i32) {
    let dark = match theme {
        1 => true,
        2 => sys_prefers_dark(),
        _ => false,
    };
    app.global::<Theme>().set_mode(if dark { 1 } else { 0 });
    app.invoke_apply_mode(dark);
}

fn main() {
    let cfg = config::load();
    // 管理员提示（运行前）：MFT 直读需要管理员权限；设置页可关闭提示
    #[cfg(windows)]
    if cfg.admin_prompt && !is_admin() && relaunch_as_admin_if_requested() {
        // 已成功以管理员身份拉起新进程，当前进程退出
        return;
    }
    // 启动目录优先级：命令行参数（显式指定，单标签）> 上次会话恢复 > 用户主目录
    let arg_dir = std::env::args()
        .nth(1)
        .filter(|s| Path::new(s).is_dir())
        .map(PathBuf::from);
    let (tabs_init, active_init): (Vec<PathBuf>, usize) = match arg_dir {
        Some(d) => (vec![d], 0),
        None => cfg
            .restored_tabs()
            .map(|(dirs, active)| (dirs.into_iter().map(PathBuf::from).collect(), active))
            .unwrap_or_else(|| {
                let home = std::env::var("USERPROFILE")
                    .ok()
                    .map(PathBuf::from)
                    .unwrap_or_else(|| PathBuf::from("C:\\"));
                (vec![home], 0)
            }),
    };

    let app = AppWindow::new().expect("slint window create failed");

    // 秒搜：全盘索引后台构建/加载（MFT 直读或文件夹枚举；就绪前搜索降级旧递归）
    instant::startup(&cfg.index_drives);

    let st = Rc::new(RefCell::new(AppState {
        entries_model: Rc::new(VecModel::default()),
        grid_model: Rc::new(VecModel::default()),
        search_model: Rc::new(VecModel::default()),
        tabs_model: Rc::new(VecModel::default()),
        side_model: Rc::new(VecModel::default()),
        crumbs_model: Rc::new(VecModel::default()),
        tabs: tabs_init.iter().enumerate().map(|(i, p)| Tab::new(i + 1, p.clone())).collect(),
        next_tab_id: tabs_init.len() + 1,
        active: active_init + 1,
        entries: Vec::new(),
        clip: None,
        stats: tasks::StatsHandle::new(),
        search: None,
        load_gen: 0,
        search_gen: 0,
        cfg,
        sizes: Arc::new(Mutex::new(HashMap::new())),
    }));
    ST.with(|c| *c.borrow_mut() = Some(Rc::clone(&st)));

    let logic = app.global::<Logic>();
    logic.set_entries(ModelRc::from(Rc::clone(&st.borrow().entries_model)));
    logic.set_grid_rows(ModelRc::from(Rc::clone(&st.borrow().grid_model)));
    logic.set_search_results(ModelRc::from(Rc::clone(&st.borrow().search_model)));
    logic.set_tabs(ModelRc::from(Rc::clone(&st.borrow().tabs_model)));
    logic.set_side_items(ModelRc::from(Rc::clone(&st.borrow().side_model)));
    logic.set_crumbs(ModelRc::from(Rc::clone(&st.borrow().crumbs_model)));
    let (sort_by, sort_asc, grid_view, show_hidden) = {
        let s = st.borrow();
        (s.cfg.sort_by, s.cfg.sort_asc, s.cfg.grid_view, s.cfg.show_hidden)
    };
    logic.set_sort_by(sort_by);
    logic.set_sort_asc(sort_asc);
    logic.set_grid_view(grid_view);
    logic.set_show_hidden(show_hidden);
    logic.set_preview_open(false);
    logic.set_dialog_mode(0);

    // 主题与设置页初始状态
    apply_theme(&app, st.borrow().cfg.theme);
    {
        let s = st.borrow();
        logic.set_settings_theme(s.cfg.theme);
        logic.set_settings_admin(s.cfg.admin_prompt);
        logic.set_hk_focus_search(
            s.cfg.hotkeys.get("focus-search").cloned().unwrap_or_default().into(),
        );
    }
    rebuild_hotkey_rows(&app);
    rebuild_ext_rows(&app);

    // ---------- 构建辅助 ----------

    fn make_entry(r: fs::RawEntry, dir_size: f32) -> EntryData {
        let size_str = if r.is_dir {
            if dir_size < 0.0 {
                SharedString::from("--")
            } else {
                SharedString::from(fs::format_size(dir_size as u64))
            }
        } else {
            SharedString::from(fs::format_size(r.size as u64))
        };
        EntryData {
            name: r.name.into(),
            path: r.path.into(),
            is_dir: r.is_dir,
            size: r.size as f32,
            dir_size,
            size_str,
            mtime: fs::format_mtime(r.mtime).into(),
            mtime_sort: r.mtime as i32,
            ext: r.ext.into(),
            category: r.category,
            hidden: r.hidden,
            selected: false,
        }
    }

    fn status_size_str(entries: &[EntryData]) -> SharedString {
        let mut total = 0u64;
        let mut any = false;
        for e in entries {
            if !e.selected {
                continue;
            }
            any = true;
            if e.is_dir {
                if e.dir_size >= 0.0 {
                    total += e.dir_size as u64;
                }
            } else {
                total += e.size as u64;
            }
        }
        if any {
            SharedString::from(format!("· 合计 {}", fs::format_size(total)))
        } else {
            SharedString::new()
        }
    }

    fn rebuild_grid(st: &RefCell<AppState>) {
        let grid: Vec<GridRow> = st
            .borrow()
            .entries
            .chunks(GRID_COLS)
            .map(|c| GridRow { items: ModelRc::new(VecModel::from(c.to_vec())) })
            .collect();
        st.borrow().grid_model.set_vec(grid);
    }

    fn tab_title(p: &std::path::Path) -> SharedString {
        let s = p.to_string_lossy();
        if remote::is_remote(&s) {
            return remote::display_label(&s).into();
        }
        match p.file_name() {
            Some(n) => n.to_string_lossy().into_owned().into(),
            None => s.into_owned().into(),
        }
    }

    fn rebuild_tabs(st: &RefCell<AppState>, app: &AppWindow) {
        let active = st.borrow().active;
        let rows: Vec<TabData> = st
            .borrow()
            .tabs
            .iter()
            .map(|t| TabData {
                id: t.id as i32,
                title: tab_title(t.current()),
                active: t.id == active,
            })
            .collect();
        st.borrow().tabs_model.set_vec(rows);
        let path = st.borrow().active_tab().current().to_string_lossy().into_owned();
        app.global::<Logic>().set_current_path(path.into());
    }

    fn rebuild_side(st: &RefCell<AppState>) {
        let mut rows: Vec<SideItem> = Vec::new();
        rows.push(SideItem { kind: 3, label: "快速访问".into(), path: "".into(), free: 0.0, total: 0.0 });
        for (label, path) in fs::quick_access() {
            rows.push(SideItem { kind: 0, label: label.into(), path: path.into(), free: 0.0, total: 0.0 });
        }
        // shell 虚拟位置：经系统资源管理器打开
        rows.push(SideItem { kind: 0, label: "应用程序".into(), path: "shell:AppsFolder".into(), free: 0.0, total: 0.0 });
        rows.push(SideItem { kind: 0, label: "回收站".into(), path: "shell:RecycleBinFolder".into(), free: 0.0, total: 0.0 });
        rows.push(SideItem { kind: 3, label: "此电脑".into(), path: "".into(), free: 0.0, total: 0.0 });
        for d in fs::list_drives() {
            rows.push(SideItem {
                kind: 1,
                label: format!("{} · 剩余 {}", d.label, fs::format_size(d.free)).into(),
                path: d.path.clone().into(),
                free: d.free as f32,
                total: d.total as f32,
            });
        }
        rows.push(SideItem { kind: 3, label: "收藏夹".into(), path: "".into(), free: 0.0, total: 0.0 });
        for f in st.borrow().cfg.favorites.iter() {
            let label = std::path::Path::new(f)
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| f.clone());
            rows.push(SideItem { kind: 2, label: label.into(), path: f.clone().into(), free: 0.0, total: 0.0 });
        }
        rows.push(SideItem { kind: 3, label: "远程访问".into(), path: "".into(), free: 0.0, total: 0.0 });
        for r in st.borrow().cfg.remotes.iter() {
            // kind 必须 < 3 才会渲染为可点击的 SideRow（>=3 是纯文本段头，无 TouchArea）
            rows.push(SideItem {
                kind: 0,
                label: SharedString::from(r.name.as_str()),
                path: SharedString::from(r.url.as_str()),
                free: 0.0,
                total: 0.0,
            });
        }
        st.borrow().side_model.set_vec(rows);
    }

    fn rebuild_crumbs(st: &RefCell<AppState>, app: &AppWindow) {
        let path = st.borrow().active_tab().current().to_string_lossy().into_owned();
        let parts: Vec<(String, String)> = if remote::is_remote(&path) {
            remote::crumbs_for(&path)
        } else {
            fs::crumbs_for(&path)
        };
        let rows: Vec<Crumb> = parts
            .into_iter()
            .map(|(label, p)| Crumb { label: label.into(), path: p.into() })
            .collect();
        st.borrow().crumbs_model.set_vec(rows);
        app.global::<Logic>().set_current_path(path.into());
    }

    /// 把缓存中的目录大小回填进条目列表
    fn apply_size_cache(entries: &mut [EntryData], cache: &Mutex<HashMap<String, i64>>) {
        let map = cache.lock().unwrap();
        for e in entries.iter_mut().filter(|e| e.is_dir) {
            if let Some(&v) = map.get(e.path.to_string().as_str()) {
                e.dir_size = v as f32;
                e.size_str = SharedString::from(fs::format_size(v as u64));
            }
        }
    }

    fn start_stats(st: &RefCell<AppState>, app: &AppWindow) {
        let (dirs, sizes) = {
            let s = st.borrow();
            (
                s.entries
                    .iter()
                    .filter(|e| e.is_dir)
                    .map(|e| PathBuf::from(e.path.to_string()))
                    .collect::<Vec<_>>(),
                Arc::clone(&s.sizes),
            )
        };
        if dirs.is_empty() {
            return;
        }
        let weak = app.as_weak();
        st.borrow().stats.start(dirs, move |path, size| {
            sizes.lock().unwrap().insert(path.clone(), size);
            let weak = weak.clone();
            let path_model: SharedString = path.into();
            weak.upgrade_in_event_loop(move |w| {
                let logic = w.global::<Logic>();
                let model = logic.get_entries();
                let n = model.row_count();
                for i in 0..n {
                    if let Some(e) = model
                        .row_data(i)
                        .filter(|e| e.path == path_model)
                    {
                        let mut ne = e.clone();
                        ne.dir_size = size as f32;
                        ne.size_str = SharedString::from(fs::format_size(size as u64));
                        model.set_row_data(i, ne);
                        break;
                    }
                }
            })
            .ok();
        });
    }

    /// 异步列举当前标签目录（后台线程 + UI 线程落地）。
    /// `clear_search`：导航/刷新时 true（搜索结果过期）；切标签恢复视图时 false。
    fn list_current(app: &AppWindow, clear_search: bool) {
        let (path, list_gen, show_hidden, sort_by, sort_asc) = {
            let stl = st_global();
            let mut s = stl.borrow_mut();
            s.load_gen += 1;
            if clear_search {
                s.search_gen += 1; // 使飞行中的搜索结果作废
                let t = s.active_tab_mut();
                t.search_active = false;
                t.search_hits.clear();
            }
            s.stats.cancel_current();
            let l = app.global::<Logic>();
            (
                s.active_tab().current().clone(),
                s.load_gen,
                l.get_show_hidden(),
                l.get_sort_by(),
                l.get_sort_asc(),
            )
        };
        if clear_search {
            let l = app.global::<Logic>();
            l.set_search_active(false);
            l.set_searching(false);
            l.set_search_input("".into());
            l.set_search_ext_filter(0);
            st_global().borrow().search_model.set_vec(Vec::new());
        }
        let path_str = path.to_string_lossy().into_owned();
        let path_remote = remote::is_remote(&path_str);
        let cred = if path_remote { cred_for(&path_str) } else { None };
        let weak = app.as_weak();
        std::thread::spawn(move || {
            let res: Result<Vec<fs::RawEntry>, String> = if path_remote {
                remote::list(&path_str, cred.as_ref())
            } else {
                fs::list_dir(&path, show_hidden).map_err(|e| e.to_string())
            };
            let _ = weak.upgrade_in_event_loop(move |w| {
                let st = st_global();
                {
                    let s = st.borrow();
                    if s.load_gen != list_gen {
                        return; // 已跳走，丢弃过期结果
                    }
                }
                let raw = match res {
                    Ok(v) => v,
                    Err(e) => {
                        let l = w.global::<Logic>();
                        l.set_toast(format!("无法读取目录：{e}").into());
                        l.set_has_toast(true);
                        return;
                    }
                };
                let mut raw = raw;
                fs::sort_entries(&mut raw, sort_by, sort_asc);
                let mut entries: Vec<EntryData> = raw.into_iter().map(|r| make_entry(r, -1.0)).collect();
                if !path_remote {
                    apply_size_cache(&mut entries, &st.borrow().sizes);
                }
                let n = entries.len();
                {
                    let mut s = st.borrow_mut();
                    s.entries = entries;
                    s.entries_model.set_vec(s.entries.clone());
                }
                let l = w.global::<Logic>();
                l.set_status_count(n as i32);
                l.set_status_selected(0);
                l.set_status_size(SharedString::new());
                rebuild_grid(&st);
                rebuild_crumbs(&st, &w);
                rebuild_side(&st);
                rebuild_tabs(&st, &w);
                if !path_remote {
                    start_stats(&st, &w);
                }
            });
        });
    }

    fn navigate(app: &AppWindow, path: PathBuf) {
        {
            let stl = st_global();
            let mut s = stl.borrow_mut();
            s.search = None;
            let t = s.active_tab_mut();
            t.history.truncate(t.hpos + 1);
            t.history.push(path.clone());
            t.hpos = t.history.len() - 1;
        }
        list_current(app, true);
    }

    fn selected_paths() -> Vec<PathBuf> {
        let stl = st_global();
        let s = stl.borrow();
        if s.active_tab().search_active {
            // 搜索视图：选中态存于 search_model 行
            let m = &s.search_model;
            (0..m.row_count())
                .filter_map(|i| m.row_data(i))
                .filter(|e| e.selected)
                .map(|e| PathBuf::from(e.path.to_string()))
                .collect()
        } else {
            s.entries
                .iter()
                .filter(|e| e.selected)
                .map(|e| PathBuf::from(e.path.to_string()))
                .collect()
        }
    }

    /// 打开批量重命名对话框（工具栏按钮与右键菜单共用入口）。
    fn open_batch_rename(app: &AppWindow) {
        let sel = selected_paths();
        // 集成测试辅助：WFM_BATCH_MIN=1 放宽多选门槛（UI 自动化的 SendInput-Ctrl
        // 无法被 winit 稳定跟踪，真实键盘不受影响）；默认 2，正常使用行为不变。
        let min_sel: usize = std::env::var("WFM_BATCH_MIN")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(2);
        if sel.len() < min_sel {
            show_error(app, "请先按住 Ctrl 多选至少 2 个条目（单条请用「重命名」）".into());
            return;
        }
        let l = app.global::<Logic>();
        l.set_dialog_input("%3d".into());
        l.set_batch_start_input("1".into());
        l.set_batch_title(format!("批量重命名 — {} 个条目", sel.len()).into());
        l.set_batch_error("".into());
        l.set_batch_has_conflict(false);
        l.set_dialog_mode(5);
        run_batch_preview(app, "%3d", "1");
    }

    /// 批量重命名实时预览：渲染选中集合 → 填充 batch-rows / batch-error。
    fn run_batch_preview(app: &AppWindow, tpl: &str, start_str: &str) {        let l = app.global::<Logic>();
        let start = match start_str.trim().parse::<u64>() {
            Ok(v) => {
                if l.get_batch_error() == "起始序号无效，按 1 处理" {
                    l.set_batch_error("".into());
                }
                v
            }
            Err(_) => {
                l.set_batch_error("起始序号无效，按 1 处理".into());
                1
            }
        };
        let sel = selected_paths();
        let names: std::collections::HashSet<String> = st_global()
            .borrow()
            .entries
            .iter()
            .map(|e| e.name.to_string())
            .collect();
        match sakinfe::render_batch(&sel, tpl, start, &names) {
            Ok(rows) => {
                let has_bad = rows.iter().any(|r| r.conflict || r.error.is_some());
                let model: Vec<BatchRow> = rows
                    .into_iter()
                    .map(|r| BatchRow {
                        old: r.old.into(),
                        new: r.new.into(),
                        conflict: r.conflict,
                        unchanged: r.unchanged,
                        error: r.error.unwrap_or_default().into(),
                    })
                    .collect();
                let vm = Rc::new(VecModel::from(model));
                l.set_batch_rows(ModelRc::from(vm));
                l.set_batch_has_conflict(has_bad);
            }
            Err(e) => {
                l.set_batch_error(e.into());
                l.set_batch_rows(ModelRc::from(Rc::new(VecModel::<BatchRow>::from(Vec::new()))));
                l.set_batch_has_conflict(true);
            }
        }
    }

    /// 粘贴逻辑（回调与菜单共用）
    fn invoke_paste(app: &AppWindow) {
        let (cut, sources) = {
            let internal = st_global().borrow().clip.clone();
            match internal {
                Some((c, p)) => (c, p),
                None => match ops::read_file_clipboard() {
                    Some(p) if !p.is_empty() => (ops::clipboard_is_cut(), p),
                    _ => {
                        show_error(app, "剪贴板中没有文件".to_string());
                        return;
                    }
                },
            }
        };
        let dst_dir = st_global().borrow().active_tab().current().clone();
        app.global::<Logic>().set_busy(true);
        let weak = app.as_weak();
        std::thread::spawn(move || {
            let cancel = std::sync::atomic::AtomicBool::new(false);
            let mut first_err: Option<String> = None;
            for src in &sources {
                let name = src
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default();
                if name.is_empty() {
                    continue;
                }
                // 自嵌套防护：目录粘贴到自身内部会无限递归（实测栈溢出崩溃）
                if src.is_dir() && dst_dir.starts_with(src) {
                    first_err = Some("不能将文件夹粘贴到其自身内部".to_string());
                    break;
                }
                let dst = ops::unique_dst(&dst_dir, &name);
                let r = if cut {
                    ops::move_one(src, &dst, &cancel)
                } else {
                    ops::copy_recursive(src, &dst, &cancel)
                };
                if let Err(e) = r {
                    first_err = Some(format!("粘贴「{name}」失败：{e}"));
                    break;
                }
            }
            let _ = weak.upgrade_in_event_loop(move |w| {
                w.global::<Logic>().set_busy(false);
                if let Some(msg) = first_err {
                    show_error(&w, msg);
                }
                list_current(&w, true);
            });
        });
    }

    /// 在后台打开系统默认程序
    fn open_with_system(app: &AppWindow, path: PathBuf) {
        app.global::<Logic>().set_busy(true);
        let weak = app.as_weak();
        std::thread::spawn(move || {
            let r = ops::open_native(&path);
            let _ = weak.upgrade_in_event_loop(move |w| {
                w.global::<Logic>().set_busy(false);
                if let Err(msg) = r {
                    show_error(&w, msg);
                }
            });
        });
    }

    /// 压缩一组路径为一个归档包（ZIP / tar.gz），后台执行；单条用原名、多条用时间戳
    fn compress_paths(app: &AppWindow, zip: bool, paths: Vec<PathBuf>, dst_dir: PathBuf) {
        let default_name = if paths.len() == 1 {
            let stem = paths[0]
                .file_stem()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| "archive".into());
            format!("{stem}.{}", if zip { "zip" } else { "tar.gz" })
        } else {
            chrono::Local::now().format("%Y%m%d_%H%M%S").to_string()
                + if zip { ".zip" } else { ".tar.gz" }
        };
        let out = ops::unique_dst(&dst_dir, &default_name);
        let weak = app.as_weak();
        app.global::<Logic>().set_busy(true);
        std::thread::spawn(move || {
            let r = if zip {
                sakinfe::pack_zip(&paths, &out)
            } else {
                sakinfe::pack_targz(&paths, &out)
            };
            let _ = weak.upgrade_in_event_loop(move |w| {
                w.global::<Logic>().set_busy(false);
                match r {
                    Ok((n, size)) => {
                        show_toast(
                            &w,
                            format!("已打包 {n} 项（{}）→ {}", fs::format_size(size), out.display()),
                        );
                        list_current(&w, true);
                    }
                    Err(e) => show_error(&w, format!("打包失败：{e}")),
                }
            });
        });
    }

    /// 删除到回收站（后台）
    fn delete_paths(app: &AppWindow, paths: Vec<PathBuf>) {
        if paths.is_empty() {
            return;
        }
        app.global::<Logic>().set_busy(true);
        let weak = app.as_weak();
        std::thread::spawn(move || {
            let r = ops::delete_to_recycle_bin(&paths);
            let _ = weak.upgrade_in_event_loop(move |w| {
                w.global::<Logic>().set_busy(false);
                if let Err(msg) = r {
                    show_error(&w, msg);
                }
                list_current(&w, true);
            });
        });
    }

    /// 复制/剪切（内部 + 系统剪贴板）
    fn clip_copy(app: &AppWindow, paths: Vec<PathBuf>, cut: bool) {
        if paths.is_empty() {
            return;
        }
        st_global().borrow_mut().clip = Some((cut, paths.clone()));
        app.global::<Logic>().set_busy(true);
        let weak = app.as_weak();
        std::thread::spawn(move || {
            let r = ops::write_file_clipboard(&paths, cut);
            let _ = weak.upgrade_in_event_loop(move |w| {
                w.global::<Logic>().set_busy(false);
                if let Err(msg) = r {
                    show_error(&w, msg);
                }
            });
        });
    }

    /// 预览更新（IO 在后台线程）
    fn update_preview(app: &AppWindow, e: &EntryData) {
        if !app.global::<Logic>().get_preview_open() {
            return;
        }
        let path = e.path.to_string();
        let is_dir = e.is_dir;
        let size = e.size as i64;
        let category = e.category;
        let name = e.name.to_string();
        let weak = app.as_weak();
        std::thread::spawn(move || {
            let d = preview::load(&path, is_dir, size, 0, category);
            let meta = format!(
                "名称：{name}\n类型：{}\n大小：{}\n路径：{path}",
                if is_dir {
                    "文件夹".to_string()
                } else {
                    fs::category_label(category).to_string()
                },
                if is_dir { "--".to_string() } else { fs::format_size(size as u64) },
            );
            let kind = d.kind;
            let text = d.text;
            let _ = weak.upgrade_in_event_loop(move |w| {
                let l = w.global::<Logic>();
                l.set_preview_kind(kind);
                l.set_preview_text(text.into());
                l.set_preview_meta(meta.into());
                if kind == 2 {
                    // slint::Image 非 Send，只能在 UI 线程加载
                    if let Ok(img) = slint::Image::load_from_path(std::path::Path::new(&path)) {
                        l.set_preview_image(img);
                    } else {
                        l.set_preview_kind(0);
                    }
                }
            });
        });
    }

    // ---------- 回调接线（均运行于 UI 线程） ----------

    // 导航
    {
        let app2 = app.as_weak();
        logic.on_nav_back(move || {
            let app = app2.unwrap();
            let moved = {
                let stl = st_global();
            let mut s = stl.borrow_mut();
                let t = s.active_tab_mut();
                if t.hpos > 0 {
                    t.hpos -= 1;
                    true
                } else {
                    false
                }
            };
            if moved {
                list_current(&app, true);
            }
        });
    }
    {
        let app2 = app.as_weak();
        logic.on_nav_forward(move || {
            let app = app2.unwrap();
            let moved = {
                let stl = st_global();
            let mut s = stl.borrow_mut();
                let t = s.active_tab_mut();
                if t.hpos + 1 < t.history.len() {
                    t.hpos += 1;
                    true
                } else {
                    false
                }
            };
            if moved {
                list_current(&app, true);
            }
        });
    }
    {
        let app2 = app.as_weak();
        logic.on_nav_up(move || {
            eprintln!("[DBG] nav-up fired, cur={}", st_global().borrow().active_tab().current().to_string_lossy());
            let app = app2.unwrap();
            let cur = st_global()
                .borrow()
                .active_tab()
                .current()
                .to_string_lossy()
                .into_owned();
            if remote::is_remote(&cur) {
                if cur.starts_with("dlna://") {
                    // DLNA 父容器需要 BrowseMetadata 网络查询，后台线程算好再跳
                    let weak = app.as_weak();
                    std::thread::spawn(move || {
                        let parent = remote::dlna_parent(&cur);
                        let _ = weak.upgrade_in_event_loop(move |w| {
                            if let Some(p) = parent {
                                navigate(&w, PathBuf::from(p));
                            }
                        });
                    });
                } else if let Some(p) = remote::parent_url(&cur) {
                    navigate(&app, PathBuf::from(p));
                }
                return;
            }
            // 注意：不能写成 `if let Some(p) = st_global().borrow()....parent()... { navigate(...) }`
            // —— if let 的 scrutinee 临时 Ref 存活到整个块结束，navigate 内部
            // 再 borrow_mut() 会 RefCell 重入 panic（点击「上一级」直接退出的根因）。
            let parent_path = {
                let stl = st_global();
                let s = stl.borrow();
                s.active_tab()
                    .current()
                    .parent()
                    .map(|p| p.to_path_buf())
            };
            if let Some(p) = parent_path {
                navigate(&app, p);
            }
        });
    }
    {
        let app2 = app.as_weak();
        logic.on_refresh(move || list_current(&app2.unwrap(), true));
    }
    {
        let app2 = app.as_weak();
        logic.on_crumb_open(move |c| {
            navigate(&app2.unwrap(), PathBuf::from(c.path.to_string()));
        });
    }
    {
        let app2 = app.as_weak();
        logic.on_path_commit(move |p| {
            let app = app2.unwrap();
            app.global::<Logic>().set_dialog_mode(0);
            let raw = p.trim().to_string();
            // 盘符根补反斜杠；远程地址按原样交给协议层
            let raw = if raw.len() == 2 && raw.as_bytes()[1] == b':' {
                format!("{raw}\\")
            } else {
                raw
            };
            if remote::is_remote(&raw) || Path::new(&raw).is_dir() {
                navigate(&app, PathBuf::from(raw));
            } else {
                show_error(&app, "路径不存在或不是目录".to_string());
            }
        });
    }

    // 侧边栏 / 标签
    {
        let app2 = app.as_weak();
        logic.on_sidebar_open(move |si| {
            let p = si.path.to_string();
            // shell: 虚拟位置交给系统处理器；SMB UNC 与 ftp/http/dlna 全部应用内导航
            if p.starts_with("shell:") {
                let w = app2.clone();
                std::thread::spawn(move || {
                    let r = ops::open_shell_uri(&p);
                    if let (Some(app), Err(e)) = (w.upgrade(), r) {
                        show_error(&app, e);
                    }
                });
                return;
            }
            if remote::is_remote(&p) || Path::new(&p).is_dir() {
                navigate(&app2.unwrap(), PathBuf::from(p));
            } else {
                show_error(&app2.unwrap(), "目录不可用或已断开".into());
            }
        });
    }
    {
        let app2 = app.as_weak();
        logic.on_tab_new(move || {
            let app = app2.unwrap();
            // 旧标签的搜索视图先快照
            let l = app.global::<Logic>();
            {
                let stl = st_global();
                let mut s = stl.borrow_mut();
                stash_tab_search(&mut s, &l);
            }
            let home = st_global().borrow().tabs[0].current().clone();
            let stl = st_global();
            let mut s = stl.borrow_mut();
            let nid = s.next_tab_id;
            s.next_tab_id += 1;
            s.tabs.push(Tab::new(nid, home));
            s.active = nid;
            drop(s);
            list_current(&app, true);
        });
    }
    {
        let app2 = app.as_weak();
        logic.on_tab_close(move |id| {
            let app = app2.unwrap();
            let l = app.global::<Logic>();
            let switched = {
                let stl = st_global();
                let mut s = stl.borrow_mut();
                stash_tab_search(&mut s, &l);
                s.tabs.retain(|t| t.id as i32 != id);
                if s.tabs.is_empty() {
                    let home = PathBuf::from("C:\\");
                    let nid = s.next_tab_id;
                    s.next_tab_id += 1;
                    s.tabs.push(Tab::new(nid, home));
                }
                if !s.tabs.iter().any(|t| t.id == s.active) {
                    s.active = s.tabs.last().unwrap().id;
                    true
                } else {
                    false
                }
            };
            rebuild_tabs(&st_global(), &app);
            if switched {
                // 目标标签保留自己的搜索视图
                s_search_gen_bump();
                restore_tab_search(&st_global(), &l);
                list_current(&app, false);
            }
        });
    }
    {
        let app2 = app.as_weak();
        logic.on_tab_select(move |id| {
            let app = app2.unwrap();
            let l = app.global::<Logic>();
            let changed = {
                let stl = st_global();
                let mut s = stl.borrow_mut();
                if s.active as i32 != id {
                    stash_tab_search(&mut s, &l);
                    s.active = id as usize;
                    true
                } else {
                    false
                }
            };
            if changed {
                s_search_gen_bump();
                restore_tab_search(&st_global(), &l);
                list_current(&app, false);
            }
        });
    }

    /// 搜索代次自增（切标签时使飞行中的搜索结果作废，防跨标签串扰）
    fn s_search_gen_bump() {
        st_global().borrow_mut().search_gen += 1;
    }

    // 视图 / 排序
    {
        let app2 = app.as_weak();
        logic.on_sort_changed(move |col| {
            let app = app2.unwrap();
            let l = app.global::<Logic>();
            let stl = st_global();
            let mut s = stl.borrow_mut();
            let prev = s.cfg.sort_by;
            let (by, asc) = if prev == col {
                (col, !l.get_sort_asc()) // 同列再次点击 → 反转
            } else {
                (col, l.get_sort_asc())
            };
            s.cfg.sort_by = col;
            sort_display(&mut s.entries, by, asc);
            s.entries_model.set_vec(s.entries.clone());
            drop(s);
            l.set_sort_by(col);
            l.set_sort_asc(asc);
            rebuild_grid(&st_global());
        });
    }
    {
        let app2 = app.as_weak();
        logic.on_sort_dir_toggle(move || {
            let app = app2.unwrap();
            let asc = !app.global::<Logic>().get_sort_asc();
            let by = app.global::<Logic>().get_sort_by();
            let stl = st_global();
            let mut s = stl.borrow_mut();
            s.cfg.sort_asc = asc;
            sort_display(&mut s.entries, by, asc);
            s.entries_model.set_vec(s.entries.clone());
            drop(s);
            app.global::<Logic>().set_sort_asc(asc);
            rebuild_grid(&st_global());
        });
    }
    {
        let app2 = app.as_weak();
        logic.on_view_toggle(move || {
            let app = app2.unwrap();
            let l = app.global::<Logic>();
            l.set_grid_view(!l.get_grid_view());
        });
    }
    {
        let app2 = app.as_weak();
        logic.on_hidden_toggle(move || {
            let app = app2.unwrap();
            let l = app.global::<Logic>();
            l.set_show_hidden(!l.get_show_hidden());
            st_global().borrow_mut().cfg.show_hidden = l.get_show_hidden();
            list_current(&app, true);
        });
    }
    {
        let app2 = app.as_weak();
        logic.on_preview_toggle(move || {
            let app = app2.unwrap();
            let l = app.global::<Logic>();
            l.set_preview_open(!l.get_preview_open());
        });
    }
    {
        let app2 = app.as_weak();
        logic.on_preview_close(move || {
            app2.unwrap().global::<Logic>().set_preview_open(false);
        });
    }

    // 条目交互（普通视图与搜索视图共用；搜索视图选中态存于 search_model 行）
    {
        let app2 = app.as_weak();
        logic.on_entry_click(move |e, ctrl| {
            let app = app2.unwrap();
            let l = app.global::<Logic>();
            let target = e.path.to_string();
            let is_search = st_global().borrow().active_tab().search_active;
            if is_search {
                let sm = st_global().borrow().search_model.clone();
                let mut clicked: Option<EntryData> = None;
                for i in 0..sm.row_count() {
                    let Some(mut row) = sm.row_data(i) else { continue };
                    let p = row.path.to_string();
                    let old = row.selected;
                    if ctrl {
                        if p == target {
                            row.selected = !row.selected;
                        }
                    } else {
                        row.selected = p == target;
                    }
                    if row.selected != old {
                        sm.set_row_data(i, row.clone());
                    }
                    if p == target {
                        clicked = Some(row);
                    }
                }
                let sel = (0..sm.row_count())
                    .filter_map(|i| sm.row_data(i))
                    .filter(|x| x.selected)
                    .count();
                l.set_status_selected(sel as i32);
                if let Some(e2) = clicked {
                    update_preview(&app, &e2);
                }
                return;
            }
            let clicked_entry = {
                let stl = st_global();
                let mut s = stl.borrow_mut();
                let mut changed: Vec<usize> = Vec::new();
                for (i, entry) in s.entries.iter_mut().enumerate() {
                    let p = entry.path.to_string();
                    let old = entry.selected;
                    if ctrl {
                        if p == target {
                            entry.selected = !entry.selected;
                        }
                    } else {
                        entry.selected = p == target;
                    }
                    if entry.selected != old {
                        changed.push(i);
                    }
                }
                // 所有选中态变化的行都要同步回模型，否则 UI 与状态不一致
                for &i in &changed {
                    s.entries_model.set_row_data(i, s.entries[i].clone());
                }
                s.entries
                    .iter()
                    .position(|x| x.path.as_str() == target)
                    .map(|idx| s.entries[idx].clone())
            };
            {
                let stl = st_global();
                let s = stl.borrow();
                let sel = s.entries.iter().filter(|x| x.selected).count();
                l.set_status_selected(sel as i32);
                l.set_status_size(status_size_str(&s.entries));
            }
            if let Some(e2) = clicked_entry {
                update_preview(&app, &e2);
            }
        });
    }
    // 右键条目：已选中 → 保持整组选中（菜单作用于多选集合）；未选中 → 单选该行
    {
        let app2 = app.as_weak();
        logic.on_entry_context(move |e| {
            let app = app2.unwrap();
            let l = app.global::<Logic>();
            let target = e.path.to_string();
            let is_search = st_global().borrow().active_tab().search_active;
            if is_search {
                let sm = st_global().borrow().search_model.clone();
                // 已选中（含多选）→ 不动；未选中 → 单选
                let already = (0..sm.row_count())
                    .filter_map(|i| sm.row_data(i))
                    .any(|row| row.path.as_str() == target && row.selected);
                if already {
                    return;
                }
                let mut sel = 0i32;
                for i in 0..sm.row_count() {
                    let Some(mut row) = sm.row_data(i) else { continue };
                    let old = row.selected;
                    row.selected = row.path.as_str() == target;
                    if row.selected != old {
                        sm.set_row_data(i, row.clone());
                    }
                    if row.selected {
                        sel += 1;
                    }
                }
                l.set_status_selected(sel);
            } else {
                {
                    let stl = st_global();
                    let mut s = stl.borrow_mut();
                    let already = s.entries.iter().any(|x| x.path.as_str() == target && x.selected);
                    if already {
                        return;
                    }
                    let mut changed: Vec<usize> = Vec::new();
                    for (i, entry) in s.entries.iter_mut().enumerate() {
                        let old = entry.selected;
                        entry.selected = entry.path.as_str() == target;
                        if entry.selected != old {
                            changed.push(i);
                        }
                    }
                    for &i in &changed {
                        s.entries_model.set_row_data(i, s.entries[i].clone());
                    }
                }
                let stl = st_global();
                let s = stl.borrow();
                let sel = s.entries.iter().filter(|x| x.selected).count();
                l.set_status_selected(sel as i32);
                l.set_status_size(status_size_str(&s.entries));
            }
            update_preview(&app, &e);
        });
    }
    // 多选模式：进入/退出（退出时清空两侧视图的选中态）
    {
        let app2 = app.as_weak();
        logic.on_multi_toggle(move || {
            let app = app2.unwrap();
            let l = app.global::<Logic>();
            let exiting = l.get_multi_mode();
            l.set_multi_mode(!exiting);
            if exiting {
                clear_all_selections(&app);
            }
        });
        // 全选/清空：作用于当前活跃视图（搜索态=搜索结果，否则=目录列表）
        let app3 = app.as_weak();
        logic.on_multi_toggle_all(move |select| {
            let app = app3.unwrap();
            let l = app.global::<Logic>();
            let is_search = st_global().borrow().active_tab().search_active;
            if is_search {
                let sm = st_global().borrow().search_model.clone();
                let n = sm.row_count();
                for i in 0..n {
                    if let Some(mut row) = sm.row_data(i)
                        && row.selected != select
                    {
                        row.selected = select;
                        sm.set_row_data(i, row);
                    }
                }
                l.set_status_selected(if select { n as i32 } else { 0 });
            } else {
                let stl = st_global();
                {
                    let mut s = stl.borrow_mut();
                    for e in s.entries.iter_mut() {
                        e.selected = select;
                    }
                }
                {
                    let s = stl.borrow();
                    for i in 0..s.entries_model.row_count() {
                        if let Some(mut row) = s.entries_model.row_data(i) {
                            row.selected = select;
                            s.entries_model.set_row_data(i, row);
                        }
                    }
                    let sel = s.entries.iter().filter(|x| x.selected).count();
                    l.set_status_selected(sel as i32);
                    l.set_status_size(status_size_str(&s.entries));
                }
            }
        });
    }
    {
        let app2 = app.as_weak();
        logic.on_entry_open(move |e| {
            let app = app2.unwrap();
            let path = PathBuf::from(e.path.to_string());
            if e.is_dir {
                navigate(&app, path);
            } else {
                let ps = e.path.to_string();
                if remote::is_remote(&ps) && !ps.starts_with("\\\\") {
                    show_error(&app, "远程文件暂不支持打开，请先在客户端下载".into());
                } else {
                    open_with_system(&app, path);
                }
            }
        });
    }

    // 文件操作（远程目录守卫）
    {
        let app2 = app.as_weak();
        logic.on_delete_selected(move || {
            let app = app2.unwrap();
            if !guard_local(&app) {
                return;
            }
            delete_paths(&app, selected_paths());
        });
    }
    {
        let app2 = app.as_weak();
        logic.on_copy_selected(move |cut| {
            let app = app2.unwrap();
            if !guard_local(&app) {
                return;
            }
            clip_copy(&app, selected_paths(), cut);
        });
    }
    {
        let app2 = app.as_weak();
        logic.on_paste(move || {
            let app = app2.unwrap();
            if !guard_local(&app) {
                return;
            }
            invoke_paste(&app);
            st_global().borrow_mut().clip = None;
        });
    }
    {
        let app2 = app.as_weak();
        logic.on_new_folder(move || {
            let app = app2.unwrap();
            if !guard_local(&app) {
                return;
            }
            let l = app.global::<Logic>();
            l.set_dialog_mode(2);
            l.set_dialog_input("新建文件夹".into());
        });
    }
    {
        let app2 = app.as_weak();
        logic.on_new_file(move || {
            let app = app2.unwrap();
            if !guard_local(&app) {
                return;
            }
            let l = app.global::<Logic>();
            l.set_dialog_mode(3);
            l.set_dialog_input("新建文本文件.txt".into());
        });
    }
    {
        logic.on_favorite_add(|| {
            let stl = st_global();
            let mut s = stl.borrow_mut();
            let p = s.active_tab().current().to_string_lossy().into_owned();
            if !s.cfg.favorites.contains(&p) {
                s.cfg.favorites.push(p);
                config::save(&s.cfg);
            }
            drop(s);
            rebuild_side(&st_global());
        });
    }
    {
        logic.on_favorite_remove(move |p| {
            let stl = st_global();
            let mut s = stl.borrow_mut();
            s.cfg.favorites.retain(|x| x != &p.to_string());
            config::save(&s.cfg);
            drop(s);
            rebuild_side(&st_global());
        });
    }
    {
        let app2 = app.as_weak();
        logic.on_reveal_path(move |p| {
            let weak = app2.clone();
            let path = PathBuf::from(p.to_string());
            std::thread::spawn(move || {
                let r = ops::reveal_in_explorer(&path);
                let _ = weak.upgrade_in_event_loop(move |w| {
                    if let Err(msg) = r {
                        show_error(&w, msg);
                    }
                });
            });
        });
    }

    // 对话框
    {
        let app2 = app.as_weak();
        logic.on_dialog_ok(move |text| {
            let app = app2.unwrap();
            let l = app.global::<Logic>();
            let mode = l.get_dialog_mode();
            l.set_dialog_mode(0);
            // 对话框内输入框会抢焦点，关闭后归还快捷键捕获器
            app.invoke_refocus();
            let name = text.to_string();
            match mode {
                1 => {
                    let old = l.get_menu_entry().path.to_string();
                    if remote::is_remote(&old) && !old.starts_with("\\\\") {
                        show_error(&app, "远程目录暂只支持浏览，写操作请在本地目录使用".into());
                        return;
                    }
                    let weak = app.as_weak();
                    std::thread::spawn(move || {
                        let r = ops::rename_item(&PathBuf::from(old), &name);
                        let _ = weak.upgrade_in_event_loop(move |w| match r {
                            Ok(_) => list_current(&w, true),
                            Err(msg) => show_error(&w, msg),
                        });
                    });
                }
                2 | 3 => {
                    if !guard_local(&app) {
                        return;
                    }
                    let dir = st_global().borrow().active_tab().current().clone();
                    let is_dir = mode == 2;
                    let weak = app.as_weak();
                    std::thread::spawn(move || {
                        let r = ops::create_new(&dir, &name, is_dir);
                        let _ = weak.upgrade_in_event_loop(move |w| match r {
                            Ok(_) => list_current(&w, true),
                            Err(msg) => show_error(&w, msg),
                        });
                    });
                }
                4 => {
                    let path = PathBuf::from(&name);
                    if path.is_dir() {
                        navigate(&app, path);
                    } else {
                        show_error(&app, "路径不存在或不是目录".to_string());
                    }
                }
                _ => {}
            }
        });
    }
    {
        let app2 = app.as_weak();
        logic.on_dialog_cancel(move || {
            let app = app2.unwrap();
            app.invoke_refocus();
            app.global::<Logic>().set_dialog_mode(0);
        });
    }

    // 搜索
    {
        let app2 = app.as_weak();
        logic.on_search_commit(move |pattern| {
            let app = app2.unwrap();
            let cur_remote = {
                let stl = st_global();
                let s = stl.borrow();
                let cur = s.active_tab().current().to_string_lossy().into_owned();
                remote::is_remote(&cur) && !cur.starts_with("\\\\")
            };
            if cur_remote {
                show_error(&app, "远程目录暂不支持搜索（SMB 共享除外）".into());
                return;
            }
            let mut pat = pattern.trim().to_string();
            let l = app.global::<Logic>();
            // 后缀筛选（设置页可配置的下拉项）→ ext: 语法；空关键字 + 筛选 = 该类型全部
            let ext_q = {
                let stl = st_global();
                let s = stl.borrow();
                s.cfg
                    .ext_filters
                    .get(l.get_search_ext_filter() as usize)
                    .and_then(config::ExtFilter::to_query)
            };
            if let Some(q) = ext_q {
                pat = format!("{pat} {q}");
            }
            if pat.trim().is_empty() {
                return;
            }
            // Tab 级搜索：根路径 = 当前标签路径（秒搜结果按此过滤，递归搜索以此为根）
            let root_dir =
                st_global().borrow().active_tab().current().to_string_lossy().into_owned();
            let root_filter = PathBuf::from(&root_dir);
            if let Some(h) = st_global().borrow_mut().search.take() {
                h.cancel();
            }
            let sgen = {
                let stl = st_global();
                let mut s = stl.borrow_mut();
                s.search_gen += 1;
                s.search_gen
            };
            {
                let stl = st_global();
                let mut st = stl.borrow_mut();
                let t = st.active_tab_mut();
                t.search_active = true;
                t.search_root = root_dir.clone();
            }
            l.set_search_active(true);
            l.set_searching(true);
            l.set_search_root(root_dir.clone().into());
            st_global().borrow().search_model.set_vec(Vec::new());
            // 焦点归还快捷键捕获器（搜索框 LineEdit 会抢走焦点，不归还则快捷键失效）
            app.invoke_refocus();
            // 状态栏计数同步（全选框判断依赖）
            l.set_status_count(0);
            l.set_status_selected(0);

            let backend = instant::detect_backend();
            eprintln!("[秒搜] 后端={backend:?} pattern={pat:?} root={root_dir:?}");
            match backend {
                // 自建全盘索引：内存全表线性扫描 → 结果限定当前标签路径子树
                instant::Backend::OwnIndex => {
                    let wb = app.as_weak();
                    let wf = app.as_weak();
                    let pat2 = pat.clone();
                    std::thread::spawn(move || {
                        let t0 = std::time::Instant::now();
                        let q = instant::parse_query(&pat2);
                        let hits = instant::index_query(&q, instant::MAX_RESULTS);
                        let total = hits.len();
                        // 限定 tab 当前路径子树（Path::starts_with 语义，无盘符误匹配）
                        let hits: Vec<_> = hits
                            .into_iter()
                            .filter(|h| Path::new(&h.path).starts_with(&root_filter))
                            .collect();
                        eprintln!(
                            "[秒搜 索引] pattern={pat2:?} root={root_filter:?} 命中={}/{} 耗时={:?}",
                            hits.len(),
                            total,
                            t0.elapsed()
                        );
                        push_hits_to_ui(wb, hits, sgen);
                        search_done_ui(wf);
                    });
                }
                // 索引未就绪：旧递归搜索（当前目录子树）
                instant::Backend::Legacy => {
                    let weak_batch = app.as_weak();
                    let weak_done = app.as_weak();
                    let handle = tasks::start_search(
                        &root_dir,
                        pat,
                        move |batch| {
                            let weak = weak_batch.clone();
                            weak.upgrade_in_event_loop(move |_w| {
                                let st = st_global();
                                {
                                    let s = st.borrow();
                                    if s.search_gen != sgen {
                                        return; // 已切标签/取消，丢弃过期结果
                                    }
                                }
                                let rows: Vec<EntryData> = batch
                                    .into_iter()
                                    .map(|(name, path)| {
                                        let is_dir = std::path::Path::new(&path).is_dir();
                                        let ext = if is_dir { String::new() } else { fs::ext_of(&name) };
                                        let cat = if is_dir { fs::CAT_FOLDER } else { fs::category_for_ext(&ext) };
                                        EntryData {
                                            name: name.into(),
                                            path: path.into(),
                                            is_dir,
                                            size: 0.0,
                                            dir_size: 0.0,
                                            size_str: String::new().into(),
                                            mtime: String::new().into(),
                                            mtime_sort: 0,
                                            ext: ext.into(),
                                            category: cat,
                                            hidden: false,
                                            selected: false,
                                        }
                                    })
                                    .collect();
                                let s = st.borrow();
                                for r in rows {
                                    s.search_model.push(r);
                                }
                                let l = _w.global::<Logic>();
                                l.set_status_count(s.search_model.row_count() as i32);
                            })
                            .ok();
                        },
                        move |cancelled| {
                            let weak = weak_done.clone();
                            weak.upgrade_in_event_loop(move |w| {
                                w.global::<Logic>().set_searching(false);
                                if cancelled {
                                    w.global::<Logic>().set_search_active(false);
                                    let stl = st_global();
                                    let mut st = stl.borrow_mut();
                                    let t = st.active_tab_mut();
                                    t.search_active = false;
                                }
                            })
                            .ok();
                        },
                    );
                    st_global().borrow_mut().search = Some(handle);
                }
            }
        });
    }
    {
        let app2 = app.as_weak();
        logic.on_search_cancel(move || {
            let app = app2.unwrap();
            app.invoke_refocus();
            if let Some(h) = st_global().borrow_mut().search.take() {
                h.cancel();
            }
            let l = app.global::<Logic>();
            l.set_searching(false);
            l.set_search_active(false);
            l.set_search_input("".into());
            l.set_status_count(0);
            l.set_status_selected(0);
            st_global().borrow().search_model.set_vec(Vec::new());
            {
                let stl = st_global();
                let mut s = stl.borrow_mut();
                s.search_gen += 1;
                let t = s.active_tab_mut();
                t.search_active = false;
                t.search_hits.clear();
                t.search_query.clear();
            }
        });
    }

    // 设置页：后缀筛选 / 快捷键行模型重建
    fn rebuild_hotkey_rows(app: &AppWindow) {
        let stl = st_global();
        let s = stl.borrow();
        let rows: Vec<HotkeyRow> = config::HOTKEY_ACTIONS
            .iter()
            .enumerate()
            .map(|(i, (name, label))| HotkeyRow {
                idx: i as i32,
                action: (*label).into(),
                key: s.cfg.hotkeys.get(*name).cloned().unwrap_or_default().into(),
            })
            .collect();
        app.global::<Logic>().set_hotkey_rows(ModelRc::from(Rc::new(VecModel::from(rows))));
    }

    fn rebuild_ext_rows(app: &AppWindow) {
        let stl = st_global();
        let s = stl.borrow();
        let rows: Vec<ExtRow> = s
            .cfg
            .ext_filters
            .iter()
            .enumerate()
            .map(|(i, f)| ExtRow {
                idx: i as i32,
                label: f.label.clone().into(),
                exts: f.exts.clone().into(),
            })
            .collect();
        let labels: Vec<SharedString> =
            s.cfg.ext_filters.iter().map(|f| f.label.clone().into()).collect();
        let l = app.global::<Logic>();
        l.set_ext_rows(ModelRc::from(Rc::new(VecModel::from(rows))));
        // 下拉模型与筛选下标同步（删除项时防越界）
        let n = labels.len();
        l.set_ext_filter_model(ModelRc::from(Rc::new(VecModel::from(labels))));
        if l.get_search_ext_filter() as usize >= n {
            l.set_search_ext_filter(0);
        }
    }

    // 快捷键分派（FocusScope → Logic.hotkey）
    {
        let app2 = app.as_weak();
        logic.on_hotkey(move |key| {
            let app = app2.unwrap();
            let l = app.global::<Logic>();
            // 统一规范化：控制字符→字母、字母统一小写（含 __cap: 捕获前缀）
            let key = normalize_key(&key);
            // 设置页捕获态：写入新键位（Esc 取消；冲突拒绝）
            if !l.get_capture_action().is_empty() {
                if key == "__cap:Escape" {
                    l.set_capture_action("".into());
                    l.set_settings_error("".into());
                    return;
                }
                let action = l.get_capture_action().to_string();
                let conflict = {
                    let stl = st_global();
                    let s = stl.borrow();
                    s.cfg
                        .hotkeys
                        .iter()
                        .any(|(a, k)| a != &action && normalize_key(k) == key)
                };
                if conflict {
                    l.set_settings_error(format!("键位 {key} 已被其他动作使用").into());
                } else {
                    {
                        let stl = st_global();
                        let mut s = stl.borrow_mut();
                        s.cfg.hotkeys.insert(action.clone(), key.clone());
                        config::save(&s.cfg);
                    }
                    l.set_capture_action("".into());
                    l.set_settings_error("".into());
                    rebuild_hotkey_rows(&app);
                }
                return;
            }
            // Escape 保留行为：关对话框 > 关搜索视图 > 退出多选模式
            if key == "Escape" {
                if l.get_dialog_mode() != 0 {
                    l.invoke_dialog_cancel();
                    return;
                }
                let search_active = st_global().borrow().active_tab().search_active;
                if search_active {
                    l.invoke_search_cancel();
                    return;
                }
                if l.get_multi_mode() {
                    l.invoke_multi_toggle();
                }
                return;
            }
            let action = {
                let stl = st_global();
                let s = stl.borrow();
                s.cfg
                    .hotkeys
                    .iter()
                    .find(|(_, k)| normalize_key(k.as_str()) == key)
                    .map(|(a, _)| a.clone())
            };
            match action.as_deref() {
                Some("delete") => {
                    if guard_local(&app) {
                        delete_paths(&app, selected_paths());
                    }
                }
                Some("rename") => {
                    let first = selected_paths().first().cloned();
                    if let Some(p) = first {
                        if guard_local(&app) {
                            l.set_dialog_mode(1);
                            l.set_dialog_input(
                                p.file_name()
                                    .map(|n| n.to_string_lossy().into_owned())
                                    .unwrap_or_default()
                                    .into(),
                            );
                        }
                    } else {
                        show_error(&app, "请先选中要重命名的条目".into());
                    }
                }
                Some("copy") => {
                    if guard_local(&app) {
                        clip_copy(&app, selected_paths(), false);
                    }
                }
                Some("cut") => {
                    if guard_local(&app) {
                        clip_copy(&app, selected_paths(), true);
                    }
                }
                Some("paste") => {
                    if guard_local(&app) {
                        invoke_paste(&app);
                        st_global().borrow_mut().clip = None;
                    }
                }
                Some("select-all") => {
                    // 全选 = 进入多选模式并全选当前视图
                    if !l.get_multi_mode() {
                        l.set_multi_mode(true);
                    }
                    l.invoke_multi_toggle_all(true);
                }
                Some("multi-mode") => {
                    l.invoke_multi_toggle();
                }
                Some("back") => l.invoke_nav_back(),
                Some("forward") => l.invoke_nav_forward(),
                Some("parent") => l.invoke_nav_up(),
                Some("refresh") => l.invoke_refresh(),
                // focus-search 由 slint 端特判（直接聚焦 LineEdit），不经过此处
                _ => {}
            }
        });
    }

    // 设置页回调
    {
        let app2 = app.as_weak();
        logic.on_settings_theme_changed(move |mode| {
            let app = app2.unwrap();
            {
                let stl = st_global();
                let mut s = stl.borrow_mut();
                s.cfg.theme = mode;
                config::save(&s.cfg);
            }
            apply_theme(&app, mode);
        });
    }
    {
        logic.on_settings_admin_changed(move |v| {
            let stl = st_global();
            let mut s = stl.borrow_mut();
            s.cfg.admin_prompt = v;
            config::save(&s.cfg);
        });
    }
    {
        let app2 = app.as_weak();
        logic.on_hotkey_capture(move |idx| {
            let app = app2.unwrap();
            let l = app.global::<Logic>();
            if let Some((name, _)) = config::HOTKEY_ACTIONS.get(idx as usize) {
                l.set_capture_action((*name).into());
                l.set_settings_error("".into());
                // 捕获键依赖 ks 焦点（若焦点在其他输入框，按键根本到不了 FocusScope）
                app.invoke_refocus();
            }
        });
    }
    {
        let app2 = app.as_weak();
        logic.on_ext_add(move |label, exts| {
            let app = app2.unwrap();
            let l = app.global::<Logic>();
            let label = label.trim().to_string();
            // 扩展名规范化：小写、去点、去空白，分号重组
            let exts = exts
                .split(';')
                .map(|s| s.trim().trim_start_matches('.').to_lowercase())
                .filter(|s| !s.is_empty())
                .collect::<Vec<_>>()
                .join(";");
            if label.is_empty() {
                l.set_settings_error("筛选名称不能为空".into());
                return;
            }
            let edit_idx = l.get_ext_edit_idx();
            let dup = {
                let stl = st_global();
                let s = stl.borrow();
                s.cfg
                    .ext_filters
                    .iter()
                    .enumerate()
                    .any(|(i, f)| f.label == label && i as i32 != edit_idx)
            };
            if dup {
                l.set_settings_error(format!("筛选「{label}」已存在").into());
                return;
            }
            if edit_idx >= 0 && (edit_idx as usize) < st_global().borrow().cfg.ext_filters.len() {
                // 编辑态：更新对应行
                {
                    let stl = st_global();
                    let mut s = stl.borrow_mut();
                    s.cfg.ext_filters[edit_idx as usize] =
                        config::ExtFilter { label: label.clone(), exts: exts.clone() };
                    config::save(&s.cfg);
                }
                l.set_ext_edit_idx(-1);
                l.set_settings_error("".into());
                l.set_settings_ext_label_input("".into());
                l.set_settings_ext_input("".into());
                rebuild_ext_rows(&app);
                show_toast(&app, format!("已更新筛选「{label}」"));
                return;
            }
            {
                let stl = st_global();
                let mut s = stl.borrow_mut();
                s.cfg.ext_filters.push(config::ExtFilter { label: label.clone(), exts: exts.clone() });
                config::save(&s.cfg);
            }
            l.set_settings_error("".into());
            l.set_settings_ext_label_input("".into());
            l.set_settings_ext_input("".into());
            rebuild_ext_rows(&app);
            // 输入框 accepted 后焦点归还（否则后续快捷键静默失效）
            app.invoke_refocus();
        });
    }
    {
        let app2 = app.as_weak();
        logic.on_ext_edit(move |idx| {
            let app = app2.unwrap();
            let l = app.global::<Logic>();
            let item = {
                let stl = st_global();
                let s = stl.borrow();
                s.cfg.ext_filters.get(idx as usize).cloned()
            };
            if let Some(f) = item {
                l.set_ext_edit_idx(idx);
                l.set_settings_ext_label_input(f.label.clone().into());
                l.set_settings_ext_input(f.exts.into());
                l.set_settings_error(format!("正在编辑「{}」，改完点「存」（删除行不受影响）", f.label).into());
            }
        });
    }
    {
        let app2 = app.as_weak();
        logic.on_ext_del(move |idx| {
            let app = app2.unwrap();
            let l = app.global::<Logic>();
            let stl = st_global();
            let mut s = stl.borrow_mut();
            if (idx as usize) == 0 {
                drop(s);
                l.set_settings_error("「全部」为固定项，不可删除".into());
                return;
            }
            if (idx as usize) >= s.cfg.ext_filters.len() {
                return;
            }
            s.cfg.ext_filters.remove(idx as usize);
            config::save(&s.cfg);
            drop(s);
            // 编辑态下标随删除修正（指向被删行→取消；在其后→前移）
            let edit_idx = l.get_ext_edit_idx();
            if edit_idx == idx {
                l.set_ext_edit_idx(-1);
            } else if edit_idx > idx {
                l.set_ext_edit_idx(edit_idx - 1);
            }
            l.set_settings_error("".into());
            rebuild_ext_rows(&app);
        });
    }

    // 右键菜单动作
    {
        let app2 = app.as_weak();
        logic.on_menu_action(move |action| {
            let app = app2.unwrap();
            let l = app.global::<Logic>();
            let mode = l.get_menu_mode();
            let entry = l.get_menu_entry();
            let side_path = l.get_menu_side_path().to_string();
            match action {
                1 => {
                    // 打开
                    if mode == 0 {
                        let path = PathBuf::from(entry.path.to_string());
                        if entry.is_dir {
                            navigate(&app, path);
                        } else {
                            open_with_system(&app, path);
                        }
                    } else if mode == 2 {
                        navigate(&app, PathBuf::from(side_path));
                    }
                }
                2 => {
                    // 在资源管理器中显示
                    let p = if mode == 0 {
                        entry.path.to_string()
                    } else {
                        st_global().borrow().active_tab().current().to_string_lossy().into_owned()
                    };
                    let weak = app.as_weak();
                    std::thread::spawn(move || {
                        let r = ops::reveal_in_explorer(&PathBuf::from(p));
                        let _ = weak.upgrade_in_event_loop(move |w| {
                            if let Err(msg) = r {
                                show_error(&w, msg);
                            }
                        });
                    });
                }
                3 | 4 => {
                    // 复制 / 剪切
                    if !guard_local(&app) {
                        return;
                    }
                    let cut = action == 4;
                    let paths = if mode == 0 {
                        vec![PathBuf::from(entry.path.to_string())]
                    } else {
                        selected_paths()
                    };
                    clip_copy(&app, paths, cut);
                }
                5 => {
                    // 重命名
                    if !guard_local(&app) {
                        return;
                    }
                    l.set_dialog_mode(1);
                    l.set_dialog_input(entry.name.clone());
                }
                6 => {
                    // 删除
                    if !guard_local(&app) {
                        return;
                    }
                    let paths = if mode == 0 {
                        vec![PathBuf::from(entry.path.to_string())]
                    } else {
                        selected_paths()
                    };
                    delete_paths(&app, paths);
                }
                7 => invoke_paste(&app),
                8 => {
                    if !guard_local(&app) {
                        return;
                    }
                    l.set_dialog_mode(2);
                    l.set_dialog_input("新建文件夹".into());
                }
                9 => {
                    if !guard_local(&app) {
                        return;
                    }
                    l.set_dialog_mode(3);
                    l.set_dialog_input("新建文本文件.txt".into());
                }
                10 => list_current(&app, true),
                11 => {
                    // 收藏此文件夹
                    {
                        let stl = st_global();
            let mut s = stl.borrow_mut();
                        let p = s.active_tab().current().to_string_lossy().into_owned();
                        if !s.cfg.favorites.contains(&p) {
                            s.cfg.favorites.push(p);
                            config::save(&s.cfg);
                        }
                    }
                    rebuild_side(&st_global());
                }
                12 => {
                    // 移除收藏
                    {
                        let stl = st_global();
            let mut s = stl.borrow_mut();
                        s.cfg.favorites.retain(|x| x != &side_path);
                        config::save(&s.cfg);
                    }
                    rebuild_side(&st_global());
                }
                13 | 14 => {
                    // 压缩为 ZIP / tar.gz（windSAkinfe 整合）
                    if !guard_local(&app) {
                        return;
                    }
                    // 多选优先：右键落在选中集合内时压整组；单条未选中 → 压右键那条
                    let mut paths = selected_paths();
                    if mode == 0 && paths.is_empty() {
                        paths = vec![PathBuf::from(entry.path.to_string())];
                    }
                    if paths.is_empty() {
                        return;
                    }
                    let zip = action == 13;
                    let dst_dir = {
                        let stl = st_global();
                        stl.borrow().active_tab().current().clone()
                    };
                    compress_paths(&app, zip, paths, dst_dir);
                }
                15 => {
                    // 解压到当前目录（windSAkinfe 整合；RAR 明确不支持）
                    if mode != 0 || !guard_local(&app) {
                        return;
                    }
                    let p = PathBuf::from(entry.path.to_string());
                    if entry.is_dir || !sakinfe::is_archive(&p) {
                        let msg = if sakinfe::is_rar(&p) {
                            "不支持 RAR 格式（可解压：zip / 7z / tar / gz / xz）".to_string()
                        } else {
                            "所选文件不是支持的归档（zip / 7z / tar / gz / xz）".into()
                        };
                        show_error(&app, msg);
                        return;
                    }
                    let dst = {
                        let stl = st_global();
                        stl.borrow().active_tab().current().clone()
                    };
                    let weak = app.as_weak();
                    std::thread::spawn(move || {
                        let r = sakinfe::extract_smart(&p, &dst);
                        let _ = weak.upgrade_in_event_loop(move |w| match r {
                            Ok((target, cnt)) => {
                                show_toast(
                                    &w,
                                    format!("已解压 {cnt} 个文件 → {}", target.display()),
                                );
                                list_current(&w, true);
                            }
                            Err(e) => show_error(&w, format!("解压失败：{e}")),
                        });
                    });
                }
                21 => {
                    // 批量解压（多选模式）：选中集合中的归档逐个解到各自父目录
                    if !guard_local(&app) {
                        return;
                    }
                    let paths: Vec<PathBuf> =
                        selected_paths().into_iter().filter(|p| p.is_file()).collect();
                    if paths.is_empty() {
                        show_error(&app, "请先多选要解压的归档文件".into());
                        return;
                    }
                    l.set_busy(true);
                    let weak = app.as_weak();
                    std::thread::spawn(move || {
                        let mut ok = 0usize;
                        let mut skip = 0usize;
                        let mut fail = 0usize;
                        let mut first_err: Option<String> = None;
                        for p in &paths {
                            if sakinfe::is_rar(p) {
                                skip += 1;
                                if first_err.is_none() {
                                    first_err = Some("不支持 RAR 格式".into());
                                }
                                continue;
                            }
                            if !sakinfe::is_archive(p) {
                                skip += 1;
                                continue;
                            }
                            let parent = match p.parent() {
                                Some(d) => d.to_path_buf(),
                                None => {
                                    skip += 1;
                                    continue;
                                }
                            };
                            match sakinfe::extract_smart(p, &parent) {
                                Ok(_) => ok += 1,
                                Err(e) => {
                                    fail += 1;
                                    if first_err.is_none() {
                                        first_err = Some(e.to_string());
                                    }
                                }
                            }
                        }
                        let _ = weak.upgrade_in_event_loop(move |w| {
                            w.global::<Logic>().set_busy(false);
                            let mut msg =
                                format!("批量解压：{ok} 成功 / {skip} 跳过 / {fail} 失败");
                            if let Some(e) = first_err {
                                msg.push_str(&format!("（{e}）"));
                            }
                            if fail > 0 || skip > 0 {
                                show_error(&w, msg);
                            } else {
                                show_toast(&w, msg);
                            }
                            list_current(&w, true);
                        });
                    });
                }
                22 => {
                    // 在新标签显示（搜索结果右键）：新开标签页并定位到该文件所在目录
                    if mode != 0 {
                        return;
                    }
                    let p = PathBuf::from(entry.path.to_string());
                    let Some(dir) = p.parent().map(|d| d.to_path_buf()) else {
                        return;
                    };
                    let l = app.global::<Logic>();
                    {
                        let stl = st_global();
                        let mut s = stl.borrow_mut();
                        stash_tab_search(&mut s, &l);
                        let nid = s.next_tab_id;
                        s.next_tab_id += 1;
                        s.tabs.push(Tab::new(nid, dir));
                        s.active = nid;
                    }
                    s_search_gen_bump();
                    list_current(&app, true);
                }
                16 => {
                    // 计算哈希：多选 ≥2 → SHA-256 汇总复制；单选 → 四算法全文复制
                    // （WFM_BATCH_MIN=1 测试开关下单文件也走批量分支，见 open_batch_rename 注释）
                    if mode != 0 || !guard_local(&app) {
                        return;
                    }
                    let min_files = std::env::var("WFM_BATCH_MIN")
                        .ok()
                        .and_then(|v| v.parse::<usize>().ok())
                        .map(|n| n.min(2))
                        .unwrap_or(2);
                    let mut files = selected_paths();
                    if files.is_empty() && !entry.is_dir {
                        files.push(PathBuf::from(entry.path.to_string()));
                    }
                    files.retain(|p| p.is_file());
                    if files.len() >= min_files {
                        let weak = app.as_weak();
                        std::thread::spawn(move || {
                            let rs = sakinfe::hash_files_sha256(&files);
                            let n = rs.len();
                            let mut fail = 0u32;
                            let mut text = String::new();
                            for (name, r) in &rs {
                                match r {
                                    Ok(h) => text.push_str(&format!("{name}\t{h}\n")),
                                    Err(e) => {
                                        fail += 1;
                                        text.push_str(&format!("{name}\t<失败: {e}>\n"));
                                    }
                                }
                            }
                            text.pop();
                            let copied = ops::write_text_clipboard(&text).is_ok();
                            let _ = weak.upgrade_in_event_loop(move |w| {
                                if !copied {
                                    show_error(&w, "哈希已算出，但写入剪贴板失败".into());
                                } else if fail > 0 {
                                    show_error(&w, format!("已计算 {n} 个文件，其中 {fail} 个失败（明细已复制）"));
                                } else {
                                    show_toast(&w, format!("已计算 {n} 个文件的 SHA-256 并复制到剪贴板"));
                                }
                            });
                        });
                        return;
                    }
                    if entry.is_dir {
                        return;
                    }
                    let p = PathBuf::from(entry.path.to_string());
                    let weak = app.as_weak();
                    std::thread::spawn(move || match sakinfe::hash_file_all(&p) {
                        Ok(text) => {
                            // toast 只显示 SHA-256 前 12 位，完整四行已复制
                            // （Algo::name() 返回小写）
                            let sha_line = text
                                .lines()
                                .find(|l| l.starts_with("sha256"))
                                .unwrap_or("")
                                .to_string();
                            let short = sha_line
                                .split_once(": ")
                                .map(|(_, h)| format!("SHA-256: {}…", &h[..h.len().min(12)]))
                                .unwrap_or_else(|| sha_line.clone());
                            let copied = ops::write_text_clipboard(&text).is_ok();
                            let _ = weak.upgrade_in_event_loop(move |w| {
                                if copied {
                                    show_toast(&w, format!("{short}（CRC32/MD5/SHA-1/SHA-256 已复制到剪贴板）"));
                                } else {
                                    show_error(&w, "哈希已算出，但写入剪贴板失败".into());
                                }
                            });
                        }
                        Err(e) => {
                            let _ = weak.upgrade_in_event_loop(move |w| {
                                show_error(&w, format!("读取失败：{e}"));
                            });
                        }
                    });
                }
                17 => {
                    // 批量重命名（windSAkinfe Template 引擎，Ctrl 多选 ≥2 条）
                    if !guard_local(&app) {
                        return;
                    }
                    open_batch_rename(&app);
                }
                18 => {
                    // 在终端打开（目录 → 自身；文件 → 父目录）
                    if mode != 0 || !guard_local(&app) {
                        return;
                    }
                    let p = PathBuf::from(entry.path.to_string());
                    let dir = if entry.is_dir {
                        p
                    } else {
                        match p.parent() {
                            Some(d) => d.to_path_buf(),
                            None => return,
                        }
                    };
                    let weak = app.as_weak();
                    std::thread::spawn(move || {
                        let r = ops::open_terminal(&dir);
                        let _ = weak.upgrade_in_event_loop(move |w| {
                            if let Err(e) = r {
                                show_error(&w, e);
                            }
                        });
                    });
                }
                19 => {
                    // 查看路径：对话框显示完整路径 + 复制
                    if mode != 0 {
                        return;
                    }
                    let p = ops::strip_verbatim(Path::new(entry.path.to_string().as_str()));
                    l.set_dialog_path(p.into());
                    l.set_dialog_mode(6);
                }
                20 => {
                    // 收藏此文件夹
                    if mode != 0 {
                        return;
                    }
                    if !entry.is_dir {
                        show_error(&app, "仅支持收藏文件夹".into());
                        return;
                    }
                    let p = entry.path.to_string();
                    let stl = st_global();
                    let mut s = stl.borrow_mut();
                    if s.cfg.favorites.contains(&p) {
                        show_toast(&app, "该文件夹已在收藏夹中".into());
                    } else {
                        s.cfg.favorites.push(p);
                        config::save(&s.cfg);
                        drop(s);
                        rebuild_side(&stl);
                        show_toast(&app, "已收藏到侧栏收藏夹".into());
                    }
                }
                99 => l.set_has_toast(false),
                _ => {}
            }
        });
    }

    // 批量重命名：实时预览
    {
        let app2 = app.as_weak();
        logic.on_batch_preview(move |tpl, start_str| {
            run_batch_preview(&app2.unwrap(), &tpl, &start_str);
        });
    }
    // 批量重命名：工具栏入口
    {
        let app2 = app.as_weak();
        logic.on_batch_selected(move || {
            let app = app2.unwrap();
            if !guard_local(&app) {
                return;
            }
            open_batch_rename(&app);
        });
    }
    {
        let app2 = app.as_weak();
        // 工具栏「压」：把选中集合打包为一个归档（13=ZIP / 14=tar.gz）
        logic.on_zip_selected(move |action| {
            let app = app2.unwrap();
            if !guard_local(&app) {
                return;
            }
            let paths = selected_paths();
            if paths.is_empty() {
                show_error(&app, "请先选中要压缩的条目（可 Ctrl 多选或开启多选模式）".into());
                return;
            }
            let dst_dir = {
                let stl = st_global();
                stl.borrow().active_tab().current().clone()
            };
            compress_paths(&app, action == 13, paths, dst_dir);
        });
    }
    // 查看路径对话框：复制路径
    {
        let app2 = app.as_weak();
        logic.on_copy_dialog_path(move || {
            let app = app2.unwrap();
            let p = app.global::<Logic>().get_dialog_path().to_string();
            match ops::write_text_clipboard(&p) {
                Ok(()) => show_toast(&app, "完整路径已复制到剪贴板".into()),
                Err(e) => show_error(&app, e),
            }
        });
    }
    // 远程访问：添加条目（地址 / 显示名 / 账号 / 密码）
    {
        let app2 = app.as_weak();
        logic.on_remote_add(move |url, name, user, pass| {
            let app = app2.unwrap();
            let url = url.trim().to_string();
            let user = user.trim().to_string();
            let pass = pass.to_string();
            let l = app.global::<Logic>();
            let (kind, default_name) = match config::RemoteItem::infer(&url) {
                Some(v) => v,
                None => {
                    show_error(
                        &app,
                        "无法识别地址类型。支持：\\\\host\\share（SMB）、ftp://、http(s)://（WebDAV）、dlna://".into(),
                    );
                    return;
                }
            };
            let name = if name.trim().is_empty() { default_name } else { name.trim().to_string() };
            let stl = st_global();
            let mut s = stl.borrow_mut();
            if s.cfg.remotes.iter().any(|r| r.url == url) {
                drop(s);
                show_error(&app, "该地址已存在".into());
                return;
            }
            s.cfg.remotes.push(config::RemoteItem {
                kind,
                name: name.clone(),
                url: url.clone(),
                user,
                pass,
            });
            config::save(&s.cfg);
            drop(s);
            rebuild_side(&stl);
            l.set_dialog_mode(0);
            l.set_dialog_input("".into());
            l.set_dialog_input2("".into());
            l.set_dialog_input3("".into());
            l.set_dialog_input4("".into());
            show_toast(&app, format!("已添加远程访问：{name}"));
        });
    }
    // 批量重命名：应用
    {
        let app2 = app.as_weak();
        logic.on_batch_apply(move |tpl, start_str| {
            let app = app2.unwrap();
            let l = app.global::<Logic>();
            let start = start_str.trim().parse::<u64>().unwrap_or(1);
            let sel = selected_paths();
            let names: std::collections::HashSet<String> = st_global()
                .borrow()
                .entries
                .iter()
                .map(|e| e.name.to_string())
                .collect();
            let rows = match sakinfe::render_batch(&sel, &tpl, start, &names) {
                Ok(r) => r,
                Err(e) => {
                    l.set_batch_error(e.into());
                    return;
                }
            };
            if rows.iter().any(|r| r.conflict || r.error.is_some()) {
                l.set_batch_error("存在重名冲突或错误条目，请修改模板后再应用".into());
                return;
            }
            let items: Vec<sakinfe::RenameItem> = sel
                .iter()
                .zip(rows.iter())
                .map(|(p, r)| sakinfe::RenameItem { path: p.clone(), new_name: r.new.clone() })
                .collect();
            let weak = app.as_weak();
            std::thread::spawn(move || {
                let o = sakinfe::batch_rename(&items);
                let _ = weak.upgrade_in_event_loop(move |w| {
                    w.global::<Logic>().set_dialog_mode(0);
                    list_current(&w, true);
                    if o.failed > 0 {
                        show_error(
                            &w,
                            format!(
                                "批量重命名：{} 成功 / {} 跳过 / {} 失败（{}）",
                                o.ok,
                                o.skipped,
                                o.failed,
                                o.first_err.unwrap_or_default()
                            ),
                        );
                    } else {
                        show_toast(&w, format!("批量重命名完成：{} 成功 / {} 跳过", o.ok, o.skipped));
                    }
                });
            });
        });
    }

    // 初始
    list_current(&app, true);

    // 应用会话窗口状态：show 之后 winit 窗口已创建，set_maximized 才可靠
    let window_max = st_global().borrow().cfg.window_max;
    let window_w = st_global().borrow().cfg.window_w;
    let window_h = st_global().borrow().cfg.window_h;
    app.show().expect("slint show failed");
    let win = app.window();
    if window_max {
        win.set_maximized(true);
    } else if window_w > 100.0 && window_h > 100.0 {
        win.set_size(slint::LogicalSize::new(window_w, window_h));
    }
    force_resizable();
    // winit 首帧/首次 apply_constraints 仍会重调 set_resizable(false) 剥掉 style，
    // 延迟双保险：约束在首帧后不再变化，之后再强制一次
    slint::Timer::single_shot(std::time::Duration::from_millis(800), force_resizable);

    // 诊断开关：WFM_DEBUG_NAVUP=1 时启动后自动触发一次 nav-up（与按钮点击同一回调链）
    if std::env::var("WFM_DEBUG_NAVUP").is_ok() {
        let weak = app.as_weak();
        std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_secs(3));
            weak.upgrade_in_event_loop(move |w| {
                eprintln!("[DBG] invoking nav-up");
                w.global::<Logic>().invoke_nav_up();
                eprintln!("[DBG] nav-up returned");
            })
            .ok();
        });
    }

    // 诊断开关：WFM_DEBUG_SEARCH=<pattern> 时等待索引就绪后自动触发一次搜索提交
    // （与搜索框回车同一回调链，用于秒搜自动化验证）
    if let Ok(pat) = std::env::var("WFM_DEBUG_SEARCH")
        && !pat.trim().is_empty()
    {
        let weak = app.as_weak();
        std::thread::spawn(move || {
            // 等索引就绪（上限 300s，大卷全盘构建需 1-2 分钟），确保走秒搜链路而非降级
            for _ in 0..600 {
                if instant::index_ready() {
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(500));
            }
            weak.upgrade_in_event_loop(move |w| {
                eprintln!("[DBG] invoking search-commit: {pat}");
                w.global::<Logic>().invoke_search_commit(pat.as_str().into());
                eprintln!("[DBG] search-commit returned");
            })
            .ok();
        });
    }

    // 诊断开关：WFM_DEBUG_MULTI=1 时启动 2s 后自动进入多选模式（自动化截图验证用）
    if std::env::var("WFM_DEBUG_MULTI").map(|v| v == "1").unwrap_or(false) {
        let weak = app.as_weak();
        std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_secs(2));
            weak.upgrade_in_event_loop(move |w| {
                w.global::<Logic>().invoke_multi_toggle();
            })
            .ok();
        });
    }

    app.run().expect("slint run failed");

    // 退出前保存配置（窗口状态 + 标签会话）
    let mut cfg_final = st_global().borrow().cfg.clone();
    let win = app.window();
    cfg_final.window_max = win.is_maximized();
    if !cfg_final.window_max {
        // 保存逻辑像素（还原时按同一 DPI 语义应用）
        let size = win.size().to_logical(win.scale_factor());
        cfg_final.window_w = size.width;
        cfg_final.window_h = size.height;
    }
    {
        let stl = st_global();
        let s = stl.borrow();
        cfg_final.last_tabs = s
            .tabs
            .iter()
            .map(|t| t.current().to_string_lossy().into_owned())
            .collect();
        cfg_final.active_tab =
            s.tabs.iter().position(|t| t.id == s.active).unwrap_or(0);
    }
    config::save(&cfg_final);
    // 退出时落盘搜索索引：持久化本次会话 RDC 积累的变更（强杀进程不执行，
    // 缺口由下次启动的 USN 差量补齐——见 docs/08-增量索引更新规格.md §3.5）
    instant::save_index();
}

/// UI 侧排序（EntryData 版，规则与 fs::sort_entries 一致）
fn sort_display(entries: &mut [EntryData], sort_by: i32, asc: bool) {
    let dir_rank = |e: &EntryData| if e.is_dir { 0 } else { 1 };
    match sort_by {
        1 => entries.sort_by(|a, b| {
            dir_rank(a)
                .cmp(&dir_rank(b))
                .then_with(|| a.size.total_cmp(&b.size))
                .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
        }),
        2 => entries.sort_by(|a, b| {
            dir_rank(a)
                .cmp(&dir_rank(b))
                .then_with(|| a.category.cmp(&b.category))
                .then_with(|| a.ext.cmp(&b.ext))
                .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
        }),
        3 => entries.sort_by(|a, b| {
            dir_rank(a)
                .cmp(&dir_rank(b))
                .then_with(|| b.mtime_sort.cmp(&a.mtime_sort))
                .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
        }),
        _ => entries.sort_by(|a, b| {
            dir_rank(a).cmp(&dir_rank(b)).then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
        }),
    }
    if !asc {
        entries.reverse();
        entries.sort_by_key(dir_rank);
    }
}

#[cfg(test)]
mod hotkey_normalize_tests {
    use super::normalize_key;

    #[test]
    fn test_normalize_key() {
        // Windows 真实行为：Ctrl+字母 → 控制字符（Slint 端组装 "Ctrl+\u{3}"）
        assert_eq!(normalize_key("\u{3}"), "c");
        assert_eq!(normalize_key("Ctrl+\u{3}"), "Ctrl+c");
        assert_eq!(normalize_key("Ctrl+Shift+\u{3}"), "Ctrl+c");
        // 字母统一小写（捕获 Shift+C 得 "C"、普通按下得 "c"）
        assert_eq!(normalize_key("C"), "c");
        assert_eq!(normalize_key("Ctrl+C"), "Ctrl+c");
        assert_eq!(normalize_key("Shift+C"), "Shift+c");
        // 多字符键名原样
        assert_eq!(normalize_key("F2"), "F2");
        assert_eq!(normalize_key("Delete"), "Delete");
        assert_eq!(normalize_key("Alt+Left"), "Alt+Left");
        assert_eq!(normalize_key("Escape"), "Escape");
        // 捕获前缀保留
        assert_eq!(normalize_key("__cap:Escape"), "__cap:Escape");
        assert_eq!(normalize_key("__cap:Ctrl+\u{3}"), "__cap:Ctrl+c");
        // 无前缀直通（与配置查找 normalize(k)==key 对称）
        assert_eq!(normalize_key(&normalize_key("Ctrl+\u{3}")), "Ctrl+c");
    }
}
