//! gzip 容器（RFC 1952），容器层自研，负载用自研 DEFLATE。
//!
//! 支持多成员流（多个 gzip 成员串接）、FEXTRA/FNAME/FCOMMENT/FHCRC 跳过。

use std::io::{self, Write};

use crate::algo::hash::crc32;
use crate::codec::deflate::{deflate, inflate};

const MAGIC: [u8; 2] = [0x1f, 0x8b];
const CM_DEFLATE: u8 = 8;

pub fn write_gzip<W: Write>(w: &mut W, data: &[u8], level: u8) -> io::Result<()> {
    let mut h = Vec::with_capacity(10);
    h.extend_from_slice(&MAGIC);
    h.push(CM_DEFLATE);
    h.push(0); // FLG
    h.extend_from_slice(&0u32.to_le_bytes()); // MTIME
    h.push(if level >= 9 { 2 } else if level <= 2 { 4 } else { 0 }); // XFL
    h.push(3); // OS = Unix
    w.write_all(&h)?;
    let comp = deflate(data, level);
    w.write_all(&comp)?;
    w.write_all(&crc32(data).to_le_bytes())?;
    w.write_all(&(data.len() as u32).to_le_bytes())?;
    Ok(())
}

/// 解压完整 gzip 流（可含多个成员）。
pub fn read_gzip(data: &[u8]) -> io::Result<Vec<u8>> {
    let mut out = Vec::new();
    let mut pos = 0usize;
    let mut members = 0usize;
    while pos + 10 <= data.len() {
        if data[pos..pos + 2] != MAGIC {
            if out.is_empty() {
                return Err(io::Error::new(io::ErrorKind::InvalidData, "不是 gzip 数据（magic 不符）"));
            }
            break; // 尾部垃圾忽略
        }
        let cm = data[pos + 2];
        if cm != CM_DEFLATE {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("gzip 压缩方法 {cm} 不支持（只支持 8 = deflate）"),
            ));
        }
        let flg = data[pos + 3];
        pos += 10;
        // FEXTRA
        if flg & 0x04 != 0 {
            if pos + 2 > data.len() {
                return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "gzip FEXTRA 截断"));
            }
            let xlen = u16::from_le_bytes([data[pos], data[pos + 1]]) as usize;
            pos += 2 + xlen;
        }
        // FNAME
        if flg & 0x08 != 0 {
            while pos < data.len() && data[pos] != 0 {
                pos += 1;
            }
            pos += 1;
        }
        // FCOMMENT
        if flg & 0x10 != 0 {
            while pos < data.len() && data[pos] != 0 {
                pos += 1;
            }
            pos += 1;
        }
        // FHCRC
        if flg & 0x02 != 0 {
            pos += 2;
        }
        if pos > data.len() {
            return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "gzip 头部越界"));
        }
        let body = inflate(&data[pos..])?;
        // 计算 deflate 流消耗了多少字节：用 ISIZE/CRC 反推不可靠，改成逐块试探不划算。
        // 这里用一个稳妥做法：ISIZE 在尾部 4 字节之前，我们从后往前用 CRC+ISIZE 校验，
        // 而 deflate 消耗长度由 inflate 的输入位置决定 —— 见 `inflate_consumed`。
        let consumed = inflate_consumed(&data[pos..])?;
        let end = pos + consumed;
        if end + 8 > data.len() {
            return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "gzip 尾部校验字段缺失"));
        }
        let expect_crc = u32::from_le_bytes([data[end], data[end + 1], data[end + 2], data[end + 3]]);
        let isize = u32::from_le_bytes([
            data[end + 4],
            data[end + 5],
            data[end + 6],
            data[end + 7],
        ]);
        if isize as usize != body.len() % (u32::MAX as usize + 1) && isize as u64 != body.len() as u64 {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "gzip ISIZE 与解压长度不符"));
        }
        let got = crc32(&body);
        if got != expect_crc {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("gzip CRC32 校验失败（期望 {expect_crc:08x}，实际 {got:08x}）"),
            ));
        }
        out.extend_from_slice(&body);
        members += 1;
        pos = end + 8;
    }
    if members == 0 && !data.is_empty() {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "gzip 数据为空或格式错误"));
    }
    Ok(out)
}

/// 返回 DEFLATE 流实际消耗的输入字节数（需要精确值才能定位尾部校验字段）。
fn inflate_consumed(data: &[u8]) -> io::Result<usize> {
    crate::codec::deflate::inflate_with_consumed(data).map(|(_, n)| n)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip() {
        let data = b"gzip roundtrip test data, repeated repeated repeated ".repeat(50);
        let mut buf = Vec::new();
        write_gzip(&mut buf, &data, 6).unwrap();
        assert_eq!(&buf[..2], &MAGIC);
        let back = read_gzip(&buf).unwrap();
        assert_eq!(back, data);
    }

    #[test]
    fn empty_payload() {
        let mut buf = Vec::new();
        write_gzip(&mut buf, b"", 6).unwrap();
        assert_eq!(read_gzip(&buf).unwrap().len(), 0);
    }

    #[test]
    fn multi_member() {
        let mut buf = Vec::new();
        write_gzip(&mut buf, b"first", 6).unwrap();
        write_gzip(&mut buf, b"second", 6).unwrap();
        assert_eq!(read_gzip(&buf).unwrap(), b"firstsecond");
    }

    #[test]
    fn bad_magic() {
        assert!(read_gzip(b"not a gzip file at all").is_err());
    }

    #[test]
    fn crc_mismatch_detected() {
        let mut buf = Vec::new();
        write_gzip(&mut buf, b"abcdef", 6).unwrap();
        let n = buf.len();
        buf[n - 8] ^= 0xff; // 破坏 CRC32 首字节
        let e = read_gzip(&buf).unwrap_err();
        assert!(e.to_string().contains("CRC32"), "{e}");
    }
}
