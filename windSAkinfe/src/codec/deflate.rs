//! DEFLATE 压缩与解压（RFC 1951）自研实现。
//!
//! 解码器采用规范 Huffman 逐位解码（puff 算法），编码器为 LZ77(hash chain) +
//! 动态/固定 Huffman 自适应选择。

use std::cmp::Reverse;
use std::collections::BinaryHeap;
use std::io;

use crate::codec::bitio::{BitReader, BitWriter};

const MAXBITS: usize = 15;
const MIN_MATCH: usize = 3;
const MAX_MATCH: usize = 258;
const WINDOW: usize = 32768;
const HASH_BITS: usize = 15;
const HASH_SIZE: usize = 1 << HASH_BITS;
const BLOCK: usize = 65535;

const LENGTH_BASE: [u16; 29] = [
    3, 4, 5, 6, 7, 8, 9, 10, 11, 13, 15, 17, 19, 23, 27, 31, 35, 43, 51, 59, 67, 83, 99, 115, 131,
    163, 195, 227, 258,
];
const LENGTH_EXTRA: [u8; 29] = [
    0, 0, 0, 0, 0, 0, 0, 0, 1, 1, 1, 1, 2, 2, 2, 2, 3, 3, 3, 3, 4, 4, 4, 4, 5, 5, 5, 5, 0,
];
const DIST_BASE: [u16; 30] = [
    1, 2, 3, 4, 5, 7, 9, 13, 17, 25, 33, 49, 65, 97, 129, 193, 257, 385, 513, 769, 1025, 1537,
    2049, 3073, 4097, 6145, 8193, 12289, 16385, 24577,
];
const DIST_EXTRA: [u8; 30] = [
    0, 0, 0, 0, 1, 1, 2, 2, 3, 3, 4, 4, 5, 5, 6, 6, 7, 7, 8, 8, 9, 9, 10, 10, 11, 11, 12, 12, 13,
    13,
];
const CLEN_ORDER: [usize; 19] = [16, 17, 18, 0, 8, 7, 9, 6, 10, 5, 11, 4, 12, 3, 13, 2, 14, 1, 15];

// ================================================================ Huffman

pub struct Huffman {
    count: [u16; MAXBITS + 1],
    symbol: Vec<u16>,
}

impl Huffman {
    pub fn new(lengths: &[u8]) -> io::Result<Huffman> {
        let mut count = [0u16; MAXBITS + 1];
        for &l in lengths {
            if l as usize > MAXBITS {
                return Err(bad("Huffman 码长超限"));
            }
            count[l as usize] += 1;
        }
        // 过订阅检查
        let mut left: i32 = 1;
        for c in count.iter().take(MAXBITS + 1).skip(1) {
            left <<= 1;
            left -= *c as i32;
            if left < 0 {
                return Err(bad("Huffman 码过订阅"));
            }
        }
        let mut offs = [0u16; MAXBITS + 2];
        for len in 1..=MAXBITS {
            offs[len + 1] = offs[len] + count[len];
        }
        let mut symbol = vec![0u16; lengths.len()];
        for (sym, &l) in lengths.iter().enumerate() {
            if l != 0 {
                symbol[offs[l as usize] as usize] = sym as u16;
                offs[l as usize] += 1;
            }
        }
        Ok(Huffman { count, symbol })
    }

    #[inline]
    pub fn decode(&self, br: &mut BitReader<'_>) -> io::Result<u16> {
        let mut code: i32 = 0;
        let mut first: i32 = 0;
        let mut index: i32 = 0;
        for len in 1..=MAXBITS {
            code |= br.bits(1)? as i32;
            let count = self.count[len] as i32;
            if code - count < first {
                let pos = (index + (code - first)) as usize;
                return self.symbol.get(pos).copied().ok_or_else(|| bad("Huffman 符号越界"));
            }
            index += count;
            first += count;
            first <<= 1;
            code <<= 1;
        }
        Err(bad("Huffman 码无效"))
    }
}

