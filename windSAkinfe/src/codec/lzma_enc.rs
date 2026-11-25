//! LZMA / LZMA2 编码器（自研）。
//!
//! 采用贪心匹配 + rep 优先的决策策略（非最优解析），压缩率略低于 7-Zip
//! 的 optimum parser，但格式完全兼容。

use crate::codec::lzma::LzmaProps;

const PROB_INIT: u16 = 1024;
const K_NUM_STATES: usize = 12;
const K_NUM_POS_STATES_MAX: usize = 16;
const K_END_POS_MODEL_INDEX: u32 = 14;
const K_NUM_FULL_DISTANCES: usize = 1 << (K_END_POS_MODEL_INDEX / 2);
const MAX_MATCH: usize = 273;
const MIN_MATCH: usize = 2;
const HASH_BITS: usize = 16;
const HASH_SIZE: usize = 1 << HASH_BITS;

// ================================================================ Range 编码

pub struct RangeEncoder {
    low: u64,
    range: u32,
    cache: u8,
    cache_size: u64,
    out: Vec<u8>,
}

impl RangeEncoder {
    pub fn new() -> Self {
        Self { low: 0, range: 0xFFFF_FFFF, cache: 0, cache_size: 1, out: Vec::new() }
    }

    fn shift_low(&mut self) {
        if self.low < 0xFF00_0000u64 || (self.low >> 32) != 0 {
            let carry = (self.low >> 32) as u8;
            let mut temp = self.cache;
            loop {
                self.out.push(temp.wrapping_add(carry));
                temp = 0xFF;
                self.cache_size -= 1;
                if self.cache_size == 0 {
                    break;
                }
            }
            self.cache = ((self.low >> 24) & 0xFF) as u8;
        }
        self.cache_size += 1;
        self.low = (self.low & 0x00FF_FFFF) << 8;
    }

    #[inline]
    pub fn bit(&mut self, prob: &mut u16, bit: u32) {
        let bound = (self.range >> 11).wrapping_mul(*prob as u32);
        if bit == 0 {
            self.range = bound;
            *prob += (2048 - *prob) >> 5;
        } else {
            self.low += bound as u64;
            self.range -= bound;
            *prob -= *prob >> 5;
        }
        while self.range < (1 << 24) {
            self.range <<= 8;
            self.shift_low();
        }
    }

    pub fn direct_bits(&mut self, value: u32, n: u32) {
        for i in (0..n).rev() {
            self.range >>= 1;
            if (value >> i) & 1 != 0 {
                self.low += self.range as u64;
            }
            while self.range < (1 << 24) {
                self.range <<= 8;
                self.shift_low();
            }
        }
    }

    pub fn tree(&mut self, probs: &mut [u16], bits: u32, value: u32) {
        let mut m = 1u32;
        for i in (0..bits).rev() {
            let b = (value >> i) & 1;
            self.bit(&mut probs[m as usize], b);
            m = (m << 1) | b;
        }
    }

    pub fn reverse_tree(&mut self, probs: &mut [u16], bits: u32, value: u32) {
        let mut m = 1u32;
        for i in 0..bits {
            let b = (value >> i) & 1;
            self.bit(&mut probs[m as usize], b);
            m = (m << 1) | b;
        }
    }

    pub fn finish(mut self) -> Vec<u8> {
        for _ in 0..5 {
            self.shift_low();
        }
        self.out
    }
}

impl Default for RangeEncoder {
    fn default() -> Self {
        Self::new()
    }
}

// ================================================================ 长度编码

struct LenEncoder {
    choice: u16,
    choice2: u16,
    low: [u16; K_NUM_POS_STATES_MAX * 8],
    mid: [u16; K_NUM_POS_STATES_MAX * 8],
    high: [u16; 256],
}

impl LenEncoder {
    fn new() -> Self {
        Self {
            choice: PROB_INIT,
            choice2: PROB_INIT,
            low: [PROB_INIT; K_NUM_POS_STATES_MAX * 8],
            mid: [PROB_INIT; K_NUM_POS_STATES_MAX * 8],
            high: [PROB_INIT; 256],
        }
    }

    fn reset(&mut self) {
        *self = Self::new();
    }

