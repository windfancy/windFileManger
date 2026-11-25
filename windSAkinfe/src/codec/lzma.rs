//! LZMA 编解码（range coder + 12 状态机）自研实现。
//!
//! 参数约定与 7-Zip 一致：`lc=3, lp=0, pb=2`，字典可配。
//! 本文件先实现解码器，编码器见 `lzma_enc` 部分。

use std::io;

const K_NUM_STATES: usize = 12;
const K_NUM_POS_BITS_MAX: usize = 4;
const K_NUM_POS_STATES_MAX: usize = 1 << K_NUM_POS_BITS_MAX;
const K_NUM_LEN_TO_POS_STATES: usize = 4;
const K_START_POS_MODEL_INDEX: u32 = 4;
const K_END_POS_MODEL_INDEX: u32 = 14;
const K_NUM_FULL_DISTANCES: usize = 1 << (K_END_POS_MODEL_INDEX / 2);
const K_MATCH_MIN_LEN: u32 = 2;
const PROB_INIT: u16 = 1024;

pub fn bad(msg: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg.into())
}

#[derive(Clone, Copy, Debug)]
pub struct LzmaProps {
    pub lc: u32,
    pub lp: u32,
    pub pb: u32,
}

impl LzmaProps {
    pub fn from_byte(b: u8) -> io::Result<Self> {
        if b >= 9 * 5 * 5 {
            return Err(bad(format!("LZMA 属性字节非法：{b}")));
        }
        let b = b as u32;
        Ok(Self { lc: b % 9, lp: (b / 9) % 5, pb: b / 45 })
    }
    pub fn to_byte(&self) -> u8 {
        (self.pb * 5 + self.lp) as u8 * 9 + self.lc as u8
    }
}

// ================================================================ Range 解码

pub struct RangeDecoder<'a> {
    data: &'a [u8],
    pos: usize,
    range: u32,
    code: u32,
    overrun: bool,
}

impl<'a> RangeDecoder<'a> {
    pub fn new(data: &'a [u8]) -> io::Result<Self> {
        if data.len() < 5 {
            return Err(bad("LZMA 数据过短"));
        }
        if data[0] != 0 {
            return Err(bad("LZMA range coder 首字节必须为 0"));
        }
        let mut code = 0u32;
        for b in data.iter().take(5).skip(1) {
            code = (code << 8) | *b as u32;
        }
        Ok(Self { data, pos: 5, range: 0xFFFF_FFFF, code, overrun: false })
    }

    #[inline]
    fn next_byte(&mut self) -> u32 {
        if self.pos < self.data.len() {
            let b = self.data[self.pos] as u32;
            self.pos += 1;
            b
        } else {
            self.overrun = true;
            0
        }
    }

    #[inline]
    fn normalize(&mut self) {
        if self.range < (1 << 24) {
            self.range <<= 8;
            self.code = (self.code << 8) | self.next_byte();
        }
    }

    #[inline]
    pub fn bit(&mut self, prob: &mut u16) -> u32 {
        let bound = (self.range >> 11).wrapping_mul(*prob as u32);
        
        let bit = if self.code < bound {
            self.range = bound;
            *prob += (2048 - *prob) >> 5;
            0
        } else {
            self.range -= bound;
            self.code -= bound;
            *prob -= *prob >> 5;
            1
        };
        self.normalize();
        bit
    }

    pub fn direct_bits(&mut self, n: u32) -> u32 {
        let mut res = 0u32;
        for _ in 0..n {
            self.range >>= 1;
            let bit = if self.code >= self.range {
                self.code -= self.range;
                1
            } else {
                0
            };
            res = (res << 1) | bit;
            self.normalize();
        }
        res
    }

    pub fn tree(&mut self, probs: &mut [u16], bits: u32) -> u32 {
        let mut m = 1u32;
        for _ in 0..bits {
            let b = self.bit(&mut probs[m as usize]);
            m = (m << 1) | b;
        }
        m - (1 << bits)
    }

    pub fn reverse_tree(&mut self, probs: &mut [u16], bits: u32) -> u32 {
        let mut m = 1u32;
        let mut sym = 0u32;
        for i in 0..bits {
            let b = self.bit(&mut probs[m as usize]);
            m = (m << 1) | b;
            sym |= b << i;
        }
        sym
    }
}

// ================================================================ 长度模型

struct LenDecoder {
    choice: u16,
    choice2: u16,
    low: [u16; K_NUM_POS_STATES_MAX * 8],
    mid: [u16; K_NUM_POS_STATES_MAX * 8],
    high: [u16; 256],
}