fn bad(msg: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg.to_string())
}

// ================================================================ 解压

/// 解压 DEFLATE 数据流（不依赖外部长度信息）。
pub fn inflate(data: &[u8]) -> io::Result<Vec<u8>> {
    let mut br = BitReader::new(data);
    let mut out = Vec::new();
    loop {
        let last = br.bits(1)?;
        let btype = br.bits(2)?;
        match btype {
            0 => inflate_stored(&mut br, &mut out)?,
            1 => {
                let (lit, dist) = fixed_tables()?;
                inflate_block(&mut br, &mut out, &lit, &dist)?;
            }
            2 => {
                let (lit, dist) = dynamic_tables(&mut br)?;
                inflate_block(&mut br, &mut out, &lit, &dist)?;
            }
            _ => return Err(bad("非法的块类型 3")),
        }
        if last == 1 {
            break;
        }
    }
    Ok(out)
}

/// 带容量上限的 inflate（zip 条目已知原始大小，可预分配并校验）。
/// 解压并返回 DEFLATE 流实际消耗的输入字节数。
///
/// gzip 容器需要在 DEFLATE 数据末尾定位 8 字节尾部（CRC32 + ISIZE），
/// 因此必须知道流本身有多长。
pub fn inflate_with_consumed(data: &[u8]) -> io::Result<(Vec<u8>, usize)> {
    let mut br = BitReader::new(data);
    let mut out = Vec::new();
    loop {
        let last = br.bits(1)?;
        let btype = br.bits(2)?;
        match btype {
            0 => inflate_stored(&mut br, &mut out)?,
            1 => {
                let (lit, dist) = fixed_tables()?;
                inflate_block(&mut br, &mut out, &lit, &dist)?;
            }
            2 => {
                let (lit, dist) = dynamic_tables(&mut br)?;
                inflate_block(&mut br, &mut out, &lit, &dist)?;
            }
            _ => return Err(bad("非法的块类型 3")),
        }
        if last == 1 {
            break;
        }
    }
    // 末尾可能是半字节对齐：DEFLATE 流按字节向上取整
    br.align();
    let consumed = br.byte_pos();
    Ok((out, consumed))
}

pub fn inflate_limited(data: &[u8], expected: Option<usize>) -> io::Result<Vec<u8>> {    let mut out = inflate(data)?;
    if let Some(n) = expected {
        if out.len() != n {
            return Err(bad(&format!("解压长度不符：期望 {n}，实际 {}", out.len())));
        }
        out.shrink_to_fit();
    }
    Ok(out)
}

fn inflate_stored(br: &mut BitReader<'_>, out: &mut Vec<u8>) -> io::Result<()> {
    br.align();
    let hdr = br.read_bytes(4)?;
    let len = u16::from_le_bytes([hdr[0], hdr[1]]);
    let nlen = u16::from_le_bytes([hdr[2], hdr[3]]);
    if len != !nlen {
        return Err(bad("stored 块长度校验失败"));
    }
    let body = br.read_bytes(len as usize)?;
    out.extend_from_slice(body);
    Ok(())
}

fn inflate_block(
    br: &mut BitReader<'_>,
    out: &mut Vec<u8>,
    lit: &Huffman,
    dist: &Huffman,
) -> io::Result<()> {
    loop {
        let sym = lit.decode(br)?;
        match sym {
            0..=255 => out.push(sym as u8),
            256 => return Ok(()),
            257..=285 => {
                let i = (sym - 257) as usize;
                let len = LENGTH_BASE[i] as usize + br.bits(LENGTH_EXTRA[i] as u32)? as usize;
                let dsym = dist.decode(br)? as usize;
                if dsym >= 30 {
                    return Err(bad("距离符号越界"));
                }
                let d = DIST_BASE[dsym] as usize + br.bits(DIST_EXTRA[dsym] as u32)? as usize;
                if d > out.len() {
                    return Err(bad("回溯距离超出已解压数据"));
                }
                let start = out.len() - d;
                for k in 0..len {
                    let b = out[start + k];
                    out.push(b);
                }
            }
            _ => return Err(bad("非法 literal/length 符号")),
        }
    }
}

