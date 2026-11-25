//! 通用工具：字节序读写、容量格式化、十六进制、时间。

use std::fmt::Write as _;
use std::io::{self, Read};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

// ---------------------------------------------------------------- 容量

const UNITS: [&str; 6] = ["B", "KiB", "MiB", "GiB", "TiB", "PiB"];

/// 人类可读容量，1024 进制，小数点后 1 位（B 不带小数）。
pub fn format_size(bytes: u64) -> String {
    if bytes < 1024 {
        return format!("{bytes} B");
    }
    let mut v = bytes as f64;
    let mut i = 0;
    while v >= 1024.0 && i < UNITS.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    format!("{v:.1} {}", UNITS[i])
}

/// 解析 `10K` `5M` `2G` `1.5GiB` `1024` 之类的容量串（1024 进制，大小写不敏感）。
pub fn parse_size(s: &str) -> Result<u64, String> {
    let t = s.trim();
    if t.is_empty() {
        return Err("空容量串".into());
    }
    let split = t.find(|c: char| !(c.is_ascii_digit() || c == '.' || c == '-')).unwrap_or(t.len());
    let (num, unit) = t.split_at(split);
    let n: f64 = num.trim().parse().map_err(|_| format!("非法数字：{num}"))?;
    if n < 0.0 {
        return Err(format!("容量不可为负：{num}"));
    }
    let u = unit.trim().to_ascii_lowercase();
    let mult: f64 = match u.as_str() {
        "" | "b" => 1.0,
        "k" | "kb" | "kib" => 1024.0,
        "m" | "mb" | "mib" => 1024.0 * 1024.0,
        "g" | "gb" | "gib" => 1024.0 * 1024.0 * 1024.0,
        "t" | "tb" | "tib" => 1024.0 * 1024.0 * 1024.0 * 1024.0,
        other => return Err(format!("未知容量单位：{other}")),
    };
    Ok((n * mult) as u64)
}

/// 解析时长 `7d` `24h` `30m` `90s` → 秒。
pub fn parse_duration(s: &str) -> Result<u64, String> {
    let t = s.trim();
    let (num, unit) = t.split_at(t.find(|c: char| c.is_ascii_alphabetic()).unwrap_or(t.len()));
    let n: u64 = num.trim().parse().map_err(|_| format!("非法时长：{t}"))?;
    let mult = match unit.trim().to_ascii_lowercase().as_str() {
        "s" | "" => 1,
        "m" => 60,
        "h" => 3600,
        "d" => 86400,
        "w" => 604800,
        other => return Err(format!("未知时长单位：{other}")),
    };
    Ok(n * mult)
}

// ---------------------------------------------------------------- 十六进制

/// 生成 `n` 位十六进制随机串（模板里的 `%u`）。
///
/// 不引 rand 库：用时间 + pid 做种子的 xorshift，够用于文件名去重，
/// 不用于任何安全场景。
pub fn rand_hex(n: usize) -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let mut x = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0x9E37_79B9_7F4A_7C15)
        ^ (std::process::id() as u64);
    let mut s = String::with_capacity(n);
    for _ in 0..n {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        s.push(char::from_digit((x & 0xf) as u32, 16).unwrap_or('0'));
    }
    s
}

pub fn hex(bytes: &[u8]) -> String {
    const D: &[u8; 16] = b"0123456789abcdef";
    let mut s = String::with_capacity(bytes.len() * 2);
    for &b in bytes {
        s.push(D[(b >> 4) as usize] as char);
        s.push(D[(b & 0xf) as usize] as char);
    }
    s
}

pub fn hex_upper(bytes: &[u8]) -> String {
    hex(bytes).to_ascii_uppercase()
}

pub fn unhex(s: &str) -> Result<Vec<u8>, String> {
    let b = s.as_bytes();
    if b.len() % 2 != 0 {
        return Err("十六进制长度必须为偶数".into());
    }
    let val = |c: u8| -> Result<u8, String> {
        match c {
            b'0'..=b'9' => Ok(c - b'0'),
            b'a'..=b'f' => Ok(c - b'a' + 10),
            b'A'..=b'F' => Ok(c - b'A' + 10),
            _ => Err(format!("非法十六进制字符：{}", c as char)),
        }
    };
    let mut out = Vec::with_capacity(b.len() / 2);
    for pair in b.chunks_exact(2) {
        out.push((val(pair[0])? << 4) | val(pair[1])?);
    }
    Ok(out)
}

// ---------------------------------------------------------------- 字节读取