impl LenDecoder {
    fn new() -> Self {
        Self {
            choice: PROB_INIT,
            choice2: PROB_INIT,
            low: [PROB_INIT; K_NUM_POS_STATES_MAX * 8],
            mid: [PROB_INIT; K_NUM_POS_STATES_MAX * 8],
            high: [PROB_INIT; 256],
        }
    }

    fn decode(&mut self, rc: &mut RangeDecoder<'_>, pos_state: usize) -> u32 {
        if rc.bit(&mut self.choice) == 0 {
            rc.tree(&mut self.low[pos_state * 8..], 3)
        } else if rc.bit(&mut self.choice2) == 0 {
            8 + rc.tree(&mut self.mid[pos_state * 8..], 3)
        } else {
            16 + rc.tree(&mut self.high, 8)
        }
    }

    fn reset(&mut self) {
        self.choice = PROB_INIT;
        self.choice2 = PROB_INIT;
        self.low = [PROB_INIT; K_NUM_POS_STATES_MAX * 8];
        self.mid = [PROB_INIT; K_NUM_POS_STATES_MAX * 8];
        self.high = [PROB_INIT; 256];
    }
}

// ================================================================ LZMA 解码器

pub struct LzmaDecoder {
    props: LzmaProps,
    state: usize,
    rep: [u32; 4],
    is_match: [u16; K_NUM_STATES * K_NUM_POS_STATES_MAX],
    is_rep: [u16; K_NUM_STATES],
    is_rep_g0: [u16; K_NUM_STATES],
    is_rep_g1: [u16; K_NUM_STATES],
    is_rep_g2: [u16; K_NUM_STATES],
    is_rep0_long: [u16; K_NUM_STATES * K_NUM_POS_STATES_MAX],
    pos_slot: [u16; K_NUM_LEN_TO_POS_STATES * 64],
    spec_pos: [u16; K_NUM_FULL_DISTANCES - K_END_POS_MODEL_INDEX as usize + 1],
    align: [u16; 16],
    len: LenDecoder,
    rep_len: LenDecoder,
    literal: Vec<u16>,
}

impl LzmaDecoder {
    pub fn new(props: LzmaProps) -> Self {
        let lit_size = 0x300usize << (props.lc + props.lp);
        let mut d = Self {
            props,
            state: 0,
            rep: [0; 4],
            is_match: [PROB_INIT; K_NUM_STATES * K_NUM_POS_STATES_MAX],
            is_rep: [PROB_INIT; K_NUM_STATES],
            is_rep_g0: [PROB_INIT; K_NUM_STATES],
            is_rep_g1: [PROB_INIT; K_NUM_STATES],
            is_rep_g2: [PROB_INIT; K_NUM_STATES],
            is_rep0_long: [PROB_INIT; K_NUM_STATES * K_NUM_POS_STATES_MAX],
            pos_slot: [PROB_INIT; K_NUM_LEN_TO_POS_STATES * 64],
            spec_pos: [PROB_INIT; K_NUM_FULL_DISTANCES - K_END_POS_MODEL_INDEX as usize + 1],
            align: [PROB_INIT; 16],
            len: LenDecoder::new(),
            rep_len: LenDecoder::new(),
            literal: vec![PROB_INIT; lit_size],
        };
        d.reset_state();
        d
    }

    /// 只重置状态机与概率模型（保留字典内容，供 LZMA2 chunk 使用）。
    pub fn reset_state(&mut self) {
        self.state = 0;
        self.rep = [0; 4];
        self.is_match = [PROB_INIT; K_NUM_STATES * K_NUM_POS_STATES_MAX];
        self.is_rep = [PROB_INIT; K_NUM_STATES];
        self.is_rep_g0 = [PROB_INIT; K_NUM_STATES];
        self.is_rep_g1 = [PROB_INIT; K_NUM_STATES];
        self.is_rep_g2 = [PROB_INIT; K_NUM_STATES];
        self.is_rep0_long = [PROB_INIT; K_NUM_STATES * K_NUM_POS_STATES_MAX];
        self.pos_slot = [PROB_INIT; K_NUM_LEN_TO_POS_STATES * 64];
        self.spec_pos = [PROB_INIT; K_NUM_FULL_DISTANCES - K_END_POS_MODEL_INDEX as usize + 1];
        self.align = [PROB_INIT; 16];
        self.len.reset();
        self.rep_len.reset();
        self.literal = vec![PROB_INIT; self.literal.len()];
    }

