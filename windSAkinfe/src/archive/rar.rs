//! RAR 只读支持（RAR 4.x 与 RAR 5.x 容器），**仅 store（不压缩）条目**。
//!
//! 为什么只能做到这里：RAR 的私有压缩算法规范从未公开，逆向复刻属于法律与
//! 工程双重禁区（见需求说明书 §1.2）。因此本模块：
//! - 能识别容器结构、列出条目、解出以「存储」方式打包的条目；
//! - 遇到私有压缩方法 / AES 加密 / 固实块 → 返回 `Unsupported`，由上层报退出码 5。
//!
//! 创建 RAR 完全不支持。

use std::io;

use crate::algo::hash::crc32;
use crate::util::{read_varint7z, ByteReader};

const MARKER4: [u8; 7] = [0x52, 0x61, 0x72, 0x21, 0x1A, 0x07, 0x00];
const MARKER5: [u8; 8] = [0x52, 0x61, 0x72, 0x21, 0x1A, 0x07, 0x01, 0x00];

fn bad(msg: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg.into())
}

fn unsupported(msg: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::Unsupported, msg.into())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RarVersion {
    V4,
    V5,
}

#[derive(Debug, Clone)]
pub struct RarEntry {
    pub name: String,
    pub unpacked_size: u64,
    pub packed_size: u64,
    pub is_dir: bool,
    pub is_encrypted: bool,
    /// 是否存储方式（可解）；false 表示私有压缩方法
    pub is_store: bool,
    pub crc32: Option<u32>,
    /// 数据在原始字节流中的偏移
    pub data_offset: usize,
    pub method: u32,
    /// 压缩算法名（用于报错提示）
    pub method_name: String,
}

/// 只按签名判断版本，不解析内容。
pub fn detect_version(data: &[u8]) -> Option<RarVersion> {
    if data.starts_with(&MARKER5) {
        Some(RarVersion::V5)
    } else if data.starts_with(&MARKER4) {
        Some(RarVersion::V4)
    } else {
        None
    }
}

pub struct RarArchive {
    pub version: RarVersion,
    pub data: Vec<u8>,
    pub entries: Vec<RarEntry>,
}

impl RarArchive {
    pub fn open(data: Vec<u8>) -> io::Result<RarArchive> {
        if data.starts_with(&MARKER5) {
            let entries = parse_rar5(&data)?;
            Ok(RarArchive { version: RarVersion::V5, data, entries })
        } else if data.starts_with(&MARKER4) {
            let entries = parse_rar4(&data)?;
            Ok(RarArchive { version: RarVersion::V4, data, entries })
        } else {
            Err(bad("不是 RAR 文件（签名不符）"))
        }
    }

    /// 解出单个条目。非存储方式或加密 → Unsupported。
    pub fn read(&self, idx: usize) -> io::Result<Vec<u8>> {
        let e = self
            .entries
            .get(idx)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "条目索引越界"))?;
        if e.is_encrypted {
            return Err(unsupported(format!(
                "“{}”是 RAR 加密条目，本工具不支持解密 RAR",
                e.name
            )));
        }
        if !e.is_store {
            return Err(unsupported(format!(
                "“{}”使用了 RAR 私有压缩方法（{}），规范未公开，无法解压",
                e.name, e.method_name
            )));
        }
        let start = e.data_offset;
        let end = start + e.packed_size as usize;
        if end > self.data.len() {
            return Err(bad("RAR 条目数据被截断"));
        }
        let body = self.data[start..end].to_vec();
        if body.len() as u64 != e.unpacked_size {
            return Err(bad("RAR store 条目长度与声明不符"));
        }
        if let Some(want) = e.crc32 {
            let got = crc32(&body);
            if want != got && want != 0 {
                return Err(bad(format!(
                    "“{}”CRC32 校验失败（期望 {want:08x}，实际 {got:08x}）",
                    e.name
                )));
            }
        }
        Ok(body)
    }

    pub fn extract_all(&self) -> io::Result<Vec<Vec<u8>>> {
        let mut out = Vec::with_capacity(self.entries.len());
        for i in 0..self.entries.len() {
            if self.entries[i].is_dir {
                out.push(Vec::new());
            } else {
                out.push(self.read(i)?);
            }
        }
        Ok(out)
    }

    /// 是否存在无法解出的条目（用于上层决定是否提前告警）。
    pub fn has_unsupported(&self) -> bool {
        self.entries.iter().any(|e| !e.is_store || e.is_encrypted)
    }
}

// ---------------------------------------------------------------- RAR 4