/// 小端/大端顺序读取器，越界返回 `UnexpectedEof`。
pub struct ByteReader<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> ByteReader<'a> {
    pub fn new(buf: &'a [u8]) -> Self {
        Self { buf, pos: 0 }
    }
    pub fn pos(&self) -> usize {
        self.pos
    }
    pub fn remaining(&self) -> usize {
        self.buf.len().saturating_sub(self.pos)
    }
    pub fn is_empty(&self) -> bool {
        self.remaining() == 0
    }
    pub fn seek(&mut self, pos: usize) -> io::Result<()> {
        if pos > self.buf.len() {
            return Err(eof());
        }
        self.pos = pos;
        Ok(())
    }
    pub fn take(&mut self, n: usize) -> io::Result<&'a [u8]> {
        if self.remaining() < n {
            return Err(eof());
        }
        let s = &self.buf[self.pos..self.pos + n];
        self.pos += n;
        Ok(s)
    }
    pub fn u8(&mut self) -> io::Result<u8> {
        Ok(self.take(1)?[0])
    }
    pub fn array<const N: usize>(&mut self) -> io::Result<[u8; N]> {
        let s = self.take(N)?;
        let mut a = [0u8; N];
        a.copy_from_slice(s);
        Ok(a)
    }
    pub fn u16le(&mut self) -> io::Result<u16> {
        Ok(u16::from_le_bytes(self.array()?))
    }
    pub fn u32le(&mut self) -> io::Result<u32> {
        Ok(u32::from_le_bytes(self.array()?))
    }
    pub fn u64le(&mut self) -> io::Result<u64> {
        Ok(u64::from_le_bytes(self.array()?))
    }
    pub fn i64le(&mut self) -> io::Result<i64> {
        Ok(i64::from_le_bytes(self.array()?))
    }
    pub fn u32be(&mut self) -> io::Result<u32> {
        Ok(u32::from_be_bytes(self.array()?))
    }
    pub fn u64be(&mut self) -> io::Result<u64> {
        Ok(u64::from_be_bytes(self.array()?))
    }
    /// 到结尾为止的全部剩余数据。
    pub fn rest(&mut self) -> &'a [u8] {
        let s = &self.buf[self.pos..];
        self.pos = self.buf.len();
        s
    }
}

pub fn eof() -> io::Error {
    io::Error::new(io::ErrorKind::UnexpectedEof, "数据意外结束")
}

/// 7z / xz 风格的变长整数（首字节高位为续读掩码）。
pub fn read_varint7z(r: &mut ByteReader<'_>) -> io::Result<u64> {
    let first = r.u8()?;
    let mut mask: u8 = 0x80;
    let mut value: u64 = 0;
    for i in 0..8u32 {
        if first & mask == 0 {
            let high = (first as u64) & (mask as u64 - 1);
            value |= high << (8 * i);
            return Ok(value);
        }
        value |= (r.u8()? as u64) << (8 * i);
        mask >>= 1;
    }
    Ok(value)
}

/// 把整数写成 7z 变长形式（首字节高位为续读掩码，0xxxxxxx 表示单字节）。
pub fn write_varint7z(out: &mut Vec<u8>, value: u64) {
    let mut first: u8 = 0;
    let mut mask: u8 = 0x80;
    let mut i = 0usize;
    loop {
        if i == 8 {
            // 8 字节全 1：first 已被 OR 成 0xFF
            break;
        }
        if value < (1u64 << (7 * (i + 1))) {
            first |= (value >> (8 * i)) as u8;
            break;
        }
        first |= mask;
        mask >>= 1;
        i += 1;
    }
    out.push(first);
    for j in 0..i {
        out.push((value >> (8 * j)) as u8);
    }
}

// ---------------------------------------------------------------- 时间

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Civil {
    pub year: i32,
    pub month: u32,
    pub day: u32,
    pub hour: u32,
    pub min: u32,
    pub sec: u32,
}

impl Civil {
    /// 支持 `YYYY MM DD HH mm ss` 及 `/ - :` 直出。
    pub fn format(&self, fmt: &str) -> String {
        let mut out = String::new();
        let b: Vec<char> = fmt.chars().collect();
        let mut i = 0;
        while i < b.len() {
            let rest: String = b[i..].iter().collect();
            if rest.starts_with("YYYY") {
                let _ = write!(out, "{:04}", self.year);
                i += 4;
            } else if rest.starts_with("MM") {
                let _ = write!(out, "{:02}", self.month);
                i += 2;
            } else if rest.starts_with("DD") {
                let _ = write!(out, "{:02}", self.day);
                i += 2;
            } else if rest.starts_with("HH") {
                let _ = write!(out, "{:02}", self.hour);
                i += 2;
            } else if rest.starts_with("mm") {
                let _ = write!(out, "{:02}", self.min);
                i += 2;
            } else if rest.starts_with("ss") {
                let _ = write!(out, "{:02}", self.sec);
                i += 2;
            } else {
                out.push(b[i]);
                i += 1;
            }
        }
        out
    }

