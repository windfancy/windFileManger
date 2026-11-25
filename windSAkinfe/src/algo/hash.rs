//! 哈希算法族：MD5 / SHA1 / SHA2 / SHA3 / CRC32 / CRC64。
//! 全部依据公开规范实现，不依赖任何第三方实现库。
//!  - MD5      : RFC 1321
//!  - SHA1     : FIPS 180-4
//!  - SHA2     : FIPS 180-4
//!  - SHA3     : FIPS 202 (Keccak-f[1600])
//!  - CRC32    : IEEE 802.3 (reflected, poly 0xEDB88320)
//!  - CRC64    : ECMA-182 (reflected, poly 0xC96C5795D7870F42)，用于 xz

use std::io;

use crate::util::{hex, read_full};

// ================================================================ 算法枚举

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Algo {
    Md5,
    Sha1,
    Sha224,
    Sha256,
    Sha384,
    Sha512,
    Sha3_224,
    Sha3_256,
    Sha3_384,
    Sha3_512,
    Blake2b512,
    Blake2s256,
    Blake3,
    Crc32,
    Crc64,
}

impl Algo {
    pub fn parse(s: &str) -> Option<Algo> {
        let t = s.trim().to_ascii_lowercase().replace(['-', '_'], "");
        Some(match t.as_str() {
            "md5" => Algo::Md5,
            "sha1" => Algo::Sha1,
            "sha224" | "sha2224" => Algo::Sha224,
            "sha256" | "sha2256" => Algo::Sha256,
            "sha384" | "sha2384" => Algo::Sha384,
            "sha512" | "sha2512" => Algo::Sha512,
            "sha3224" => Algo::Sha3_224,
            "sha3256" => Algo::Sha3_256,
            "sha3384" => Algo::Sha3_384,
            "sha3512" => Algo::Sha3_512,
            "blake2b" | "blake2b512" => Algo::Blake2b512,
            "blake2s" | "blake2s256" => Algo::Blake2s256,
            "blake3" => Algo::Blake3,
            "crc32" => Algo::Crc32,
            "crc64" => Algo::Crc64,
            _ => return None,
        })
    }

    pub fn name(self) -> &'static str {
        match self {
            Algo::Md5 => "md5",
            Algo::Sha1 => "sha1",
            Algo::Sha224 => "sha224",
            Algo::Sha256 => "sha256",
            Algo::Sha384 => "sha384",
            Algo::Sha512 => "sha512",
            Algo::Sha3_224 => "sha3-224",
            Algo::Sha3_256 => "sha3-256",
            Algo::Sha3_384 => "sha3-384",
            Algo::Sha3_512 => "sha3-512",
            Algo::Blake2b512 => "blake2b",
            Algo::Blake2s256 => "blake2s",
            Algo::Blake3 => "blake3",
            Algo::Crc32 => "crc32",
            Algo::Crc64 => "crc64",
        }
    }

    pub fn out_len(self) -> usize {
        match self {
            Algo::Md5 => 16,
            Algo::Sha1 => 20,
            Algo::Sha224 => 28,
            Algo::Sha256 => 32,
            Algo::Sha384 => 48,
            Algo::Sha512 => 64,
            Algo::Sha3_224 => 28,
            Algo::Sha3_256 => 32,
            Algo::Sha3_384 => 48,
            Algo::Sha3_512 => 64,
            Algo::Blake2b512 => 64,
            Algo::Blake2s256 => 32,
            Algo::Blake3 => 32,
            Algo::Crc32 => 4,
            Algo::Crc64 => 8,
        }
    }

    pub const ALL: [Algo; 15] = [
        Algo::Md5,
        Algo::Sha1,
        Algo::Sha224,
        Algo::Sha256,
        Algo::Sha384,
        Algo::Sha512,
        Algo::Sha3_224,
        Algo::Sha3_256,
        Algo::Sha3_384,
        Algo::Sha3_512,
        Algo::Blake2b512,
        Algo::Blake2s256,
        Algo::Blake3,
        Algo::Crc32,
        Algo::Crc64,
    ];
}

// ================================================================ 统一入口

#[allow(clippy::large_enum_variant)]
enum State {
    Md5(Md5),
    Sha1(Sha1),
    Sha256(Sha256),
    Sha512(Sha512),
    Sha3(Sha3),
    Blake2b(crate::algo::blake::Blake2b),
    Blake2s(crate::algo::blake::Blake2s),
    Blake3(crate::algo::blake::Blake3),
    Crc32(Crc32),
    Crc64(Crc64),
}

pub struct Hasher {
    state: State,
}

impl Hasher {
    pub fn new(algo: Algo) -> Self {
        let state = match algo {
            Algo::Md5 => State::Md5(Md5::new()),
            Algo::Sha1 => State::Sha1(Sha1::new()),
            Algo::Sha224 => State::Sha256(Sha256::new(true)),
            Algo::Sha256 => State::Sha256(Sha256::new(false)),
            Algo::Sha384 => State::Sha512(Sha512::new(true)),
            Algo::Sha512 => State::Sha512(Sha512::new(false)),
            Algo::Sha3_224 => State::Sha3(Sha3::new(28)),
            Algo::Sha3_256 => State::Sha3(Sha3::new(32)),
            Algo::Sha3_384 => State::Sha3(Sha3::new(48)),
            Algo::Sha3_512 => State::Sha3(Sha3::new(64)),
            Algo::Blake2b512 => State::Blake2b(crate::algo::blake::Blake2b::new(64)),
            Algo::Blake2s256 => State::Blake2s(crate::algo::blake::Blake2s::new(32)),
            Algo::Blake3 => State::Blake3(crate::algo::blake::Blake3::new()),
            Algo::Crc32 => State::Crc32(Crc32::new()),
            Algo::Crc64 => State::Crc64(Crc64::new()),
        };
        Self { state }
    }

