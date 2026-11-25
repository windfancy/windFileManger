//! 归档容器层。每种容器独立成模块，互不依赖，上层用 `archive::Kind` 路由。

pub mod gz;
pub mod rar;
pub mod sevenz;
pub mod tar;
pub mod xz;
pub mod zip;

use std::io;
use std::path::Path;

/// 支持的归档/压缩格式。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Zip,
    SevenZ,
    Tar,
    Gz,
    Xz,
    Rar,
}

impl Kind {
    pub fn name(self) -> &'static str {
        match self {
            Kind::Zip => "zip",
            Kind::SevenZ => "7z",
            Kind::Tar => "tar",
            Kind::Gz => "gz",
            Kind::Xz => "xz",
            Kind::Rar => "rar",
        }
    }

    /// 该格式是否支持「多文件打包」（gzip / xz 只能装单流）。
    pub fn multi_entry(self) -> bool {
        matches!(self, Kind::Zip | Kind::SevenZ | Kind::Tar | Kind::Rar)
    }

    /// 该格式是否支持创建。
    pub fn can_create(self) -> bool {
        !matches!(self, Kind::Rar)
    }

    pub fn from_name(s: &str) -> Option<Kind> {
        match s.trim().trim_start_matches('.').to_ascii_lowercase().as_str() {
            "zip" => Some(Kind::Zip),
            "7z" | "sevenz" => Some(Kind::SevenZ),
            "tar" => Some(Kind::Tar),
            "gz" | "gzip" | "tgz" => Some(Kind::Gz),
            "xz" | "txz" => Some(Kind::Xz),
            "rar" => Some(Kind::Rar),
            _ => None,
        }
    }

    /// 按扩展名推断。
    pub fn from_path(p: &Path) -> Option<Kind> {
        let ext = p.extension()?.to_string_lossy().to_ascii_lowercase();
        Kind::from_name(&ext)
    }

    /// 是否复合归档（.tar.gz 这类：先解外层再解 tar）。
    pub fn inner_tar(p: &Path) -> bool {
        let n = p
            .file_name()
            .map(|s| s.to_string_lossy().to_ascii_lowercase())
            .unwrap_or_default();
        n.ends_with(".tar.gz") || n.ends_with(".tgz") || n.ends_with(".tar.xz") || n.ends_with(".txz")
    }
}

/// 统一的归档条目视图（各容器都映射到它）。
#[derive(Debug, Clone)]
pub struct Entry {
    /// 归档内路径，已统一成 `/` 分隔、无前导 `/`
    pub name: String,
    pub size: u64,
    pub is_dir: bool,
    pub mtime: Option<u64>,
    pub encrypted: bool,
}

/// 归一化归档内路径，防 Zip Slip（需求 F13）。
///
/// 返回 `None` 表示该路径非法（含 `..` 逃逸、盘符、UNC、绝对路径、Windows 非法字符）。
pub fn safe_join(root: &Path, name: &str) -> Option<std::path::PathBuf> {
    let cleaned = name.replace('\\', "/");
    if cleaned.is_empty() {
        return None;
    }
    if cleaned.len() >= 2 && cleaned.as_bytes()[1] == b':' {
        return None;
    }
    if cleaned.starts_with("//") {
        return None;
    }
    let mut out = root.to_path_buf();
    let mut depth = 0i32;
    for part in cleaned.split('/') {
        match part {
            "" | "." => continue,
            ".." => {
                depth -= 1;
                if depth < 0 {
                    return None;
                }
                out.pop();
            }
            other => {
                if other
                    .chars()
                    .any(|c| matches!(c, '\0' | '<' | '>' | ':' | '"' | '|' | '?' | '*'))
                {
                    return None;
                }
                out.push(other);
                depth += 1;
            }
        }
    }
    if depth <= 0 {
        return None;
    }
    Some(out)
}

/// 统一的「打开任何归档」入口，返回条目列表 + 取数据的能力。
pub enum AnyArchive {
    Zip(crate::archive::zip::ZipArchive),
    SevenZ(crate::archive::sevenz::SevenZArchive),
    Tar(crate::archive::tar::TarArchive),
    /// 单流压缩（无内层归档）：解压后的数据 + 建议文件名
    Single(Vec<u8>, String, Kind),
    Rar(crate::archive::rar::RarArchive),
}

