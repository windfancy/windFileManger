//! ZIP 容器（PKWARE APPNOTE）自研读写。
//!
//! 支持：store / deflate、UTF-8 文件名、ZipCrypto 传统加密（读写）、Zip64 读取、
//! CRC32 校验。不创建 AES 加密条目（遇到 method 99 明确报错）。

use std::io::{self, Write};

use crate::algo::hash::{crc32, crc32_raw};
use crate::codec::deflate;
use crate::util::{ByteReader, Civil};

const SIG_LOCAL: u32 = 0x0403_4b50;
const SIG_CD: u32 = 0x0201_4b50;
const SIG_EOCD: u32 = 0x0605_4b50;
const SIG_Z64_EOCD: u32 = 0x0606_4b50;

pub const METHOD_STORE: u16 = 0;
pub const METHOD_DEFLATE: u16 = 8;

pub fn bad(msg: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg.into())
}

// ---------------------------------------------------------------- ZipCrypto

pub struct ZipCrypto {
    k0: u32,
    k1: u32,
    k2: u32,
}

impl ZipCrypto {
    pub fn new(password: &[u8]) -> Self {
        let mut s = Self { k0: 0x1234_5678, k1: 0x2345_6789, k2: 0x3456_7890 };
        for &b in password {
            s.update(b);
        }
        s
    }
    fn update(&mut self, b: u8) {
        self.k0 = crc32_raw(self.k0, b);
        self.k1 = self
            .k1
            .wrapping_add(self.k0 & 0xFF)
            .wrapping_mul(134_775_813)
            .wrapping_add(1);
        self.k2 = crc32_raw(self.k2, (self.k1 >> 24) as u8);
    }
    fn stream_byte(&self) -> u8 {
        let t = (self.k2 & 0xFFFF) | 2;
        ((t.wrapping_mul(t ^ 1)) >> 8) as u8
    }

    /// 加密：`update_keys` 喂**明文**（ZipCrypto 规范）。
    pub fn encrypt(&mut self, data: &mut [u8]) {
        for b in data.iter_mut() {
            let k = self.stream_byte();
            let plain = *b;
            *b = plain ^ k;
            self.update(plain);
        }
    }

    /// 解密：解出明文后用**明文**推进密钥状态。
    pub fn decrypt(&mut self, data: &mut [u8]) {
        for b in data.iter_mut() {
            let k = self.stream_byte();
            let plain = *b ^ k;
            *b = plain;
            self.update(plain);
        }
    }
}

// ---------------------------------------------------------------- 时间

pub fn dos_datetime(c: &Civil) -> (u16, u16) {
    let year = ((c.year - 1980).clamp(0, 127)) as u16;
    let date = (year << 9) | ((c.month as u16 & 0x0F) << 5) | (c.day as u16 & 0x1F);
    let time = ((c.hour as u16 & 0x1F) << 11) | ((c.min as u16 & 0x3F) << 5) | ((c.sec / 2) as u16 & 0x1F);
    (date, time)
}

pub fn from_dos(date: u16, time: u16) -> Civil {
    Civil {
        year: 1980 + ((date >> 9) & 0x7F) as i32,
        month: ((date >> 5) & 0x0F) as u32,
        day: (date & 0x1F) as u32,
        hour: ((time >> 11) & 0x1F) as u32,
        min: ((time >> 5) & 0x3F) as u32,
        sec: ((time & 0x1F) * 2) as u32,
    }
}

// ---------------------------------------------------------------- 写

struct CdEntry {
    name: String,
    method: u16,
    flags: u16,
    date: u16,
    time: u16,
    crc: u32,
    csize: u64,
    usize: u64,
    offset: u64,
    is_dir: bool,
}

pub struct ZipWriter<W: Write> {
    w: W,
    entries: Vec<CdEntry>,
    offset: u64,
    level: u8,
    password: Option<Vec<u8>>,
}

impl<W: Write> ZipWriter<W> {
    pub fn new(w: W, level: u8, password: Option<&[u8]>) -> Self {
        Self {
            w,
            entries: Vec::new(),
            offset: 0,
            level,
            password: password.map(|p| p.to_vec()),
        }
    }

