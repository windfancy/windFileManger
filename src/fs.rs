//! 文件系统层：目录列举、类型分类、格式化工具。
//!
//! 铁律（项目记忆）：Windows 上批量遍历只用 `DirEntry::metadata()` / `file_type()`
//! （直接复用 readdir 的 WIN32_FIND_DATA，零系统调用），绝不逐文件补 stat。

use std::path::{Path, PathBuf};

// ---- 类型分类（复刻 NanoFile AppxManifest 的 14 组 fileTypeAssociation）----

pub const CAT_FOLDER: i32 = 0;
pub const CAT_IMAGE: i32 = 1;
pub const CAT_VIDEO: i32 = 2;
pub const CAT_AUDIO: i32 = 3;
pub const CAT_PDF: i32 = 4;
pub const CAT_WORD: i32 = 5;
pub const CAT_PRESENTATION: i32 = 6;
pub const CAT_SPREADSHEET: i32 = 7;
pub const CAT_BOOK: i32 = 8;
pub const CAT_NOTE: i32 = 9;
pub const CAT_CODE: i32 = 10;
pub const CAT_CONFIG: i32 = 11;
pub const CAT_DATABASE: i32 = 12;
pub const CAT_ARCHIVE: i32 = 13;
pub const CAT_EXECUTABLE: i32 = 14;
pub const CAT_UNKNOWN: i32 = 15;