fn fixed_tables() -> io::Result<(Huffman, Huffman)> {
    let mut lit_len = vec![0u8; 288];
    for (i, l) in lit_len.iter_mut().enumerate() {
        *l = match i {
            0..=143 => 8,
            144..=255 => 9,
            256..=279 => 7,
            _ => 8,
        };
    }
    let dist_len = vec![5u8; 30];
    Ok((Huffman::new(&lit_len)?, Huffman::new(&dist_len)?))
}

fn dynamic_tables(br: &mut BitReader<'_>) -> io::Result<(Huffman, Huffman)> {
    let hlit = br.bits(5)? as usize + 257;
    let hdist = br.bits(5)? as usize + 1;
    let hclen = br.bits(4)? as usize + 4;
    if hlit > 286 {
        return Err(bad("HLIT 越界"));
    }
    if hdist > 30 {
        return Err(bad("HDIST 越界"));
    }
    let mut clen_len = [0u8; 19];
    for i in 0..hclen {
        clen_len[CLEN_ORDER[i]] = br.bits(3)? as u8;
    }
    let clen = Huffman::new(&clen_len)?;
    let mut lengths = vec![0u8; hlit + hdist];
    let mut i = 0usize;
    while i < lengths.len() {
        let sym = clen.decode(br)?;
        match sym {
            0..=15 => {
                lengths[i] = sym as u8;
                i += 1;
            }
            16 => {
                if i == 0 {
                    return Err(bad("码长复制缺少前值"));
                }
                let prev = lengths[i - 1];
                let rep = 3 + br.bits(2)? as usize;
                for _ in 0..rep {
                    if i >= lengths.len() {
                        return Err(bad("码长复制越界"));
                    }
                    lengths[i] = prev;
                    i += 1;
                }
            }
            17 => {
                let rep = 3 + br.bits(3)? as usize;
                i += rep;
                if i > lengths.len() {
                    return Err(bad("码长零复制越界"));
                }
            }
            18 => {
                let rep = 11 + br.bits(7)? as usize;
                i += rep;
                if i > lengths.len() {
                    return Err(bad("码长零复制越界"));
                }
            }
            _ => return Err(bad("非法码长符号")),
        }
    }
    let lit = Huffman::new(&lengths[..hlit])?;
    let dist = Huffman::new(&lengths[hlit..])?;
    Ok((lit, dist))
}

// ================================================================ 压缩

/// 压缩为原始 DEFLATE 流。`level` 0 = 仅存储，1..9 递增压缩强度。
pub fn deflate(data: &[u8], level: u8) -> Vec<u8> {
    let mut bw = BitWriter::with_capacity(data.len() / 2 + 64);
    if data.is_empty() {
        // 空输入：单个固定 Huffman 块 + EOB
        bw.write_bits(1, 1);
        bw.write_bits(1, 2);
        bw.write_code(0, 7);
        return bw.finish();
    }
    if level == 0 {
        let nblocks = data.len().div_ceil(BLOCK);
        for (i, chunk) in data.chunks(BLOCK).enumerate() {
            let last = i + 1 == nblocks;
            bw.write_bits(last as u32, 1);
            bw.write_bits(0, 2);
            bw.align();
            let len = chunk.len() as u16;
            bw.write_bytes(&len.to_le_bytes());
            bw.write_bytes(&(!len).to_le_bytes());
            bw.write_bytes(chunk);
        }
        return bw.finish();
    }
    let nblocks = data.len().div_ceil(BLOCK);
    for (i, chunk) in data.chunks(BLOCK).enumerate() {
        let last = i + 1 == nblocks;
        encode_block(&mut bw, data, i * BLOCK, chunk.len(), last, level);
    }
    bw.finish()
}

#[derive(Clone, Copy, Debug)]
enum Tok {
    Lit(u8),
    Match { len: u16, dist: u16 },
}

