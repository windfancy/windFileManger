//! 预览面板数据：图片（slint::Image）/ 文本（前 64 KB）/ 元信息

use std::path::Path;

pub const TEXT_PREVIEW_BYTES: usize = 64 * 1024;
pub const MAX_IMAGE_BYTES: u64 = 30 * 1024 * 1024;

pub struct PreviewData {
    pub kind: i32, // 0 无/元信息, 1 文本, 2 图片（图片由 UI 线程另行加载）
    pub text: String,
}

pub fn load(path: &str, is_dir: bool, size: i64, mtime: i64, category: i32) -> PreviewData {
    let _ = (size, mtime);
    if is_dir {
        return PreviewData { kind: 0, text: String::new() };
    }
    let p = Path::new(path);
    let Ok(md) = std::fs::metadata(p) else {
        return PreviewData { kind: 0, text: String::new() };
    };
    let len = md.len();
    if category == crate::fs::CAT_IMAGE && len > 0 && len <= MAX_IMAGE_BYTES {
        // 图片由 UI 线程用 slint::Image::load_from_path 加载（Image 非 Send）
        return PreviewData { kind: 2, text: String::new() };
    }
    if matches!(category, crate::fs::CAT_NOTE | crate::fs::CAT_CODE | crate::fs::CAT_CONFIG) && len > 0 {
        // 文本类（note/code/config）按文本预览
        if let Ok(bytes) = std::fs::read(p) {
            let mut slice = &bytes[..bytes.len().min(TEXT_PREVIEW_BYTES)];
            // 去 BOM
            if slice.starts_with(&[0xEF, 0xBB, 0xBF]) {
                slice = &slice[3..];
            }
            let mut text = String::from_utf8_lossy(slice).into_owned();
            if bytes.len() > TEXT_PREVIEW_BYTES {
                text.push_str("\n\n…（已截断，仅显示前 64 KB）");
            }
            return PreviewData { kind: 1, text };
        }
    }
    PreviewData { kind: 0, text: String::new() }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_text_preview_truncate() {
        let tmp = std::env::temp_dir().join(format!("wfm_prev_{}", std::process::id()));
        std::fs::create_dir_all(&tmp).unwrap();
        let f = tmp.join("a.txt");
        std::fs::write(&f, vec![b'a'; 100]).unwrap();
        let d = load(f.to_str().unwrap(), false, 100, 0, crate::fs::CAT_NOTE);
        assert_eq!(d.kind, 1);
        assert!(d.text.starts_with('a'));
        std::fs::write(&f, vec![0xEF, 0xBB, 0xBF, b'h', b'i']).unwrap();
        let d = load(f.to_str().unwrap(), false, 5, 0, crate::fs::CAT_NOTE);
        assert_eq!(d.text, "hi");
        std::fs::remove_dir_all(&tmp).unwrap();
    }

    #[test]
    fn test_dir_no_preview() {
        let tmp = std::env::temp_dir();
        let d = load(tmp.to_str().unwrap(), true, 0, 0, crate::fs::CAT_FOLDER);
        assert_eq!(d.kind, 0);
    }
}