    pub fn stamp(&self) -> String {
        self.format("YYYY-MM-DD HH:mm:ss")
    }

    pub fn compact(&self) -> String {
        self.format("YYYYMMDD-HHmmss")
    }
}

/// Howard Hinnant 的 civil_from_days。
pub fn civil_from_days(z: i64) -> (i32, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    ((if m <= 2 { y + 1 } else { y }) as i32, m, d)
}

/// civil_from_days 的逆运算。
pub fn days_from_civil(y: i32, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y } as i64;
    let m = m as i64;
    let era = (if y >= 0 { y } else { y - 399 }) / 400;
    let yoe = y - era * 400;
    let mp = if m > 2 { m - 3 } else { m + 9 };
    let doy = (153 * mp + 2) / 5 + d as i64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

fn civil_from_secs_utc(secs: i64) -> Civil {
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    Civil {
        year,
        month,
        day,
        hour: (rem / 3600) as u32,
        min: ((rem % 3600) / 60) as u32,
        sec: (rem % 60) as u32,
    }
}

/// 本地时区相对 UTC 的偏移秒数（东八区 = +28800）。
#[cfg(windows)]
pub fn local_offset_secs() -> i64 {
    #[repr(C)]
    struct SysTime {
        y: u16,
        mo: u16,
        dow: u16,
        d: u16,
        h: u16,
        mi: u16,
        s: u16,
        ms: u16,
    }
    extern "system" {
        fn GetLocalTime(t: *mut SysTime);
        fn GetSystemTime(t: *mut SysTime);
    }
    let (mut l, mut u) = (
        SysTime { y: 0, mo: 0, dow: 0, d: 0, h: 0, mi: 0, s: 0, ms: 0 },
        SysTime { y: 0, mo: 0, dow: 0, d: 0, h: 0, mi: 0, s: 0, ms: 0 },
    );
    unsafe {
        GetLocalTime(&mut l);
        GetSystemTime(&mut u);
    }
    let to_secs = |t: &SysTime| -> i64 {
        days_from_civil(t.y as i32, t.mo as u32, t.d as u32) * 86_400
            + t.h as i64 * 3600
            + t.mi as i64 * 60
            + t.s as i64
    };
    to_secs(&l) - to_secs(&u)
}

#[cfg(unix)]
pub fn local_offset_secs() -> i64 {
    #[repr(C)]
    struct Tm {
        tm_sec: i32,
        tm_min: i32,
        tm_hour: i32,
        tm_mday: i32,
        tm_mon: i32,
        tm_year: i32,
        tm_wday: i32,
        tm_yday: i32,
        tm_isdst: i32,
        tm_gmtoff: i64,
        tm_zone: *const i8,
    }
    extern "C" {
        fn localtime_r(t: *const i64, tm: *mut Tm) -> *mut Tm;
    }
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let mut tm = Tm {
        tm_sec: 0,
        tm_min: 0,
        tm_hour: 0,
        tm_mday: 0,
        tm_mon: 0,
        tm_year: 0,
        tm_wday: 0,
        tm_yday: 0,
        tm_isdst: 0,
        tm_gmtoff: 0,
        tm_zone: std::ptr::null(),
    };
    unsafe {
        localtime_r(&now, &mut tm);
    }
    tm.tm_gmtoff
}

pub fn now_local() -> Civil {
    let utc = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or(Duration::ZERO)
        .as_secs() as i64;
    civil_from_secs_utc(utc + local_offset_secs())
}

/// 文件修改时间 → 本地时间。
pub fn system_time_to_local(t: SystemTime) -> Civil {
    match t.duration_since(UNIX_EPOCH) {
        Ok(d) => civil_from_secs_utc(d.as_secs() as i64 + local_offset_secs()),
        Err(e) => {
            let back = e.duration().as_secs() as i64;
            civil_from_secs_utc(-back + local_offset_secs())
        }
    }
}

/// 本地时间 → 距 UNIX_EPOCH 的秒数（反向于 `system_time_to_local`）。
pub fn local_to_unix_secs(c: &Civil) -> i64 {
    days_from_civil(c.year, c.month, c.day) * 86_400 + c.hour as i64 * 3600
        + c.min as i64 * 60
        + c.sec as i64
        - local_offset_secs()
}