    pub fn update(&mut self, data: &[u8]) {
        match &mut self.state {
            State::Md5(h) => h.update(data),
            State::Sha1(h) => h.update(data),
            State::Sha256(h) => h.update(data),
            State::Sha512(h) => h.update(data),
            State::Sha3(h) => h.update(data),
            State::Blake2b(h) => h.update(data),
            State::Blake2s(h) => h.update(data),
            State::Blake3(h) => h.update(data),
            State::Crc32(h) => h.update(data),
            State::Crc64(h) => h.update(data),
        }
    }

    pub fn finish(self) -> Vec<u8> {
        match self.state {
            State::Md5(h) => h.finalize().to_vec(),
            State::Sha1(h) => h.finalize().to_vec(),
            State::Sha256(h) => {
                let is224 = h.is224;
                let out = h.finalize();
                if is224 {
                    out[..28].to_vec()
                } else {
                    out.to_vec()
                }
            }
            State::Sha512(h) => {
                let is384 = h.is384;
                let out = h.finalize();
                if is384 {
                    out[..48].to_vec()
                } else {
                    out.to_vec()
                }
            }
            State::Sha3(h) => h.finalize().to_vec(),
            State::Blake2b(h) => h.finalize().to_vec(),
            State::Blake2s(h) => h.finalize().to_vec(),
            State::Blake3(h) => h.finalize().to_vec(),
            State::Crc32(h) => h.finalize().to_vec(),
            State::Crc64(h) => h.finalize().to_vec(),
        }
    }
}

pub fn hash_bytes(algo: Algo, data: &[u8]) -> Vec<u8> {
    let mut h = Hasher::new(algo);
    h.update(data);
    h.finish()
}

pub fn hash_hex(algo: Algo, data: &[u8]) -> String {
    hex(&hash_bytes(algo, data))
}

/// 流式哈希文件（64 KiB 块，恒定内存）。
pub fn hash_file(algo: Algo, path: &std::path::Path) -> io::Result<Vec<u8>> {
    let mut f = std::fs::File::open(path)?;
    let mut h = Hasher::new(algo);
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        let n = read_full(&mut f, &mut buf)?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
        if n < buf.len() {
            break;
        }
    }
    Ok(h.finish())
}

// ================================================================ MD5

const MD5_K: [u32; 64] = [
    0xd76aa478, 0xe8c7b756, 0x242070db, 0xc1bdceee, 0xf57c0faf, 0x4787c62a, 0xa8304613, 0xfd469501,
    0x698098d8, 0x8b44f7af, 0xffff5bb1, 0x895cd7be, 0x6b901122, 0xfd987193, 0xa679438e, 0x49b40821,
    0xf61e2562, 0xc040b340, 0x265e5a51, 0xe9b6c7aa, 0xd62f105d, 0x02441453, 0xd8a1e681, 0xe7d3fbc8,
    0x21e1cde6, 0xc33707d6, 0xf4d50d87, 0x455a14ed, 0xa9e3e905, 0xfcefa3f8, 0x676f02d9, 0x8d2a4c8a,
    0xfffa3942, 0x8771f681, 0x6d9d6122, 0xfde5380c, 0xa4beea44, 0x4bdecfa9, 0xf6bb4b60, 0xbebfbc70,
    0x289b7ec6, 0xeaa127fa, 0xd4ef3085, 0x04881d05, 0xd9d4d039, 0xe6db99e5, 0x1fa27cf8, 0xc4ac5665,
    0xf4292244, 0x432aff97, 0xab9423a7, 0xfc93a039, 0x655b59c3, 0x8f0ccc92, 0xffeff47d, 0x85845dd1,
    0x6fa87e4f, 0xfe2ce6e0, 0xa3014314, 0x4e0811a1, 0xf7537e82, 0xbd3af235, 0x2ad7d2bb, 0xeb86d391,
];

const MD5_S: [u32; 64] = [
    7, 12, 17, 22, 7, 12, 17, 22, 7, 12, 17, 22, 7, 12, 17, 22, 5, 9, 14, 20, 5, 9, 14, 20, 5, 9,
    14, 20, 5, 9, 14, 20, 4, 11, 16, 23, 4, 11, 16, 23, 4, 11, 16, 23, 4, 11, 16, 23, 6, 10, 15,
    21, 6, 10, 15, 21, 6, 10, 15, 21, 6, 10, 15, 21,
];

pub struct Md5 {
    h: [u32; 4],
    buf: [u8; 64],
    buflen: usize,
    len: u64,
}

impl Md5 {
    pub fn new() -> Self {
        Self { h: [0x67452301, 0xefcdab89, 0x98badcfe, 0x10325476], buf: [0; 64], buflen: 0, len: 0 }
    }

    fn block(&mut self, block: &[u8]) {
        let mut m = [0u32; 16];
        for (i, w) in m.iter_mut().enumerate() {
            *w = u32::from_le_bytes([block[i * 4], block[i * 4 + 1], block[i * 4 + 2], block[i * 4 + 3]]);
        }
        let (mut a, mut b, mut c, mut d) = (self.h[0], self.h[1], self.h[2], self.h[3]);
        for i in 0..64 {
            let (f, g) = match i / 16 {
                0 => ((b & c) | (!b & d), i),
                1 => ((d & b) | (!d & c), (5 * i + 1) % 16),
                2 => (b ^ c ^ d, (3 * i + 5) % 16),
                _ => (c ^ (b | !d), (7 * i) % 16),
            };
            let tmp = d;
            d = c;
            c = b;
            let sum = a
                .wrapping_add(f)
                .wrapping_add(MD5_K[i])
                .wrapping_add(m[g]);
            b = b.wrapping_add(sum.rotate_left(MD5_S[i]));
            a = tmp;
        }
        self.h[0] = self.h[0].wrapping_add(a);
        self.h[1] = self.h[1].wrapping_add(b);
        self.h[2] = self.h[2].wrapping_add(c);
        self.h[3] = self.h[3].wrapping_add(d);
    }

