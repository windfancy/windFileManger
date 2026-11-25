//! xz 容器格式（.xz，LZMA2 单过滤器），自研实现。
//!
//! 结构：Stream Header → Block* → Index → Stream Footer。
//! 支持读取 CRC32 / CRC64 / 无校验三种 stream check；写出固定用 CRC32。

use std::io::{self, Write};

use crate::algo::hash::{crc32, crc64};
use crate::codec::lzma::lzma2_decompress;
use crate::codec::lzma_enc::lzma2_compress;
use crate::util::ByteReader;

const MAGIC: [u8; 6] = [0xFD, b'7', b'z', b'X', b'Z', 0x00];
const FOOTER_MAGIC: [u8; 2] = *b"YZ";

/// 过滤器 ID
const FILTER_LZMA2: u64 = 0x21;
const FILTER_DELTA: u64 = 0x03;

/// stream check 类型
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CheckType {
    None,
    Crc32,
    Crc64,
    Sha256,
}

impl CheckType {
    fn from_id(id: u8) -> Option<CheckType> {
        match id {
            0 => Some(CheckType::None),
            1 => Some(CheckType::Crc32),
            4 => Some(CheckType::Crc64),
            10 => Some(CheckType::Sha256),
            _ => None,
        }
    }

    fn id(self) -> u8 {
        match self {
            CheckType::None => 0,
            CheckType::Crc32 => 1,
            CheckType::Crc64 => 4,
            CheckType::Sha256 => 10,
        }
    }

    fn size(self) -> usize {
        match self {
            CheckType::None => 0,
            CheckType::Crc32 => 4,
            CheckType::Crc64 => 8,
            CheckType::Sha256 => 32,
        }
    }
}

/// xz 的 VLI（variable-length integer）：每字节 7 bit，低位组在前。
/// 注意与 7z 的变长整数（8 bit 分组）不是同一套编码，不能混用。
fn write_vli(out: &mut Vec<u8>, mut v: u64) {
    while v >= 0x80 {
        out.push((v as u8 & 0x7F) | 0x80);
        v >>= 7;
    }
    out.push(v as u8);
}

fn read_vli(r: &mut ByteReader<'_>) -> io::Result<u64> {
    let mut v = 0u64;
    let mut shift = 0u32;
    loop {
        let b = r.u8()?;
        if shift >= 64 {
            return Err(bad("xz VLI 过长"));
        }
        v |= ((b & 0x7F) as u64) << shift;
        if b & 0x80 == 0 {
            return Ok(v);
        }
        shift += 7;
    }
}

fn bad(msg: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg.into())
}

// ---------------------------------------------------------------- 写出

/// XZ 字典大小 → LZMA2 属性字节（dict = (2|(p&1)) << (p/2+11)）。
pub fn lzma2_dict_prop(dict_size: u64) -> u8 {
    for i in 0u8..40 {
        let size = (2u64 | (i as u64 & 1)) << (i as u64 / 2 + 11);
        if dict_size <= size {
            return i;
        }
    }
    40
}

pub fn dict_prop_to_size(p: u8) -> u64 {
    if p > 40 {
        return u64::MAX;
    }
    (2u64 | (p as u64 & 1)) << (p as u64 / 2 + 11)
}

