//! 设置持久化：%LOCALAPPDATA%\windFileManger\config.toml

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use serde::{Deserialize, Serialize};

/// 远程访问条目：kind = smb / ftp / webdav / dlna
#[derive(Serialize, Deserialize, Clone, Default)]
#[serde(default)]
pub struct RemoteItem {
    pub kind: String,
    pub name: String,
    pub url: String,
    /// 登录账号（空 = 匿名/Windows 当前用户）
    pub user: String,
    /// 登录密码（仅保存在本机配置文件）
    pub pass: String,
}

#[derive(Serialize, Deserialize, Clone)]
#[serde(default)]
pub struct Config {
    pub window_w: f32,
    pub window_h: f32,
    /// 退出时是否最大化（还原尺寸另存，不取最大化尺寸）
    pub window_max: bool,
    pub grid_view: bool,
    pub sort_by: i32,
    pub sort_asc: bool,
    pub show_hidden: bool,
    pub favorites: Vec<String>,
    /// 每个标签的当前目录（按标签顺序，退出时保存）
    pub last_tabs: Vec<String>,
    /// 激活标签在 last_tabs 中的下标
    pub active_tab: usize,
    /// 远程访问条目（侧栏「远程访问」段）
    pub remotes: Vec<RemoteItem>,
    /// 秒搜自建索引的根列表（如 ["D:", "E:"]）；空 = 全部固定盘
    pub index_drives: Vec<String>,
    /// 启动时非管理员是否提示提权（true=提示）
    pub admin_prompt: bool,
    /// UI 色调模式：0 浅色 / 1 深色 / 2 跟随系统
    pub theme: i32,
    /// 快捷键表：动作名 → 键名（如 "delete" → "Delete"）
    pub hotkeys: HashMap<String, String>,
    /// 搜索后缀筛选类型（含「全部」；exts 为分号分隔的扩展名，空串=全部）
    pub ext_filters: Vec<ExtFilter>,
}

/// 搜索后缀筛选项（设置页可增删）
#[derive(Serialize, Deserialize, Clone)]
pub struct ExtFilter {
    pub label: String,
    /// 分号分隔的扩展名（如 "jpg;jpeg;png"），空串 = 全部
    pub exts: String,
}

impl ExtFilter {
    /// ext: 语法串（Everything 兼容）：空串 → None；否则 "ext:a;b;c"
    pub fn to_query(&self) -> Option<String> {
        if self.exts.is_empty() {
            None
        } else {
            Some(format!("ext:{}", self.exts))
        }
    }
}

/// 默认快捷键表（动作名 → 默认键）
pub fn default_hotkeys() -> HashMap<String, String> {
    [
        ("delete", "Delete"),
        ("rename", "F2"),
        ("copy", "Ctrl+C"),
        ("cut", "Ctrl+X"),
        ("paste", "Ctrl+V"),
        ("select-all", "Ctrl+A"),
        ("back", "Alt+Left"),
        ("forward", "Alt+Right"),
        ("parent", "Backspace"),
        ("refresh", "F5"),
        ("multi-mode", "Ctrl+M"),
        ("focus-search", "Ctrl+F"),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_string(), v.to_string()))
    .collect()
}

/// 动作名 → 设置页显示名（顺序即设置页展示顺序）
pub const HOTKEY_ACTIONS: &[(&str, &str)] = &[
    ("delete", "删除到回收站"),
    ("rename", "重命名"),
    ("copy", "复制"),
    ("cut", "剪切"),
    ("paste", "粘贴"),
    ("select-all", "全选"),
    ("multi-mode", "多选模式"),
    ("back", "后退"),
    ("forward", "前进"),
    ("parent", "上级目录"),
    ("refresh", "刷新"),
    ("focus-search", "聚焦搜索框"),
];

