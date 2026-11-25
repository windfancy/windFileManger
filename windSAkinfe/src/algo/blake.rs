//! BLAKE2b / BLAKE2s / BLAKE3 自研实现。
//!  - BLAKE2 : RFC 7693
//!  - BLAKE3 : 官方规范（chunk 1024B / block 64B / CV 栈合并）

// ================================================================ 公共常量

const IV32: [u32; 8] = [
    0x6A09_E667, 0xBB67_AE85, 0x3C6E_F372, 0xA54F_F53A, 0x510E_527F, 0x9B05_688C, 0x1F83_D9AB,
    0x5BE0_CD19,
];

const IV64: [u64; 8] = [
    0x6A09_E667_F3BC_C908, 0xBB67_AE85_84CA_A73B, 0x3C6E_F372_FE94_F82B, 0xA54F_F53A_5F1D_36F1,
    0x510E_527F_ADE6_82D1, 0x9B05_688C_2B3E_6C1F, 0x1F83_D9AB_FB41_BD6B, 0x5BE0_CD19_137E_2179,
];

const SIGMA: [[usize; 16]; 10] = [
    [0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15],
    [14, 10, 4, 8, 9, 15, 13, 6, 1, 12, 0, 2, 11, 7, 5, 3],
    [11, 8, 12, 0, 5, 2, 15, 13, 10, 14, 3, 6, 7, 1, 9, 4],
    [7, 9, 3, 1, 13, 12, 11, 14, 2, 6, 5, 10, 4, 0, 15, 8],
    [9, 0, 5, 7, 2, 4, 10, 15, 14, 1, 11, 12, 6, 8, 3, 13],
    [2, 12, 6, 10, 0, 11, 8, 3, 4, 13, 7, 5, 15, 14, 1, 9],
    [12, 5, 1, 15, 14, 13, 4, 10, 0, 7, 6, 3, 9, 2, 8, 11],
    [13, 11, 7, 14, 12, 1, 3, 9, 5, 0, 15, 4, 8, 6, 2, 10],
    [6, 15, 14, 9, 11, 3, 0, 8, 12, 2, 13, 7, 1, 4, 10, 5],
    [10, 2, 8, 4, 7, 6, 1, 5, 15, 11, 9, 14, 3, 12, 13, 0],
];

// ================================================================ BLAKE2b

pub struct Blake2b {
    h: [u64; 8],
    buf: [u8; 128],
    buflen: usize,
    t: u128,
    outlen: usize,
}

impl Blake2b {
    pub fn new(outlen: usize) -> Self {
        let mut h = IV64;
        // 参数块：digest_length | key_length<<8 | fanout=1 | depth=1
        h[0] ^= 0x0101_0000 ^ (outlen as u64);
        Self { h, buf: [0; 128], buflen: 0, t: 0, outlen }
    }

    fn compress(h: &mut [u64; 8], block: &[u8], t: u128, last: bool) {
        let mut m = [0u64; 16];
        for (i, w) in m.iter_mut().enumerate() {
            let mut b = [0u8; 8];
            b.copy_from_slice(&block[i * 8..i * 8 + 8]);
            *w = u64::from_le_bytes(b);
        }
        let mut v = [0u64; 16];
        v[..8].copy_from_slice(h);
        v[8..].copy_from_slice(&IV64);
        v[12] ^= t as u64;
        v[13] ^= (t >> 64) as u64;
        if last {
            v[14] = !v[14];
        }
        // 12 轮
        for r in 0..12 {
            let s = SIGMA[r % 10];
            g64(&mut v, 0, 4, 8, 12, m[s[0]], m[s[1]]);
            g64(&mut v, 1, 5, 9, 13, m[s[2]], m[s[3]]);
            g64(&mut v, 2, 6, 10, 14, m[s[4]], m[s[5]]);
            g64(&mut v, 3, 7, 11, 15, m[s[6]], m[s[7]]);
            g64(&mut v, 0, 5, 10, 15, m[s[8]], m[s[9]]);
            g64(&mut v, 1, 6, 11, 12, m[s[10]], m[s[11]]);
            g64(&mut v, 2, 7, 8, 13, m[s[12]], m[s[13]]);
            g64(&mut v, 3, 4, 9, 14, m[s[14]], m[s[15]]);
        }
        for i in 0..8 {
            h[i] ^= v[i] ^ v[i + 8];
        }
    }