    fn encode(&mut self, rc: &mut RangeEncoder, pos_state: usize, len: u32) {
        let l = len - MIN_MATCH as u32;
        if l < 8 {
            rc.bit(&mut self.choice, 0);
            rc.tree(&mut self.low[pos_state * 8..], 3, l);
        } else if l < 16 {
            rc.bit(&mut self.choice, 1);
            rc.bit(&mut self.choice2, 0);
            rc.tree(&mut self.mid[pos_state * 8..], 3, l - 8);
        } else {
            rc.bit(&mut self.choice, 1);
            rc.bit(&mut self.choice2, 1);
            rc.tree(&mut self.high, 8, l - 16);
        }
    }
}

// ================================================================ 匹配查找

struct MatchFinder {
    head: Vec<u32>,
    prev: Vec<u32>,
    window: usize,
    max_chain: usize,
}

impl MatchFinder {
    fn new(window: usize, max_chain: usize) -> Self {
        Self { head: vec![u32::MAX; HASH_SIZE], prev: vec![u32::MAX; window.max(1)], window, max_chain }
    }

    #[inline]
    fn hash(d: &[u8], i: usize) -> usize {
        if i + 4 > d.len() {
            return 0;
        }
        let v = u32::from_le_bytes([d[i], d[i + 1], d[i + 2], d[i + 3]]);
        (v.wrapping_mul(2_654_435_761) >> (32 - HASH_BITS)) as usize
    }

    #[inline]
    fn insert(&mut self, d: &[u8], pos: usize) {
        if pos + 4 > d.len() {
            return;
        }
        let h = Self::hash(d, pos);
        self.prev[pos % self.window] = self.head[h];
        self.head[h] = pos as u32;
    }

    /// 在 `pos` 处查找最长匹配，返回 (len, dist-1)。
    fn find(&self, d: &[u8], pos: usize, limit: usize) -> (usize, u32) {
        if limit < MIN_MATCH || pos + 4 > d.len() {
            return (0, 0);
        }
        let h = Self::hash(d, pos);
        let mut cand = self.head[h];
        let mut best_len = 0usize;
        let mut best_dist = 0u32;
        let mut chain = 0usize;
        while cand != u32::MAX && chain < self.max_chain {
            let c = cand as usize;
            if c >= pos {
                break;
            }
            let dist = pos - c;
            if dist > self.window {
                break;
            }
            if best_len == 0 || (best_len < limit && d[c + best_len] == d[pos + best_len]) {
                let mut l = 0usize;
                while l < limit && d[c + l] == d[pos + l] {
                    l += 1;
                }
                if l > best_len {
                    best_len = l;
                    best_dist = (dist - 1) as u32;
                    if l >= limit {
                        break;
                    }
                }
            }
            let nxt = self.prev[c % self.window];
            if nxt != u32::MAX && nxt as usize >= c {
                break;
            }
            cand = nxt;
            chain += 1;
        }
        (best_len, best_dist)
    }

    fn rep_len(&self, d: &[u8], pos: usize, rep: u32, limit: usize) -> usize {
        let dist = rep as usize + 1;
        if dist > pos {
            return 0;
        }
        let mut l = 0usize;
        while l < limit && d[pos - dist + l] == d[pos + l] {
            l += 1;
        }
        l
    }
}

// ================================================================ LZMA 编码器

pub struct LzmaEncoder {
    props: LzmaProps,
    state: usize,
    rep: [u32; 4],
    is_match: [u16; K_NUM_STATES * K_NUM_POS_STATES_MAX],
    is_rep: [u16; K_NUM_STATES],
    is_rep_g0: [u16; K_NUM_STATES],
    is_rep_g1: [u16; K_NUM_STATES],
    is_rep_g2: [u16; K_NUM_STATES],
    is_rep0_long: [u16; K_NUM_STATES * K_NUM_POS_STATES_MAX],
    pos_slot: [u16; 4 * 64],
    spec_pos: [u16; K_NUM_FULL_DISTANCES - K_END_POS_MODEL_INDEX as usize + 1],
    align: [u16; 16],
    len: LenEncoder,
    rep_len: LenEncoder,
    literal: Vec<u16>,
    finder: MatchFinder,
    need_reset: bool,
}