    pub fn update(&mut self, mut data: &[u8]) {
        self.len = self.len.wrapping_add(data.len() as u64);
        if self.buflen > 0 {
            let take = (64 - self.buflen).min(data.len());
            self.buf[self.buflen..self.buflen + take].copy_from_slice(&data[..take]);
            self.buflen += take;
            data = &data[take..];
            if self.buflen == 64 {
                let b = self.buf;
                self.block(&b);
                self.buflen = 0;
            }
        }
        while data.len() >= 64 {
            let (head, tail) = data.split_at(64);
            self.block(head);
            data = tail;
        }
        if !data.is_empty() {
            self.buf[..data.len()].copy_from_slice(data);
            self.buflen = data.len();
        }
    }

    pub fn finalize(mut self) -> [u8; 16] {
        let bitlen = self.len.wrapping_mul(8);
        let mut pad = vec![0x80u8];
        let rem = (self.buflen + 1) % 64;
        let zeros = if rem <= 56 { 56 - rem } else { 120 - rem };
        pad.extend(std::iter::repeat_n(0u8, zeros));
        pad.extend_from_slice(&bitlen.to_le_bytes());
        self.update(&pad);
        debug_assert_eq!(self.buflen, 0);
        let mut out = [0u8; 16];
        for (i, v) in self.h.iter().enumerate() {
            out[i * 4..i * 4 + 4].copy_from_slice(&v.to_le_bytes());
        }
        out
    }
}

impl Default for Md5 {
    fn default() -> Self {
        Self::new()
    }
}

// ================================================================ SHA1

pub struct Sha1 {
    h: [u32; 5],
    buf: [u8; 64],
    buflen: usize,
    len: u64,
}

impl Sha1 {
    pub fn new() -> Self {
        Self {
            h: [0x67452301, 0xefcdab89, 0x98badcfe, 0x10325476, 0xc3d2e1f0],
            buf: [0; 64],
            buflen: 0,
            len: 0,
        }
    }

    fn block(&mut self, block: &[u8]) {
        let mut w = [0u32; 80];
        for i in 0..16 {
            w[i] = u32::from_be_bytes([block[i * 4], block[i * 4 + 1], block[i * 4 + 2], block[i * 4 + 3]]);
        }
        for i in 16..80 {
            w[i] = (w[i - 3] ^ w[i - 8] ^ w[i - 14] ^ w[i - 16]).rotate_left(1);
        }
        let (mut a, mut b, mut c, mut d, mut e) = (self.h[0], self.h[1], self.h[2], self.h[3], self.h[4]);
        for (i, wi) in w.iter().enumerate() {
            let (f, k) = match i / 20 {
                0 => ((b & c) | (!b & d), 0x5a827999u32),
                1 => (b ^ c ^ d, 0x6ed9eba1),
                2 => ((b & c) | (b & d) | (c & d), 0x8f1bbcdc),
                _ => (b ^ c ^ d, 0xca62c1d6),
            };
            let tmp = a
                .rotate_left(5)
                .wrapping_add(f)
                .wrapping_add(e)
                .wrapping_add(k)
                .wrapping_add(*wi);
            e = d;
            d = c;
            c = b.rotate_left(30);
            b = a;
            a = tmp;
        }
        self.h[0] = self.h[0].wrapping_add(a);
        self.h[1] = self.h[1].wrapping_add(b);
        self.h[2] = self.h[2].wrapping_add(c);
        self.h[3] = self.h[3].wrapping_add(d);
        self.h[4] = self.h[4].wrapping_add(e);
    }

    pub fn update(&mut self, mut data: &[u8]) {
        self.len = self.len.wrapping_add(data.len() as u64);
        if self.buflen > 0 {
            let take = (64 - self.buflen).min(data.len());
            self.buf[self.buflen..self.buflen + take].copy_from_slice(&data[..take]);
            self.buflen += take;
            data = &data[take..];
            if self.buflen == 64 {
                let b = self.buf;
                self.block(&b);
                self.buflen = 0;
            }
        }
        while data.len() >= 64 {
            let (head, tail) = data.split_at(64);
            self.block(head);
            data = tail;
        }
        if !data.is_empty() {
            self.buf[..data.len()].copy_from_slice(data);
            self.buflen = data.len();
        }
    }

    pub fn finalize(mut self) -> [u8; 20] {
        let bitlen = self.len.wrapping_mul(8);
        let mut pad = vec![0x80u8];
        let rem = (self.buflen + 1) % 64;
        let zeros = if rem <= 56 { 56 - rem } else { 120 - rem };
        pad.extend(std::iter::repeat_n(0u8, zeros));
        pad.extend_from_slice(&bitlen.to_be_bytes());
        self.update(&pad);
        let mut out = [0u8; 20];
        for (i, v) in self.h.iter().enumerate() {
            out[i * 4..i * 4 + 4].copy_from_slice(&v.to_be_bytes());
        }
        out
    }
}

impl Default for Sha1 {
    fn default() -> Self {
        Self::new()
    }
}

// ================================================================ SHA256 / SHA224

const SHA256_K: [u32; 64] = [
    0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4, 0xab1c5ed5,
    0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe, 0x9bdc06a7, 0xc19bf174,
    0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f, 0x4a7484aa, 0x5cb0a9dc, 0x76f988da,
    0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7, 0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967,
    0x27b70a85, 0x2e1b2138, 0x4d2c6dfc, 0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85,
    0xa2bfe8a1, 0xa81a664b, 0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070,
    0x19a4c116, 0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
    0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7, 0xc67178f2,
];

pub struct Sha256 {
    h: [u32; 8],
    buf: [u8; 64],
    buflen: usize,
    len: u64,
    is224: bool,
}

