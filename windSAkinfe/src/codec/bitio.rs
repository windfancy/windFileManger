//! 位流读写（LSB-first），供 DEFLATE / gzip 使用。

use std::io;

pub struct BitWriter {
    out: Vec<u8>,
    buf: u64,
    cnt: u32,
}

impl BitWriter {
    pub fn new() -> Self {
        Self { out: Vec::new(), buf: 0, cnt: 0 }
    }

    pub fn with_capacity(n: usize) -> Self {
        Self { out: Vec::with_capacity(n), buf: 0, cnt: 0 }
    }

    #[inline]
    pub fn write_bits(&mut self, v: u32, n: u32) {
        debug_assert!(n <= 32);
        self.buf |= ((v as u64) & ((1u64 << n) - 1)) << self.cnt;
        self.cnt += n;
        while self.cnt >= 8 {
            self.out.push((self.buf & 0xFF) as u8);
            self.buf >>= 8;
            self.cnt -= 8;
        }
    }

    /// 写入 Huffman 码（码本身 MSB-first，位流 LSB-first，故需反转）。
    #[inline]
    pub fn write_code(&mut self, code: u16, len: u8) {
        self.write_bits(reverse_bits(code, len) as u32, len as u32);
    }

    /// 补齐到字节边界。
    pub fn align(&mut self) {
        if self.cnt > 0 {
            self.out.push((self.buf & 0xFF) as u8);
            self.buf = 0;
            self.cnt = 0;
        }
    }

    pub fn write_bytes(&mut self, data: &[u8]) {
        debug_assert_eq!(self.cnt, 0, "写字节前必须字节对齐");
        self.out.extend_from_slice(data);
    }

    pub fn bit_len(&self) -> usize {
        self.out.len() * 8 + self.cnt as usize
    }

    pub fn finish(mut self) -> Vec<u8> {
        self.align();
        self.out
    }

    pub fn into_inner(mut self) -> Vec<u8> {
        self.align();
        self.out
    }
}

impl Default for BitWriter {
    fn default() -> Self {
        Self::new()
    }
}

pub fn reverse_bits(code: u16, len: u8) -> u16 {
    let mut r = 0u16;
    let mut c = code;
    for _ in 0..len {
        r = (r << 1) | (c & 1);
        c >>= 1;
    }
    r
}

pub struct BitReader<'a> {
    data: &'a [u8],
    pos: usize,
    buf: u64,
    cnt: u32,
}

impl<'a> BitReader<'a> {
    pub fn new(data: &'a [u8]) -> Self {
        Self { data, pos: 0, buf: 0, cnt: 0 }
    }

    #[inline]
    pub fn bits(&mut self, n: u32) -> io::Result<u32> {
        debug_assert!(n <= 32);
        if n == 0 {
            return Ok(0);
        }
        while self.cnt < n {
            if self.pos >= self.data.len() {
                return Err(crate::util::eof());
            }
            self.buf |= (self.data[self.pos] as u64) << self.cnt;
            self.pos += 1;
            self.cnt += 8;
        }
        let v = (self.buf & ((1u64 << n) - 1)) as u32;
        self.buf >>= n;
        self.cnt -= n;
        Ok(v)
    }

    /// 丢弃当前不完整字节，`pos` 指回下一未读字节。
    pub fn align(&mut self) {
        let drop = self.cnt % 8;
        self.buf >>= drop;
        self.cnt -= drop;
        let bytes = (self.cnt / 8) as usize;
        self.pos -= bytes;
        self.buf = 0;
        self.cnt = 0;
    }

    /// 当前已消费的字节数（要求已对齐）。
    pub fn byte_pos(&self) -> usize {
        debug_assert_eq!(self.cnt, 0);
        self.pos
    }

    pub fn read_bytes(&mut self, n: usize) -> io::Result<&'a [u8]> {
        self.align();
        if self.pos + n > self.data.len() {
            return Err(crate::util::eof());
        }
        let s = &self.data[self.pos..self.pos + n];
        self.pos += n;
        Ok(s)
    }

    pub fn remaining(&self) -> usize {
        self.data.len().saturating_sub(self.pos)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bits_roundtrip() {
        let mut w = BitWriter::new();
        w.write_bits(0b101, 3);
        w.write_bits(0b1111, 4);
        w.write_bits(0xABCD, 16);
        let data = w.finish();
        let mut r = BitReader::new(&data);
        assert_eq!(r.bits(3).unwrap(), 0b101);
        assert_eq!(r.bits(4).unwrap(), 0b1111);
        assert_eq!(r.bits(16).unwrap(), 0xABCD);
    }

    #[test]
    fn reverse() {
        assert_eq!(reverse_bits(0b0011, 4), 0b1100);
        assert_eq!(reverse_bits(0b1, 1), 0b1);
    }

    #[test]
    fn align_restores_pos() {
        let data = [0xFFu8, 0xAA, 0xBB];
        let mut r = BitReader::new(&data);
        assert_eq!(r.bits(3).unwrap(), 7);
        r.align();
        assert_eq!(r.read_bytes(1).unwrap(), &[0xAA]);
    }
}