fn method_name4(m: u8) -> String {
    match m {
        0x30 => "store".to_string(),
        0x31..=0x35 => format!("rar-m{}", m - 0x30),
        _ => format!("unknown-{m:#04x}"),
    }
}

fn parse_rar4(data: &[u8]) -> io::Result<Vec<RarEntry>> {
    let mut out = Vec::new();
    let mut pos = MARKER4.len();
    while pos + 7 <= data.len() {
        let h = &data[pos..];
        let head_type = h[2];
        let head_flags = u16::from_le_bytes([h[3], h[4]]);
        let head_size = u16::from_le_bytes([h[5], h[6]]) as usize;
        if head_size < 7 || pos + head_size > data.len() {
            return Err(bad("RAR4 块头长度非法"));
        }
        let long_block = head_flags & 0x8000 != 0;
        let base = if long_block { 11 } else { 7 };
        match head_type {
            0x74 => {
                // FILE_HEAD
                if head_size < base + 25 {
                    return Err(bad("RAR4 文件头过短"));
                }
                let mut r = ByteReader::new(&h[base..head_size]);
                let pack_size = r.u32le()? as u64;
                let unp_size = r.u32le()? as u64;
                let _host_os = r.u8()?;
                let file_crc = r.u32le()?;
                let _ftime = r.u32le()?;
                let _unp_ver = r.u8()?;
                let method = r.u8()?;
                let name_size = r.u16le()? as usize;
                let _attr = r.u32le()?;
                let mut pack_size = pack_size;
                let mut unp_size = unp_size;
                if head_flags & 0x0100 != 0 {
                    // LHD_LARGE
                    pack_size |= (r.u32le()? as u64) << 32;
                    unp_size |= (r.u32le()? as u64) << 32;
                }
                let name_bytes = r.take(name_size)?;
                let name = decode_rar4_name(name_bytes);
                let is_dir = head_flags & 0x00E0 == 0x00E0 || name.ends_with('\\') || name.ends_with('/');
                let is_encrypted = head_flags & 0x0004 != 0;
                out.push(RarEntry {
                    name: name.trim_end_matches(['\\', '/']).replace('\\', "/"),
                    unpacked_size: if is_dir { 0 } else { unp_size },
                    packed_size: if is_dir { 0 } else { pack_size },
                    is_dir,
                    is_encrypted,
                    is_store: method == 0x30,
                    crc32: if is_dir { None } else { Some(file_crc) },
                    data_offset: pos + head_size,
                    method: method as u32,
                    method_name: method_name4(method),
                });
                pos += head_size;
                // 跳过数据（add_size 已在 long_block 里；这里用 pack_size 更可靠）
                pos += pack_size as usize;
                continue;
            }
            0x7B => break, // ENDARC
            _ => {}
        }
        // 其他块：用 ADD_SIZE 或 HEAD_SIZE 前进
        let add = if long_block {
            u32::from_le_bytes([h[7], h[8], h[9], h[10]]) as usize
        } else {
            0
        };
        pos += head_size + add;
    }
    Ok(out)
}

/// RAR4 名字：默认是单字节 OEM 代码页；带 0x0200 标志时是 UTF-16LE。
fn decode_rar4_name(bytes: &[u8]) -> String {
    if bytes.len() >= 2 && bytes.len() % 2 == 0 {
        // 先尝试 UTF-16LE（RAR4 的 Unicode 名字用这个形式，且高位字节多为 0）
        let mut u16s = Vec::with_capacity(bytes.len() / 2);
        let mut ok = true;
        for ch in bytes.chunks_exact(2) {
            let v = u16::from_le_bytes([ch[0], ch[1]]);
            if v == 0 {
                ok = false;
                break;
            }
            u16s.push(v);
        }
        if ok {
            if let Ok(s) = String::from_utf16(&u16s) {
                // 只有当字节流看起来确实是 UTF-16 才采用（ASCII 区高位为 0）
                if (bytes.chunks_exact(2).all(|c| c[1] == 0) || s.chars().all(|c| c as u32 >= 0x80 || c.is_ascii()))
                    && !bytes.chunks_exact(2).all(|c| c[1] == 0) {
                        return s;
                    }
            }
        }
    }
    // 回退：按 UTF-8 解，失败再按 Latin-1
    match std::str::from_utf8(bytes) {
        Ok(s) => s.to_string(),
        Err(_) => bytes.iter().map(|&b| b as char).collect(),
    }
}

// ---------------------------------------------------------------- RAR 5