/// 写出一个单块 xz 流。
pub fn write_xz<W: Write>(w: &mut W, data: &[u8], level: u8) -> io::Result<()> {
    let dict_log: u32 = if data.len() < 1 << 20 { 20 } else { 22 };
    let dict_size = 1u64 << dict_log;
    let comp = lzma2_compress(data, level, Some(dict_log));

    // ---- Stream Header
    let mut out = Vec::with_capacity(comp.len() + 128);
    out.extend_from_slice(&MAGIC);
    let flags = [0u8, CheckType::Crc32.id()];
    out.extend_from_slice(&flags);
    out.extend_from_slice(&crc32(&flags).to_le_bytes());

    // ---- Block Header：1 个 LZMA2 过滤器，不写可选的压缩/解压长度
    let prop = lzma2_dict_prop(dict_size);
    let mut fields = Vec::new();
    fields.push(0u8); // Block Flags：bit0-1 = numFilters-1 = 0
    write_vli(&mut fields, FILTER_LZMA2);
    write_vli(&mut fields, 1); // props size
    fields.push(prop);
    // 补零到 (1 + fields + crc) 是 4 的倍数
    let raw = 1 + fields.len() + 4;
    let pad = (4 - raw % 4) % 4;
    let block_header_size = raw + pad;
    let mut bh = Vec::with_capacity(block_header_size);
    bh.push(((block_header_size / 4) - 1) as u8);
    bh.extend_from_slice(&fields);
    bh.extend(std::iter::repeat_n(0u8, pad));
    let bhc = crc32(&bh);
    bh.extend_from_slice(&bhc.to_le_bytes());
    debug_assert_eq!(bh.len(), block_header_size);
    out.extend_from_slice(&bh);

    // ---- Block data（补齐到 4 的倍数，padding 不计入 unpadded size）
    out.extend_from_slice(&comp);
    let dpad = (4 - comp.len() % 4) % 4;
    out.extend(std::iter::repeat_n(0u8, dpad));

    let check = crc32(data);
    out.extend_from_slice(&check.to_le_bytes());

    let unpadded = block_header_size as u64 + comp.len() as u64 + 4;
    let uncompressed = data.len() as u64;

    // ---- Index
    let mut idx = Vec::new();
    idx.push(0u8);
    write_vli(&mut idx, 1); // 记录数
    write_vli(&mut idx, unpadded);
    write_vli(&mut idx, uncompressed);
    // 使 (idx + crc) 是 4 的倍数
    let ipad = (4 - (idx.len() + 4) % 4) % 4;
    idx.extend(std::iter::repeat_n(0u8, ipad));
    let idxc = crc32(&idx);
    idx.extend_from_slice(&idxc.to_le_bytes());
    let index_size = idx.len() as u64;
    out.extend_from_slice(&idx);

    // ---- Stream Footer
    let mut foot = Vec::with_capacity(12);
    let backward = index_size / 4 - 1;
    let mut fbody = Vec::with_capacity(6);
    fbody.extend_from_slice(&(backward as u32).to_le_bytes());
    fbody.extend_from_slice(&flags);
    foot.extend_from_slice(&crc32(&fbody).to_le_bytes());
    foot.extend_from_slice(&fbody);
    foot.extend_from_slice(&FOOTER_MAGIC);
    debug_assert_eq!(foot.len(), 12);
    out.extend_from_slice(&foot);

    w.write_all(&out)?;
    Ok(())
}

// ---------------------------------------------------------------- 读入

#[derive(Debug, Clone)]
pub struct XzBlock {
    pub unpadded_size: u64,
    pub uncompressed_size: u64,
    pub header_size: usize,
    pub compressed_offset: usize,
    pub compressed_size: usize,
}

pub struct XzInfo {
    pub check: CheckType,
    pub blocks: Vec<XzBlock>,
}

fn parse_index(data: &[u8], start: usize) -> io::Result<(Vec<(u64, u64)>, usize)> {
    let slice = &data[start..];
    let mut r = ByteReader::new(slice);
    let ind = r.u8()?;
    if ind != 0 {
        return Err(bad("index 指示符不是 0x00"));
    }
    let count = read_vli(&mut r)?;
    let mut recs = Vec::with_capacity(count.min(4096) as usize);
    for _ in 0..count {
        let unpadded = read_vli(&mut r)?;
        let unc = read_vli(&mut r)?;
        recs.push((unpadded, unc));
    }
    let consumed = slice.len() - r.remaining();
    Ok((recs, consumed))
}