    fn write_all(&mut self, data: &[u8]) -> io::Result<()> {
        self.w.write_all(data)?;
        self.offset += data.len() as u64;
        Ok(())
    }

    fn add_raw(&mut self, name: &str, data: &[u8], mtime: Option<&Civil>, is_dir: bool) -> io::Result<()> {
        let norm = name.replace('\\', "/");
        let norm = norm.trim_start_matches('/').to_string();
        let (date, time) = dos_datetime(mtime.unwrap_or(&crate::util::now_local()));
        let crc = crc32(data);
        let (method, payload) = if is_dir || data.is_empty() {
            (METHOD_STORE, Vec::new())
        } else if self.level == 0 {
            // 仅存储：必须原样写入数据（曾漏写导致归档里条目为 0 字节）
            (METHOD_STORE, data.to_vec())
        } else {
            let c = deflate::deflate(data, self.level);
            if c.len() < data.len() {
                (METHOD_DEFLATE, c)
            } else {
                (METHOD_STORE, data.to_vec())
            }
        };
        self.add_entry_final(&norm, &payload, data.len() as u64, crc, method, date, time, is_dir)?;
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn add_entry_final(
        &mut self,
        name: &str,
        payload: &[u8],
        usize_: u64,
        crc: u32,
        method: u16,
        date: u16,
        time: u16,
        is_dir: bool,
    ) -> io::Result<()> {
        let name_bytes = name.as_bytes();
        let mut flags: u16 = 0x0800; // UTF-8
        let mut body = payload.to_vec();
        let mut enc_header: Vec<u8> = Vec::new();
        if let Some(pw) = &self.password {
            flags |= 0x0001;
            let mut c = ZipCrypto::new(pw);
            let mut hdr = crypto_header(crc);
            c.encrypt(&mut hdr);
            c.encrypt(&mut body);
            enc_header = hdr;
        }
        let offset = self.offset;
        let mut lh = Vec::with_capacity(30 + name_bytes.len() + 12);
        lh.extend_from_slice(&SIG_LOCAL.to_le_bytes());
        lh.extend_from_slice(&20u16.to_le_bytes());
        lh.extend_from_slice(&flags.to_le_bytes());
        lh.extend_from_slice(&method.to_le_bytes());
        lh.extend_from_slice(&time.to_le_bytes());
        lh.extend_from_slice(&date.to_le_bytes());
        lh.extend_from_slice(&crc.to_le_bytes());
        lh.extend_from_slice(&((body.len() + enc_header.len()) as u32).to_le_bytes());
        lh.extend_from_slice(&(usize_ as u32).to_le_bytes());
        lh.extend_from_slice(&(name_bytes.len() as u16).to_le_bytes());
        lh.extend_from_slice(&0u16.to_le_bytes());
        lh.extend_from_slice(name_bytes);
        self.write_all(&lh)?;
        if !enc_header.is_empty() {
            self.write_all(&enc_header)?;
        }
        self.write_all(&body)?;
        self.entries.push(CdEntry {
            name: name.to_string(),
            method,
            flags,
            date,
            time,
            crc,
            csize: (body.len() + enc_header.len()) as u64,
            usize: usize_,
            offset,
            is_dir,
        });
        Ok(())
    }

    pub fn add_file(&mut self, name: &str, data: &[u8], mtime: Option<&Civil>) -> io::Result<()> {
        self.add_raw(name, data, mtime, false)
    }

    pub fn add_dir(&mut self, name: &str, mtime: Option<&Civil>) -> io::Result<()> {
        let mut n = name.replace('\\', "/");
        if !n.ends_with('/') {
            n.push('/');
        }
        let (date, time) = dos_datetime(mtime.unwrap_or(&crate::util::now_local()));
        self.add_entry_final(&n, &[], 0, 0, METHOD_STORE, date, time, true)
    }

    pub fn finish(mut self) -> io::Result<W> {
        let cd_start = self.offset;
        let mut cd = Vec::new();
        for e in &self.entries {
            let name_bytes = e.name.as_bytes();
            cd.extend_from_slice(&SIG_CD.to_le_bytes());
            cd.extend_from_slice(&20u16.to_le_bytes()); // version made by
            cd.extend_from_slice(&20u16.to_le_bytes()); // version needed
            cd.extend_from_slice(&e.flags.to_le_bytes());
            cd.extend_from_slice(&e.method.to_le_bytes());
            cd.extend_from_slice(&e.time.to_le_bytes());
            cd.extend_from_slice(&e.date.to_le_bytes());
            cd.extend_from_slice(&e.crc.to_le_bytes());
            cd.extend_from_slice(&(e.csize as u32).to_le_bytes());
            cd.extend_from_slice(&(e.usize as u32).to_le_bytes());
            cd.extend_from_slice(&(name_bytes.len() as u16).to_le_bytes());
            cd.extend_from_slice(&0u16.to_le_bytes()); // extra
            cd.extend_from_slice(&0u16.to_le_bytes()); // comment
            cd.extend_from_slice(&0u16.to_le_bytes()); // disk
            cd.extend_from_slice(&0u16.to_le_bytes()); // internal attr
            let ext_attr: u32 = if e.is_dir { 0x10 } else { 0 };
            cd.extend_from_slice(&ext_attr.to_le_bytes());
            cd.extend_from_slice(&(e.offset as u32).to_le_bytes());
            cd.extend_from_slice(name_bytes);
        }
        let cd_size = cd.len() as u64;
        let n = self.entries.len() as u16;
        self.write_all(&cd)?;
        let mut eocd = Vec::with_capacity(22);
        eocd.extend_from_slice(&SIG_EOCD.to_le_bytes());
        eocd.extend_from_slice(&0u16.to_le_bytes());
        eocd.extend_from_slice(&0u16.to_le_bytes());
        eocd.extend_from_slice(&n.to_le_bytes());
        eocd.extend_from_slice(&n.to_le_bytes());
        eocd.extend_from_slice(&(cd_size as u32).to_le_bytes());
        eocd.extend_from_slice(&(cd_start as u32).to_le_bytes());
        eocd.extend_from_slice(&0u16.to_le_bytes());
        self.write_all(&eocd)?;
        self.w.flush()?;
        Ok(self.w)
    }
}

fn crypto_header(crc: u32) -> Vec<u8> {
    // 11 字节伪随机 + CRC 高字节
    let mut h = Vec::with_capacity(12);
    let mut state = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0x9E37_79B9_7F4A_7C15)
        | 1;
    for _ in 0..11 {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        h.push((state & 0xFF) as u8);
    }
    h.push((crc >> 24) as u8);
    h
}

