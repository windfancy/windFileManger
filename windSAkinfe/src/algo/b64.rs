//! Base64（RFC 4648）：标准 / URL-safe / 无填充，支持流式。

#[derive(Clone, Copy, Debug)]
pub struct B64Config {
    pub url_safe: bool,
    pub pad: bool,
}

impl Default for B64Config {
    fn default() -> Self {
        Self { url_safe: false, pad: true }
    }
}

const STD: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
const URL: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";

fn table(cfg: &B64Config) -> &'static [u8; 64] {
    if cfg.url_safe {
        URL
    } else {
        STD
    }
}

fn decode_char(c: u8, url_safe: bool) -> Option<u32> {
    // 解码时两套字母表都接受：`-`/`_` 不可能合法出现在标准字母表里，
    // `+`/`/` 也不可能出现在 URL-safe 里，因此合并没有任何歧义。
    // 这样 `b64 -u --no-pad x | b64 -d -` 不带 `-u` 也能正确还原。
    let _ = url_safe;
    Some(match c {
        b'A'..=b'Z' => (c - b'A') as u32,
        b'a'..=b'z' => (c - b'a') as u32 + 26,
        b'0'..=b'9' => (c - b'0') as u32 + 52,
        b'+' | b'-' => 62,
        b'/' | b'_' => 63,
        _ => return None,
    })
}

/// 一次性编码。
pub fn encode(data: &[u8], cfg: B64Config) -> Vec<u8> {
    let t = table(&cfg);
    let mut out = Vec::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = *chunk.get(1).unwrap_or(&0) as u32;
        let b2 = *chunk.get(2).unwrap_or(&0) as u32;
        let n = (b0 << 16) | (b1 << 8) | b2;
        out.push(t[(n >> 18) as usize & 63]);
        out.push(t[(n >> 12) as usize & 63]);
        if chunk.len() > 1 {
            out.push(t[(n >> 6) as usize & 63]);
        } else if cfg.pad {
            out.push(b'=');
        }
        if chunk.len() > 2 {
            out.push(t[n as usize & 63]);
        } else if cfg.pad {
            out.push(b'=');
        }
    }
    out
}

/// 一次性解码（容忍空白字符与缺失填充）。
pub fn decode(input: &[u8], cfg: B64Config) -> Result<Vec<u8>, String> {
    let mut d = Decoder::new(cfg);
    let mut out = Vec::with_capacity(input.len() / 4 * 3);
    out.extend_from_slice(&d.push(input)?);
    out.extend_from_slice(&d.finish()?);
    Ok(out)
}

/// 流式编码器：`push` 可多次调用，`finish` 收尾。
pub struct Encoder {
    cfg: B64Config,
    carry: [u8; 2],
    carry_len: usize,
}

impl Encoder {
    pub fn new(cfg: B64Config) -> Self {
        Self { cfg, carry: [0; 2], carry_len: 0 }
    }

    pub fn push(&mut self, data: &[u8]) -> Vec<u8> {
        let t = table(&self.cfg);
        let mut out = Vec::with_capacity(data.len().div_ceil(3) * 4 + 4);
        let mut idx = 0usize;
        // 先把上次残留补齐成 3 字节
        if self.carry_len > 0 {
            let mut buf = [0u8; 3];
            buf[..self.carry_len].copy_from_slice(&self.carry[..self.carry_len]);
            let need = 3 - self.carry_len;
            let take = need.min(data.len());
            buf[self.carry_len..self.carry_len + take].copy_from_slice(&data[..take]);
            idx += take;
            if self.carry_len + take == 3 {
                emit3(&buf, t, &mut out);
                self.carry_len = 0;
            } else {
                self.carry[..self.carry_len + take].copy_from_slice(&buf[..self.carry_len + take]);
                self.carry_len += take;
                return out;
            }
        }
        let rest = &data[idx..];
        let whole = rest.len() - rest.len() % 3;
        for chunk in rest[..whole].chunks_exact(3) {
            emit3(chunk, t, &mut out);
        }
        let tail = &rest[whole..];
        if !tail.is_empty() {
            self.carry[..tail.len()].copy_from_slice(tail);
            self.carry_len = tail.len();
        }
        out
    }

    pub fn finish(self) -> Vec<u8> {
        let t = table(&self.cfg);
        let mut out = Vec::new();
        if self.carry_len > 0 {
            let mut buf = [0u8; 3];
            buf[..self.carry_len].copy_from_slice(&self.carry[..self.carry_len]);
            let n = ((buf[0] as u32) << 16) | ((buf[1] as u32) << 8) | buf[2] as u32;
            out.push(t[(n >> 18) as usize & 63]);
            out.push(t[(n >> 12) as usize & 63]);
            if self.carry_len > 1 {
                out.push(t[(n >> 6) as usize & 63]);
            } else if self.cfg.pad {
                out.push(b'=');
            }
            if self.cfg.pad {
                out.push(b'=');
            }
        }
        out
    }
}