    pub fn update(&mut self, mut data: &[u8]) {
        while !data.is_empty() {
            // 缓冲满则先压缩（保留最后一块给 finalize）
            if self.buflen == 128 {
                self.t += 128;
                let b = self.buf;
                Self::compress(&mut self.h, &b, self.t, false);
                self.buflen = 0;
            }
            let take = (128 - self.buflen).min(data.len());
            self.buf[self.buflen..self.buflen + take].copy_from_slice(&data[..take]);
            self.buflen += take;
            data = &data[take..];
        }
    }

    pub fn finalize(mut self) -> Vec<u8> {
        self.t += self.buflen as u128;
        for b in self.buf[self.buflen..].iter_mut() {
            *b = 0;
        }
        let b = self.buf;
        Self::compress(&mut self.h, &b, self.t, true);
        let mut out = Vec::with_capacity(64);
        for v in self.h.iter() {
            out.extend_from_slice(&v.to_le_bytes());
        }
        out.truncate(self.outlen);
        out
    }
}

fn g64(v: &mut [u64; 16], a: usize, b: usize, c: usize, d: usize, x: u64, y: u64) {
    v[a] = v[a].wrapping_add(v[b]).wrapping_add(x);
    v[d] = (v[d] ^ v[a]).rotate_right(32);
    v[c] = v[c].wrapping_add(v[d]);
    v[b] = (v[b] ^ v[c]).rotate_right(24);
    v[a] = v[a].wrapping_add(v[b]).wrapping_add(y);
    v[d] = (v[d] ^ v[a]).rotate_right(16);
    v[c] = v[c].wrapping_add(v[d]);
    v[b] = (v[b] ^ v[c]).rotate_right(63);
}

// ================================================================ BLAKE2s

pub struct Blake2s {
    h: [u32; 8],
    buf: [u8; 64],
    buflen: usize,
    t: u64,
    outlen: usize,
}

impl Blake2s {
    pub fn new(outlen: usize) -> Self {
        let mut h = IV32;
        h[0] ^= 0x0101_0000 ^ (outlen as u32);
        Self { h, buf: [0; 64], buflen: 0, t: 0, outlen }
    }

    fn compress(h: &mut [u32; 8], block: &[u8], t: u64, last: bool) {
        let mut m = [0u32; 16];
        for (i, w) in m.iter_mut().enumerate() {
            let mut b = [0u8; 4];
            b.copy_from_slice(&block[i * 4..i * 4 + 4]);
            *w = u32::from_le_bytes(b);
        }
        let mut v = [0u32; 16];
        v[..8].copy_from_slice(h);
        v[8..].copy_from_slice(&IV32);
        v[12] ^= t as u32;
        v[13] ^= (t >> 32) as u32;
        if last {
            v[14] = !v[14];
        }
        for r in 0..10 {
            let s = SIGMA[r % 10];
            g32(&mut v, 0, 4, 8, 12, m[s[0]], m[s[1]]);
            g32(&mut v, 1, 5, 9, 13, m[s[2]], m[s[3]]);
            g32(&mut v, 2, 6, 10, 14, m[s[4]], m[s[5]]);
            g32(&mut v, 3, 7, 11, 15, m[s[6]], m[s[7]]);
            g32(&mut v, 0, 5, 10, 15, m[s[8]], m[s[9]]);
            g32(&mut v, 1, 6, 11, 12, m[s[10]], m[s[11]]);
            g32(&mut v, 2, 7, 8, 13, m[s[12]], m[s[13]]);
            g32(&mut v, 3, 4, 9, 14, m[s[14]], m[s[15]]);
        }
        for i in 0..8 {
            h[i] ^= v[i] ^ v[i + 8];
        }
    }

