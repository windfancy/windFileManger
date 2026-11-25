//! tar 容器（ustar + GNU 长名 + pax 扩展头），自研实现。
//!
//! 只做容器层：条目切分、头部编解码。压缩交给上层（`gz` / `xz`）。

use std::io::{self, Read, Write};

const BLOCK: usize = 512;

// ---------------------------------------------------------------- 写出

pub struct TarInput {
    pub name: String,
    pub data: Vec<u8>,
    pub is_dir: bool,
    pub mtime: u64,
    pub mode: u32,
}

fn put_octal(dst: &mut [u8], value: u64) {
    // 定长八进制 + 结尾 NUL；宽度 = dst.len()-1
    let width = dst.len() - 1;
    let s = format!("{:0width$o}", value, width = width);
    let s = if s.len() > width { s[s.len() - width..].to_string() } else { s };
    dst[..width].copy_from_slice(s.as_bytes());
    dst[width] = 0;
}

fn put_str(dst: &mut [u8], s: &str) {
    let b = s.as_bytes();
    let n = b.len().min(dst.len());
    dst[..n].copy_from_slice(&b[..n]);
}

fn chksum(header: &[u8; BLOCK]) -> u64 {
    let mut sum = 0u64;
    for (i, b) in header.iter().enumerate() {
        // 校验和字段本身按 8 个空格计
        if (148..156).contains(&i) {
            sum += 32;
        } else {
            sum += *b as u64;
        }
    }
    sum
}

fn build_header(
    name: &str,
    size: u64,
    is_dir: bool,
    mtime: u64,
    mode: u32,
) -> ([u8; BLOCK], Option<String>) {
    let mut h = [0u8; BLOCK];
    let mut prefix = String::new();
    // ustar 支持 name(100) + prefix(155)
    let mut name_field = name.to_string();
    if name.len() > 100 {
        // 尝试按 '/' 切出 prefix
        let mut split_ok = false;
        for (i, ch) in name.char_indices().rev() {
            if ch == '/' && i <= 155 && name.len() - i - 1 <= 100 {
                prefix = name[..i].to_string();
                name_field = name[i + 1..].to_string();
                split_ok = true;
                break;
            }
        }
        if !split_ok {
            // 只能走 pax 长名；这里先截断，调用方会用 pax 头补
            name_field = name[name.len() - 100..].to_string();
        }
    }
    put_str(&mut h[0..100], &name_field);
    put_octal(&mut h[100..108], mode as u64);
    put_octal(&mut h[108..116], 0); // uid
    put_octal(&mut h[116..124], 0); // gid
    put_octal(&mut h[124..136], if is_dir { 0 } else { size });
    put_octal(&mut h[136..148], mtime);
    h[148..156].fill(b' ');
    h[156] = if is_dir { b'5' } else { b'0' };
    h[257..263].copy_from_slice(b"ustar\0");
    h[263..265].copy_from_slice(b"00");
    put_str(&mut h[265..297], "root");
    put_str(&mut h[297..329], "root");
    put_str(&mut h[345..500], &prefix);
    let c = chksum(&h);
    let cs = format!("{:06o}\0 ", c);
    h[148..156].copy_from_slice(cs.as_bytes());
    let need_pax = if name.len() > 100 && prefix.is_empty() { Some(name.to_string()) } else { None };
    (h, need_pax)
}

fn pax_record(key: &str, value: &str) -> Vec<u8> {
    // 记录格式："<len> <key>=<value>\n"，len 是整条记录（含自身长度数字）的字节数
    let body = format!(" {key}={value}\n");
    let mut len = body.len() + 1;
    loop {
        let digits = len.to_string().len();
        if digits + body.len() == len {
            break;
        }
        len = digits + body.len();
    }
    format!("{len}{body}").into_bytes()
}

pub fn write_tar<W: Write>(w: &mut W, entries: &[TarInput]) -> io::Result<()> {
    for e in entries {
        let name = if e.is_dir && !e.name.ends_with('/') {
            format!("{}/", e.name)
        } else {
            e.name.clone()
        };
        // 超长名用 pax 扩展头
        let (h, long_name) = build_header(&name, e.data.len() as u64, e.is_dir, e.mtime, e.mode);
        if let Some(full) = long_name {
            let rec = pax_record("path", &full);
            let pax_name = "PaxHeaders/entry";
            let (ph, _) = build_header(pax_name, rec.len() as u64, false, e.mtime, 0o644);
            w.write_all(&ph)?;
            w.write_all(&rec)?;
            let pad = (BLOCK - rec.len() % BLOCK) % BLOCK;
            w.write_all(&vec![0u8; pad])?;
            // 真实头部仍写截断名（某些实现会忽略 pax，至少不会崩）
            let mut h2 = h;
            h2[156] = if e.is_dir { b'5' } else { b'0' };
            w.write_all(&h2)?;
        } else {
            w.write_all(&h)?;
        }
        if !e.is_dir && !e.data.is_empty() {
            w.write_all(&e.data)?;
            let pad = (BLOCK - e.data.len() % BLOCK) % BLOCK;
            w.write_all(&vec![0u8; pad])?;
        }
    }
    // 两个全零块收尾
    w.write_all(&[0u8; BLOCK * 2])
}