/// SHA-NI（x86 SHA 扩展指令）加速路径。
///
/// 只是把「自研的 SHA-256 轮函数」换成 CPU 提供的等价指令，
/// 不引入任何第三方算法库；不支持的机器自动回落到标量实现。
#[cfg(target_arch = "x86_64")]
mod shani {
    use super::SHA256_K;
    use std::sync::atomic::{AtomicU8, Ordering};

    static SUPPORT: AtomicU8 = AtomicU8::new(0); // 0 未探测 / 1 支持 / 2 不支持

    /// 仅供测试：强制打开/关闭/自动探测，用来交叉验证两条实现。
    #[cfg(test)]
    pub fn set_forced(enabled: Option<bool>) {
        let v = match enabled {
            Some(true) => 1,
            Some(false) => 2,
            None => 0,
        };
        SUPPORT.store(v, Ordering::Relaxed);
    }

    pub fn available() -> bool {
        match SUPPORT.load(Ordering::Relaxed) {
            1 => true,
            2 => false,
            _ => {
                let ok = std::is_x86_feature_detected!("sha")
                    && std::is_x86_feature_detected!("sse4.1")
                    && std::is_x86_feature_detected!("ssse3");
                SUPPORT.store(if ok { 1 } else { 2 }, Ordering::Relaxed);
                ok
            }
        }
    }

    /// 压缩一个 64 字节块。`data.len()` 必须 >= 64。
    ///
    /// # Safety
    /// 调用方必须已确认 CPU 支持 sha / sse4.1 / ssse3，且 `data` 至少 64 字节。
    #[target_feature(enable = "sha,sse2,ssse3,sse4.1")]
    pub unsafe fn block(h: &mut [u32; 8], data: &[u8]) {
        use std::arch::x86_64::*;

        // 字节序掩码：把 16 字节反转成 big-endian 字序
        let mask = _mm_set_epi64x(0x0c0d_0e0f_0809_0a0b_u64 as i64, 0x0405_0607_0001_0203_u64 as i64);

        // 装入状态并重排成指令要求的 ABEF / CDGH 两组
        let mut tmp = _mm_loadu_si128(h.as_ptr() as *const __m128i);
        let mut cdgh = _mm_loadu_si128(h.as_ptr().add(4) as *const __m128i);
        tmp = _mm_shuffle_epi32(tmp, 0xB1); // CDAB
        cdgh = _mm_shuffle_epi32(cdgh, 0x1B); // HGFE
        let mut abef = _mm_alignr_epi8(tmp, cdgh, 8); // ABEF
        cdgh = _mm_blend_epi16(cdgh, tmp, 0xF0); // CDGH
        let abef_save = abef;
        let cdgh_save = cdgh;

        // 前 4 组（轮 0..16）的消息直接来自数据
        let mut m = [
            _mm_shuffle_epi8(_mm_loadu_si128(data.as_ptr() as *const __m128i), mask),
            _mm_shuffle_epi8(_mm_loadu_si128(data.as_ptr().add(16) as *const __m128i), mask),
            _mm_shuffle_epi8(_mm_loadu_si128(data.as_ptr().add(32) as *const __m128i), mask),
            _mm_shuffle_epi8(_mm_loadu_si128(data.as_ptr().add(48) as *const __m128i), mask),
        ];

        for g in 0..16usize {
            let i = g & 3;
            if g >= 4 {
                // 消息扩展：sig0 部分由 sha256msg1 做，sig1 部分由 msg2 + alignr 做
                let p1 = (g + 1) & 3;
                let p2 = (g + 2) & 3;
                let p3 = (g + 3) & 3;
                m[i] = _mm_sha256msg1_epu32(m[i], m[p1]);
                m[i] = _mm_add_epi32(m[i], _mm_alignr_epi8(m[p3], m[p2], 4));
                m[i] = _mm_sha256msg2_epu32(m[i], m[p3]);
            }
            let k = _mm_loadu_si128(SHA256_K.as_ptr().add(g * 4) as *const __m128i);
            let t = _mm_add_epi32(m[i], k);
            cdgh = _mm_sha256rnds2_epu32(cdgh, abef, t);
            let t2 = _mm_shuffle_epi32(t, 0x0E);
            abef = _mm_sha256rnds2_epu32(abef, cdgh, t2);
        }

        // 累加初值并还原成 ABCD / EFGH 布局
        abef = _mm_add_epi32(abef, abef_save);
        cdgh = _mm_add_epi32(cdgh, cdgh_save);
        let t = _mm_shuffle_epi32(abef, 0x1B); // FEBA
        let u = _mm_shuffle_epi32(cdgh, 0xB1); // DCHG
        let out0 = _mm_blend_epi16(t, u, 0xF0); // DCBA
        let out1 = _mm_alignr_epi8(u, t, 8); // HGFE
        _mm_storeu_si128(h.as_mut_ptr() as *mut __m128i, out0);
        _mm_storeu_si128(h.as_mut_ptr().add(4) as *mut __m128i, out1);
    }
}

impl Sha256 {
    pub fn new(is224: bool) -> Self {
        let h = if is224 {
            [0xc1059ed8, 0x367cd507, 0x3070dd17, 0xf70e5939, 0xffc00b31, 0x68581511, 0x64f98fa7, 0xbefa4fa4]
        } else {
            [0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab, 0x5be0cd19]
        };
        Self { h, buf: [0; 64], buflen: 0, len: 0, is224 }
    }

    /// 压缩一个块：优先走 SHA-NI，不支持的机器回落标量实现。
    fn block(&mut self, block: &[u8]) {
        #[cfg(target_arch = "x86_64")]
        if block.len() >= 64 && shani::available() {
            // SAFETY: available() 已确认 CPU 具备 sha/sse4.1/ssse3，且长度已检查
            unsafe { shani::block(&mut self.h, block) };
            return;
        }
        self.block_scalar(block);
    }