    pub fn update(&mut self, mut data: &[u8]) {
        while !data.is_empty() {
            if self.buflen == 64 {
                self.t += 64;
                let b = self.buf;
                Self::compress(&mut self.h, &b, self.t, false);
                self.buflen = 0;
            }
            let take = (64 - self.buflen).min(data.len());
            self.buf[self.buflen..self.buflen + take].copy_from_slice(&data[..take]);
            self.buflen += take;
            data = &data[take..];
        }
    }

    pub fn finalize(mut self) -> Vec<u8> {
        self.t += self.buflen as u64;
        for b in self.buf[self.buflen..].iter_mut() {
            *b = 0;
        }
        let b = self.buf;
        Self::compress(&mut self.h, &b, self.t, true);
        let mut out = Vec::with_capacity(32);
        for v in self.h.iter() {
            out.extend_from_slice(&v.to_le_bytes());
        }
        out.truncate(self.outlen);
        out
    }
}

fn g32(v: &mut [u32; 16], a: usize, b: usize, c: usize, d: usize, x: u32, y: u32) {
    v[a] = v[a].wrapping_add(v[b]).wrapping_add(x);
    v[d] = (v[d] ^ v[a]).rotate_right(16);
    v[c] = v[c].wrapping_add(v[d]);
    v[b] = (v[b] ^ v[c]).rotate_right(12);
    v[a] = v[a].wrapping_add(v[b]).wrapping_add(y);
    v[d] = (v[d] ^ v[a]).rotate_right(8);
    v[c] = v[c].wrapping_add(v[d]);
    v[b] = (v[b] ^ v[c]).rotate_right(7);
}

// ================================================================ BLAKE3

const B3_CHUNK_START: u32 = 1 << 0;
const B3_CHUNK_END: u32 = 1 << 1;
const B3_PARENT: u32 = 1 << 2;
const B3_ROOT: u32 = 1 << 3;
const B3_MSG_PERMUTATION: [usize; 16] = [2, 6, 3, 10, 7, 0, 4, 13, 1, 11, 12, 5, 9, 14, 15, 8];

const CHUNK_LEN: usize = 1024;
const BLOCK_LEN: usize = 64;

fn b3_compress(cv: &[u32; 8], block: &[u8; BLOCK_LEN], counter: u64, block_len: u32, flags: u32) -> [u32; 16] {
    let mut m = [0u32; 16];
    for (i, w) in m.iter_mut().enumerate() {
        let mut b = [0u8; 4];
        b.copy_from_slice(&block[i * 4..i * 4 + 4]);
        *w = u32::from_le_bytes(b);
    }
    let mut s = [0u32; 16];
    s[..8].copy_from_slice(cv);
    s[8..12].copy_from_slice(&IV32[..4]);
    s[12] = counter as u32;
    s[13] = (counter >> 32) as u32;
    s[14] = block_len;
    s[15] = flags;
    for _ in 0..7 {
        g32(&mut s, 0, 4, 8, 12, m[0], m[1]);
        g32(&mut s, 1, 5, 9, 13, m[2], m[3]);
        g32(&mut s, 2, 6, 10, 14, m[4], m[5]);
        g32(&mut s, 3, 7, 11, 15, m[6], m[7]);
        g32(&mut s, 0, 5, 10, 15, m[8], m[9]);
        g32(&mut s, 1, 6, 11, 12, m[10], m[11]);
        g32(&mut s, 2, 7, 8, 13, m[12], m[13]);
        g32(&mut s, 3, 4, 9, 14, m[14], m[15]);
        let mut perm = [0u32; 16];
        for i in 0..16 {
            perm[i] = m[B3_MSG_PERMUTATION[i]];
        }
        m = perm;
    }
    let mut out = [0u32; 16];
    for i in 0..8 {
        out[i] = s[i] ^ s[i + 8];
        out[i + 8] = s[i + 8] ^ cv[i];
    }
    out
}