// ---------------------------------------------------------------- 读

#[derive(Clone, Debug)]
pub struct ZipEntryInfo {
    pub name: String,
    pub method: u16,
    pub flags: u16,
    pub crc: u32,
    pub csize: u64,
    pub usize: u64,
    pub offset: u64,
    pub is_dir: bool,
    pub mtime: Civil,
}

pub struct ZipArchive {
    data: Vec<u8>,
    entries: Vec<ZipEntryInfo>,
}

impl ZipArchive {
    pub fn open(data: Vec<u8>) -> io::Result<Self> {
        let entries = parse_central(&data)?;
        Ok(Self { data, entries })
    }

    pub fn entries(&self) -> &[ZipEntryInfo] {
        &self.entries
    }

    pub fn read(&self, idx: usize, password: Option<&[u8]>) -> io::Result<Vec<u8>> {
        let e = self.entries.get(idx).ok_or_else(|| bad("条目索引越界"))?;
        if e.is_dir {
            return Ok(Vec::new());
        }
        if e.flags & 0x0001 != 0 {
            let pw = password.ok_or_else(|| bad("该条目已加密，需要密码 (-p)"))?;
            return self.read_encrypted(e, pw);
        }
        self.read_plain(e)
    }

    fn payload(&self, e: &ZipEntryInfo, extra_skip: usize) -> io::Result<&[u8]> {
        let mut r = ByteReader::new(&self.data);
        r.seek(e.offset as usize)?;
        let sig = r.u32le()?;
        if sig != SIG_LOCAL {
            return Err(bad(format!("本地头签名错误：{:#x}", sig)));
        }
        r.take(2)?; // version
        r.take(2)?; // flags
        r.take(2)?; // method
        r.take(4)?; // time+date
        r.take(4)?; // crc
        r.take(4)?; // csize
        r.take(4)?; // usize
        let nlen = r.u16le()? as usize;
        let elen = r.u16le()? as usize;
        r.take(nlen)?;
        r.take(elen)?;
        let start = r.pos() + extra_skip;
        let end = start + (e.csize as usize - extra_skip);
        if end > self.data.len() {
            return Err(bad("条目数据越界"));
        }
        Ok(&self.data[start..end])
    }