pub fn read_xz_info(data: &[u8]) -> io::Result<XzInfo> {
    if data.len() < 12 + 12 {
        return Err(bad("xz 数据太短"));
    }
    if data[..6] != MAGIC {
        return Err(bad("不是 xz 数据（magic 不符）"));
    }
    let flags = [data[6], data[7]];
    let hcrc = u32::from_le_bytes([data[8], data[9], data[10], data[11]]);
    if crc32(&flags) != hcrc {
        return Err(bad("xz stream header CRC 校验失败"));
    }
    if flags[0] != 0 {
        return Err(bad("xz stream flags 保留位非 0"));
    }
    let check = CheckType::from_id(flags[1])
        .ok_or_else(|| bad(format!("xz 校验类型 {} 不支持", flags[1])))?;

    // 从尾部读 footer
    let n = data.len();
    let footer = &data[n - 12..];
    if footer[10..12] != FOOTER_MAGIC {
        return Err(bad("xz footer magic 不符"));
    }
    let ffcrc = u32::from_le_bytes([footer[0], footer[1], footer[2], footer[3]]);
    if crc32(&footer[4..10]) != ffcrc {
        return Err(bad("xz footer CRC 校验失败"));
    }
    if footer[8..10] != flags {
        return Err(bad("xz footer 的 stream flags 与 header 不一致"));
    }
    let backward = u32::from_le_bytes([footer[4], footer[5], footer[6], footer[7]]) as u64;
    let index_size = (backward + 1) * 4;
    if index_size as usize + 12 > n {
        return Err(bad("xz index 大小越界"));
    }
    let index_start = n - 12 - index_size as usize;
    let (recs, consumed) = parse_index(data, index_start)?;
    if consumed as u64 > index_size {
        return Err(bad("xz index 记录超出声明长度"));
    }

    // 顺序走块头部，结合 index 的 unpadded 尺寸推出压缩长度
    let mut blocks = Vec::with_capacity(recs.len());
    let mut pos = 12usize;
    for (unpadded, unc) in recs {
        if pos >= data.len() {
            return Err(bad("xz 块数据越界"));
        }
        let b = data[pos];
        let header_size = (b as usize + 1) * 4;
        if pos + header_size > data.len() {
            return Err(bad("xz 块头部越界"));
        }
        let hdr = &data[pos..pos + header_size];
        let hcrc = u32::from_le_bytes([
            hdr[header_size - 4],
            hdr[header_size - 3],
            hdr[header_size - 2],
            hdr[header_size - 1],
        ]);
        if crc32(&hdr[..header_size - 4]) != hcrc {
            return Err(bad("xz 块头部 CRC 校验失败"));
        }
        let comp_size = unpadded
            .checked_sub(header_size as u64 + check.size() as u64)
            .ok_or_else(|| bad("xz index 的 unpadded size 小于块头+校验"))?;
        let compressed_offset = pos + header_size;
        if compressed_offset + comp_size as usize > data.len() {
            return Err(bad("xz 块数据越界"));
        }
        blocks.push(XzBlock {
            unpadded_size: unpadded,
            uncompressed_size: unc,
            header_size,
            compressed_offset,
            compressed_size: comp_size as usize,
        });
        // 块数据补齐到 4 的倍数
        let padded = (comp_size as usize).div_ceil(4) * 4;
        pos = compressed_offset + padded + check.size();
    }
    Ok(XzInfo { check, blocks })
}

fn parse_filter_chain(hdr: &[u8], header_size: usize) -> io::Result<Vec<(u64, Vec<u8>)>> {
    let flags = hdr[1];
    let nfilters = (flags & 0x03) as usize + 1;
    let mut r = ByteReader::new(&hdr[2..header_size - 4]);
    if flags & 0x40 != 0 {
        read_vli(&mut r)?; // compressed size，已由 index 提供
    }
    if flags & 0x80 != 0 {
        read_vli(&mut r)?; // uncompressed size
    }
    let mut out = Vec::with_capacity(nfilters);
    for _ in 0..nfilters {
        let id = read_vli(&mut r)?;
        let plen = read_vli(&mut r)? as usize;
        let props = r.take(plen)?.to_vec();
        out.push((id, props));
    }
    Ok(out)
}