/// 默认后缀筛选类型（与 ExtFilter 顺序对应工具栏下拉序号）
pub fn default_ext_filters() -> Vec<ExtFilter> {
    vec![
        ExtFilter { label: "全部".into(), exts: String::new() },
        ExtFilter { label: "txt".into(), exts: "txt".into() },
        ExtFilter { label: "md".into(), exts: "md".into() },
        ExtFilter { label: "pdf".into(), exts: "pdf".into() },
        ExtFilter { label: "图片".into(), exts: "jpg;jpeg;png;gif;webp;bmp;svg".into() },
        ExtFilter { label: "视频".into(), exts: "mp4;mkv;avi;mov;flv;wmv;webm".into() },
    ]
}

impl Default for Config {
    fn default() -> Self {
        Self {
            window_w: 1180.0,
            window_h: 760.0,
            window_max: false,
            grid_view: false,
            sort_by: 0,
            sort_asc: true,
            show_hidden: false,
            favorites: Vec::new(),
            last_tabs: Vec::new(),
            active_tab: 0,
            remotes: Vec::new(),
            index_drives: Vec::new(),
            admin_prompt: true,
            theme: 0,
            hotkeys: default_hotkeys(),
            ext_filters: default_ext_filters(),
        }
    }
}

impl Config {
    /// 会话恢复的标签目录列表（剔除失效项），空则返回 None。
    /// 远程路径（SMB/FTP/WebDAV/DLNA）无法用 is_dir 校验，按前缀放行。
    pub fn restored_tabs(&self) -> Option<(Vec<String>, usize)> {
        let dirs: Vec<String> = self
            .last_tabs
            .iter()
            .filter(|p| Path::new(p.as_str()).is_dir() || is_remote_path(p))
            .cloned()
            .collect();
        if dirs.is_empty() {
            return None;
        }
        let active = self.active_tab.min(dirs.len() - 1);
        Some((dirs, active))
    }
}

/// 远程路径前缀判断（与 remote::is_remote 语义一致，此处独立实现避免模块耦合）
pub fn is_remote_path(p: &str) -> bool {
    p.starts_with("\\\\")
        || ["ftp://", "http://", "https://", "dlna://"].iter().any(|s| p.starts_with(s))
}

impl RemoteItem {
    /// 从地址推断 (kind, 默认名称)。支持：
    /// `\\host\share`（SMB）、`ftp(s)://…`、`http(s)://…`（WebDAV）、`dlna://…`
    pub fn infer(url: &str) -> Option<(String, String)> {
        let u = url.trim();
        if u.starts_with("\\\\") {
            let name = u.trim_start_matches('\\')
                .split(['\\', '/'])
                .nth(1)
                .filter(|s| !s.is_empty())
                .unwrap_or("SMB 共享");
            return Some(("smb".into(), name.to_string()));
        }
        let (scheme, rest) = u.split_once("://")?;
        if rest.is_empty() {
            return None;
        }
        match scheme {
            "ftp" | "ftps" => Some(("ftp".into(), host_of(rest))),
            "http" | "https" => Some(("webdav".into(), host_of(rest))),
            "dlna" => Some(("dlna".into(), host_of(rest))),
            _ => None,
        }
    }
}

/// host[:port][/path] → host[:port]
fn host_of(rest: &str) -> String {
    let hostport = rest.split(['/', '?']).next().unwrap_or(rest);
    if hostport.is_empty() { "远程".into() } else { hostport.to_string() }
}

pub fn config_dir() -> PathBuf {
    let base = std::env::var("LOCALAPPDATA").unwrap_or_else(|_| std::env::temp_dir().to_string_lossy().into_owned());
    PathBuf::from(base).join("windFileManger")
}

pub fn config_path() -> PathBuf {
    config_dir().join("config.toml")
}

pub fn load() -> Config {
    match std::fs::read_to_string(config_path()) {
        Ok(s) => toml::from_str(&s).unwrap_or_default(),
        Err(_) => Config::default(),
    }
}