    fn read_plain(&self, e: &ZipEntryInfo) -> io::Result<Vec<u8>> {
        if e.method == 99 {
            return Err(bad("该条目使用 AES 加密（method 99），v1.0 不支持"));
        }
        let raw = self.payload(e, 0)?;
        let out = match e.method {
            METHOD_STORE => raw.to_vec(),
            METHOD_DEFLATE => deflate::inflate_limited(raw, Some(e.usize as usize))?,
            12 => return Err(bad("BZIP2 条目不支持（未实现该解码器）")),
            93 => return Err(bad("ZSTD 条目不支持（未实现该解码器）")),
            m => return Err(bad(format!("不支持的 ZIP 压缩方法 {m}"))),
        };
        if crc32(&out) != e.crc {
            return Err(bad(format!("CRC32 校验失败：{}", e.name)));
        }
        Ok(out)
    }

    fn read_encrypted(&self, e: &ZipEntryInfo, password: &[u8]) -> io::Result<Vec<u8>> {
        let mut raw = self.payload(e, 0)?.to_vec();
        if raw.len() < 12 {
            return Err(bad("加密条目过短"));
        }
        let mut c = ZipCrypto::new(password);
        c.decrypt(&mut raw);
        if raw[11] != (e.crc >> 24) as u8 {
            return Err(bad("密码错误（加密头校验失败）"));
        }
        let body = &raw[12..];
        let out = match e.method {
            METHOD_STORE => body.to_vec(),
            METHOD_DEFLATE => deflate::inflate_limited(body, Some(e.usize as usize))?,
            m => return Err(bad(format!("加密条目的压缩方法 {m} 不支持"))),
        };
        if crc32(&out) != e.crc {
            return Err(bad("密码错误或数据损坏（CRC 不符）"));
        }
        Ok(out)
    }
}