fn b3_cv_from_out(out: &[u32; 16]) -> [u32; 8] {
    let mut cv = [0u32; 8];
    cv.copy_from_slice(&out[..8]);
    cv
}

fn b3_words_to_bytes(w: &[u32]) -> Vec<u8> {
    let mut v = Vec::with_capacity(w.len() * 4);
    for x in w {
        v.extend_from_slice(&x.to_le_bytes());
    }
    v
}

/// 计算一个 chunk 的链值；`root=true` 时返回最终根输出（32 字节）。
fn b3_chunk(data: &[u8], counter: u64, root: bool) -> ([u32; 8], Option<Vec<u8>>) {
    debug_assert!(data.len() <= CHUNK_LEN);
    let nblocks = if data.is_empty() { 1 } else { data.len().div_ceil(BLOCK_LEN) };
    let mut cv = IV32;
    for i in 0..nblocks {
        let start = i * BLOCK_LEN;
        let end = (start + BLOCK_LEN).min(data.len());
        let mut block = [0u8; BLOCK_LEN];
        block[..end - start].copy_from_slice(&data[start..end]);
        let mut flags = 0;
        if i == 0 {
            flags |= B3_CHUNK_START;
        }
        let is_last = i == nblocks - 1;
        if is_last {
            flags |= B3_CHUNK_END;
            if root {
                flags |= B3_ROOT;
            }
        }
        let out = b3_compress(&cv, &block, counter, (end - start) as u32, flags);
        if is_last {
            if root {
                return (b3_cv_from_out(&out), Some(b3_words_to_bytes(&out[..8])));
            }
            return (b3_cv_from_out(&out), None);
        }
        cv = b3_cv_from_out(&out);
    }
    unreachable!()
}

fn b3_parent_cv(left: &[u32; 8], right: &[u32; 8], root: bool) -> ([u32; 8], Option<Vec<u8>>) {
    // 父节点 block = 左子 CV(32B) + 右子 CV(32B)
    let mut block = [0u8; BLOCK_LEN];
    let rb = b3_words_to_bytes(right);
    block[..32].copy_from_slice(&rb);
    let mut flags = B3_PARENT;
    if root {
        flags |= B3_ROOT;
    }
    let out = b3_compress(left, &block, 0, BLOCK_LEN as u32, flags);
    if root {
        (b3_cv_from_out(&out), Some(b3_words_to_bytes(&out[..8])))
    } else {
        (b3_cv_from_out(&out), None)
    }
}

pub struct Blake3 {
    chunk: Vec<u8>,
    chunk_counter: u64,
    cv_stack: Vec<[u32; 8]>,
}

impl Blake3 {
    pub fn new() -> Self {
        Self { chunk: Vec::with_capacity(CHUNK_LEN), chunk_counter: 0, cv_stack: Vec::new() }
    }

    fn push_chunk_cv(&mut self) {
        let (cv, _) = b3_chunk(&self.chunk, self.chunk_counter, false);
        let mut new_cv = cv;
        let mut total = self.chunk_counter + 1;
        while total & 1 == 0 {
            let left = self.cv_stack.pop().expect("CV 栈不平衡");
            new_cv = b3_parent_cv(&left, &new_cv, false).0;
            total >>= 1;
        }
        self.cv_stack.push(new_cv);
        self.chunk_counter += 1;
        self.chunk.clear();
    }

    pub fn update(&mut self, mut data: &[u8]) {
        while !data.is_empty() {
            let take = (CHUNK_LEN - self.chunk.len()).min(data.len());
            self.chunk.extend_from_slice(&data[..take]);
            data = &data[take..];
            if self.chunk.len() == CHUNK_LEN {
                self.push_chunk_cv();
            }
        }
    }

    pub fn finalize(mut self) -> Vec<u8> {
        if self.cv_stack.is_empty() {
            // 单 chunk 即根
            let (_, out) = b3_chunk(&self.chunk, self.chunk_counter, true);
            return out.expect("根输出必存在");
        }
        let (mut cv, _) = b3_chunk(&self.chunk, self.chunk_counter, false);
        while self.cv_stack.len() > 1 {
            let left = self.cv_stack.pop().unwrap();
            cv = b3_parent_cv(&left, &cv, false).0;
        }
        let left = self.cv_stack[0];
        b3_parent_cv(&left, &cv, true).1.expect("根输出必存在")
    }
}

