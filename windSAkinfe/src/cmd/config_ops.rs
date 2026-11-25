//! 配置命令：`cfg list|get|set|path|reset`。
//!
//! 配置文件是极简 `key = value` 文本（不引 toml 依赖，保持零算法依赖红线）。
//! 位置：Windows `%APPDATA%\windSAkinfe\config.toml`，其它平台
//! `~/.config/windSAkinfe/config.toml`。

use std::fs;
use std::path::PathBuf;

use crate::cli::CfgArgs;
use crate::plan::{Fail, Summary};
use crate::term::{self, JVal, Out};
use crate::util::path_to_display;

use super::Global;

pub fn config_path() -> Option<PathBuf> {
    #[cfg(windows)]
    {
        let base = std::env::var_os("APPDATA")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("USERPROFILE").map(|p| PathBuf::from(p).join("AppData/Roaming")))?;
        Some(base.join("windSAkinfe").join("config.toml"))
    }
    #[cfg(not(windows))]
    {
        let base = std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))?;
        Some(base.join("windSAkinfe").join("config.toml"))
    }
}

#[derive(Debug, Clone)]
#[derive(Default)]
pub struct Config {
    pub jobs: Option<usize>,
    pub on_conflict: Option<String>,
    pub confirm_count: Option<usize>,
    pub confirm_bytes: Option<u64>,
    pub trash: Option<bool>,
    pub color: Option<bool>,
    pub level: Option<u8>,
}


impl Config {
    pub fn load() -> Config {
        let Some(p) = config_path() else { return Config::default() };
        let Ok(text) = fs::read_to_string(p) else { return Config::default() };
        let mut c = Config::default();
        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') || line.starts_with('[') {
                continue;
            }
            let Some((k, v)) = line.split_once('=') else { continue };
            let key = k.trim();
            let val = v.trim().trim_matches('"');
            match key {
                "jobs" => c.jobs = val.parse().ok(),
                "on_conflict" => c.on_conflict = Some(val.to_string()),
                "confirm_count" => c.confirm_count = val.parse().ok(),
                "confirm_bytes" => c.confirm_bytes = crate::util::parse_size(val).ok(),
                "trash" => c.trash = parse_bool(val),
                "color" => c.color = parse_bool(val),
                "level" => c.level = val.parse().ok(),
                _ => {}
            }
        }
        c
    }

    pub fn to_text(&self) -> String {
        let mut s = String::from("# windSAkinfe 配置文件（key = value）\n");
        if let Some(v) = self.jobs {
            s.push_str(&format!("jobs = {v}\n"));
        }
        if let Some(v) = &self.on_conflict {
            s.push_str(&format!("on_conflict = \"{v}\"\n"));
        }
        if let Some(v) = self.confirm_count {
            s.push_str(&format!("confirm_count = {v}\n"));
        }
        if let Some(v) = self.confirm_bytes {
            s.push_str(&format!("confirm_bytes = {v}\n"));
        }
        if let Some(v) = self.trash {
            s.push_str(&format!("trash = {v}\n"));
        }
        if let Some(v) = self.color {
            s.push_str(&format!("color = {v}\n"));
        }
        if let Some(v) = self.level {
            s.push_str(&format!("level = {v}\n"));
        }
        s
    }

    pub fn get(&self, key: &str) -> Option<String> {
        match key {
            "jobs" => self.jobs.map(|v| v.to_string()),
            "on_conflict" => self.on_conflict.clone(),
            "confirm_count" => self.confirm_count.map(|v| v.to_string()),
            "confirm_bytes" => self.confirm_bytes.map(|v| v.to_string()),
            "trash" => self.trash.map(|v| v.to_string()),
            "color" => self.color.map(|v| v.to_string()),
            "level" => self.level.map(|v| v.to_string()),
            _ => None,
        }
    }

    pub fn set(&mut self, key: &str, val: &str) -> Result<(), String> {
        match key {
            "jobs" => self.jobs = Some(val.parse().map_err(|_| "jobs 需要正整数".to_string())?),
            "on_conflict" => {
                if crate::plan::ConflictPolicy::parse(val).is_none() {
                    return Err("on_conflict 只能是 skip/overwrite/rename".into());
                }
                self.on_conflict = Some(val.to_ascii_lowercase());
            }
            "confirm_count" => {
                self.confirm_count = Some(val.parse().map_err(|_| "confirm_count 需要正整数".to_string())?)
            }
            "confirm_bytes" => self.confirm_bytes = Some(crate::util::parse_size(val)?),
            "trash" => self.trash = Some(parse_bool(val).ok_or("trash 只能是 true/false")?),
            "color" => self.color = Some(parse_bool(val).ok_or("color 只能是 true/false")?),
            "level" => {
                let v: u8 = val.parse().map_err(|_| "level 需要 0..9".to_string())?;
                if v > 9 {
                    return Err("level 需要 0..9".into());
                }
                self.level = Some(v);
            }
            other => return Err(format!("未知配置项：{other}")),
        }
        Ok(())
    }

    pub const KEYS: [&'static str; 7] =
        ["jobs", "on_conflict", "confirm_count", "confirm_bytes", "trash", "color", "level"];
}