fn parse_central(data: &[u8]) -> io::Result<Vec<ZipEntryInfo>> {
    if data.len() < 22 {
        return Err(bad("文件过小，不是 ZIP"));
    }
    // 从尾部向前搜 EOCD
    let scan_start = data.len().saturating_sub(22 + 65_535);
    let mut eocd_pos = None;
    let mut i = data.len() - 22;
    loop {
        if i + 4 <= data.len() && u32::from_le_bytes(data[i..i + 4].try_into().unwrap()) == SIG_EOCD {
            eocd_pos = Some(i);
            break;
        }
        if i == scan_start {
            break;
        }
        i -= 1;
    }
    let eocd_pos = eocd_pos.ok_or_else(|| bad("未找到 ZIP 中央目录结尾（EOCD）"))?;
    let mut r = ByteReader::new(data);
    r.seek(eocd_pos)?;
    r.u32le()?;
    r.u16le()?;
    r.u16le()?;
    let mut total = r.u16le()? as u64;
    r.u16le()?;
    let cd_size = r.u32le()? as u64;
    let mut cd_off = r.u32le()? as u64;

    // Zip64 EOCD
    if total == 0xFFFF || cd_size == 0xFFFF_FFFF || cd_off == 0xFFFF_FFFF {
        let z64 = find_sig_back(data, SIG_Z64_EOCD).ok_or_else(|| bad("缺少 Zip64 EOCD"))?;
        let mut zr = ByteReader::new(data);
        zr.seek(z64)?;
        zr.u32le()?; // sig
        zr.u64le()?; // size
        zr.u16le()?;
        zr.u16le()?;
        zr.u32le()?;
        zr.u32le()?;
        total = zr.u64le()?;
        zr.u64le()?; // cd size（本实现按条目逐个读，无需使用）
        cd_off = zr.u64le()?;
    }

    let mut out = Vec::with_capacity(total as usize);
    let mut r = ByteReader::new(data);
    r.seek(cd_off as usize)?;
    for _ in 0..total {
        let sig = r.u32le()?;
        if sig != SIG_CD {
            return Err(bad("中央目录签名错误"));
        }
        r.u16le()?; // made by
        r.u16le()?; // needed
        let flags = r.u16le()?;
        let method = r.u16le()?;
        let time = r.u16le()?;
        let date = r.u16le()?;
        let crc = r.u32le()?;
        let mut csize = r.u32le()? as u64;
        let mut usize_ = r.u32le()? as u64;
        let nlen = r.u16le()? as usize;
        let elen = r.u16le()? as usize;
        let clen = r.u16le()? as usize;
        r.u16le()?; // disk
        r.u16le()?; // internal
        r.u32le()?; // external
        let mut offset = r.u32le()? as u64;
        let name_raw = r.take(nlen)?;
        let extra = r.take(elen)?;
        r.take(clen)?;
        let name = if flags & 0x0800 != 0 {
            String::from_utf8_lossy(name_raw).into_owned()
        } else {
            decode_cp437_or_utf8(name_raw)
        };
        // Zip64 extra
        if csize == 0xFFFF_FFFF || usize_ == 0xFFFF_FFFF || offset == 0xFFFF_FFFF {
            let mut er = ByteReader::new(extra);
            while !er.is_empty() {
                let id = er.u16le()?;
                let sz = er.u16le()? as usize;
                let body = er.take(sz)?;
                if id == 0x0001 {
                    let mut zr = ByteReader::new(body);
                    if usize_ == 0xFFFF_FFFF && zr.remaining() >= 8 {
                        usize_ = zr.u64le()?;
                    }
                    if csize == 0xFFFF_FFFF && zr.remaining() >= 8 {
                        csize = zr.u64le()?;
                    }
                    if offset == 0xFFFF_FFFF && zr.remaining() >= 8 {
                        offset = zr.u64le()?;
                    }
                }
            }
        }
        let is_dir = name.ends_with('/') || name.ends_with('\\');
        out.push(ZipEntryInfo {
            name,
            method,
            flags,
            crc,
            csize,
            usize: usize_,
            offset,
            is_dir,
            mtime: from_dos(date, time),
        });
    }
    Ok(out)
}

fn find_sig_back(data: &[u8], sig: u32) -> Option<usize> {
    if data.len() < 4 {
        return None;
    }
    let mut i = data.len() - 4;
    loop {
        if u32::from_le_bytes(data[i..i + 4].try_into().ok()?) == sig {
            return Some(i);
        }
        if i == 0 {
            return None;
        }
        i -= 1;
    }
}

fn decode_cp437_or_utf8(raw: &[u8]) -> String {
    match std::str::from_utf8(raw) {
        Ok(s) => s.to_string(),
        Err(_) => raw.iter().map(|&b| cp437_char(b)).collect(),
    }
}