    fn block_scalar(&mut self, block: &[u8]) {
        let mut w = [0u32; 64];
        for i in 0..16 {
            w[i] = u32::from_be_bytes([block[i * 4], block[i * 4 + 1], block[i * 4 + 2], block[i * 4 + 3]]);
        }
        for i in 16..64 {
            let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
            let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
            w[i] = w[i - 16].wrapping_add(s0).wrapping_add(w[i - 7]).wrapping_add(s1);
        }
        let mut v = self.h;
        for i in 0..64 {
            let s1 = v[4].rotate_right(6) ^ v[4].rotate_right(11) ^ v[4].rotate_right(25);
            let ch = (v[4] & v[5]) ^ (!v[4] & v[6]);
            let t1 = v[7].wrapping_add(s1).wrapping_add(ch).wrapping_add(SHA256_K[i]).wrapping_add(w[i]);
            let s0 = v[0].rotate_right(2) ^ v[0].rotate_right(13) ^ v[0].rotate_right(22);
            let maj = (v[0] & v[1]) ^ (v[0] & v[2]) ^ (v[1] & v[2]);
            let t2 = s0.wrapping_add(maj);
            v[7] = v[6];
            v[6] = v[5];
            v[5] = v[4];
            v[4] = v[3].wrapping_add(t1);
            v[3] = v[2];
            v[2] = v[1];
            v[1] = v[0];
            v[0] = t1.wrapping_add(t2);
        }
        for (dst, src) in self.h.iter_mut().zip(v.iter()) {
            *dst = dst.wrapping_add(*src);
        }
    }

    pub fn update(&mut self, mut data: &[u8]) {
        self.len = self.len.wrapping_add(data.len() as u64);
        if self.buflen > 0 {
            let take = (64 - self.buflen).min(data.len());
            self.buf[self.buflen..self.buflen + take].copy_from_slice(&data[..take]);
            self.buflen += take;
            data = &data[take..];
            if self.buflen == 64 {
                let b = self.buf;
                self.block(&b);
                self.buflen = 0;
            }
        }
        while data.len() >= 64 {
            let (head, tail) = data.split_at(64);
            self.block(head);
            data = tail;
        }
        if !data.is_empty() {
            self.buf[..data.len()].copy_from_slice(data);
            self.buflen = data.len();
        }
    }

    pub fn finalize(mut self) -> [u8; 32] {
        let is224 = self.is224;
        let bitlen = self.len.wrapping_mul(8);
        let mut pad = vec![0x80u8];
        let rem = (self.buflen + 1) % 64;
        let zeros = if rem <= 56 { 56 - rem } else { 120 - rem };
        pad.extend(std::iter::repeat_n(0u8, zeros));
        pad.extend_from_slice(&bitlen.to_be_bytes());
        self.update(&pad);
        let mut out = [0u8; 32];
        for (i, v) in self.h.iter().enumerate() {
            out[i * 4..i * 4 + 4].copy_from_slice(&v.to_be_bytes());
        }
        if is224 {
            // SHA-224 输出前 28 字节，后续字节清零避免泄漏中间态
            for b in out[28..].iter_mut() {
                *b = 0;
            }
        }
        out
    }
}

// ================================================================ SHA512 / SHA384

const SHA512_K: [u64; 80] = [
    0x428a2f98d728ae22, 0x7137449123ef65cd, 0xb5c0fbcfec4d3b2f, 0xe9b5dba58189dbbc,
    0x3956c25bf348b538, 0x59f111f1b605d019, 0x923f82a4af194f9b, 0xab1c5ed5da6d8118,
    0xd807aa98a3030242, 0x12835b0145706fbe, 0x243185be4ee4b28c, 0x550c7dc3d5ffb4e2,
    0x72be5d74f27b896f, 0x80deb1fe3b1696b1, 0x9bdc06a725c71235, 0xc19bf174cf692694,
    0xe49b69c19ef14ad2, 0xefbe4786384f25e3, 0x0fc19dc68b8cd5b5, 0x240ca1cc77ac9c65,
    0x2de92c6f592b0275, 0x4a7484aa6ea6e483, 0x5cb0a9dcbd41fbd4, 0x76f988da831153b5,
    0x983e5152ee66dfab, 0xa831c66d2db43210, 0xb00327c898fb213f, 0xbf597fc7beef0ee4,
    0xc6e00bf33da88fc2, 0xd5a79147930aa725, 0x06ca6351e003826f, 0x142929670a0e6e70,
    0x27b70a8546d22ffc, 0x2e1b21385c26c926, 0x4d2c6dfc5ac42aed, 0x53380d139d95b3df,
    0x650a73548baf63de, 0x766a0abb3c77b2a8, 0x81c2c92e47edaee6, 0x92722c851482353b,
    0xa2bfe8a14cf10364, 0xa81a664bbc423001, 0xc24b8b70d0f89791, 0xc76c51a30654be30,
    0xd192e819d6ef5218, 0xd69906245565a910, 0xf40e35855771202a, 0x106aa07032bbd1b8,
    0x19a4c116b8d2d0c8, 0x1e376c085141ab53, 0x2748774cdf8eeb99, 0x34b0bcb5e19b48a8,
    0x391c0cb3c5c95a63, 0x4ed8aa4ae3418acb, 0x5b9cca4f7763e373, 0x682e6ff3d6b2b8a3,
    0x748f82ee5defb2fc, 0x78a5636f43172f60, 0x84c87814a1f0ab72, 0x8cc702081a6439ec,
    0x90befffa23631e28, 0xa4506cebde82bde9, 0xbef9a3f7b2c67915, 0xc67178f2e372532b,
    0xca273eceea26619c, 0xd186b8c721c0c207, 0xeada7dd6cde0eb1e, 0xf57d4f7fee6ed178,
    0x06f067aa72176fba, 0x0a637dc5a2c898a6, 0x113f9804bef90dae, 0x1b710b35131c471b,
    0x28db77f523047d84, 0x32caab7b40c72493, 0x3c9ebe0a15c9bebc, 0x431d67c49c100d4c,
    0x4cc5d4becb3e42b6, 0x597f299cfc657e2a, 0x5fcb6fab3ad6faec, 0x6c44198c4a475817,
];