    fn decode_distance(&mut self, rc: &mut RangeDecoder<'_>, len: u32) -> u32 {
        let len_state = ((len - K_MATCH_MIN_LEN) as usize).min(K_NUM_LEN_TO_POS_STATES - 1);
        let slot = rc.tree(&mut self.pos_slot[len_state * 64..], 6);
        if slot < K_START_POS_MODEL_INDEX {
            return slot;
        }
        let num_direct = (slot >> 1) - 1;
        let mut dist = (2 | (slot & 1)) << num_direct;
        if slot < K_END_POS_MODEL_INDEX {
            let idx = (dist - slot) as usize;
            dist += rc.reverse_tree(&mut self.spec_pos[idx..], num_direct);
        } else {
            dist += rc.direct_bits(num_direct - 4) << 4;
            dist += rc.reverse_tree(&mut self.align, 4);
        }
        dist
    }

    /// 解码 `packed` 中的 LZMA 数据，向 `out` 追加 `unpack_size` 字节。
    /// `dict_start` 为字典有效起点（LZMA2 dict reset 时更新）。
    pub fn decode_chunk(
        &mut self,
        packed: &[u8],
        out: &mut Vec<u8>,
        unpack_size: usize,
        dict_start: usize,
    ) -> io::Result<()> {
        let mut rc = RangeDecoder::new(packed)?;
        let out_start = out.len();
        let end = out_start + unpack_size;
        let pb_mask = (1usize << self.props.pb) - 1;
        let lp_mask = (1usize << self.props.lp) - 1;
        let lc = self.props.lc;

        while out.len() < end {
            let now = out.len() - out_start;
            let pos_state = now & pb_mask;
            let ms_idx = self.state * K_NUM_POS_STATES_MAX + pos_state;
            if rc.bit(&mut self.is_match[ms_idx]) == 0 {
                // 字面量
                let prev = if out.is_empty() { 0u8 } else { *out.last().unwrap() };
                let lit_state =
                    (((now & lp_mask) << lc) + ((prev as usize) >> (8 - lc))) * 0x300;
                let probs = &mut self.literal[lit_state..lit_state + 0x300];
                let mut symbol: u32 = 1;
                if self.state >= 7 {
                    let dist = self.rep[0] as usize + 1;
                    if dist > out.len() - dict_start {
                        return Err(bad("LZMA 字面量匹配距离越界"));
                    }
                    let mut match_byte = out[out.len() - dist] as u32;
                    loop {
                        let match_bit = (match_byte >> 7) & 1;
                        match_byte <<= 1;
                        let idx = ((1 + match_bit) as usize) * 0x100 + symbol as usize;
                        let bit = rc.bit(&mut probs[idx]);
                        symbol = (symbol << 1) | bit;
                        if match_bit != bit {
                            break;
                        }
                        if symbol >= 0x100 {
                            break;
                        }
                    }
                }
                while symbol < 0x100 {
                    let bit = rc.bit(&mut probs[symbol as usize]);
                    symbol = (symbol << 1) | bit;
                }
                out.push((symbol - 0x100) as u8);
                self.state = if self.state < 4 {
                    0
                } else if self.state < 10 {
                    self.state - 3
                } else {
                    self.state - 6
                };
                continue;
            }

            let len: u32;
            if rc.bit(&mut self.is_rep[self.state]) == 1 {
                // rep 匹配
                if rc.bit(&mut self.is_rep_g0[self.state]) == 0 {
                    if rc.bit(&mut self.is_rep0_long[ms_idx]) == 0 {
                        // short rep
                        self.state = if self.state < 7 { 9 } else { 11 };
                        let dist = self.rep[0] as usize + 1;
                        if dist > out.len() - dict_start {
                            return Err(bad("LZMA short rep 距离越界"));
                        }
                        let b = out[out.len() - dist];
                        out.push(b);
                        continue;
                    }
                } else {
                    let dist;
                    if rc.bit(&mut self.is_rep_g1[self.state]) == 0 {
                        dist = self.rep[1];
                    } else {
                        if rc.bit(&mut self.is_rep_g2[self.state]) == 0 {
                            dist = self.rep[2];
                        } else {
                            dist = self.rep[3];
                            self.rep[3] = self.rep[2];
                        }
                        self.rep[2] = self.rep[1];
                    }
                    self.rep[1] = self.rep[0];
                    self.rep[0] = dist;
                }
                len = self.rep_len.decode(&mut rc, pos_state) + K_MATCH_MIN_LEN;
                self.state = if self.state < 7 { 8 } else { 11 };
            } else {
                // 新匹配
                self.rep[3] = self.rep[2];
                self.rep[2] = self.rep[1];
                self.rep[1] = self.rep[0];
                len = self.len.decode(&mut rc, pos_state) + K_MATCH_MIN_LEN;
                self.state = if self.state < 7 { 7 } else { 10 };
                self.rep[0] = self.decode_distance(&mut rc, len);
            }

            let dist = self.rep[0] as usize + 1;
            if dist > out.len() - dict_start {
                return Err(bad(format!("LZMA 匹配距离越界：{dist}")));
            }
            let mut copied = 0usize;
            while copied < len as usize && out.len() < end {
                let b = out[out.len() - dist];
                out.push(b);
                copied += 1;
            }
        }

        if rc.overrun {
            return Err(bad("LZMA 数据提前结束"));
        }
        Ok(())
    }
}