/// 扩展名 -> 分类索引（小写、不含点）。取自 NanoFile 清单扩展名表（节选常用项）。
const EXT_TABLE: &[(&str, i32)] = &[
    // image
    ("ai", CAT_IMAGE), ("apng", CAT_IMAGE), ("arw", CAT_IMAGE), ("avif", CAT_IMAGE),
    ("avifs", CAT_IMAGE), ("bmp", CAT_IMAGE), ("cr2", CAT_IMAGE), ("cur", CAT_IMAGE),
    ("dds", CAT_IMAGE), ("dib", CAT_IMAGE), ("eps", CAT_IMAGE), ("exr", CAT_IMAGE),
    ("gif", CAT_IMAGE), ("hdr", CAT_IMAGE), ("heic", CAT_IMAGE), ("heif", CAT_IMAGE),
    ("ico", CAT_IMAGE), ("jfif", CAT_IMAGE), ("jpe", CAT_IMAGE), ("jpeg", CAT_IMAGE),
    ("jpg", CAT_IMAGE), ("jxl", CAT_IMAGE), ("nef", CAT_IMAGE), ("orf", CAT_IMAGE),
    ("pbm", CAT_IMAGE), ("pgm", CAT_IMAGE), ("png", CAT_IMAGE), ("ppm", CAT_IMAGE),
    ("psd", CAT_IMAGE), ("qoi", CAT_IMAGE), ("raw", CAT_IMAGE), ("sr2", CAT_IMAGE),
    ("svg", CAT_IMAGE), ("svgz", CAT_IMAGE), ("tga", CAT_IMAGE), ("tif", CAT_IMAGE),
    ("tiff", CAT_IMAGE), ("webp", CAT_IMAGE), ("xcf", CAT_IMAGE),
    // video
    ("3g2", CAT_VIDEO), ("3gp", CAT_VIDEO), ("asf", CAT_VIDEO), ("av2", CAT_VIDEO),
    ("avi", CAT_VIDEO), ("bdmv", CAT_VIDEO), ("dav", CAT_VIDEO), ("dv", CAT_VIDEO),
    ("f4v", CAT_VIDEO), ("flv", CAT_VIDEO), ("hevc", CAT_VIDEO), ("m2ts", CAT_VIDEO),
    ("m2v", CAT_VIDEO), ("m4v", CAT_VIDEO), ("mkv", CAT_VIDEO), ("mov", CAT_VIDEO),
    ("mp4", CAT_VIDEO), ("mpeg", CAT_VIDEO), ("mpg", CAT_VIDEO), ("mxf", CAT_VIDEO),
    ("ogv", CAT_VIDEO), ("qt", CAT_VIDEO), ("rm", CAT_VIDEO), ("rmvb", CAT_VIDEO),
    ("vob", CAT_VIDEO), ("webm", CAT_VIDEO), ("wmv", CAT_VIDEO),
    // audio
    ("3ga", CAT_AUDIO), ("aac", CAT_AUDIO), ("ac3", CAT_AUDIO), ("aif", CAT_AUDIO),
    ("aiff", CAT_AUDIO), ("alac", CAT_AUDIO), ("amr", CAT_AUDIO), ("ape", CAT_AUDIO),
    ("au", CAT_AUDIO), ("caf", CAT_AUDIO), ("dff", CAT_AUDIO), ("dsf", CAT_AUDIO),
    ("flac", CAT_AUDIO), ("m4a", CAT_AUDIO), ("m4b", CAT_AUDIO), ("m4p", CAT_AUDIO),
    ("m4r", CAT_AUDIO), ("mid", CAT_AUDIO), ("midi", CAT_AUDIO), ("mka", CAT_AUDIO),
    ("mp1", CAT_AUDIO), ("mp2", CAT_AUDIO), ("mp3", CAT_AUDIO), ("mpc", CAT_AUDIO),
    ("oga", CAT_AUDIO), ("ogg", CAT_AUDIO), ("opus", CAT_AUDIO), ("pcm", CAT_AUDIO),
    ("ra", CAT_AUDIO), ("silk", CAT_AUDIO), ("spx", CAT_AUDIO), ("tak", CAT_AUDIO),
    ("tta", CAT_AUDIO), ("voc", CAT_AUDIO), ("wav", CAT_AUDIO), ("weba", CAT_AUDIO),
    ("wma", CAT_AUDIO), ("wv", CAT_AUDIO),
    // pdf
    ("pdf", CAT_PDF),
    // word
    ("doc", CAT_WORD), ("docm", CAT_WORD), ("docx", CAT_WORD), ("dot", CAT_WORD),
    ("dotm", CAT_WORD), ("dotx", CAT_WORD), ("odt", CAT_WORD), ("ott", CAT_WORD),
    ("pages", CAT_WORD), ("rtf", CAT_WORD), ("wps", CAT_WORD),
    // presentation
    ("dps", CAT_PRESENTATION), ("key", CAT_PRESENTATION), ("odp", CAT_PRESENTATION),
    ("pot", CAT_PRESENTATION), ("potm", CAT_PRESENTATION), ("potx", CAT_PRESENTATION),
    ("pps", CAT_PRESENTATION), ("ppsx", CAT_PRESENTATION), ("ppt", CAT_PRESENTATION),
    ("pptm", CAT_PRESENTATION), ("pptx", CAT_PRESENTATION),
    // spreadsheet
    ("csv", CAT_SPREADSHEET), ("et", CAT_SPREADSHEET), ("numbers", CAT_SPREADSHEET),
    ("ods", CAT_SPREADSHEET), ("tsv", CAT_SPREADSHEET), ("xls", CAT_SPREADSHEET),
    ("xlsm", CAT_SPREADSHEET), ("xlsx", CAT_SPREADSHEET), ("xlt", CAT_SPREADSHEET),
    ("xltm", CAT_SPREADSHEET), ("xltx", CAT_SPREADSHEET),
    // book
    ("azw", CAT_BOOK), ("azw3", CAT_BOOK), ("djvu", CAT_BOOK), ("epub", CAT_BOOK),
    ("fb2", CAT_BOOK), ("mobi", CAT_BOOK), ("prc", CAT_BOOK),
    // note (text)
    ("ass", CAT_NOTE), ("eml", CAT_NOTE), ("log", CAT_NOTE), ("lrc", CAT_NOTE),
    ("markdown", CAT_NOTE), ("md", CAT_NOTE), ("msg", CAT_NOTE), ("srt", CAT_NOTE),
    ("ssa", CAT_NOTE), ("text", CAT_NOTE), ("txt", CAT_NOTE), ("vtt", CAT_NOTE),
    // code
    ("adb", CAT_CODE), ("ads", CAT_CODE), ("ahk", CAT_CODE), ("as", CAT_CODE),
    ("asm", CAT_CODE), ("asp", CAT_CODE), ("aspx", CAT_CODE), ("astro", CAT_CODE),
    ("au3", CAT_CODE), ("bas", CAT_CODE), ("bash", CAT_CODE), ("bat", CAT_CODE),
    ("bib", CAT_CODE), ("c", CAT_CODE), ("cbl", CAT_CODE), ("cc", CAT_CODE),
    ("cfm", CAT_CODE), ("cgi", CAT_CODE), ("cjs", CAT_CODE), ("clj", CAT_CODE),
    ("cls", CAT_CODE), ("cmake", CAT_CODE), ("cmd", CAT_CODE), ("cpp", CAT_CODE),
    ("cr", CAT_CODE), ("cs", CAT_CODE), ("csh", CAT_CODE), ("csproj", CAT_CODE),
    ("css", CAT_CODE), ("cu", CAT_CODE), ("cuh", CAT_CODE), ("cxx", CAT_CODE),
    ("d", CAT_CODE), ("dart", CAT_CODE), ("diff", CAT_CODE), ("dpr", CAT_CODE),
    ("el", CAT_CODE), ("erl", CAT_CODE), ("ex", CAT_CODE), ("exs", CAT_CODE),
    ("f90", CAT_CODE), ("f95", CAT_CODE), ("for", CAT_CODE), ("frag", CAT_CODE),
    ("fsproj", CAT_CODE), ("gbr", CAT_CODE), ("glsl", CAT_CODE), ("go", CAT_CODE),
    ("gradle", CAT_CODE), ("graphql", CAT_CODE), ("groovy", CAT_CODE),
    ("h", CAT_CODE), ("haml", CAT_CODE), ("hh", CAT_CODE), ("hlsl", CAT_CODE),
    ("hpp", CAT_CODE), ("hs", CAT_CODE), ("htm", CAT_CODE), ("html", CAT_CODE),
    ("hxx", CAT_CODE), ("inc", CAT_CODE), ("inf", CAT_CODE), ("iss", CAT_CODE),
    ("java", CAT_CODE), ("jl", CAT_CODE), ("js", CAT_CODE), ("json", CAT_CODE),
    ("jsx", CAT_CODE), ("kt", CAT_CODE), ("kts", CAT_CODE), ("less", CAT_CODE),
    ("lhs", CAT_CODE), ("lisp", CAT_CODE), ("lua", CAT_CODE), ("mjs", CAT_CODE),
    ("mk", CAT_CODE), ("ml", CAT_CODE), ("mm", CAT_CODE), ("nasm", CAT_CODE),
    ("nim", CAT_CODE), ("nsh", CAT_CODE), ("nsi", CAT_CODE), ("pas", CAT_CODE),
    ("patch", CAT_CODE), ("php", CAT_CODE), ("pl", CAT_CODE), ("pm", CAT_CODE),
    ("pod", CAT_CODE), ("pp", CAT_CODE), ("prg", CAT_CODE), ("prj", CAT_CODE),
    ("pro", CAT_CODE), ("project", CAT_CODE), ("proto", CAT_CODE), ("ps1", CAT_CODE),
    ("psd1", CAT_CODE), ("psm1", CAT_CODE), ("py", CAT_CODE), ("pyw", CAT_CODE),
    ("r", CAT_CODE), ("rb", CAT_CODE), ("rbw", CAT_CODE), ("rc", CAT_CODE),
    ("resx", CAT_CODE), ("rs", CAT_CODE), ("s", CAT_CODE), ("sas", CAT_CODE),
    ("sass", CAT_CODE), ("scala", CAT_CODE), ("sch", CAT_CODE), ("scm", CAT_CODE),
    ("scss", CAT_CODE), ("sh", CAT_CODE), ("sln", CAT_CODE), ("spec", CAT_CODE),
    ("sql", CAT_CODE), ("ss", CAT_CODE), ("st", CAT_CODE), ("step", CAT_CODE),
    ("sty", CAT_CODE), ("svelte", CAT_CODE), ("swift", CAT_CODE), ("tcl", CAT_CODE),
    ("tex", CAT_CODE), ("tf", CAT_CODE), ("ts", CAT_CODE), ("tsx", CAT_CODE),
    ("vb", CAT_CODE), ("vbproj", CAT_CODE), ("vbs", CAT_CODE), ("vcxproj", CAT_CODE),
    ("v", CAT_CODE), ("vert", CAT_CODE), ("vhdl", CAT_CODE), ("vsd", CAT_CODE),
    ("vsdx", CAT_CODE), ("vue", CAT_CODE), ("wasm", CAT_CODE), ("wgsl", CAT_CODE),
    ("xaml", CAT_CODE), ("xhtml", CAT_CODE), ("xsd", CAT_CODE), ("xsl", CAT_CODE),
    ("xslt", CAT_CODE), ("zig", CAT_CODE), ("zsh", CAT_CODE),
    // config
    ("atom", CAT_CONFIG), ("cfg", CAT_CONFIG), ("conf", CAT_CONFIG),
    ("config", CAT_CONFIG), ("env", CAT_CONFIG), ("ini", CAT_CONFIG),
    ("jsonl", CAT_CONFIG), ("map", CAT_CONFIG), ("opml", CAT_CONFIG),
    ("plist", CAT_CONFIG), ("properties", CAT_CONFIG), ("rss", CAT_CONFIG),
    ("toml", CAT_CONFIG), ("webmanifest", CAT_CONFIG), ("xml", CAT_CONFIG),
    ("yaml", CAT_CONFIG), ("yml", CAT_CONFIG),
    // database
    ("avro", CAT_DATABASE), ("db", CAT_DATABASE), ("dta", CAT_DATABASE),
    ("feather", CAT_DATABASE), ("ipynb", CAT_DATABASE), ("mat", CAT_DATABASE),
    ("mdb", CAT_DATABASE), ("nb", CAT_DATABASE), ("orc", CAT_DATABASE),
    ("parquet", CAT_DATABASE), ("rda", CAT_DATABASE), ("rdata", CAT_DATABASE),
    ("rds", CAT_DATABASE), ("sav", CAT_DATABASE), ("sqlite", CAT_DATABASE),
    // archive
    ("7z", CAT_ARCHIVE), ("ar", CAT_ARCHIVE), ("br", CAT_ARCHIVE), ("bz2", CAT_ARCHIVE),
    ("bzip2", CAT_ARCHIVE), ("cab", CAT_ARCHIVE), ("cbr", CAT_ARCHIVE), ("cbz", CAT_ARCHIVE),
    ("cpio", CAT_ARCHIVE), ("gz", CAT_ARCHIVE), ("gzip", CAT_ARCHIVE), ("lz", CAT_ARCHIVE),
    ("lz4", CAT_ARCHIVE), ("lzma", CAT_ARCHIVE), ("pk3", CAT_ARCHIVE), ("pk4", CAT_ARCHIVE),
    ("rar", CAT_ARCHIVE), ("tar", CAT_ARCHIVE), ("tbz", CAT_ARCHIVE), ("tbz2", CAT_ARCHIVE),
    ("tgz", CAT_ARCHIVE), ("txz", CAT_ARCHIVE), ("war", CAT_ARCHIVE), ("xz", CAT_ARCHIVE),
    ("z", CAT_ARCHIVE), ("zip", CAT_ARCHIVE), ("zipx", CAT_ARCHIVE), ("zst", CAT_ARCHIVE),
    // executable / installer
    ("appx", CAT_EXECUTABLE), ("appxbundle", CAT_EXECUTABLE), ("apk", CAT_EXECUTABLE),
    ("deb", CAT_EXECUTABLE), ("dmg", CAT_EXECUTABLE), ("elf", CAT_EXECUTABLE),
    ("exe", CAT_EXECUTABLE), ("ipa", CAT_EXECUTABLE), ("jar", CAT_EXECUTABLE),
    ("msi", CAT_EXECUTABLE), ("msix", CAT_EXECUTABLE), ("msixbundle", CAT_EXECUTABLE),
    ("pkg", CAT_EXECUTABLE), ("rpm", CAT_EXECUTABLE), ("vsix", CAT_EXECUTABLE),
    ("whl", CAT_EXECUTABLE), ("iso", CAT_ARCHIVE), ("img", CAT_ARCHIVE), ("vhd", CAT_ARCHIVE),
    ("vhdx", CAT_ARCHIVE), ("wim", CAT_ARCHIVE), ("esd", CAT_ARCHIVE),
];