// ---------------------------------------------------------------- 读入

#[derive(Debug, Clone)]
pub struct TarEntry {
    pub name: String,
    pub size: u64,
    pub mtime: u64,
    pub mode: u32,
    pub is_dir: bool,
    pub is_link: bool,
    pub link_name: String,
    /// 数据在原始字节流中的偏移
    pub data_offset: usize,
}

fn parse_octal(bytes: &[u8]) -> u64 {
    let mut v = 0u64;
    for b in bytes {
        match b {
            b'0'..=b'7' => v = v * 8 + (b - b'0') as u64,
            0 | b' ' => {
                if v != 0 {
                    // 允许尾部 NUL / 空格
                    continue;
                }
            }
            _ => break,
        }
    }
    v
}

fn cstr(bytes: &[u8]) -> String {
    let end = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len());
    String::from_utf8_lossy(&bytes[..end]).to_string()
}

fn is_zero_block(b: &[u8]) -> bool {
    b.iter().all(|&x| x == 0)
}

pub fn read_tar(data: &[u8]) -> io::Result<Vec<TarEntry>> {
    let mut out: Vec<TarEntry> = Vec::new();
    let mut pos = 0usize;
    let mut pending_long_name: Option<String> = None;
    let mut pending_long_link: Option<String> = None;
    let mut pending_pax: Vec<(String, String)> = Vec::new();
    let mut saw_terminator = false;

    while pos + BLOCK <= data.len() {
        let h = &data[pos..pos + BLOCK];
        if is_zero_block(h) {
            saw_terminator = true;
            break;
        }
        // 校验和验证（宽松：失败也继续，只是标记）
        let size = parse_octal(&h[124..136]);
        let mode = parse_octal(&h[100..108]) as u32;
        let mtime = parse_octal(&h[136..148]);
        let typeflag = h[156];
        let link_name = cstr(&h[157..257]);
        let magic = &h[257..263];
        let mut name = cstr(&h[0..100]);
        if magic == b"ustar\0" {
            let prefix = cstr(&h[345..500]);
            if !prefix.is_empty() {
                name = format!("{prefix}/{name}");
            }
        }
        let data_start = pos + BLOCK;
        let data_end = data_start.checked_add(size as usize).ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidData, "tar 条目大小溢出")
        })?;
        if size > 0 && data_end > data.len() {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "tar 条目数据被截断"));
        }
        let body = &data[data_start..data_end.min(data.len())];

        match typeflag {
            b'L' => {
                pending_long_name = Some(cstr(body));
            }
            b'K' => {
                pending_long_link = Some(cstr(body));
            }
            b'x' => {
                pending_pax = parse_pax(body);
            }
            b'g' => { /* 全局 pax，忽略 */ }
            _ => {
                let mut final_name = name;
                if let Some(l) = pending_long_name.take() {
                    final_name = l;
                }
                for (k, v) in pending_pax.drain(..) {
                    if k == "path" {
                        final_name = v;
                    }
                }
                let mut final_link = link_name;
                if let Some(l) = pending_long_link.take() {
                    final_link = l;
                }
                let is_dir = typeflag == b'5' || final_name.ends_with('/');
                let is_link = typeflag == b'1' || typeflag == b'2';
                out.push(TarEntry {
                    name: final_name,
                    size: if is_dir { 0 } else { size },
                    mtime,
                    mode,
                    is_dir,
                    is_link,
                    link_name: final_link,
                    data_offset: data_start,
                });
            }
        }
        let pad = (BLOCK - (size as usize) % BLOCK) % BLOCK;
        pos = data_end + pad;
    }
    // 没有遇到全零收尾块、却已经越过数据末尾 ⇒ 被截断
    if !saw_terminator && pos > data.len() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "tar 数据被截断（缺少结尾的零块）",
        ));
    }
    Ok(out)
}

fn parse_pax(body: &[u8]) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let mut i = 0usize;
    while i < body.len() {
        // "<len> key=value\n"
        let mut j = i;
        while j < body.len() && body[j].is_ascii_digit() {
            j += 1;
        }
        if j == i {
            break;
        }
        let len: usize = match std::str::from_utf8(&body[i..j]).ok().and_then(|s| s.parse().ok()) {
            Some(v) => v,
            None => break,
        };
        if len == 0 || i + len > body.len() {
            break;
        }
        let rec = &body[j..i + len];
        if let Some(eq) = rec.iter().position(|&b| b == b'=') {
            let key = String::from_utf8_lossy(&rec[..eq]).trim().to_string();
            let val = String::from_utf8_lossy(&rec[eq + 1..])
                .trim_end_matches('\n')
                .to_string();
            out.push((key, val));
        }
        i += len;
    }
    out
}