pub struct Sha512 {
    h: [u64; 8],
    buf: [u8; 128],
    buflen: usize,
    len: u128,
    is384: bool,
}

impl Sha512 {
    pub fn new(is384: bool) -> Self {
        let h = if is384 {
            [
                0xcbbb9d5dc1059ed8, 0x629a292a367cd507, 0x9159015a3070dd17, 0x152fecd8f70e5939,
                0x67332667ffc00b31, 0x8eb44a8768581511, 0xdb0c2e0d64f98fa7, 0x47b5481dbefa4fa4,
            ]
        } else {
            [
                0x6a09e667f3bcc908, 0xbb67ae8584caa73b, 0x3c6ef372fe94f82b, 0xa54ff53a5f1d36f1,
                0x510e527fade682d1, 0x9b05688c2b3e6c1f, 0x1f83d9abfb41bd6b, 0x5be0cd19137e2179,
            ]
        };
        Self { h, buf: [0; 128], buflen: 0, len: 0, is384 }
    }

    fn block(&mut self, block: &[u8]) {
        let mut w = [0u64; 80];
        for i in 0..16 {
            let mut b = [0u8; 8];
            b.copy_from_slice(&block[i * 8..i * 8 + 8]);
            w[i] = u64::from_be_bytes(b);
        }
        for i in 16..80 {
            let s0 = w[i - 15].rotate_right(1) ^ w[i - 15].rotate_right(8) ^ (w[i - 15] >> 7);
            let s1 = w[i - 2].rotate_right(19) ^ w[i - 2].rotate_right(61) ^ (w[i - 2] >> 6);
            w[i] = w[i - 16].wrapping_add(s0).wrapping_add(w[i - 7]).wrapping_add(s1);
        }
        let mut v = self.h;
        for i in 0..80 {
            let s1 = v[4].rotate_right(14) ^ v[4].rotate_right(18) ^ v[4].rotate_right(41);
            let ch = (v[4] & v[5]) ^ (!v[4] & v[6]);
            let t1 = v[7].wrapping_add(s1).wrapping_add(ch).wrapping_add(SHA512_K[i]).wrapping_add(w[i]);
            let s0 = v[0].rotate_right(28) ^ v[0].rotate_right(34) ^ v[0].rotate_right(39);
            let maj = (v[0] & v[1]) ^ (v[0] & v[2]) ^ (v[1] & v[2]);
            let t2 = s0.wrapping_add(maj);
            v[7] = v[6];
            v[6] = v[5];
            v[5] = v[4];
            v[4] = v[3].wrapping_add(t1);
            v[3] = v[2];
            v[2] = v[1];
            v[1] = v[0];
            v[0] = t1.wrapping_add(t2);
        }
        for (dst, src) in self.h.iter_mut().zip(v.iter()) {
            *dst = dst.wrapping_add(*src);
        }
    }

    pub fn update(&mut self, mut data: &[u8]) {
        self.len = self.len.wrapping_add(data.len() as u128);
        if self.buflen > 0 {
            let take = (128 - self.buflen).min(data.len());
            self.buf[self.buflen..self.buflen + take].copy_from_slice(&data[..take]);
            self.buflen += take;
            data = &data[take..];
            if self.buflen == 128 {
                let b = self.buf;
                self.block(&b);
                self.buflen = 0;
            }
        }
        while data.len() >= 128 {
            let (head, tail) = data.split_at(128);
            self.block(head);
            data = tail;
        }
        if !data.is_empty() {
            self.buf[..data.len()].copy_from_slice(data);
            self.buflen = data.len();
        }
    }

    pub fn finalize(mut self) -> [u8; 64] {
        let bitlen = self.len.wrapping_mul(8);
        let mut pad = vec![0x80u8];
        let rem = (self.buflen + 1) % 128;
        let zeros = if rem <= 112 { 112 - rem } else { 240 - rem };
        pad.extend(std::iter::repeat_n(0u8, zeros));
        pad.extend_from_slice(&bitlen.to_be_bytes());
        self.update(&pad);
        let mut out = [0u8; 64];
        for (i, v) in self.h.iter().enumerate() {
            out[i * 8..i * 8 + 8].copy_from_slice(&v.to_be_bytes());
        }
        if self.is384 {
            for b in out[48..].iter_mut() {
                *b = 0;
            }
        }
        out
    }
}

// ================================================================ SHA3 (Keccak)

const KECCAK_RC: [u64; 24] = [
    0x0000000000000001, 0x0000000000008082, 0x800000000000808a, 0x8000000080008000,
    0x000000000000808b, 0x0000000080000001, 0x8000000080008081, 0x8000000000008009,
    0x000000000000008a, 0x0000000000000088, 0x0000000080008009, 0x000000008000000a,
    0x000000008000808b, 0x800000000000008b, 0x8000000000008089, 0x8000000000008003,
    0x8000000000008002, 0x8000000000000080, 0x000000000000800a, 0x800000008000000a,
    0x8000000080008081, 0x8000000000008080, 0x0000000080000001, 0x8000000080008008,
];

/// rho 旋转量，按 a[x + 5y] 索引。
const KECCAK_RHO: [u32; 25] = [
    0, 1, 62, 28, 27, 36, 44, 6, 55, 20, 3, 10, 43, 25, 39, 41, 45, 15, 21, 8, 18, 2, 61, 56, 14,
];