/// 分类 -> 展示名
pub fn category_label(cat: i32) -> &'static str {
    match cat {
        CAT_FOLDER => "文件夹",
        CAT_IMAGE => "图片",
        CAT_VIDEO => "视频",
        CAT_AUDIO => "音频",
        CAT_PDF => "PDF 文档",
        CAT_WORD => "Word 文档",
        CAT_PRESENTATION => "演示文稿",
        CAT_SPREADSHEET => "表格",
        CAT_BOOK => "电子书",
        CAT_NOTE => "文本",
        CAT_CODE => "代码",
        CAT_CONFIG => "配置",
        CAT_DATABASE => "数据库",
        CAT_ARCHIVE => "压缩包",
        CAT_EXECUTABLE => "可执行/安装包",
        _ => "文件",
    }
}

/// 查扩展名分类
pub fn category_for_ext(ext: &str) -> i32 {
    use std::collections::HashMap;
    use std::sync::LazyLock;
    static MAP: LazyLock<HashMap<&'static str, i32>> =
        LazyLock::new(|| EXT_TABLE.iter().copied().collect());
    if ext.is_empty() {
        return CAT_UNKNOWN;
    }
    let key = ext.to_ascii_lowercase();
    // 先试原样（表里全是小写，绝大多数调用已是小写），再试强转小写
    MAP.get(key.as_str()).copied().unwrap_or(CAT_UNKNOWN)
}