impl LzmaEncoder {
    pub fn new(props: LzmaProps, level: u8) -> Self {
        let lit_size = 0x300usize << (props.lc + props.lp);
        let max_chain = match level {
            0..=1 => 8,
            2 => 16,
            3 => 24,
            4 => 32,
            5 => 48,
            6 => 64,
            7 => 96,
            8 => 128,
            _ => 192,
        };
        let mut e = Self {
            props,
            state: 0,
            rep: [0; 4],
            is_match: [PROB_INIT; K_NUM_STATES * K_NUM_POS_STATES_MAX],
            is_rep: [PROB_INIT; K_NUM_STATES],
            is_rep_g0: [PROB_INIT; K_NUM_STATES],
            is_rep_g1: [PROB_INIT; K_NUM_STATES],
            is_rep_g2: [PROB_INIT; K_NUM_STATES],
            is_rep0_long: [PROB_INIT; K_NUM_STATES * K_NUM_POS_STATES_MAX],
            pos_slot: [PROB_INIT; 4 * 64],
            spec_pos: [PROB_INIT; K_NUM_FULL_DISTANCES - K_END_POS_MODEL_INDEX as usize + 1],
            align: [PROB_INIT; 16],
            len: LenEncoder::new(),
            rep_len: LenEncoder::new(),
            literal: vec![PROB_INIT; lit_size],
            finder: MatchFinder::new(1 << 20, max_chain),
            need_reset: true,
        };
        e.reset_state();
        e
    }

    pub fn reset_state(&mut self) {
        self.state = 0;
        self.rep = [0; 4];
        self.is_match = [PROB_INIT; K_NUM_STATES * K_NUM_POS_STATES_MAX];
        self.is_rep = [PROB_INIT; K_NUM_STATES];
        self.is_rep_g0 = [PROB_INIT; K_NUM_STATES];
        self.is_rep_g1 = [PROB_INIT; K_NUM_STATES];
        self.is_rep_g2 = [PROB_INIT; K_NUM_STATES];
        self.is_rep0_long = [PROB_INIT; K_NUM_STATES * K_NUM_POS_STATES_MAX];
        self.pos_slot = [PROB_INIT; 4 * 64];
        self.spec_pos = [PROB_INIT; K_NUM_FULL_DISTANCES - K_END_POS_MODEL_INDEX as usize + 1];
        self.align = [PROB_INIT; 16];
        self.len.reset();
        self.rep_len.reset();
        self.literal = vec![PROB_INIT; self.literal.len()];
    }

    fn encode_distance(&mut self, rc: &mut RangeEncoder, dist: u32, len: u32) {
        let len_state = ((len - MIN_MATCH as u32) as usize).min(3);
        let base_slot = len_state * 64;
        if dist < 4 {
            rc.tree(&mut self.pos_slot[base_slot..base_slot + 64], 6, dist);
            return;
        }
        let n = 31 - dist.leading_zeros();
        let slot = (n << 1) | ((dist >> (n - 1)) & 1);
        rc.tree(&mut self.pos_slot[base_slot..base_slot + 64], 6, slot);
        let num_direct = (slot >> 1) - 1;
        let base = (2 | (slot & 1)) << num_direct;
        let rem = dist - base;
        if slot < K_END_POS_MODEL_INDEX {
            let idx = (base - slot) as usize;
            rc.reverse_tree(&mut self.spec_pos[idx..], num_direct, rem);
        } else {
            rc.direct_bits(rem >> 4, num_direct - 4);
            rc.reverse_tree(&mut self.align, 4, rem & 15);
        }
    }