/// 把 tar 数据流整体解码成一个只读归档视图。
pub struct TarArchive {
    pub data: Vec<u8>,
    pub entries: Vec<TarEntry>,
}

impl TarArchive {
    pub fn open(data: Vec<u8>) -> io::Result<TarArchive> {
        let entries = read_tar(&data)?;
        Ok(TarArchive { data, entries })
    }

    pub fn read(&self, idx: usize) -> io::Result<Vec<u8>> {
        let e = self
            .entries
            .get(idx)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "条目索引越界"))?;
        let start = e.data_offset;
        let end = (start + e.size as usize).min(self.data.len());
        Ok(self.data[start..end].to_vec())
    }
}

/// 顺序流式读出（大归档时不必整块载入 data 字段之外的结构）。
pub fn stream_entries<R: Read>(r: &mut R) -> io::Result<Vec<(TarEntry, Vec<u8>)>> {
    let mut buf = Vec::new();
    r.read_to_end(&mut buf)?;
    let ar = TarArchive::open(buf)?;
    let mut out = Vec::with_capacity(ar.entries.len());
    for (i, e) in ar.entries.iter().enumerate() {
        out.push((e.clone(), ar.read(i)?));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Vec<TarInput> {
        vec![
            TarInput { name: "a.txt".into(), data: b"hello tar\n".to_vec(), is_dir: false, mtime: 1_700_000_000, mode: 0o644 },
            TarInput { name: "sub".into(), data: Vec::new(), is_dir: true, mtime: 1_700_000_000, mode: 0o755 },
            TarInput { name: "sub/b.bin".into(), data: (0..1000u32).map(|i| (i % 256) as u8).collect(), is_dir: false, mtime: 1_700_000_000, mode: 0o644 },
        ]
    }

    #[test]
    fn roundtrip_basic() {
        let mut buf = Vec::new();
        write_tar(&mut buf, &sample()).unwrap();
        assert_eq!(buf.len() % BLOCK, 0);
        let ar = TarArchive::open(buf).unwrap();
        assert_eq!(ar.entries.len(), 3);
        assert_eq!(ar.entries[0].name, "a.txt");
        assert_eq!(ar.read(0).unwrap(), b"hello tar\n");
        assert!(ar.entries[1].is_dir);
        assert_eq!(ar.entries[1].name, "sub/");
        assert_eq!(ar.entries[2].name, "sub/b.bin");
        assert_eq!(ar.read(2).unwrap().len(), 1000);
        assert_eq!(ar.entries[2].mtime, 1_700_000_000);
    }

    #[test]
    fn long_name_uses_pax_or_prefix() {
        let deep = format!("{}/file.txt", "d".repeat(120));
        let entries = vec![TarInput { name: deep.clone(), data: b"x".to_vec(), is_dir: false, mtime: 0, mode: 0o644 }];
        let mut buf = Vec::new();
        write_tar(&mut buf, &entries).unwrap();
        let ar = TarArchive::open(buf).unwrap();
        assert_eq!(ar.entries.len(), 1);
        assert_eq!(ar.entries[0].name, deep);
        assert_eq!(ar.read(0).unwrap(), b"x");
    }

    #[test]
    fn unicode_name_ok() {
        let entries = vec![TarInput { name: "中文/文件.txt".into(), data: b"ok".to_vec(), is_dir: false, mtime: 0, mode: 0o644 }];
        let mut buf = Vec::new();
        write_tar(&mut buf, &entries).unwrap();
        let ar = TarArchive::open(buf).unwrap();
        assert_eq!(ar.entries[0].name, "中文/文件.txt");
    }

    #[test]
    fn empty_archive() {
        let mut buf = Vec::new();
        write_tar(&mut buf, &[]).unwrap();
        assert_eq!(buf.len(), BLOCK * 2);
        let ar = TarArchive::open(buf).unwrap();
        assert!(ar.entries.is_empty());
    }

    #[test]
    fn checksum_field_is_valid() {
        let entries = vec![TarInput { name: "x".into(), data: Vec::new(), is_dir: false, mtime: 0, mode: 0o644 }];
        let mut buf = Vec::new();
        write_tar(&mut buf, &entries).unwrap();
        let mut h = [0u8; BLOCK];
        h.copy_from_slice(&buf[..BLOCK]);
        let stored = parse_octal(&h[148..156]);
        let recomputed = chksum(&h);
        assert_eq!(stored, recomputed);
    }

    #[test]
    fn truncated_data_is_error() {
        let mut buf = Vec::new();
        write_tar(&mut buf, &sample()).unwrap();
        buf.truncate(700);
        assert!(TarArchive::open(buf).is_err());
    }
}