/// 解出整个 xz 流的数据（多块按序拼接）。
pub fn read_xz(data: &[u8]) -> io::Result<Vec<u8>> {
    let info = read_xz_info(data)?;
    let mut out = Vec::new();
    for b in &info.blocks {
        let hdr = &data[b.compressed_offset - b.header_size..b.compressed_offset];
        let chain = parse_filter_chain(hdr, b.header_size)?;
        let mut payload = data[b.compressed_offset..b.compressed_offset + b.compressed_size].to_vec();
        // 过滤器链从后往前应用（写出顺序是 编码顺序）
        for (id, props) in chain.iter().rev() {
            match *id {
                FILTER_LZMA2 => {
                    let dict = props.first().map(|p| dict_prop_to_size(*p)).unwrap_or(1 << 24);
                    payload = lzma2_decompress(&payload, Some(dict as usize))?;
                }
                FILTER_DELTA => {
                    payload = delta_decode(&payload, props)?;
                }
                other => {
                    return Err(io::Error::new(
                        io::ErrorKind::Unsupported,
                        format!("xz 过滤器 {other:#x} 不支持"),
                    ));
                }
            }
        }
        if payload.len() as u64 != b.uncompressed_size {
            return Err(bad(format!(
                "xz 块解压长度不符：期望 {}，实际 {}",
                b.uncompressed_size,
                payload.len()
            )));
        }
        // 校验
        let check_off = b.compressed_offset + (b.compressed_size).div_ceil(4) * 4;
        match info.check {
            CheckType::None => {}
            CheckType::Crc32 => {
                let want = u32::from_le_bytes(read4(data, check_off)?);
                let got = crc32(&payload);
                if want != got {
                    return Err(bad(format!("xz 块 CRC32 校验失败（期望 {want:08x}，实际 {got:08x}）")));
                }
            }
            CheckType::Crc64 => {
                let want = u64::from_le_bytes(read8(data, check_off)?);
                let got = crc64(&payload);
                if want != got {
                    return Err(bad(format!("xz 块 CRC64 校验失败（期望 {want:016x}，实际 {got:016x}）")));
                }
            }
            CheckType::Sha256 => {
                let want = data
                    .get(check_off..check_off + 32)
                    .ok_or_else(|| bad("xz SHA256 校验字段越界"))?;
                let got = crate::algo::hash::hash_bytes(crate::algo::hash::Algo::Sha256, &payload);
                if want != got.as_slice() {
                    return Err(bad("xz 块 SHA256 校验失败"));
                }
            }
        }
        out.extend_from_slice(&payload);
    }
    Ok(out)
}

fn read4(data: &[u8], at: usize) -> io::Result<[u8; 4]> {
    data.get(at..at + 4)
        .map(|s| [s[0], s[1], s[2], s[3]])
        .ok_or_else(|| bad("xz 校验字段越界"))
}

fn read8(data: &[u8], at: usize) -> io::Result<[u8; 8]> {
    let s = data.get(at..at + 8).ok_or_else(|| bad("xz 校验字段越界"))?;
    let mut a = [0u8; 8];
    a.copy_from_slice(s);
    Ok(a)
}

fn delta_decode(data: &[u8], props: &[u8]) -> io::Result<Vec<u8>> {
    let dist = props.first().copied().unwrap_or(0) as usize + 1;
    let mut out = Vec::with_capacity(data.len());
    let mut hist = vec![0u8; dist];
    for (pos, &b) in data.iter().enumerate() {
        let slot = pos % dist;
        let v = b.wrapping_add(hist[slot]);
        hist[slot] = v;
        out.push(v);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_small() {
        let data = b"xz roundtrip payload, hello hello hello".repeat(20);
        let mut buf = Vec::new();
        write_xz(&mut buf, &data, 6).unwrap();
        assert_eq!(&buf[..6], &MAGIC);
        let back = read_xz(&buf).unwrap();
        assert_eq!(back, data);
    }

    #[test]
    fn roundtrip_empty() {
        let mut buf = Vec::new();
        write_xz(&mut buf, b"", 6).unwrap();
        assert_eq!(read_xz(&buf).unwrap().len(), 0);
    }

    #[test]
    fn roundtrip_binary() {
        let data: Vec<u8> = (0..70000u32).map(|i| (i.wrapping_mul(2654435761) >> 13) as u8).collect();
        let mut buf = Vec::new();
        write_xz(&mut buf, &data, 4).unwrap();
        assert_eq!(read_xz(&buf).unwrap(), data);
    }

    #[test]
    fn info_reports_one_block() {
        let data = b"abc".repeat(1000);
        let mut buf = Vec::new();
        write_xz(&mut buf, &data, 6).unwrap();
        let info = read_xz_info(&buf).unwrap();
        assert_eq!(info.check, CheckType::Crc32);
        assert_eq!(info.blocks.len(), 1);
        assert_eq!(info.blocks[0].uncompressed_size, 3000);
    }

    #[test]
    fn corrupt_check_detected() {
        let data = b"corrupt me please".repeat(30);
        let mut buf = Vec::new();
        write_xz(&mut buf, &data, 6).unwrap();
        // 破坏块数据里第一个字节（在流头之后）
        buf[13] ^= 0x01;
        assert!(read_xz(&buf).is_err());
    }

    #[test]
    fn dict_prop_roundtrip() {
        for log in 12u32..=26 {
            let p = lzma2_dict_prop(1 << log);
            assert!(dict_prop_to_size(p) >= 1 << log);
        }
    }
}