    /// 编码一个块（`all` 为完整数据，`start..start+len` 为本次范围），返回 range 流。
    pub fn encode_chunk(&mut self, all: &[u8], start: usize, len: usize) -> Vec<u8> {
        let mut rc = RangeEncoder::new();
        let pb_mask = (1usize << self.props.pb) - 1;
        let lp_mask = (1usize << self.props.lp) - 1;
        let lc = self.props.lc as usize;
        let end = (start + len).min(all.len());

        // 预填历史位置的 hash 链（跨块引用）
        let hist_start = start.saturating_sub(self.finder.window);
        let mut hp = hist_start;
        while hp < start {
            self.finder.insert(all, hp);
            hp += 1;
        }

        let mut pos = start;
        while pos < end {
            let limit = MAX_MATCH.min(end - pos);
            // 解码器按块内偏移还原 pos_state，编码端必须一致
            let inner = pos - start;
            let pos_state = inner & pb_mask;

            // rep 匹配（优先，代价低）
            let mut best_rep_len = 0usize;
            let mut best_rep_idx = usize::MAX;
            if limit >= MIN_MATCH {
                for i in 0..4 {
                    let l = self.finder.rep_len(all, pos, self.rep[i], limit);
                    if l > best_rep_len {
                        best_rep_len = l;
                        best_rep_idx = i;
                    }
                }
            }

            // 新匹配
            let (mut mlen, mut mdist) = (0usize, 0u32);
            if limit >= MIN_MATCH {
                let (l, d) = self.finder.find(all, pos, limit);
                mlen = l;
                mdist = d;
            }

            // 决策：rep 匹配长度 ≥ 新匹配时优先用 rep（省距离编码位）
            let use_rep = best_rep_idx != usize::MAX && best_rep_len >= MIN_MATCH && best_rep_len + 1 >= mlen;

            let ms_idx = self.state * K_NUM_POS_STATES_MAX + pos_state;
            if use_rep {
                let rl = best_rep_len as u32;
                rc.bit(&mut self.is_match[ms_idx], 1);
                rc.bit(&mut self.is_rep[self.state], 1);
                if best_rep_idx == 0 {
                    rc.bit(&mut self.is_rep_g0[self.state], 0);
                    rc.bit(&mut self.is_rep0_long[ms_idx], 1);
                } else {
                    rc.bit(&mut self.is_rep_g0[self.state], 1);
                    if best_rep_idx == 1 {
                        rc.bit(&mut self.is_rep_g1[self.state], 0);
                    } else {
                        rc.bit(&mut self.is_rep_g1[self.state], 1);
                        if best_rep_idx == 2 {
                            rc.bit(&mut self.is_rep_g2[self.state], 0);
                        } else {
                            rc.bit(&mut self.is_rep_g2[self.state], 1);
                        }
                    }
                    let d = self.rep[best_rep_idx];
                    for i in (1..=best_rep_idx).rev() {
                        self.rep[i] = self.rep[i - 1];
                    }
                    self.rep[0] = d;
                }
                self.rep_len.encode(&mut rc, pos_state, rl);
                self.state = if self.state < 7 { 8 } else { 11 };
                let mut k = pos;
                let stop = pos + best_rep_len;
                while k < stop {
                    self.finder.insert(all, k);
                    k += 1;
                }
                pos = stop;
                continue;
            }

            if mlen >= MIN_MATCH {
                let ml = mlen as u32;
                rc.bit(&mut self.is_match[ms_idx], 1);
                rc.bit(&mut self.is_rep[self.state], 0);
                self.len.encode(&mut rc, pos_state, ml);
                self.encode_distance(&mut rc, mdist, ml);
                self.rep[3] = self.rep[2];
                self.rep[2] = self.rep[1];
                self.rep[1] = self.rep[0];
                self.rep[0] = mdist;
                self.state = if self.state < 7 { 7 } else { 10 };
                let mut k = pos;
                let stop = pos + mlen;
                while k < stop {
                    self.finder.insert(all, k);
                    k += 1;
                }
                pos = stop;
                continue;
            }

            // 字面量
            self.finder.insert(all, pos);
            let byte = all[pos];
            let prev = if pos == 0 { 0u8 } else { all[pos - 1] };
            let lit_base = (((inner & lp_mask) << lc) + ((prev as usize) >> (8 - lc))) * 0x300;
            rc.bit(&mut self.is_match[ms_idx], 0);
            let mut symbol: usize = 1;
            {
                let probs = &mut self.literal[lit_base..lit_base + 0x300];
                if self.state >= 7 && (self.rep[0] as usize) < pos {
                    let mut match_byte = all[pos - (self.rep[0] as usize) - 1];
                    let mut i = 7i32;
                    while i >= 0 {
                        let match_bit = ((match_byte >> 7) & 1) as u32;
                        match_byte <<= 1;
                        let b = ((byte >> i) & 1) as u32;
                        let idx = ((1 + match_bit) as usize) * 0x100 + symbol;
                        rc.bit(&mut probs[idx], b);
                        symbol = (symbol << 1) | b as usize;
                        i -= 1;
                        if match_bit != b || symbol >= 0x100 {
                            break;
                        }
                    }
                    while i >= 0 && symbol < 0x100 {
                        let b = ((byte >> i) & 1) as u32;
                        rc.bit(&mut probs[symbol], b);
                        symbol = (symbol << 1) | b as usize;
                        i -= 1;
                    }
                } else {
                    for i in (0..8).rev() {
                        let b = ((byte >> i) & 1) as u32;
                        rc.bit(&mut probs[symbol], b);
                        symbol = (symbol << 1) | b as usize;
                    }
                }
            }
            self.state = if self.state < 4 {
                0
            } else if self.state < 10 {
                self.state - 3
            } else {
                self.state - 6
            };
            pos += 1;
        }

        rc.finish()
    }
}