/// 原始 .lzma（LZMA-alone）解码。
pub fn lzma_alone_decompress(data: &[u8]) -> io::Result<Vec<u8>> {
    if data.len() < 13 {
        return Err(bad("LZMA-alone 头不完整"));
    }
    let props = LzmaProps::from_byte(data[0])?;
    let _dict = u32::from_le_bytes([data[1], data[2], data[3], data[4]]);
    let usize_ = u64::from_le_bytes(data[5..13].try_into().unwrap());
    if usize_ == u64::MAX {
        return Err(bad("LZMA-alone 未声明解压长度，暂不支持流式未知长度"));
    }
    let mut out = Vec::with_capacity(usize_ as usize);
    let mut dec = LzmaDecoder::new(props);
    dec.decode_chunk(&data[13..], &mut out, usize_ as usize, 0)?;
    Ok(out)
}

// ================================================================ LZMA2

/// LZMA2 解压。`dict_hint` 仅影响预分配。
pub fn lzma2_decompress(data: &[u8], dict_hint: Option<usize>) -> io::Result<Vec<u8>> {
    let mut out: Vec<u8> = Vec::with_capacity(dict_hint.unwrap_or(0).min(1 << 26));
    let mut dec: Option<LzmaDecoder> = None;
    let mut dict_start = 0usize;
    let mut i = 0usize;
    loop {
        if i >= data.len() {
            return Err(bad("LZMA2 流缺少终止块"));
        }
        let control = data[i];
        i += 1;
        if control == 0 {
            break;
        }
        if control == 1 || control == 2 {
            if i + 2 > data.len() {
                return Err(bad("LZMA2 未压缩块头不完整"));
            }
            let size = u16::from_be_bytes([data[i], data[i + 1]]) as usize + 1;
            i += 2;
            if i + size > data.len() {
                return Err(bad("LZMA2 未压缩块数据越界"));
            }
            if control == 2 {
                // dict reset：清空字典历史
                dict_start = out.len();
                if let Some(d) = &mut dec {
                    d.reset_state();
                }
            }
            out.extend_from_slice(&data[i..i + size]);
            i += size;
            continue;
        }
        if control < 0x80 {
            return Err(bad(format!("LZMA2 非法控制字节 {control:#x}")));
        }
        let reset = (control >> 5) & 0x03;
        if i + 4 > data.len() {
            return Err(bad("LZMA2 LZMA 块头不完整"));
        }
        let unpack_size =
            (((control & 0x1F) as usize) << 16) + u16::from_be_bytes([data[i], data[i + 1]]) as usize + 1;
        let pack_size = u16::from_be_bytes([data[i + 2], data[i + 3]]) as usize + 1;
        i += 4;
        let mut props = None;
        if reset >= 2 {
            if i >= data.len() {
                return Err(bad("LZMA2 缺少属性字节"));
            }
            props = Some(LzmaProps::from_byte(data[i])?);
            i += 1;
        }
        if reset == 3 {
            dict_start = out.len();
        }
        if i + pack_size > data.len() {
            return Err(bad("LZMA2 压缩数据越界"));
        }
        let need_new = dec.is_none() || reset >= 2;
        if need_new {
            let p = props.ok_or_else(|| bad("LZMA2 首个 LZMA 块缺少属性"))?;
            dec = Some(LzmaDecoder::new(p));
        } else if reset == 1 {
            if let Some(d) = &mut dec {
                d.reset_state();
            }
        }
        let d = dec.as_mut().unwrap();
        d.decode_chunk(&data[i..i + pack_size], &mut out, unpack_size, dict_start)?;
        i += pack_size;
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn props_roundtrip() {
        for b in 0u8..225 {
            let p = LzmaProps::from_byte(b).unwrap();
            assert_eq!(p.to_byte(), b);
        }
        assert!(LzmaProps::from_byte(225).is_err());
    }

    #[test]
    fn props_default_is_93() {
        // lc=3 lp=0 pb=2 → 2*45 + 0*9 + 3 = 93
        assert_eq!(LzmaProps::from_byte(93).unwrap().lc, 3);
        assert_eq!(LzmaProps::from_byte(93).unwrap().pb, 2);
    }
}