pub fn ext_of(name: &str) -> String {
    match name.rsplit_once('.') {
        Some((base, ext)) if !base.is_empty() && !ext.is_empty() => ext.to_ascii_lowercase(),
        _ => String::new(),
    }
}

// ---- 格式化 ----

pub fn format_size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut v = bytes as f64;
    let mut u = 0;
    while v >= 1024.0 && u < UNITS.len() - 1 {
        v /= 1024.0;
        u += 1;
    }
    if u == 0 {
        format!("{bytes} B")
    } else {
        format!("{v:.1} {}", UNITS[u])
    }
}

pub fn format_mtime(epoch_secs: i64) -> String {
    chrono::DateTime::from_timestamp(epoch_secs, 0)
        .map(|t| t.with_timezone(&chrono::Local).format("%Y-%m-%d %H:%M").to_string())
        .unwrap_or_default()
}

/// 面包屑分段：`D:\a\b` -> [("D:", "D:\\"), ("a", "D:\\a"), ("b", "D:\\a\\b")]
pub fn crumbs_for(path: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let p = Path::new(path);
    let mut acc = PathBuf::new();
    for comp in p.components() {
        acc.push(comp.as_os_str());
        let label = match comp {
            std::path::Component::Prefix(_) => acc.to_string_lossy().trim_end_matches('\\').to_string(),
            std::path::Component::RootDir => continue,
            c => c.as_os_str().to_string_lossy().to_string(),
        };
        // 根目录形如 "C:"，补上反斜杠
        let full = if label.ends_with(':') {
            format!("{label}\\")
        } else {
            acc.to_string_lossy().to_string()
        };
        out.push((label, full));
    }
    out
}