fn keccak_f(a: &mut [u64; 25]) {
    for rc in KECCAK_RC {
        // theta
        let mut c = [0u64; 5];
        for x in 0..5 {
            c[x] = a[x] ^ a[x + 5] ^ a[x + 10] ^ a[x + 15] ^ a[x + 20];
        }
        for x in 0..5 {
            let d = c[(x + 4) % 5] ^ c[(x + 1) % 5].rotate_left(1);
            for y in 0..5 {
                a[x + 5 * y] ^= d;
            }
        }
        // rho + pi
        let mut b = [0u64; 25];
        for x in 0..5 {
            for y in 0..5 {
                b[y + 5 * ((2 * x + 3 * y) % 5)] = a[x + 5 * y].rotate_left(KECCAK_RHO[x + 5 * y]);
            }
        }
        // chi
        for y in 0..5 {
            for x in 0..5 {
                a[x + 5 * y] = b[x + 5 * y] ^ ((!b[(x + 1) % 5 + 5 * y]) & b[(x + 2) % 5 + 5 * y]);
            }
        }
        // iota
        a[0] ^= rc;
    }
}

pub struct Sha3 {
    state: [u64; 25],
    buf: Vec<u8>,
    rate: usize,
    out_len: usize,
}

impl Sha3 {
    pub fn new(out_len: usize) -> Self {
        Self { state: [0; 25], buf: Vec::with_capacity(200), rate: 200 - 2 * out_len, out_len }
    }

    fn absorb_block(&mut self, block: &[u8]) {
        debug_assert_eq!(block.len(), self.rate);
        for (i, chunk) in block.chunks_exact(8).enumerate() {
            let mut b = [0u8; 8];
            b.copy_from_slice(chunk);
            self.state[i] ^= u64::from_le_bytes(b);
        }
        keccak_f(&mut self.state);
    }

    pub fn update(&mut self, mut data: &[u8]) {
        while !data.is_empty() {
            let take = (self.rate - self.buf.len()).min(data.len());
            self.buf.extend_from_slice(&data[..take]);
            data = &data[take..];
            if self.buf.len() == self.rate {
                let blk = std::mem::take(&mut self.buf);
                self.absorb_block(&blk);
                self.buf = blk;
                self.buf.clear();
            }
        }
    }

    pub fn finalize(mut self) -> Vec<u8> {
        // SHA3 域分隔 0x06，末字节置 0x80（pad10*1）
        let mut pad = vec![0x06u8];
        while (self.buf.len() + pad.len()) % self.rate != 0 {
            pad.push(0);
        }
        if let Some(last) = pad.last_mut() {
            *last |= 0x80;
        }
        let mut all = std::mem::take(&mut self.buf);
        all.extend_from_slice(&pad);
        for blk in all.chunks(self.rate) {
            let blk = blk.to_vec();
            self.absorb_block(&blk);
        }
        let mut out = Vec::with_capacity(self.out_len);
        'outer: loop {
            for i in 0..self.rate / 8 {
                out.extend_from_slice(&self.state[i].to_le_bytes());
                if out.len() >= self.out_len {
                    break 'outer;
                }
            }
            keccak_f(&mut self.state);
        }
        out.truncate(self.out_len);
        out
    }
}

// ================================================================ CRC32 / CRC64

fn crc_table32() -> [u32; 256] {
    let mut t = [0u32; 256];
    let mut i = 0;
    while i < 256 {
        let mut c = i as u32;
        let mut k = 0;
        while k < 8 {
            c = if c & 1 != 0 { 0xEDB8_8320 ^ (c >> 1) } else { c >> 1 };
            k += 1;
        }
        t[i] = c;
        i += 1;
    }
    t
}

fn crc_table64() -> [u64; 256] {
    let mut t = [0u64; 256];
    let mut i = 0;
    while i < 256 {
        let mut c = i as u64;
        let mut k = 0;
        while k < 8 {
            c = if c & 1 != 0 { 0xC96C_5795_D787_0F42 ^ (c >> 1) } else { c >> 1 };
            k += 1;
        }
        t[i] = c;
        i += 1;
    }
    t
}

pub struct Crc32 {
    table: [u32; 256],
    crc: u32,
}

impl Crc32 {
    pub fn new() -> Self {
        Self { table: crc_table32(), crc: 0xFFFF_FFFF }
    }
    pub fn update(&mut self, data: &[u8]) {
        let mut c = self.crc;
        for &b in data {
            c = self.table[((c ^ b as u32) & 0xFF) as usize] ^ (c >> 8);
        }
        self.crc = c;
    }
    pub fn finalize(self) -> [u8; 4] {
        (self.crc ^ 0xFFFF_FFFF).to_le_bytes()
    }
    pub fn value(&self) -> u32 {
        self.crc ^ 0xFFFF_FFFF
    }
}

impl Default for Crc32 {
    fn default() -> Self {
        Self::new()
    }
}

pub struct Crc64 {
    table: [u64; 256],
    crc: u64,
}

impl Crc64 {
    pub fn new() -> Self {
        Self { table: crc_table64(), crc: u64::MAX }
    }
    pub fn update(&mut self, data: &[u8]) {
        let mut c = self.crc;
        for &b in data {
            c = self.table[((c ^ b as u64) & 0xFF) as usize] ^ (c >> 8);
        }
        self.crc = c;
    }
    pub fn finalize(self) -> [u8; 8] {
        (self.crc ^ u64::MAX).to_be_bytes()
    }
    pub fn value(&self) -> u64 {
        self.crc ^ u64::MAX
    }
}

impl Default for Crc64 {
    fn default() -> Self {
        Self::new()
    }
}

/// 便捷函数：计算数据的 CRC32（zip / gzip / 7z 用）。
pub fn crc32(data: &[u8]) -> u32 {
    let mut c = Crc32::new();
    c.update(data);
    c.value()
}

/// CRC32 原始状态推进（无 init / 无 xorout），ZipCrypto 密钥流依赖它。
pub fn crc32_raw(crc: u32, byte: u8) -> u32 {
    static TABLE: std::sync::OnceLock<[u32; 256]> = std::sync::OnceLock::new();
    let t = TABLE.get_or_init(crc_table32);
    (crc >> 8) ^ t[((crc ^ byte as u32) & 0xFF) as usize]
}