fn hash3(d: &[u8], i: usize) -> usize {
    let a = d[i] as usize;
    let b = d[i + 1] as usize;
    let c = d[i + 2] as usize;
    ((a << 10) ^ (b << 5) ^ c) & (HASH_SIZE - 1)
}

fn match_len(d: &[u8], a: usize, b: usize, limit: usize) -> usize {
    let mut l = 0usize;
    while l + 8 <= limit {
        let x = u64::from_le_bytes(d[a + l..a + l + 8].try_into().unwrap());
        let y = u64::from_le_bytes(d[b + l..b + l + 8].try_into().unwrap());
        if x != y {
            return l + ((x ^ y).trailing_zeros() / 8) as usize;
        }
        l += 8;
    }
    while l < limit && d[a + l] == d[b + l] {
        l += 1;
    }
    l
}

fn lz77(all: &[u8], off: usize, len: usize, level: u8) -> Vec<Tok> {
    let end = off + len;
    let max_chain = match level {
        1 => 8,
        2 => 16,
        3 => 32,
        4 => 64,
        5 => 128,
        6 => 256,
        7 => 512,
        8 => 1024,
        _ => 2048,
    };
    let mut head = vec![u32::MAX; HASH_SIZE];
    let mut prev = vec![u32::MAX; WINDOW];
    let mut toks = Vec::with_capacity(len / 2 + 16);
    let mut pos = off;

    // 预填窗口内历史（保证跨块引用）
    let hist_start = off.saturating_sub(WINDOW);
    let mut hp = hist_start;
    while hp + 2 < off {
        let h = hash3(all, hp);
        prev[hp % WINDOW] = head[h];
        head[h] = hp as u32;
        hp += 1;
    }

    while pos < end {
        let limit = MAX_MATCH.min(end - pos);
        let (mut best_len, mut best_dist) = (0usize, 0usize);
        if limit >= MIN_MATCH {
            let h = hash3(all, pos);
            let mut cand = head[h];
            let mut chain = 0usize;
            while cand != u32::MAX && chain < max_chain {
                let c = cand as usize;
                if c >= pos {
                    break;
                }
                let dist = pos - c;
                if dist > WINDOW {
                    break;
                }
                if best_len == 0 || (best_len < limit && all[c + best_len] == all[pos + best_len]) {
                    let l = match_len(all, c, pos, limit);
                    if l > best_len {
                        best_len = l;
                        best_dist = dist;
                        if l >= limit {
                            break;
                        }
                    }
                }
                let nxt = prev[c % WINDOW];
                if nxt != u32::MAX && nxt as usize >= c {
                    break;
                }
                cand = nxt;
                chain += 1;
            }
        }
        if best_len >= MIN_MATCH {
            toks.push(Tok::Match { len: best_len as u16, dist: best_dist as u16 });
            let stop = (pos + best_len).min(end);
            let mut k = pos;
            while k < stop {
                if k + 2 < end {
                    let h = hash3(all, k);
                    prev[k % WINDOW] = head[h];
                    head[h] = k as u32;
                }
                k += 1;
            }
            pos = stop;
        } else {
            toks.push(Tok::Lit(all[pos]));
            if pos + 2 < end {
                let h = hash3(all, pos);
                prev[pos % WINDOW] = head[h];
                head[h] = pos as u32;
            }
            pos += 1;
        }
    }
    toks
}

fn length_symbol(len: u16) -> (u16, u8, u16) {
    for i in 0..29 {
        let base = LENGTH_BASE[i];
        let span = 1u16 << LENGTH_EXTRA[i];
        if len >= base && len < base + span {
            return (257 + i as u16, LENGTH_EXTRA[i], len - base);
        }
    }
    (285, 0, 0)
}

fn dist_symbol(dist: u16) -> (u16, u8, u16) {
    for i in 0..30 {
        let base = DIST_BASE[i];
        let span = 1u16 << DIST_EXTRA[i];
        if dist >= base && dist < base + span {
            return (i as u16, DIST_EXTRA[i], dist - base);
        }
    }
    (29, 13, dist.saturating_sub(24577))
}