/// 解析 `YYYY-MM-DD`、`YYYYMMDD`、`YYYY-MM-DD HH:MM[:SS]`、`YYYYMMDDHHMM[SS]`。
pub fn parse_datetime(s: &str) -> Result<Civil, String> {
    let digits: String = s.chars().filter(|c| c.is_ascii_digit()).collect();
    if digits.len() < 8 {
        return Err(format!("无法解析日期：{s}"));
    }
    let num = |a: usize, b: usize| -> Result<u32, String> {
        digits
            .get(a..b)
            .ok_or_else(|| format!("日期位数不足：{s}"))?
            .parse()
            .map_err(|_| format!("无法解析日期：{s}"))
    };
    let year = num(0, 4)? as i32;
    let month = num(4, 6)?;
    let day = num(6, 8)?;
    let (hour, min, sec) = if digits.len() >= 14 {
        (num(8, 10)?, num(10, 12)?, num(12, 14)?)
    } else if digits.len() >= 12 {
        (num(8, 10)?, num(10, 12)?, 0)
    } else {
        (0, 0, 0)
    };
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return Err(format!("日期越界：{s}"));
    }
    Ok(Civil { year, month, day, hour, min, sec })
}

// ---------------------------------------------------------------- 杂项

/// 从 Reader 精确读满缓冲区；返回实际读取字节数（0 表示 EOF）。
pub fn read_full<R: Read>(r: &mut R, buf: &mut [u8]) -> io::Result<usize> {
    let mut filled = 0;
    while filled < buf.len() {
        match r.read(&mut buf[filled..]) {
            Ok(0) => break,
            Ok(n) => filled += n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        }
    }
    Ok(filled)
}

/// 逐字节读满（用于解压流，必须精确）。
pub fn read_exact<R: Read>(r: &mut R, buf: &mut [u8]) -> io::Result<()> {
    let n = read_full(r, buf)?;
    if n != buf.len() {
        return Err(eof());
    }
    Ok(())
}

pub fn path_to_display(p: &std::path::Path) -> String {
    p.to_string_lossy().into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn size_roundtrip() {
        assert_eq!(format_size(0), "0 B");
        assert_eq!(format_size(1023), "1023 B");
        assert_eq!(format_size(1024), "1.0 KiB");
        assert_eq!(format_size(1536), "1.5 KiB");
        assert_eq!(parse_size("10K").unwrap(), 10240);
        assert_eq!(parse_size("1.5M").unwrap(), 1_572_864);
        assert_eq!(parse_size("2GiB").unwrap(), 2_147_483_648);
        assert_eq!(parse_size("512").unwrap(), 512);
    }

    #[test]
    fn duration_parse() {
        assert_eq!(parse_duration("7d").unwrap(), 604_800);
        assert_eq!(parse_duration("24h").unwrap(), 86_400);
        assert_eq!(parse_duration("90").unwrap(), 90);
    }

    #[test]
    fn hex_roundtrip() {
        let data = [0u8, 1, 15, 16, 255];
        assert_eq!(hex(&data), "00010f10ff");
        assert_eq!(unhex("00010f10ff").unwrap(), data);
        assert_eq!(hex_upper(&data), "00010F10FF");
        assert!(unhex("abc").is_err());
    }

    #[test]
    fn civil_math() {
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(days_from_civil(1970, 1, 1), 0);
        assert_eq!(days_from_civil(2024, 2, 29), 19_782);
        assert_eq!(civil_from_days(19_782), (2024, 2, 29));
        let c = civil_from_secs_utc(1_700_000_000);
        assert_eq!(c.year, 2023);
        assert_eq!(c.month, 11);
        assert_eq!(c.day, 14);
    }

    #[test]
    fn datetime_parse() {
        assert_eq!(parse_datetime("2024-01-31").unwrap().month, 1);
        assert_eq!(parse_datetime("20240131").unwrap().day, 31);
        let c = parse_datetime("2024-01-31 09:08:07").unwrap();
        assert_eq!((c.hour, c.min, c.sec), (9, 8, 7));
        assert_eq!(c.format("YYYY/MM/DD HH:mm:ss"), "2024/01/31 09:08:07");
    }

    #[test]
    fn varint_7z_roundtrip() {
        for v in [0u64, 1, 127, 128, 255, 16_384, 1 << 40, u64::MAX] {
            let mut buf = Vec::new();
            write_varint7z(&mut buf, v);
            let mut r = ByteReader::new(&buf);
            assert_eq!(read_varint7z(&mut r).unwrap(), v, "value {v}");
        }
    }

    #[test]
    fn byte_reader_bounds() {
        let data = [1u8, 2, 3, 4];
        let mut r = ByteReader::new(&data);
        assert_eq!(r.u16le().unwrap(), 0x0201);
        assert_eq!(r.u16le().unwrap(), 0x0403);
        assert!(r.u8().is_err());
    }
}