fn parse_bool(s: &str) -> Option<bool> {
    match s.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => Some(true),
        "0" | "false" | "no" | "off" => Some(false),
        _ => None,
    }
}

pub fn cfg(a: &CfgArgs, _g: &Global, out: &mut Out) -> Summary {
    let mut sum = Summary::new();
    let path = match config_path() {
        Some(p) => p,
        None => {
            sum.fail(Fail::usage("无法定位配置目录"));
            return sum;
        }
    };
    let mut conf = Config::load();

    match a.action.to_ascii_lowercase().as_str() {
        "path" => {
            out.raw(&path_to_display(&path));
        }
        "list" | "show" => {
            out.raw(&term::bold("配置文件"));
            out.raw(&format!("  {}", path_to_display(&path)));
            out.raw(&term::bold("当前值"));
            for k in Config::KEYS {
                let v = conf.get(k).unwrap_or_else(|| term::dim("(未设置)"));
                out.raw(&format!("  {k:<14} = {v}"));
            }
            out.json_item(JVal::Obj(vec![
                ("status".to_string(), JVal::s("ok")),
                ("action".to_string(), JVal::s("cfg-list")),
                ("path".to_string(), JVal::s(path_to_display(&path))),
            ]));
        }
        "get" => {
            let Some(key) = &a.key else {
                sum.fail(Fail::usage("用法：cfg get <key>"));
                return sum;
            };
            match conf.get(key) {
                Some(v) => out.raw(&v),
                None => {
                    out.raw("");
                    sum.fail(Fail::usage(format!("{key} 未设置")));
                }
            }
        }
        "set" => {
            let (Some(key), Some(val)) = (&a.key, &a.value) else {
                sum.fail(Fail::usage("用法：cfg set <key> <value>"));
                return sum;
            };
            if let Err(e) = conf.set(key, val) {
                sum.fail(Fail::usage(e));
                return sum;
            }
            if let Some(parent) = path.parent() {
                if let Err(e) = fs::create_dir_all(parent) {
                    sum.fail(Fail::io(parent.to_path_buf(), e));
                    return sum;
                }
            }
            if let Err(e) = fs::write(&path, conf.to_text()) {
                sum.fail(Fail::io(path.clone(), e));
                return sum;
            }
            out.raw(&format!("{key} = {val}"));
            sum.ok = 1;
        }
        "reset" => {
            if path.exists() {
                if let Err(e) = fs::remove_file(&path) {
                    sum.fail(Fail::io(path.clone(), e));
                    return sum;
                }
            }
            out.raw(&format!("已重置 {}", path_to_display(&path)));
            sum.ok = 1;
        }
        other => {
            sum.fail(Fail::usage(format!(
                "未知动作 {other}（支持 list/get/set/path/reset）"
            )));
        }
    }
    sum
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn set_and_get_roundtrip() {
        let mut c = Config::default();
        c.set("jobs", "8").unwrap();
        assert_eq!(c.get("jobs").as_deref(), Some("8"));
        c.set("on_conflict", "rename").unwrap();
        assert_eq!(c.get("on_conflict").as_deref(), Some("rename"));
        assert!(c.set("on_conflict", "nonsense").is_err());
        assert!(c.set("nope", "1").is_err());
        assert!(c.set("level", "99").is_err());
        c.set("confirm_bytes", "100M").unwrap();
        assert_eq!(c.get("confirm_bytes").as_deref(), Some("104857600"));
        c.set("trash", "true").unwrap();
        assert_eq!(c.get("trash").as_deref(), Some("true"));
    }

    #[test]
    fn text_roundtrip() {
        let mut c = Config::default();
        c.set("jobs", "4").unwrap();
        c.set("on_conflict", "skip").unwrap();
        let t = c.to_text();
        assert!(t.contains("jobs = 4"));
        assert!(t.contains("on_conflict = \"skip\""));
    }

    #[test]
    fn config_path_exists() {
        assert!(config_path().is_some());
    }
}