/// 便捷函数：计算数据的 CRC64（xz 用）。
pub fn crc64(data: &[u8]) -> u64 {
    let mut c = Crc64::new();
    c.update(data);
    c.value()
}

// ================================================================ 测试

#[cfg(test)]
mod tests {
    use super::*;

    fn hx(a: Algo, s: &str) -> String {
        hash_hex(a, s.as_bytes())
    }

    /// SHA-NI 与标量实现必须给出同一结果（跨多个块、含非整块尾巴）。
    #[cfg(target_arch = "x86_64")]
    #[test]
    fn shani_matches_scalar() {
        if !shani::available() {
            return; // 机器不支持 SHA-NI，标量即唯一实现
        }
        // 故意造一个「乱序、非全零」的语料，长度也不是 64 的倍数
        let data: Vec<u8> = (0..5000u32).map(|i| (i.wrapping_mul(37) % 251) as u8).collect();

        let digest = |ni: Option<bool>, d: &[u8]| {
            shani::set_forced(ni);
            let mut h = Sha256::new(false);
            h.update(d);
            h.finalize()
        };

        // 多块 + 尾巴
        assert_eq!(digest(Some(true), &data), digest(Some(false), &data));
        // 恰好整块
        assert_eq!(digest(Some(true), &data[..4096]), digest(Some(false), &data[..4096]));
        // 单块以内
        assert_eq!(digest(Some(true), &data[..37]), digest(Some(false), &data[..37]));
        // 空输入
        assert_eq!(digest(Some(true), b""), digest(Some(false), b""));

        shani::set_forced(None); // 恢复自动探测
    }

    #[test]
    fn md5_vectors() {
        assert_eq!(hx(Algo::Md5, ""), "d41d8cd98f00b204e9800998ecf8427e");
        assert_eq!(hx(Algo::Md5, "abc"), "900150983cd24fb0d6963f7d28e17f72");
        assert_eq!(hx(Algo::Md5, "message digest"), "f96b697d7cb7938d525a2f31aaf161d0");
        assert_eq!(
            hx(Algo::Md5, "12345678901234567890123456789012345678901234567890123456789012345678901234567890"),
            "57edf4a22be3c955ac49da2e2107b67a"
        );
    }

    #[test]
    fn sha1_vectors() {
        assert_eq!(hx(Algo::Sha1, "abc"), "a9993e364706816aba3e25717850c26c9cd0d89d");
        assert_eq!(hx(Algo::Sha1, ""), "da39a3ee5e6b4b0d3255bfef95601890afd80709");
        assert_eq!(
            hx(Algo::Sha1, "abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq"),
            "84983e441c3bd26ebaae4aa1f95129e5e54670f1"
        );
    }

    #[test]
    fn sha256_vectors() {
        assert_eq!(hx(Algo::Sha256, "abc"), "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad");
        assert_eq!(hx(Algo::Sha256, ""), "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855");
        assert_eq!(
            hx(Algo::Sha224, "abc"),
            "23097d223405d8228642a477bda255b32aadbce4bda0b3f7e36c9da7"
        );
    }

    #[test]
    fn sha512_vectors() {
        assert_eq!(
            hx(Algo::Sha512, "abc"),
            "ddaf35a193617abacc417349ae20413112e6fa4e89a97ea20a9eeee64b55d39a\
             2192992a274fc1a836ba3c23a3feebbd454d4423643ce80e2a9ac94fa54ca49f"
                .replace(char::is_whitespace, "")
        );
        assert_eq!(
            hx(Algo::Sha384, "abc"),
            "cb00753f45a35e8bb5a03d699ac65007272c32ab0eded1631a8b605a43ff5bed\
             8086072ba1e7cc2358baeca134c825a7"
                .replace(char::is_whitespace, "")
        );
    }

    #[test]
    fn sha3_vectors() {
        assert_eq!(hx(Algo::Sha3_256, "abc"), "3a985da74fe225b2045c172d6bd390bd855f086e3e9d525b46bfe24511431532");
        assert_eq!(hx(Algo::Sha3_256, ""), "a7ffc6f8bf1ed76651c14756a061d662f580ff4de43b49fa82d80a4b80f8434a");
        assert_eq!(
            hx(Algo::Sha3_512, "abc"),
            "b751850b1a57168a5693cd924b6b096e08f621827444f70d884f5d0240d2712e\
             10e116e9192af3c91a7ec57647e3934057340b4cf408d5a56592f8274eec53f0"
                .replace(char::is_whitespace, "")
        );
        assert_eq!(
            hx(Algo::Sha3_224, "abc"),
            "e642824c3f8cf24ad09234ee7d3c766fc9a3a5168d0c94ad73b46fdf"
        );
    }

    #[test]
    fn crc_vectors() {
        assert_eq!(crc32(b"123456789"), 0xcbf43926);
        assert_eq!(crc32(b""), 0);
        assert_eq!(crc64(b"123456789"), 0x995dc9bbdf1939fa);
    }

    #[test]
    fn streaming_matches_oneshot() {
        let data: Vec<u8> = (0..100_000u32).map(|i| (i % 251) as u8).collect();
        for algo in [Algo::Md5, Algo::Sha1, Algo::Sha256, Algo::Sha512, Algo::Sha3_256, Algo::Crc32] {
            let one = hash_bytes(algo, &data);
            let mut h = Hasher::new(algo);
            for chunk in data.chunks(997) {
                h.update(chunk);
            }
            assert_eq!(one, h.finish(), "algo={}", algo.name());
        }
    }

    #[test]
    fn algo_parse_aliases() {
        assert_eq!(Algo::parse("SHA-256"), Some(Algo::Sha256));
        assert_eq!(Algo::parse("sha3_256"), Some(Algo::Sha3_256));
        assert_eq!(Algo::parse("blake3"), Some(Algo::Blake3));
        assert_eq!(Algo::parse("nope"), None);
    }
}