fn build_lengths(freqs: &[u32], max_bits: u8) -> Vec<u8> {
    let n = freqs.len();
    let mut lengths = vec![0u8; n];
    let used: Vec<usize> = (0..n).filter(|&i| freqs[i] > 0).collect();
    if used.is_empty() {
        return lengths;
    }
    if used.len() == 1 {
        lengths[used[0]] = 1;
        return lengths;
    }
    let mut f: Vec<u32> = freqs.to_vec();
    loop {
        let mut heap: BinaryHeap<Reverse<(u32, usize)>> = BinaryHeap::new();
        let mut left: Vec<Option<usize>> = Vec::new();
        let mut right: Vec<Option<usize>> = Vec::new();
        let mut leaf: Vec<Option<usize>> = vec![None; n];
        for &i in &used {
            let idx = left.len();
            left.push(None);
            right.push(None);
            leaf[i] = Some(idx);
            heap.push(Reverse((f[i], idx)));
        }
        while heap.len() > 1 {
            let Reverse((fa, ia)) = heap.pop().unwrap();
            let Reverse((fb, ib)) = heap.pop().unwrap();
            let idx = left.len();
            left.push(Some(ia));
            right.push(Some(ib));
            heap.push(Reverse((fa.saturating_add(fb), idx)));
        }
        let root = heap.pop().unwrap().0 .1;
        let mut depth = vec![0u32; left.len()];
        let mut stack = vec![(root, 0u32)];
        while let Some((i, d)) = stack.pop() {
            depth[i] = d;
            if let (Some(l), Some(r)) = (left[i], right[i]) {
                stack.push((l, d + 1));
                stack.push((r, d + 1));
            }
        }
        let mut maxd = 0u32;
        for (i, lf) in leaf.iter().enumerate() {
            if let Some(node) = lf {
                lengths[i] = depth[*node] as u8;
                maxd = maxd.max(depth[*node]);
            }
        }
        if maxd <= max_bits as u32 {
            return lengths;
        }
        for x in f.iter_mut() {
            if *x > 0 {
                *x = x.div_ceil(2);
            }
        }
    }
}

fn canonical_codes(lengths: &[u8]) -> Vec<u16> {
    let max = lengths.iter().copied().max().unwrap_or(0) as usize;
    let mut bl_count = vec![0u16; max + 1];
    for &l in lengths {
        if l > 0 {
            bl_count[l as usize] += 1;
        }
    }
    let mut next_code = vec![0u16; max + 2];
    let mut code = 0u16;
    for bits in 1..=max {
        code = (code + bl_count[bits - 1]) << 1;
        next_code[bits] = code;
    }
    let mut codes = vec![0u16; lengths.len()];
    for (i, &l) in lengths.iter().enumerate() {
        if l > 0 {
            codes[i] = next_code[l as usize];
            next_code[l as usize] += 1;
        }
    }
    codes
}