// ---- 目录列举 ----

const FILE_ATTRIBUTE_HIDDEN: u32 = 0x2;
const FILE_ATTRIBUTE_SYSTEM: u32 = 0x4;

/// 生成的 EntryData（字段与 ui/app.slint 的 EntryData 对应）
#[derive(Debug)]
pub struct RawEntry {
    pub name: String,
    pub path: String,
    pub is_dir: bool,
    pub size: i64,
    pub mtime: i64,
    pub ext: String,
    pub category: i32,
    pub hidden: bool,
}

pub fn list_dir(path: &Path, show_hidden: bool) -> std::io::Result<Vec<RawEntry>> {
    use std::os::windows::fs::MetadataExt;
    let mut out = Vec::new();
    for entry in std::fs::read_dir(path)? {
        let Ok(entry) = entry else { continue };
        // DirEntry::metadata() 在 Windows 复用 WIN32_FIND_DATA，零额外系统调用
        let Ok(md) = entry.metadata() else { continue };
        let attrs = md.file_attributes();
        let hidden = attrs & (FILE_ATTRIBUTE_HIDDEN | FILE_ATTRIBUTE_SYSTEM) != 0;
        if hidden && !show_hidden {
            continue;
        }
        let is_dir = md.is_dir();
        let name = entry.file_name().to_string_lossy().into_owned();
        let ext = if is_dir { String::new() } else { ext_of(&name) };
        let mtime = md
            .modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        let category = if is_dir { CAT_FOLDER } else { category_for_ext(&ext) };
        out.push(RawEntry {
            path: entry.path().to_string_lossy().into_owned(),
            name,
            is_dir,
            size: if is_dir { 0 } else { md.len() as i64 },
            mtime,
            ext,
            category,
            hidden,
        });
    }
    Ok(out)
}