impl AnyArchive {
    pub fn open(path: &Path, data: Vec<u8>) -> io::Result<AnyArchive> {
        let kind = Kind::from_path(path);
        let (kind, data) = match kind {
            Some(Kind::Gz) if Kind::inner_tar(path) => {
                (Kind::Tar, crate::archive::gz::read_gzip(&data)?)
            }
            Some(Kind::Xz) if Kind::inner_tar(path) => {
                (Kind::Tar, crate::archive::xz::read_xz(&data)?)
            }
            Some(k) => (k, data),
            None => {
                if data.starts_with(b"PK\x03\x04") || data.starts_with(b"PK\x05\x06") {
                    (Kind::Zip, data)
                } else if data.starts_with(&[0x37, 0x7A, 0xBC, 0xAF, 0x27, 0x1C]) {
                    (Kind::SevenZ, data)
                } else if data.starts_with(b"Rar!") {
                    (Kind::Rar, data)
                } else if data.starts_with(&[0x1F, 0x8B]) {
                    (Kind::Gz, data)
                } else if data.starts_with(&[0xFD, b'7', b'z', b'X', b'Z', 0x00]) {
                    (Kind::Xz, data)
                } else if data.len() > 262 && &data[257..262] == b"ustar" {
                    (Kind::Tar, data)
                } else {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "无法识别归档格式（扩展名与文件头都不匹配）",
                    ));
                }
            }
        };
        let arch = match kind {
            Kind::Zip => AnyArchive::Zip(crate::archive::zip::ZipArchive::open(data)?),
            Kind::SevenZ => AnyArchive::SevenZ(crate::archive::sevenz::SevenZArchive::open(data)?),
            Kind::Tar => AnyArchive::Tar(crate::archive::tar::TarArchive::open(data)?),
            Kind::Rar => AnyArchive::Rar(crate::archive::rar::RarArchive::open(data)?),
            Kind::Gz => {
                let out = crate::archive::gz::read_gzip(&data)?;
                let n = strip_compress_ext(path);
                AnyArchive::Single(out, n, Kind::Gz)
            }
            Kind::Xz => {
                let out = crate::archive::xz::read_xz(&data)?;
                let n = strip_compress_ext(path);
                AnyArchive::Single(out, n, Kind::Xz)
            }
        };
        Ok(arch)
    }

    pub fn kind(&self) -> Kind {
        match self {
            AnyArchive::Zip(_) => Kind::Zip,
            AnyArchive::SevenZ(_) => Kind::SevenZ,
            AnyArchive::Tar(_) => Kind::Tar,
            AnyArchive::Single(_, _, k) => *k,
            AnyArchive::Rar(_) => Kind::Rar,
        }
    }

    pub fn entries(&self) -> Vec<Entry> {
        match self {
            AnyArchive::Zip(a) => a
                .entries()
                .iter()
                .map(|e| Entry {
                    name: e.name.clone(),
                    size: e.usize,
                    is_dir: e.is_dir,
                    mtime: Some(crate::util::local_to_unix_secs(&e.mtime).max(0) as u64),
                    encrypted: e.flags & 0x0001 != 0,
                })
                .collect(),
            AnyArchive::SevenZ(a) => a
                .entries
                .iter()
                .map(|e| Entry {
                    name: e.name.clone(),
                    size: e.size,
                    is_dir: e.is_dir,
                    mtime: None,
                    encrypted: false,
                })
                .collect(),
            AnyArchive::Tar(a) => a
                .entries
                .iter()
                .map(|e| Entry {
                    name: e.name.trim_end_matches('/').to_string(),
                    size: e.size,
                    is_dir: e.is_dir || e.is_link,
                    mtime: if e.mtime > 0 { Some(e.mtime) } else { None },
                    encrypted: false,
                })
                .collect(),
            AnyArchive::Single(d, n, _) => vec![Entry {
                name: n.clone(),
                size: d.len() as u64,
                is_dir: false,
                mtime: None,
                encrypted: false,
            }],
            AnyArchive::Rar(a) => a
                .entries
                .iter()
                .map(|e| Entry {
                    name: e.name.clone(),
                    size: e.unpacked_size,
                    is_dir: e.is_dir,
                    mtime: None,
                    encrypted: e.is_encrypted,
                })
                .collect(),
        }
    }

    /// 解出第 idx 个条目的数据。
    pub fn read(&self, idx: usize, password: Option<&str>) -> io::Result<Vec<u8>> {
        match self {
            AnyArchive::Zip(a) => a.read(idx, password.map(|s| s.as_bytes())),
            AnyArchive::SevenZ(a) => {
                let all = a.extract_all()?;
                Ok(all.get(idx).cloned().unwrap_or_default())
            }
            AnyArchive::Tar(a) => a.read(idx),
            AnyArchive::Single(d, _, _) => Ok(d.clone()),
            AnyArchive::Rar(a) => a.read(idx),
        }
    }
}

/// `a.txt.gz` → `a.txt`
fn strip_compress_ext(p: &Path) -> String {
    let n = p
        .file_name()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_default();
    match n.rfind('.') {
        Some(i) if i > 0 => n[..i].to_string(),
        _ => format!("{n}.out"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn kind_from_names() {
        assert_eq!(Kind::from_name("zip"), Some(Kind::Zip));
        assert_eq!(Kind::from_name(".7Z"), Some(Kind::SevenZ));
        assert_eq!(Kind::from_name("tgz"), Some(Kind::Gz));
        assert_eq!(Kind::from_name("rar"), Some(Kind::Rar));
        assert_eq!(Kind::from_name("exe"), None);
        assert!(!Kind::Rar.can_create());
        assert!(Kind::Zip.can_create());
        assert_eq!(Kind::Rar.name(), "rar");
        assert!(Kind::Zip.multi_entry());
        assert!(!Kind::Gz.multi_entry());
    }

    #[test]
    fn inner_tar_detect() {
        assert!(Kind::inner_tar(&PathBuf::from("a.tar.gz")));
        assert!(Kind::inner_tar(&PathBuf::from("a.TGZ")));
        assert!(Kind::inner_tar(&PathBuf::from("a.tar.xz")));
        assert!(!Kind::inner_tar(&PathBuf::from("a.gz")));
    }

    #[test]
    fn zip_slip_rejected() {
        let root = PathBuf::from("out");
        assert!(safe_join(&root, "../etc/passwd").is_none());
        assert!(safe_join(&root, "a/../../x").is_none());
        assert!(safe_join(&root, "C:/win.ini").is_none());
        assert!(safe_join(&root, "//server/share/x").is_none());
        assert!(safe_join(&root, "").is_none());
        assert!(safe_join(&root, "..").is_none());
        assert!(safe_join(&root, "a/./b/../c").is_some());
    }

    #[test]
    fn safe_join_normal() {
        let root = PathBuf::from("out");
        assert_eq!(safe_join(&root, "a/b.txt"), Some(PathBuf::from("out/a/b.txt")));
        assert_eq!(safe_join(&root, "a\\b.txt"), Some(PathBuf::from("out/a/b.txt")));
        assert_eq!(safe_join(&root, "中文/文件.txt"), Some(PathBuf::from("out/中文/文件.txt")));
    }
}