fn encode_block(bw: &mut BitWriter, all: &[u8], off: usize, len: usize, last: bool, level: u8) {
    let toks = lz77(all, off, len, level);

    let mut lf = [0u32; 286];
    let mut df = [0u32; 30];
    lf[256] = 1;
    let mut extra_bits_total = 0u64;
    for t in &toks {
        match *t {
            Tok::Lit(b) => lf[b as usize] += 1,
            Tok::Match { len, dist } => {
                let (ls, le, _) = length_symbol(len);
                lf[ls as usize] += 1;
                extra_bits_total += le as u64;
                let (ds, de, _) = dist_symbol(dist);
                df[ds as usize] += 1;
                extra_bits_total += de as u64;
            }
        }
    }

    let llen = build_lengths(&lf, 15);
    let dlen = build_lengths(&df, 15);
    let max_lsym = last_nonzero(&llen, 257);
    let max_dsym = last_nonzero(&dlen, 1);
    let hlit = max_lsym.max(257);
    let hdist = max_dsym.max(1);

    // 三种方式估算位数
    let dyn_payload: u64 = (0..286).map(|i| lf[i] as u64 * llen[i] as u64).sum::<u64>()
        + (0..30).map(|i| df[i] as u64 * dlen[i] as u64).sum::<u64>()
        + extra_bits_total;

    let rle = rle_lengths(&llen[..hlit], &dlen[..hdist]);
    let mut cl_freq = [0u32; 19];
    for &(s, _) in &rle {
        cl_freq[s as usize] += 1;
    }
    let clen = build_lengths(&cl_freq, 7);
    let hclen = {
        let mut n = 19usize;
        while n > 4 && clen[CLEN_ORDER[n - 1]] == 0 {
            n -= 1;
        }
        n
    };
    let mut header_bits = 3 + 5 + 5 + 4 + (hclen as u64) * 3;
    for &(s, _) in &rle {
        header_bits += clen[s as usize] as u64;
        header_bits += match s {
            16 => 2,
            17 => 3,
            18 => 7,
            _ => 0,
        };
    }
    let dyn_total = header_bits + dyn_payload;

    // 固定 Huffman
    let mut flit = vec![0u8; 288];
    for (i, l) in flit.iter_mut().enumerate() {
        *l = match i {
            0..=143 => 8,
            144..=255 => 9,
            256..=279 => 7,
            _ => 8,
        };
    }
    let fdist = vec![5u8; 30];
    let fixed_total: u64 = (0..286).map(|i| lf[i] as u64 * flit[i] as u64).sum::<u64>()
        + (0..30).map(|i| df[i] as u64 * fdist[i] as u64).sum::<u64>()
        + extra_bits_total
        + 3;

    // 存储块
    let stored_bits = (len as u64 + 5) * 8;

    if stored_bits < dyn_total && stored_bits < fixed_total {
        bw.write_bits(last as u32, 1);
        bw.write_bits(0, 2);
        bw.align();
        let n = len as u16;
        bw.write_bytes(&n.to_le_bytes());
        bw.write_bytes(&(!n).to_le_bytes());
        bw.write_bytes(&all[off..off + len]);
        return;
    }

    if fixed_total <= dyn_total {
        bw.write_bits(last as u32, 1);
        bw.write_bits(1, 2);
        let fcodes = canonical_codes(&flit);
        let fdcodes = canonical_codes(&fdist);
        for t in &toks {
            match *t {
                Tok::Lit(b) => bw.write_code(fcodes[b as usize], flit[b as usize]),
                Tok::Match { len, dist } => {
                    let (ls, le, lv) = length_symbol(len);
                    bw.write_code(fcodes[ls as usize], flit[ls as usize]);
                    if le > 0 {
                        bw.write_bits(lv as u32, le as u32);
                    }
                    let (ds, de, dv) = dist_symbol(dist);
                    bw.write_code(fdcodes[ds as usize], fdist[ds as usize]);
                    if de > 0 {
                        bw.write_bits(dv as u32, de as u32);
                    }
                }
            }
        }
        bw.write_code(fcodes[256], flit[256]);
        return;
    }

    // 动态 Huffman
    bw.write_bits(last as u32, 1);
    bw.write_bits(2, 2);
    bw.write_bits((hlit - 257) as u32, 5);
    bw.write_bits((hdist - 1) as u32, 5);
    bw.write_bits((hclen - 4) as u32, 4);
    for i in 0..hclen {
        bw.write_bits(clen[CLEN_ORDER[i]] as u32, 3);
    }
    let ccodes = canonical_codes(&clen);
    for &(s, extra) in &rle {
        bw.write_code(ccodes[s as usize], clen[s as usize]);
        match s {
            16 => bw.write_bits(extra as u32, 2),
            17 => bw.write_bits(extra as u32, 3),
            18 => bw.write_bits(extra as u32, 7),
            _ => {}
        }
    }
    let lcodes = canonical_codes(&llen);
    let dcodes = canonical_codes(&dlen);
    for t in &toks {
        match *t {
            Tok::Lit(b) => bw.write_code(lcodes[b as usize], llen[b as usize]),
            Tok::Match { len, dist } => {
                let (ls, le, lv) = length_symbol(len);
                bw.write_code(lcodes[ls as usize], llen[ls as usize]);
                if le > 0 {
                    bw.write_bits(lv as u32, le as u32);
                }
                let (ds, de, dv) = dist_symbol(dist);
                bw.write_code(dcodes[ds as usize], dlen[ds as usize]);
                if de > 0 {
                    bw.write_bits(dv as u32, de as u32);
                }
            }
        }
    }
    bw.write_code(lcodes[256], llen[256]);
}