/// 排序：文件夹恒优先，其次按 key。
pub fn sort_entries(entries: &mut [RawEntry], sort_by: i32, asc: bool) {
    let dir_rank = |e: &RawEntry| if e.is_dir { 0 } else { 1 };
    match sort_by {
        1 => entries.sort_by(|a, b| {
            dir_rank(a).cmp(&dir_rank(b)).then_with(|| a.size.cmp(&b.size).then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase())))
        }),
        2 => entries.sort_by(|a, b| {
            dir_rank(a)
                .cmp(&dir_rank(b))
                .then_with(|| a.category.cmp(&b.category))
                .then_with(|| a.ext.cmp(&b.ext))
                .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
        }),
        3 => entries.sort_by(|a, b| {
            dir_rank(a).cmp(&dir_rank(b)).then_with(|| b.mtime.cmp(&a.mtime)).then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
        }),
        // 0: 名称（自然序近似：小写比较）
        _ => entries.sort_by(|a, b| {
            dir_rank(a).cmp(&dir_rank(b)).then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
        }),
    }
    if !asc {
        // 反转同 rank 内的顺序：整体反转后再按 dir_rank 稳定排序
        entries.reverse();
        entries.sort_by_key(dir_rank);
    }
}

/// 磁盘列表：(盘符路径, 卷标?, 总量, 可用量, 是否可读)
pub struct DriveInfo {
    pub path: String,
    pub label: String,
    pub total: u64,
    pub free: u64,
}

pub fn list_drives() -> Vec<DriveInfo> {
    use windows_sys::Win32::Storage::FileSystem::{GetDriveTypeW, GetLogicalDrives};
    let mut out = Vec::new();
    let mask = unsafe { GetLogicalDrives() };
    if mask == 0 {
        return out;
    }
    for i in 0..26u32 {
        if mask & (1 << i) == 0 {
            continue;
        }
        let letter = (b'A' + i as u8) as char;
        let root = format!("{letter}:\\");
        let wide: Vec<u16> = root.encode_utf16().chain(std::iter::once(0)).collect();
        let dtype = unsafe { GetDriveTypeW(wide.as_ptr()) };
        // DRIVE_UNKNOWN=0 DRIVE_NO_ROOT_DIR=1：跳过不可访问的
        if dtype == 0 || dtype == 1 {
            continue;
        }
        let (label, total, free) = drive_label_and_space(&root);
        let _ = dtype;
        out.push(DriveInfo { path: root, label, total, free });
    }
    out
}

fn drive_label_and_space(root: &str) -> (String, u64, u64) {
    use windows_sys::Win32::Storage::FileSystem::GetDiskFreeSpaceExW;
    let wide: Vec<u16> = root.encode_utf16().chain(std::iter::once(0)).collect();
    let mut total: u64 = 0;
    let mut free: u64 = 0;
    let ok = unsafe {
        GetDiskFreeSpaceExW(wide.as_ptr(), std::ptr::null_mut(), &mut total, &mut free)
    };
    let label = if ok != 0 { format!("本地磁盘 ({})", root.trim_end_matches('\\')) } else { root.trim_end_matches('\\').to_string() };
    (label, total, free)
}

// ---- 快速访问目录 ----

/// 用 SHGetKnownFolderPath 解析 Known Folder（桌面/文档等常被 OneDrive 重定向，
/// 不能硬编码 USERPROFILE 子目录）。
pub fn known_dir(guid: &windows_sys::core::GUID) -> Option<String> {
    unsafe {
        let mut pw: *mut u16 = std::ptr::null_mut();
        if windows_sys::Win32::UI::Shell::SHGetKnownFolderPath(
            guid,
            0,
            std::ptr::null_mut(),
            &mut pw,
        ) == 0
            && !pw.is_null()
        {
            let len = (0..).take_while(|&i| *pw.add(i) != 0).count();
            let s = String::from_utf16_lossy(std::slice::from_raw_parts(pw, len));
            windows_sys::Win32::System::Com::CoTaskMemFree(pw as _);
            Some(s)
        } else {
            None
        }
    }
}