pub fn save(cfg: &Config) {
    let dir = config_dir();
    let _ = std::fs::create_dir_all(&dir);
    if let Ok(s) = toml::to_string_pretty(cfg) {
        let _ = std::fs::write(config_path(), s);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_roundtrip() {
        let cfg = Config {
            grid_view: true,
            sort_by: 3,
            sort_asc: false,
            favorites: vec!["D:\\a".into()],
            window_max: true,
            last_tabs: vec!["D:\\x".into(), "D:\\y".into()],
            active_tab: 1,
            admin_prompt: false,
            theme: 1,
            hotkeys: [("delete".to_string(), "F9".to_string())].into_iter().collect(),
            ext_filters: vec![ExtFilter { label: "源码".into(), exts: "rs;toml".into() }],
            ..Default::default()
        };
        let s = toml::to_string_pretty(&cfg).unwrap();
        let back: Config = toml::from_str(&s).unwrap();
        assert_eq!(back.sort_by, 3);
        assert!(!back.sort_asc);
        assert!(back.grid_view);
        assert_eq!(back.favorites, vec!["D:\\a"]);
        assert!(back.window_max);
        assert_eq!(back.last_tabs, vec!["D:\\x", "D:\\y"]);
        assert_eq!(back.active_tab, 1);
        // 新字段往返
        assert!(!back.admin_prompt);
        assert_eq!(back.theme, 1);
        assert_eq!(back.hotkeys.get("delete").map(String::as_str), Some("F9"));
        assert_eq!(back.ext_filters.len(), 1);
        assert_eq!(back.ext_filters[0].to_query().as_deref(), Some("ext:rs;toml"));
    }

    #[test]
    fn test_ext_filter_all() {
        // 「全部」项 exts 为空 → to_query 返回 None（不加 ext 子句）
        let f = ExtFilter { label: "全部".into(), exts: String::new() };
        assert!(f.to_query().is_none());
    }

    #[test]
    fn test_defaults() {
        let cfg = Config::default();
        // 默认快捷键齐全且与动作表对齐
        for (name, _) in HOTKEY_ACTIONS {
            assert!(
                cfg.hotkeys.contains_key(*name),
                "默认快捷键缺少动作 {name}"
            );
        }
        // 默认后缀筛选 = 六项且首项为「全部」
        assert_eq!(cfg.ext_filters.len(), 6);
        assert_eq!(cfg.ext_filters[0].label, "全部");
        // 旧配置文件（无新字段）能以默认值补齐
        let old: Config = toml::from_str("window_w = 1000.0\n").unwrap();
        assert!(old.admin_prompt);
        assert_eq!(old.theme, 0);
        assert_eq!(old.ext_filters.len(), 6);
        assert!(old.hotkeys.contains_key("delete"));
    }

    #[test]
    fn test_restore_tabs() {
        // 失效项被剔除、越界 active 回落
        let cfg = Config {
            last_tabs: vec!["C:\\nonexistent_dir_xyz".into(), "C:\\".into()],
            active_tab: 9,
            ..Default::default()
        };
        let (dirs, active) = cfg.restored_tabs().unwrap();
        assert_eq!(dirs, vec!["C:\\"]);
        assert_eq!(active, 0);
        // 全部失效 → None
        let cfg2 = Config { last_tabs: vec!["C:\\nonexistent_dir_xyz".into()], ..Default::default() };
        assert!(cfg2.restored_tabs().is_none());
        // 空配置 → None
        assert!(Config::default().restored_tabs().is_none());
    }

    #[test]
    fn test_remote_infer() {
        assert_eq!(
            RemoteItem::infer(r"\\nas\media").unwrap(),
            ("smb".into(), "media".into())
        );
        assert_eq!(
            RemoteItem::infer("ftp://192.168.1.10:2121/").unwrap(),
            ("ftp".into(), "192.168.1.10:2121".into())
        );
        assert_eq!(
            RemoteItem::infer("https://nas.example.com:5005/dav").unwrap(),
            ("webdav".into(), "nas.example.com:5005".into())
        );
        assert_eq!(
            RemoteItem::infer("dlna://192.168.1.5:8200").unwrap(),
            ("dlna".into(), "192.168.1.5:8200".into())
        );
        assert!(RemoteItem::infer("http://").is_none());
        assert!(RemoteItem::infer("gopher://x").is_none());
        assert!(RemoteItem::infer("just text").is_none());
    }
}