fn parse_rar5(data: &[u8]) -> io::Result<Vec<RarEntry>> {
    let mut out = Vec::new();
    let mut pos = MARKER5.len();
    while pos + 5 <= data.len() {
        let crc_stored = u32::from_le_bytes([data[pos], data[pos + 1], data[pos + 2], data[pos + 3]]);
        let mut r = ByteReader::new(&data[pos + 4..]);
        let head_size = read_varint7z(&mut r)? as usize;
        if head_size == 0 || pos + 4 + head_size > data.len() {
            return Err(bad("RAR5 块头长度非法"));
        }
        // 校验：CRC32 覆盖 HEAD_SIZE 字段 + 头部数据
        let crc_cover_start = pos + 4;
        let crc_cover_end = pos + 4 + head_size;
        if crc32(&data[crc_cover_start..crc_cover_end]) != crc_stored {
            return Err(bad("RAR5 块头 CRC32 校验失败"));
        }
        let body_start = pos + 4 + head_size;
        let mut hr = ByteReader::new(&data[crc_cover_start..crc_cover_end]);
        read_varint7z(&mut hr)?; // head_size 已读
        let head_type = read_varint7z(&mut hr)?;
        let head_flags = read_varint7z(&mut hr)?;
        let extra_size = if head_flags & 0x0001 != 0 { read_varint7z(&mut hr)? as usize } else { 0 };
        let data_size = if head_flags & 0x0002 != 0 { read_varint7z(&mut hr)? as usize } else { 0 };
        match head_type {
            2 => {
                // FILE
                let file_flags = read_varint7z(&mut hr)?;
                let unp_size = read_varint7z(&mut hr)?;
                let _attr = read_varint7z(&mut hr)?;
                if file_flags & 0x0002 != 0 {
                    hr.u32le()?; // mtime
                }
                let crc = if file_flags & 0x0004 != 0 { Some(hr.u32le()?) } else { None };
                let comp_info = read_varint7z(&mut hr)?;
                let _host_os = read_varint7z(&mut hr)?;
                let name_size = read_varint7z(&mut hr)? as usize;
                let name_bytes = hr.take(name_size)?;
                let name = String::from_utf8_lossy(name_bytes).replace('\\', "/");
                let is_dir = file_flags & 0x0001 != 0;
                // COMP_INFO: bits0-5 version, bit6 solid, bits7-9 method, bits10-13 dict
                let method = (comp_info >> 7) & 0x07;
                let is_encrypted = file_flags & 0x0008 != 0; // 加密标志在 file_flags 之外，稍后由 service 块修正
                let entry = RarEntry {
                    name: name.trim_end_matches('/').to_string(),
                    unpacked_size: if is_dir { 0 } else { unp_size },
                    packed_size: if is_dir { 0 } else { data_size as u64 },
                    is_dir,
                    is_encrypted,
                    is_store: method == 0,
                    crc32: if is_dir { None } else { crc },
                    data_offset: body_start + extra_size,
                    method: method as u32,
                    method_name: if method == 0 { "store".into() } else { format!("rar5-m{method}") },
                };
                out.push(entry);
            }
            4 => {
                return Err(unsupported("RAR5 归档使用 AES 加密，本工具不支持解密 RAR"));
            }
            5 => break, // END
            3 => { /* service header（含加密服务块），忽略 */ }
            _ => {}
        }
        pos = body_start + extra_size + data_size;
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_non_rar() {
        assert!(RarArchive::open(b"not rar at all".to_vec()).is_err());
        assert!(RarArchive::open(Vec::new()).is_err());
    }

    #[test]
    fn detects_versions() {
        let v4 = [MARKER4.as_slice(), &[0u8; 32]].concat();
        let v5 = [MARKER5.as_slice(), &[0u8; 32]].concat();
        assert_eq!(detect_version(&v4), Some(RarVersion::V4));
        assert_eq!(detect_version(&v5), Some(RarVersion::V5));
        assert_eq!(detect_version(b"nope"), None);
        // 签名正确但内容损坏时必须报错而不是静默成功
        assert!(RarArchive::open(v4).is_err());
        assert!(RarArchive::open(v5).is_err());
    }

    #[test]
    fn method_names() {
        assert_eq!(method_name4(0x30), "store");
        assert_eq!(method_name4(0x33), "rar-m3");
    }

    #[test]
    fn ascii_name_decode() {
        assert_eq!(decode_rar4_name(b"hello.txt"), "hello.txt");
        // UTF-16LE 形式（部分实现用）
        let mut b = Vec::new();
        for u in "文件.txt".encode_utf16() {
            b.extend_from_slice(&u.to_le_bytes());
        }
        assert_eq!(decode_rar4_name(&b), "文件.txt");
    }
}