pub fn quick_access() -> Vec<(String, String)> {
    use windows_sys::Win32::UI::Shell::{FOLDERID_Desktop, FOLDERID_Documents, FOLDERID_Downloads, FOLDERID_Music, FOLDERID_Pictures, FOLDERID_Videos};
    let home = std::env::var("USERPROFILE").unwrap_or_default();
    let mut items: Vec<(String, String)> = Vec::new();
    let mut push = |label: &str, p: Option<String>| {
        if let Some(p) = p.filter(|p| Path::new(p).is_dir()) {
            items.push((label.to_string(), p));
        }
    };
    push("桌面", known_dir(&FOLDERID_Desktop).or(Some(format!("{home}\\Desktop"))));
    push("下载", known_dir(&FOLDERID_Downloads).or(Some(format!("{home}\\Downloads"))));
    push("文档", known_dir(&FOLDERID_Documents).or(Some(format!("{home}\\Documents"))));
    push("图片", known_dir(&FOLDERID_Pictures).or(Some(format!("{home}\\Pictures"))));
    push("音乐", known_dir(&FOLDERID_Music).or(Some(format!("{home}\\Music"))));
    push("视频", known_dir(&FOLDERID_Videos).or(Some(format!("{home}\\Videos"))));
    items
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_category() {
        assert_eq!(category_for_ext("jpg"), CAT_IMAGE);
        assert_eq!(category_for_ext("JPG"), CAT_IMAGE);
        assert_eq!(category_for_ext("MKV"), CAT_VIDEO);
        assert_eq!(category_for_ext("rs"), CAT_CODE);
        assert_eq!(category_for_ext("zip"), CAT_ARCHIVE);
        assert_eq!(category_for_ext("exe"), CAT_EXECUTABLE);
        assert_eq!(category_for_ext("不存在的"), CAT_UNKNOWN);
        assert_eq!(category_for_ext(""), CAT_UNKNOWN);
    }

    #[test]
    fn test_ext_of() {
        assert_eq!(ext_of("a.BMP"), "bmp");
        assert_eq!(ext_of("noext"), "");
        assert_eq!(ext_of(".gitignore"), "");
        assert_eq!(ext_of("archive.tar.gz"), "gz");
    }

    #[test]
    fn test_format_size() {
        assert_eq!(format_size(0), "0 B");
        assert_eq!(format_size(1023), "1023 B");
        assert_eq!(format_size(1024), "1.0 KB");
        assert_eq!(format_size(1536), "1.5 KB");
        assert_eq!(format_size(1024 * 1024 * 3), "3.0 MB");
    }

    #[test]
    fn test_crumbs() {
        let c = crumbs_for(r"D:\code\windFileManger");
        assert_eq!(c.len(), 3);
        assert_eq!(c[0], ("D:".to_string(), "D:\\".to_string()));
        assert_eq!(c[1], ("code".to_string(), "D:\\code".to_string()));
        assert_eq!(c[2], ("windFileManger".to_string(), "D:\\code\\windFileManger".to_string()));
    }

    #[test]
    fn test_sort_dirs_first() {
        let mut es = vec![
            RawEntry { name: "z.txt".into(), path: "".into(), is_dir: false, size: 9, mtime: 1, ext: "txt".into(), category: CAT_NOTE, hidden: false },
            RawEntry { name: "dir".into(), path: "".into(), is_dir: true, size: 0, mtime: 2, ext: "".into(), category: CAT_FOLDER, hidden: false },
            RawEntry { name: "a.txt".into(), path: "".into(), is_dir: false, size: 1, mtime: 3, ext: "txt".into(), category: CAT_NOTE, hidden: false },
        ];
        sort_entries(&mut es, 0, true);
        assert_eq!(es[0].name, "dir");
        assert_eq!(es[1].name, "a.txt");
        assert_eq!(es[2].name, "z.txt");
        sort_entries(&mut es, 0, false);
        assert_eq!(es[0].name, "dir");
        assert_eq!(es[1].name, "z.txt");
        assert_eq!(es[2].name, "a.txt");
    }
}