fn cp437_char(b: u8) -> char {
    // 只覆盖常见的高位字符；低位与 ASCII 一致
    const HIGH: [char; 128] = [
        'Ç', 'ü', 'é', 'â', 'ä', 'à', 'å', 'ç', 'ê', 'ë', 'è', 'ï', 'î', 'ì', 'Ä', 'Å', 'É', 'æ',
        'Æ', 'ô', 'ö', 'ò', 'û', 'ù', 'ÿ', 'Ö', 'Ü', '¢', '£', '¥', '₧', 'ƒ', 'á', 'í', 'ó', 'ú',
        'ñ', 'Ñ', 'ª', 'º', '¿', '⌐', '¬', '½', '¼', '¡', '«', '»', '░', '▒', '▓', '│', '┤', '╡',
        '╢', '╖', '╕', '╣', '║', '╗', '╝', '╜', '╛', '┐', '└', '┴', '┬', '├', '─', '┼', '╞', '╟',
        '╚', '╔', '╩', '╦', '╠', '═', '╬', '╧', '╨', '╤', '╥', '╙', '╘', '╒', '╓', '╫', '╪', '┘',
        '┌', '█', '▄', '▌', '▐', '▀', 'α', 'ß', 'Γ', 'π', 'Σ', 'σ', 'µ', 'τ', 'Φ', 'Θ', 'Ω', 'δ',
        '∞', 'φ', 'ε', '∩', '≡', '±', '≥', '≤', '⌠', '⌡', '÷', '≈', '°', '∙', '·', '√', 'ⁿ', '²',
        '■', '\u{a0}',
    ];
    if b < 0x80 {
        b as char
    } else {
        HIGH[(b - 0x80) as usize]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_basic() {
        let mut buf = Vec::new();
        {
            let mut w = ZipWriter::new(&mut buf, 6, None);
            w.add_file("hello.txt", b"hello world hello world hello world", None).unwrap();
            w.add_file("dir/a.bin", &[0u8; 1000], None).unwrap();
            w.add_dir("empty", None).unwrap();
            w.finish().unwrap();
        }
        let ar = ZipArchive::open(buf).unwrap();
        assert_eq!(ar.entries().len(), 3);
        assert_eq!(ar.read(0, None).unwrap(), b"hello world hello world hello world");
        assert_eq!(ar.read(1, None).unwrap(), vec![0u8; 1000]);
        assert!(ar.read(2, None).unwrap().is_empty());
        assert!(ar.entries()[0].method == METHOD_DEFLATE);
    }

    #[test]
    fn store_level_keeps_data() {
        // 回归：level=0（仅存储）曾把 payload 写成空，导致归档里条目 0 字节
        let data: Vec<u8> = (0..5000u32).map(|i| (i * 37 % 256) as u8).collect();
        let mut buf = Vec::new();
        {
            let mut w = ZipWriter::new(&mut buf, 0, None);
            w.add_file("raw.bin", &data, None).unwrap();
            w.finish().unwrap();
        }
        let ar = ZipArchive::open(buf).unwrap();
        assert_eq!(ar.entries()[0].method, METHOD_STORE);
        assert_eq!(ar.entries()[0].usize, data.len() as u64);
        assert_eq!(ar.read(0, None).unwrap(), data);
    }

    #[test]
    fn incompressible_falls_back_to_store() {
        // 伪随机数据压缩后更大，应回退到 STORE 且内容不丢
        let mut x: u32 = 12345;
        let data: Vec<u8> = (0..8000).map(|_| {
            x ^= x << 13;
            x ^= x >> 17;
            x ^= x << 5;
            (x & 0xff) as u8
        }).collect();
        let mut buf = Vec::new();
        {
            let mut w = ZipWriter::new(&mut buf, 9, None);
            w.add_file("rand.bin", &data, None).unwrap();
            w.finish().unwrap();
        }
        let ar = ZipArchive::open(buf).unwrap();
        assert_eq!(ar.read(0, None).unwrap(), data);
    }

    #[test]
    fn roundtrip_encrypted() {
        let mut buf = Vec::new();
        {
            let mut w = ZipWriter::new(&mut buf, 6, Some(b"secret"));
            w.add_file("secret.txt", b"top secret data 12345", None).unwrap();
            w.finish().unwrap();
        }
        let ar = ZipArchive::open(buf).unwrap();
        assert_eq!(ar.read(0, Some(b"secret")).unwrap(), b"top secret data 12345");
        assert!(ar.read(0, Some(b"wrong")).is_err());
        assert!(ar.read(0, None).is_err());
    }

    #[test]
    fn empty_archive() {
        let mut buf = Vec::new();
        {
            let w = ZipWriter::new(&mut buf, 6, None);
            w.finish().unwrap();
        }
        let ar = ZipArchive::open(buf).unwrap();
        assert!(ar.entries().is_empty());
    }

    #[test]
    fn dos_time_roundtrip() {
        let c = Civil { year: 2024, month: 3, day: 5, hour: 14, min: 27, sec: 38 };
        let (d, t) = dos_datetime(&c);
        let back = from_dos(d, t);
        assert_eq!(back.year, 2024);
        assert_eq!(back.month, 3);
        assert_eq!(back.day, 5);
        assert_eq!(back.hour, 14);
        assert_eq!(back.min, 27);
        assert_eq!(back.sec, 38);
    }
}