/// 生成 LZMA2 流。
pub fn lzma2_compress(data: &[u8], level: u8, dict_log: Option<u32>) -> Vec<u8> {
    let props = LzmaProps { lc: 3, lp: 0, pb: 2 };
    let mut enc = LzmaEncoder::new(props, level);
    if let Some(dl) = dict_log {
        // 字典大小只影响匹配窗口上限，这里按需调整
        let w = 1usize << dl.min(26);
        enc.finder = MatchFinder::new(w, enc.finder.max_chain);
    }
    let mut out: Vec<u8> = Vec::with_capacity(data.len() / 2 + 64);
    let mut first = true;
    for (ci, chunk) in data.chunks(65536).enumerate() {
        if enc.need_reset {
            enc.reset_state();
        }
        let packed = enc.encode_chunk(data, ci * 65536, chunk.len());
        if packed.len() >= chunk.len() {
            // 未压缩块：control 1（无重置）/ 2（dict 重置）
            let control: u8 = if first { 2 } else { 1 };
            out.push(control);
            out.extend_from_slice(&((chunk.len() - 1) as u16).to_be_bytes());
            out.extend_from_slice(chunk);
            // 未压缩块后 LZMA 状态必须重置，否则与解码器不同步
            enc.need_reset = true;
        } else {
            let reset: u8 = if first {
                3
            } else if enc.need_reset {
                1
            } else {
                0
            };
            let control =
                0x80u8 | (reset << 5) | (((chunk.len() - 1) >> 16) as u8 & 0x1F);
            out.push(control);
            out.extend_from_slice(&((chunk.len() - 1) as u16).to_be_bytes());
            out.extend_from_slice(&((packed.len() - 1) as u16).to_be_bytes());
            if reset >= 2 {
                out.push(props.to_byte());
            }
            out.extend_from_slice(&packed);
            enc.need_reset = false;
        }
        first = false;
    }
    out.push(0);
    out
}

/// 生成 LZMA-alone（.lzma）流。
pub fn lzma_alone_compress(data: &[u8], level: u8) -> Vec<u8> {
    let props = LzmaProps { lc: 3, lp: 0, pb: 2 };
    let mut enc = LzmaEncoder::new(props, level);
    let packed = enc.encode_chunk(data, 0, data.len());
    let mut out = Vec::with_capacity(packed.len() + 13);
    out.push(props.to_byte());
    out.extend_from_slice(&(1u32 << 20).to_le_bytes());
    out.extend_from_slice(&(data.len() as u64).to_le_bytes());
    out.extend_from_slice(&packed);
    out
}

/// 校验：LZMA2 输出必须能被自家解码器还原（内部用于兜底自检）。
pub fn lzma2_roundtrip_ok(data: &[u8], packed: &[u8]) -> bool {
    match crate::codec::lzma::lzma2_decompress(packed, Some(data.len())) {
        Ok(v) => v == data,
        Err(_) => false,
    }
}