fn emit3(b: &[u8], t: &[u8; 64], out: &mut Vec<u8>) {
    let n = ((b[0] as u32) << 16) | ((b[1] as u32) << 8) | b[2] as u32;
    out.push(t[(n >> 18) as usize & 63]);
    out.push(t[(n >> 12) as usize & 63]);
    out.push(t[(n >> 6) as usize & 63]);
    out.push(t[n as usize & 63]);
}

/// 流式解码器。
pub struct Decoder {
    cfg: B64Config,
    acc: u32,
    nbits: u32,
    pad_seen: bool,
    consumed: u64,
    leftover: u32,
}

impl Decoder {
    pub fn new(cfg: B64Config) -> Self {
        Self { cfg, acc: 0, nbits: 0, pad_seen: false, consumed: 0, leftover: 0 }
    }

    pub fn push(&mut self, data: &[u8]) -> Result<Vec<u8>, String> {
        let mut out = Vec::with_capacity(data.len() * 3 / 4 + 3);
        for &b in data {
            self.consumed += 1;
            match b {
                b'\r' | b'\n' | b' ' | b'\t' | 0x0b | 0x0c => continue,
                b'=' => {
                    self.pad_seen = true;
                    continue;
                }
                _ => {}
            }
            if self.pad_seen {
                return Err(format!("字符出现在填充之后（偏移 {}）", self.consumed));
            }
            let v = decode_char(b, self.cfg.url_safe).ok_or_else(|| {
                format!(
                    "非法 Base64 字符 0x{b:02x}（偏移 {}）",
                    self.consumed
                )
            })?;
            self.acc = (self.acc << 6) | v;
            self.nbits += 6;
            if self.nbits >= 8 {
                self.nbits -= 8;
                out.push((self.acc >> self.nbits) as u8);
                self.acc &= (1 << self.nbits) - 1;
            }
            self.leftover = self.acc;
        }
        Ok(out)
    }

    /// 收尾；严格模式下残留位必须为 0。
    pub fn finish(self) -> Result<Vec<u8>, String> {
        if self.nbits >= 6 {
            return Err(format!("Base64 尾部有 {} 个多余位", self.nbits));
        }
        if self.leftover != 0 {
            return Err("Base64 尾部残留位非零（数据可能被截断）".into());
        }
        Ok(Vec::new())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn enc(s: &str) -> String {
        String::from_utf8(encode(s.as_bytes(), B64Config::default())).unwrap()
    }

    #[test]
    fn rfc4648_vectors() {
        assert_eq!(enc(""), "");
        assert_eq!(enc("f"), "Zg==");
        assert_eq!(enc("fo"), "Zm8=");
        assert_eq!(enc("foo"), "Zm9v");
        assert_eq!(enc("foob"), "Zm9vYg==");
        assert_eq!(enc("fooba"), "Zm9vYmE=");
        assert_eq!(enc("foobar"), "Zm9vYmFy");
    }

    #[test]
    fn roundtrip_all_bytes() {
        let data: Vec<u8> = (0..=255u8).collect();
        let e = encode(&data, B64Config::default());
        assert_eq!(decode(&e, B64Config::default()).unwrap(), data);
    }

    #[test]
    fn url_safe_and_nopad() {
        let data = [0xfb, 0xef, 0xbe];
        let cfg = B64Config { url_safe: true, pad: false };
        let e = encode(&data, cfg);
        assert_eq!(String::from_utf8(e.clone()).unwrap(), "----");
        assert_eq!(decode(&e, cfg).unwrap(), data);
        // 标准表同一数据应产出 +/ 形式
        let std_e = encode(&data, B64Config::default());
        assert_eq!(String::from_utf8(std_e).unwrap(), "++++");
    }

    #[test]
    fn tolerate_whitespace() {
        let e = b"Zm9v\nYmFy\r\n";
        assert_eq!(decode(e, B64Config::default()).unwrap(), b"foobar");
    }

    #[test]
    fn reject_illegal() {
        assert!(decode(b"Zm9v!!!", B64Config::default()).is_err());
        assert!(decode(b"Zm9vYmFy=", B64Config::default()).is_ok());
        assert!(decode(b"Zg==Zg==", B64Config::default()).is_err());
    }

    #[test]
    fn streaming_consistency() {
        let data: Vec<u8> = (0..10_000u32).map(|i| (i * 31 % 256) as u8).collect();
        let one = encode(&data, B64Config::default());
        let mut s = Encoder::new(B64Config::default());
        let mut streamed = Vec::new();
        for chunk in data.chunks(97) {
            streamed.extend_from_slice(&s.push(chunk));
        }
        streamed.extend_from_slice(&s.finish());
        assert_eq!(one, streamed);

        let mut d = Decoder::new(B64Config::default());
        let mut back = Vec::new();
        for chunk in one.chunks(53) {
            back.extend_from_slice(&d.push(chunk).unwrap());
        }
        back.extend_from_slice(&d.finish().unwrap());
        assert_eq!(back, data);
    }
}