impl Default for Blake3 {
    fn default() -> Self {
        Self::new()
    }
}

// ================================================================ 测试

#[cfg(test)]
mod tests {
    use super::*;

    fn b2b(s: &str) -> String {
        let mut h = Blake2b::new(64);
        h.update(s.as_bytes());
        crate::util::hex(&h.finalize())
    }

    #[test]
    fn blake2b_vectors() {
        assert_eq!(
            b2b("abc"),
            "ba80a53f981c4d0d6a2797b69f12f6e94c212f14685ac4b74b12bb6fdbffa2d1\
             7d87c5392aab792dc252d5de4533cc9518d38aa8dbf1925ab92386edd4009923"
                .replace(char::is_whitespace, "")
        );
        assert_eq!(
            b2b(""),
            "786a02f742015903c6c6fd852552d272912f4740e15847618a86e217f71f5419\
             d25e1031afee585313896444934eb04b903a685b1448b755d56f701afe9be2ce"
                .replace(char::is_whitespace, "")
        );
    }

    #[test]
    fn blake2s_vector() {
        let mut h = Blake2s::new(32);
        h.update(b"abc");
        assert_eq!(
            crate::util::hex(&h.finalize()),
            "508c5e8c327c14e2e1a72ba34eeb452f37458b209ed63a294d999b4c86675982"
        );
    }

    #[test]
    fn blake3_vectors() {
        let h = |s: &[u8]| -> String {
            let mut b = Blake3::new();
            b.update(s);
            crate::util::hex(&b.finalize())
        };
        assert_eq!(h(b"abc"), "6437b3ac38465133ffb63b75273a8db548c558465d79db03fd359c6cd5bd9d85");
        assert_eq!(h(b""), "af1349b9f5f9a1a6a0404dea36dcc9499bcb25c9adc112b7cc9a93cae41f3262");
    }

    #[test]
    fn blake3_streaming_consistency() {
        let data: Vec<u8> = (0..5000u32).map(|i| (i * 7 % 256) as u8).collect();
        let mut whole = Blake3::new();
        whole.update(&data);
        let a = whole.finalize();
        // 多种切分粒度都必须得到同一摘要（含跨 chunk 边界、非对齐长度）
        for step in [1usize, 63, 64, 65, 511, 1024, 1300, 4999] {
            let mut p2 = Blake3::new();
            let mut off = 0usize;
            while off < data.len() {
                let end = (off + step).min(data.len());
                p2.update(&data[off..end]);
                off = end;
            }
            assert_eq!(a, p2.finalize(), "切分粒度 {step} 结果不一致");
        }
    }

    #[test]
    fn blake3_exact_chunk_boundary() {
        // 1024 字节：第一个 chunk 满，第二个为空 chunk
        let data = vec![0x41u8; 1024];
        let mut h = Blake3::new();
        h.update(&data);
        let onepass = h.finalize();
        let mut h2 = Blake3::new();
        h2.update(&data[..500]);
        h2.update(&data[500..]);
        assert_eq!(onepass, h2.finalize());
    }

    #[test]
    fn blake2_streaming_consistency() {
        let data: Vec<u8> = (0..3000u32).map(|i| (i % 253) as u8).collect();
        let mut one = Blake2b::new(64);
        one.update(&data);
        let a = one.finalize();
        let mut two = Blake2b::new(64);
        for chunk in data.chunks(200) {
            two.update(chunk);
        }
        assert_eq!(a, two.finalize());

        let mut one = Blake2s::new(32);
        one.update(&data);
        let a = one.finalize();
        let mut two = Blake2s::new(32);
        for chunk in data.chunks(200) {
            two.update(chunk);
        }
        assert_eq!(a, two.finalize());
    }
}