fn last_nonzero(lengths: &[u8], min: usize) -> usize {
    let mut n = lengths.len();
    while n > min && lengths[n - 1] == 0 {
        n -= 1;
    }
    n.max(min)
}

/// 码长序列的游程编码 → (符号, 附加值)
fn rle_lengths(ll: &[u8], dl: &[u8]) -> Vec<(u8, u8)> {
    let all: Vec<u8> = ll.iter().chain(dl.iter()).copied().collect();
    let mut out = Vec::new();
    let mut i = 0usize;
    while i < all.len() {
        let v = all[i];
        let mut run = 1usize;
        while i + run < all.len() && all[i + run] == v {
            run += 1;
        }
        if v == 0 {
            let mut left = run;
            while left >= 11 {
                let take = left.min(138);
                out.push((18, (take - 11) as u8));
                left -= take;
            }
            while left >= 3 {
                let take = left.min(10);
                out.push((17, (take - 3) as u8));
                left -= take;
            }
            for _ in 0..left {
                out.push((0, 0));
            }
        } else {
            out.push((v, 0));
            let mut left = run - 1;
            while left >= 3 {
                let take = left.min(6);
                out.push((16, (take - 3) as u8));
                left -= take;
            }
            for _ in 0..left {
                out.push((v, 0));
            }
        }
        i += run;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_various() {
        let cases: Vec<Vec<u8>> = vec![
            b"".to_vec(),
            b"a".to_vec(),
            b"abcabcabcabcabcabc".to_vec(),
            b"Hello, world! Hello, world! Hello, world!".to_vec(),
            (0..=255u8).cycle().take(10_000).collect(),
            vec![0u8; 100_000],
            (0..50_000u32).map(|i| (i % 7) as u8).collect(),
        ];
        for (idx, data) in cases.iter().enumerate() {
            for level in [1u8, 6, 9] {
                let c = deflate(data, level);
                let d = inflate(&c).unwrap();
                assert_eq!(&d, data, "case {idx} level {level}");
            }
        }
    }

    #[test]
    fn level0_and_empty() {
        let data = b"stored block payload".to_vec();
        let c = deflate(&data, 0);
        assert_eq!(inflate(&c).unwrap(), data);
        let e = deflate(b"", 6);
        assert_eq!(inflate(&e).unwrap(), b"");
    }

    #[test]
    fn repetitive_compresses_well() {
        let data = vec![b'x'; 100_000];
        let c = deflate(&data, 9);
        assert!(c.len() < data.len() / 100, "压缩率不足：{}", c.len());
    }

    #[test]
    fn random_stored_fallback() {
        // 伪随机数据不可压缩，应回退到 stored 且体积接近原始
        let mut x = 0x1234_5678u32;
        let data: Vec<u8> = (0..5000)
            .map(|_| {
                x ^= x << 13;
                x ^= x >> 17;
                x ^= x << 5;
                (x & 0xFF) as u8
            })
            .collect();
        let c = deflate(&data, 6);
        assert_eq!(inflate(&c).unwrap(), data);
        assert!(c.len() < data.len() + 64, "体积膨胀过多：{}", c.len());
    }

    #[test]
    fn huffman_invalid() {
        assert!(Huffman::new(&[1, 1, 1]).is_err());
    }
}
