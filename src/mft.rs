//! NTFS MFT 直读（复刻 Everything 逆向报告 §3.2 原理，零第三方依赖）
//!
//! 路径：打开卷设备 `\\.\X:`（需管理员）→ `FSCTL_GET_NTFS_VOLUME_DATA` 取
//! BytesPerFileRecordSegment / MftStartLcn / MftValidDataLength → 从
//! `MftStartLcn × BytesPerCluster` 起顺序读 $MFT → 逐条解析文件记录：
//! USA fixup（扇区尾序列号回填）→ `$FILE_NAME`（名字/父 FRN/修改时间）+
//! `$DATA`（真实大小，驻留/非驻留两种形态）。
//!
//! 权限现实：卷设备 GENERIC_READ 需要管理员令牌；非提权打开失败时返回 None，
//! 由 instant.rs 回退文件夹索引模式（报告 §3.5）。任何失败都是静默降级。

/// MFT 记录解析出的原始条目（rec = File Reference Number 低 48 位）
#[derive(Debug, Clone)]
pub struct RawEntry {
    pub rec: u64,
    pub parent: u64,
    pub name: String,
    pub dir: bool,
    /// 逻辑大小（$DATA real size / 驻留 valueLen）
    pub size: u64,
    /// unix 秒（$FILE_NAME 修改时间）
    pub mtime: i64,
}

mod win {
    pub use windows_sys::Win32::Foundation::{
        CloseHandle, GENERIC_READ, GetLastError, HANDLE, INVALID_HANDLE_VALUE,
    };
    pub use windows_sys::Win32::Storage::FileSystem::{
        CreateFileW, GetVolumeInformationW, ReadFile, SetFilePointerEx, FILE_BEGIN,
        FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
    };
    pub use windows_sys::Win32::System::IO::DeviceIoControl;
    pub use windows_sys::Win32::System::Ioctl::{FSCTL_GET_NTFS_VOLUME_DATA, NTFS_VOLUME_DATA_BUFFER};
}

fn wide0(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// 卷序列号（GetVolumeInformationW，32 位；落盘索引的新鲜度校验用）
pub fn volume_serial(root: &str) -> Option<u32> {
    let wide = wide0(root);
    let mut serial = 0u32;
    let ok = unsafe {
        win::GetVolumeInformationW(
            wide.as_ptr(),
            std::ptr::null_mut(),
            0,
            &mut serial,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            0,
        )
    };
    if ok != 0 {
        Some(serial)
    } else {
        None
    }
}

/// 文件时间（100ns 自 1601-01-01）→ unix 秒
fn filetime_to_unix(ft: i64) -> i64 {
    const EPOCH_DIFF: i64 = 11_644_473_600 * 10_000_000;
    (ft - EPOCH_DIFF) / 10_000_000
}

/// 读取卷设备的 $MFT。root 形如 "C:\"。任何失败返回 None（调用方回退枚举）。
/// 卷上记录读不出来（坏道/加密）时返回已解析的部分。
pub fn enum_mft(root: &str) -> Option<Vec<RawEntry>> {
    if root.len() < 2 || root.as_bytes()[1] != b':' {
        return None;
    }
    let vol = format!(r"\\.\{}", &root[..2]);
    let vol_w = wide0(&vol);
    let t0 = std::time::Instant::now();
    let h = unsafe {
        win::CreateFileW(
            vol_w.as_ptr(),
            win::GENERIC_READ,
            win::FILE_SHARE_READ | win::FILE_SHARE_WRITE,
            std::ptr::null(),
            win::OPEN_EXISTING,
            0,
            std::ptr::null_mut(),
        )
    };
    if h == win::INVALID_HANDLE_VALUE {
        crate::flog!("[MFT] {root} 卷设备打开失败（需管理员），回退文件夹索引");
        return None;
    }
    let mut vdb: win::NTFS_VOLUME_DATA_BUFFER = unsafe { std::mem::zeroed() };
    let mut ret = 0u32;
    let ok = unsafe {
        win::DeviceIoControl(
            h,
            win::FSCTL_GET_NTFS_VOLUME_DATA,
            std::ptr::null(),
            0,
            &mut vdb as *mut _ as *mut core::ffi::c_void,
            std::mem::size_of::<win::NTFS_VOLUME_DATA_BUFFER>() as u32,
            &mut ret,
            std::ptr::null_mut(),
        )
    };
    if ok == 0 {
        crate::flog!("[MFT] {root} FSCTL_GET_NTFS_VOLUME_DATA 失败（非 NTFS 卷？）");
        unsafe { win::CloseHandle(h) };
        return None;
    }
    let bprs = vdb.BytesPerFileRecordSegment as u64;
    let sect = vdb.BytesPerSector as u64;
    if bprs == 0 || bprs > 4096 || !bprs.is_power_of_two() || sect < 512 || !sect.is_power_of_two() {
        crate::flog!("[MFT] {root} 非法记录尺寸 bprs={bprs} sect={sect}");
        unsafe { win::CloseHandle(h) };
        return None;
    }
    let valid = vdb.MftValidDataLength as u64;
    let nrec = (valid / bprs).min(64_000_000);
    let start = (vdb.MftStartLcn as u64) * vdb.BytesPerCluster as u64;
    // $MFT 在大卷上几乎必然碎片化：先读第 0 号记录（必在 $MFT 起始的首个
    // 连续段内），从它自己的 $DATA runlist 取完整碎片映射，再按碎片读。
    const CHUNK: u64 = 16 * 1024 * 1024;
    let chunk_recs = CHUNK / bprs;
    let mut buf = vec![0u8; (chunk_recs * bprs) as usize];
    let mut tmp = vec![0u8; (chunk_recs * bprs + sect) as usize];
    let mut rec0 = vec![0u8; bprs as usize];
    if let Err(code) = read_aligned(h, start, &mut rec0, &mut tmp, sect) {
        crate::flog!("[MFT] {root} 记录 0 读取失败（Win32 错误码 {code}）→ 回退文件夹索引");
        unsafe { win::CloseHandle(h) };
        return None;
    }
    let cbytes = vdb.BytesPerCluster as u64;
    let extents = match extract_mft_extents(&mut rec0, cbytes) {
        Some(e) => e,
        None => {
            crate::flog!("[MFT] {root} $MFT 碎片映射解析失败 → 回退文件夹索引");
            unsafe { win::CloseHandle(h) };
            return None;
        }
    };
    let total_bytes: u64 = extents.iter().map(|(_, l)| l).sum();
    if total_bytes < nrec * bprs {
        crate::flog!(
            "[MFT] {root} 碎片映射总长 {total_bytes} < $MFT 有效长度 {} → 回退文件夹索引",
            nrec * bprs
        );
        unsafe { win::CloseHandle(h) };
        return None;
    }
    crate::flog!(
        "[MFT] {root} 开始直读：bprs={bprs} sect={sect} 记录数≈{nrec} 碎片={} start={start} serial={:x}",
        extents.len(),
        vdb.VolumeSerialNumber as u64
    );
    let mut out: Vec<RawEntry> = Vec::with_capacity((nrec.min(20_000_000)) as usize);
    let mut rec_no = 0u64;
    while rec_no < nrec {
        let count = chunk_recs.min(nrec - rec_no);
        let want = (count * bprs) as usize;
        let voff = rec_no * bprs;
        if let Err(code) = read_extents(h, &extents, voff, &mut buf[..want], &mut tmp, sect) {
            crate::flog!(
                "[MFT] {root} 碎片读取中断于记录 {rec_no}/{nrec}（虚拟偏移 {voff}，Win32 错误码 {code}）→ 放弃部分数据，回退文件夹索引"
            );
            unsafe { win::CloseHandle(h) };
            // 铁律：部分数据绝不能当完整索引返回——孤儿级联剔除后条目大面积
            // 缩水，且卷根非空绕过自愈检查，坏快照会覆盖好快照（2026-11-16 事故）
            return None;
        }
        for i in 0..count as usize {
            let b = &mut buf[i * bprs as usize..(i + 1) * bprs as usize];
            if let Some(e) = parse_record(b, rec_no + i as u64) {
                out.push(e);
            }
        }
        rec_no += count;
    }
    unsafe { win::CloseHandle(h) };
    crate::flog!("[MFT] {root} 直读完成：{} 条目，耗时 {:?}", out.len(), t0.elapsed());
    if out.is_empty() { None } else { Some(out) }
}

/// 卷设备对齐读（offset 与长度都必须落在扇区边界；内部用 tmp 缓冲做对齐搬运）。
/// 失败时返回 Win32 错误码供诊断日志使用。
fn read_aligned(h: win::HANDLE, off: u64, out: &mut [u8], tmp: &mut [u8], sect: u64) -> Result<(), u32> {
    let a_off = off / sect * sect;
    let delta = (off - a_off) as usize;
    let a_len = (delta + out.len()).div_ceil(sect as usize) * sect as usize;
    if a_len > tmp.len() {
        return Err(0);
    }
    unsafe {
        if win::SetFilePointerEx(h, a_off as i64, std::ptr::null_mut(), win::FILE_BEGIN) == 0 {
            return Err(win::GetLastError());
        }
        let mut done = 0usize;
        while done < a_len {
            let mut got = 0u32;
            let ok = win::ReadFile(
                h,
                tmp[done..].as_mut_ptr(),
                (a_len - done) as u32,
                &mut got,
                std::ptr::null_mut(),
            );
            if ok == 0 || got == 0 {
                return Err(win::GetLastError());
            }
            done += got as usize;
        }
    }
    out.copy_from_slice(&tmp[delta..delta + out.len()]);
    Ok(())
}

/// 从 MFT 记录 0 提取未命名 $DATA（$MFT 本体）的非驻留 runlist → 碎片映射。
/// 返回 [(卷内字节偏移, 字节长度)]（VCN 顺序）。
/// $MFT 大卷上几乎必然碎片化——线性读超过首个连续段后全是无关簇数据，
/// 解析存活率骤降到 ~10%（2026-11-16 事故根因），必须按碎片映射读。
/// 驻留 / 缺失 / 解码失败 / 稀疏运行 → None（调用方回退文件夹枚举）。
fn extract_mft_extents(rec: &mut [u8], cbytes: u64) -> Option<Vec<(u64, u64)>> {
    if !apply_fixup(rec) || rec.len() < 0x40 || &rec[..4] != b"FILE" {
        return None;
    }
    let rd16 = |o: usize| u16::from_le_bytes([rec[o], rec[o + 1]]);
    let rd32 = |o: usize| u32::from_le_bytes([rec[o], rec[o + 1], rec[o + 2], rec[o + 3]]);
    let rd64 = |o: usize| u64::from_le_bytes(rec[o..o + 8].try_into().unwrap());
    let attrs_off = rd16(0x14) as usize;
    let bound = (rd32(0x18) as usize).min(rec.len());
    let mut off = attrs_off;
    while off + 0x40 <= bound {
        let at = rd32(off);
        if at == 0xFFFF_FFFF {
            break;
        }
        let alen = rd32(off + 4) as usize;
        if alen < 24 || off + alen > bound {
            break;
        }
        let nonres = rec[off + 8];
        let aname_len = rec[off + 9];
        if at == 0x80 && aname_len == 0 && nonres != 0 {
            if rd64(off + 0x10) != 0 {
                return None; // 起始 VCN 非零 = $DATA 分段，$MFT 不会出现
            }
            let mut p = off + rd16(off + 0x20) as usize;
            let mut lcn: i64 = 0;
            let mut out: Vec<(u64, u64)> = Vec::new();
            loop {
                if p >= bound {
                    return None;
                }
                let h = rec[p];
                if h == 0 {
                    break; // runlist 结束
                }
                // 头字节：低半字节 = 长度（簇数）字段尺寸，高半字节 = 相对 LCN
                // 字段尺寸（UMass CS365 / hackmag 实证）；长度在前、偏移在后
                let lsz = (h & 0x0F) as usize;
                let osz = (h >> 4) as usize;
                p += 1;
                if lsz == 0 || lsz > 8 || osz > 8 || p + lsz + osz > bound {
                    return None;
                }
                let mut len = 0u64;
                for k in 0..lsz {
                    len |= (rec[p + k] as u64) << (8 * k);
                }
                p += lsz;
                if len == 0 {
                    return None;
                }
                if osz == 0 {
                    return None; // 稀疏运行：$MFT 不会出现，防御性拒绝
                }
                let mut d: u64 = 0;
                for k in 0..osz {
                    d |= (rec[p + k] as u64) << (8 * k);
                }
                p += osz;
                let bits = 8 * osz;
                lcn += ((d << (64 - bits)) as i64) >> (64 - bits); // 有符号相对偏移
                if lcn < 0 {
                    return None;
                }
                out.push((lcn as u64 * cbytes, len * cbytes));
            }
            if out.is_empty() {
                return None;
            }
            return Some(out);
        }
        off += alen;
    }
    None
}

/// 按碎片映射读 $MFT 虚拟连续字节。extents = [(卷内字节偏移, 字节长度)]（VCN 序）。
/// voff 为 $MFT 内部虚拟字节偏移；越界返回 Err(0)。
fn read_extents(
    h: win::HANDLE,
    extents: &[(u64, u64)],
    voff: u64,
    out: &mut [u8],
    tmp: &mut [u8],
    sect: u64,
) -> Result<(), u32> {
    let mut done = 0usize;
    let mut v = voff;
    let mut acc = 0u64;
    let mut idx = 0usize;
    while done < out.len() {
        while idx < extents.len() && v >= acc + extents[idx].1 {
            acc += extents[idx].1;
            idx += 1;
        }
        if idx >= extents.len() {
            return Err(0);
        }
        let (eo, el) = extents[idx];
        let within = (v - acc) as usize;
        let n = (el as usize - within).min(out.len() - done);
        if n == 0 {
            return Err(0);
        }
        read_aligned(h, eo + within as u64, &mut out[done..done + n], tmp, sect)?;
        done += n;
        v += n as u64;
    }
    Ok(())
}

/// USA（Update Sequence Array）回填：每个 512 字节扇区末 2 字节被换成序列号，/// 原值存于 usa[i]。校验序列号一致后回填；不一致 → 记录损坏返回 false。
fn apply_fixup(buf: &mut [u8]) -> bool {
    if buf.len() < 8 {
        return false;
    }
    let usa_off = u16::from_le_bytes([buf[4], buf[5]]) as usize;
    let usa_cnt = u16::from_le_bytes([buf[6], buf[7]]) as usize;
    if usa_cnt == 0 || usa_off == 0 || usa_off + usa_cnt * 2 > buf.len() {
        return false;
    }
    let us = u16::from_le_bytes([buf[usa_off], buf[usa_off + 1]]);
    for i in 1..usa_cnt {
        let end = i * 512 - 2;
        if end + 2 > buf.len() {
            return false;
        }
        let tail = u16::from_le_bytes([buf[end], buf[end + 1]]);
        if tail != us {
            return false;
        }
        buf[end] = buf[usa_off + i * 2];
        buf[end + 1] = buf[usa_off + i * 2 + 1];
    }
    true
}

fn utf16_bytes(bytes: &[u8]) -> Option<String> {
    if !bytes.len().is_multiple_of(2) {
        return None;
    }
    let v: Vec<u16> = bytes
        .as_chunks::<2>()
        .0
        .iter()
        .map(|c| u16::from_le_bytes(*c))
        .collect();
    Some(String::from_utf16_lossy(&v))
}

/// $FILE_NAME 命名空间优先级：Win32(1) > Win32&DOS(3) > POSIX(0) > DOS 别名(2)
fn ns_rank(ns: u8) -> u8 {
    match ns {
        1 => 0,
        3 => 1,
        0 => 2,
        2 => 3,
        _ => 255,
    }
}

/// 解析一条 1024 字节 MFT 记录。返回 None = 元文件/未使用/扩展记录/损坏。
fn parse_record(buf: &mut [u8], rec_no: u64) -> Option<RawEntry> {
    if buf.len() < 0x30 || &buf[..4] != b"FILE" {
        return None;
    }
    if !apply_fixup(buf) {
        return None;
    }
    let rd16 = |o: usize| u16::from_le_bytes([buf[o], buf[o + 1]]);
    let rd32 = |o: usize| u32::from_le_bytes([buf[o], buf[o + 1], buf[o + 2], buf[o + 3]]);
    let rd64 = |o: usize| {
        u64::from_le_bytes(buf[o..o + 8].try_into().unwrap())
    };
    let flags = rd16(0x16);
    if flags & 0x01 == 0 {
        return None; // 未使用
    }
    // 元文件（记录 0..24，含 5=卷根：parent 引用 5 表示卷根，但记录 5 本身不
    // 作为条目入索引）与扩展记录（属性外溢）跳过
    if rec_no < 24 {
        return None;
    }
    if rd64(0x20) & 0x0000_FFFF_FFFF_FFFF != 0 {
        return None;
    }
    let dir = flags & 0x02 != 0;
    let attrs_off = rd16(0x14) as usize;
    let bound = (rd32(0x18) as usize).min(buf.len());
    let mut best: Option<(u8, String)> = None;
    let mut parent = 0u64;
    let mut mtime = 0i64;
    let mut size = 0u64;
    let mut off = attrs_off;
    while off + 24 <= bound {
        let at = rd32(off);
        if at == 0xFFFF_FFFF {
            break;
        }
        let alen = rd32(off + 4) as usize;
        if alen < 24 || off + alen > bound {
            break;
        }
        let nonres = buf[off + 8];
        let aname_len = buf[off + 9];
        if at == 0x30 && nonres == 0 {
            // $FILE_NAME（驻留）。值布局：parent@0x00 + 4 时间戳(0x08..0x28) +
            // 分配/真实大小(0x28..0x38) + flags@0x38 + EAs@0x3C → 名字长度@0x40、
            // 命名空间@0x41、名字@0x42（固定头 0x42 字节）
            let vlen = rd32(off + 16) as usize;
            let voff = off + rd16(off + 20) as usize;
            if vlen >= 0x42 && voff + vlen <= bound {
                let prn = rd64(voff);
                // 0x10 = 最后修改时间（0x18 是 MFT 变更时间，语义不符）
                let ft = rd64(voff + 0x10) as i64;
                let nlen = buf[voff + 0x40] as usize;
                let ns = buf[voff + 0x41];
                if nlen > 0 && (voff + 0x42 + nlen * 2) <= voff + vlen
                    && let Some(name) = utf16_bytes(&buf[voff + 0x42..voff + 0x42 + nlen * 2])
                {
                    let r = ns_rank(ns);
                    if r != 255 {
                        let better = match &best {
                            None => true,
                            Some((br, _)) => r < *br,
                        };
                        if better {
                            best = Some((r, name));
                            parent = prn & 0x0000_FFFF_FFFF_FFFF;
                            mtime = filetime_to_unix(ft);
                        }
                    }
                }
            }
        } else if at == 0x80 && aname_len == 0 {
            // 未命名 $DATA：真实大小。非驻留头布局（0x24 为 4 字节填充）：
            // 0x10 起始VCN / 0x18 结束VCN / 0x20 runlist偏移 / 0x22 压缩单元 /
            // 0x28 分配大小 / 0x30 真实大小 / 0x38 已初始化大小。
            // 真实大小取 0x30（0x38 是 initialized，稀疏/未写满文件会偏小）
            size = if nonres != 0 {
                rd64(off + 0x30)
            } else {
                rd32(off + 16) as u64
            };
        }
        off += alen;
    }
    let (_, name) = best?;
    // 名字含路径分隔符 = POSIX 别名无法经 Win32 访问，丢弃
    if name.is_empty() || name.contains(['\\', '/']) {
        return None;
    }
    Some(RawEntry {
        rec: rec_no,
        parent,
        name,
        dir,
        size: if dir { 0 } else { size },
        mtime,
    })
}

// ============================================================
// 单元测试（合成记录构造，覆盖 fixup / 属性解析 / 各类跳过）
// ============================================================

#[cfg(test)]
mod tests {
    use super::*;

    /// 构造一条带正确 fixup 的 1024 字节 MFT 记录
    /// attrs：调用方写入属性区后调用本函数封装头与 fixup
    fn build_record(rec_no: u64, flags: u16, base: u64, attrs: &[u8]) -> Vec<u8> {
        let mut b = vec![0u8; 1024];
        b[..4].copy_from_slice(b"FILE");
        // USA @0x30，3 项（头 + 2 扇区）
        b[4..6].copy_from_slice(&0x30u16.to_le_bytes());
        b[6..8].copy_from_slice(&3u16.to_le_bytes());
        b[0x10..0x12].copy_from_slice(&1u16.to_le_bytes()); // seq
        b[0x14..0x16].copy_from_slice(&0x38u16.to_le_bytes()); // attrs_off
        b[0x16..0x18].copy_from_slice(&flags.to_le_bytes());
        let used = (0x38 + attrs.len()).min(1024) as u32;
        b[0x18..0x1c].copy_from_slice(&used.to_le_bytes());
        b[0x1c..0x20].copy_from_slice(&1024u32.to_le_bytes());
        b[0x20..0x28].copy_from_slice(&base.to_le_bytes());
        b[0x38..0x38 + attrs.len()].copy_from_slice(attrs);
        // fixup：扇区尾存序列号，原值存 USA
        let orig1 = [b[510], b[511]];
        let orig2 = [b[1022], b[1023]];
        b[510..512].copy_from_slice(&1u16.to_le_bytes());
        b[1022..1024].copy_from_slice(&1u16.to_le_bytes());
        b[0x30..0x32].copy_from_slice(&1u16.to_le_bytes()); // USA[0] = 序列号
        b[0x32..0x34].copy_from_slice(&orig1);
        b[0x34..0x36].copy_from_slice(&orig2);
        let _ = rec_no;
        b
    }

    /// 构造驻留 $FILE_NAME 属性
    fn attr_file_name(parent: u64, mtime: i64, ns: u8, name: &str) -> Vec<u8> {
        let name_u16: Vec<u8> = name
            .encode_utf16()
            .flat_map(|u| u.to_le_bytes())
            .collect();
        // 真实 $FILE_NAME 布局：固定头 0x42 字节（parent/4×时间戳/2×大小/flags/EAs/
        // 名字长度@0x40/命名空间@0x41），名字自 0x42 起
        let mut v = vec![0u8; 0x42 + name_u16.len()];
        v[0..8].copy_from_slice(&parent.to_le_bytes());
        v[0x10..0x18].copy_from_slice(&((mtime + 11_644_473_600) * 10_000_000).to_le_bytes());
        v[0x40] = name.encode_utf16().count() as u8;
        v[0x41] = ns;
        v[0x42..].copy_from_slice(&name_u16);
        // 属性头（驻留，无名）
        let mut a = vec![0u8; 24];
        a[0..4].copy_from_slice(&0x30u32.to_le_bytes());
        let alen = (24 + v.len()) as u32;
        a[4..8].copy_from_slice(&alen.to_le_bytes());
        a[8] = 0; // 非驻留=0
        a[9] = 0; // 属性名长度=0
        a[16..20].copy_from_slice(&(v.len() as u32).to_le_bytes());
        a[20..22].copy_from_slice(&24u16.to_le_bytes()); // value 偏移
        let mut out = a;
        out.extend_from_slice(&v);
        out
    }

    /// 构造非驻留 $DATA 属性（只填真实大小；0x24 填充后 0x28 分配 / 0x30 真实 / 0x38 已初始化）
    fn attr_data_nonresident(size: u64) -> Vec<u8> {
        let mut a = vec![0u8; 0x48];
        a[0..4].copy_from_slice(&0x80u32.to_le_bytes());
        a[4..8].copy_from_slice(&0x48u32.to_le_bytes());
        a[8] = 1; // 非驻留
        a[9] = 0; // 未命名流
        a[0x28..0x30].copy_from_slice(&size.to_le_bytes()); // 分配大小
        a[0x30..0x38].copy_from_slice(&size.to_le_bytes()); // 真实大小
        a[0x38..0x40].copy_from_slice(&size.to_le_bytes()); // 已初始化大小
        a
    }

    #[test]
    fn test_parse_record_file() {
        let mt = 1_700_000_000i64;
        let mut attrs = attr_file_name(12_345, mt, 3, "报告.txt");
        attrs.extend(attr_data_nonresident(4096));
        attrs.extend_from_slice(&0xFFFF_FFFFu32.to_le_bytes());
        let mut b = build_record(500, 0x0001, 0, &attrs);
        let e = parse_record(&mut b, 500).unwrap();
        assert_eq!(e.rec, 500);
        assert_eq!(e.parent, 12_345);
        assert_eq!(e.name, "报告.txt");
        assert!(!e.dir);
        assert_eq!(e.size, 4096);
        assert_eq!(e.mtime, mt);
    }

    /// 构造带 runlist 的非驻留 $DATA 属性（runs 为原始映射对字节流）
    fn attr_data_runs(runs: &[u8], real: u64) -> Vec<u8> {
        let mut a = vec![0u8; 0x40];
        a[0..4].copy_from_slice(&0x80u32.to_le_bytes());
        a[8] = 1; // 非驻留
        a[9] = 0; // 未命名流
        a[0x10..0x18].copy_from_slice(&0u64.to_le_bytes()); // 起始 VCN = 0
        a[0x20..0x22].copy_from_slice(&0x40u16.to_le_bytes()); // runlist 偏移
        a[0x28..0x30].copy_from_slice(&real.to_le_bytes());
        a[0x30..0x38].copy_from_slice(&real.to_le_bytes());
        a.extend_from_slice(runs);
        let alen = a.len() as u32;
        a[4..8].copy_from_slice(&alen.to_le_bytes());
        a
    }

    #[test]
    fn test_extract_extents_single_run() {
        // lsz=1, osz=3：长度 100 簇，相对 LCN 786432（3GB/4K 簇）
        let runs = [0x31, 0x64, 0x00, 0x00, 0x0C, 0x00];
        let mut attrs = attr_data_runs(&runs, 100 * 4096);
        attrs.extend_from_slice(&0xFFFF_FFFFu32.to_le_bytes());
        let mut b = build_record(0, 0x0001, 0, &attrs);
        let ext = extract_mft_extents(&mut b, 4096).unwrap();
        assert_eq!(ext, vec![(786432 * 4096, 100 * 4096)]);
    }

    #[test]
    fn test_extract_extents_fragment_negative_delta() {
        // run1: lsz=2,osz=2 → len 10 簇，LCN 1000；run2: len 5 簇，delta=-15 → LCN 985
        let runs = [0x22, 0x0A, 0x00, 0xE8, 0x03, 0x22, 0x05, 0x00, 0xF1, 0xFF, 0x00];
        let mut attrs = attr_data_runs(&runs, 15 * 4096);
        attrs.extend_from_slice(&0xFFFF_FFFFu32.to_le_bytes());
        let mut b = build_record(0, 0x0001, 0, &attrs);
        let ext = extract_mft_extents(&mut b, 4096).unwrap();
        assert_eq!(ext, vec![(1000 * 4096, 10 * 4096), (985 * 4096, 5 * 4096)]);
    }

    #[test]
    fn test_extract_extents_malformed() {
        // lsz=0（高半字节为 0）→ 非法
        let runs = [0x01, 0xE8, 0x03, 0x00];
        let mut attrs = attr_data_runs(&runs, 4096);
        attrs.extend_from_slice(&0xFFFF_FFFFu32.to_le_bytes());
        let mut b = build_record(0, 0x0001, 0, &attrs);
        assert!(extract_mft_extents(&mut b, 4096).is_none());
    }

    #[test]
    fn test_extract_extents_rejects_sparse_run() {
        // osz=0（稀疏运行）→ $MFT 不会出现，防御性拒绝
        let runs = [0x10, 0x64, 0x00];
        let mut attrs = attr_data_runs(&runs, 100 * 4096);
        attrs.extend_from_slice(&0xFFFF_FFFFu32.to_le_bytes());
        let mut b = build_record(0, 0x0001, 0, &attrs);
        assert!(extract_mft_extents(&mut b, 4096).is_none());
    }

    #[test]
    fn test_parse_record_dir() {
        let attrs = attr_file_name(5, 1_600_000_000, 1, "alpha");
        let mut b = build_record(600, 0x0003, 0, &attrs); // in-use + directory
        let e = parse_record(&mut b, 600).unwrap();
        assert!(e.dir);
        assert_eq!(e.size, 0);
        assert_eq!(e.parent, 5);
    }

    #[test]
    fn test_namespace_preference() {
        // ns=2（DOS 别名）与 ns=1（Win32）并存 → 取 Win32
        let mut attrs = attr_file_name(10, 0, 2, "REPORT~1.TXT");
        attrs.extend(attr_file_name(10, 0, 1, "report+.txt"));
        attrs.extend_from_slice(&0xFFFF_FFFFu32.to_le_bytes());
        let mut b = build_record(700, 1, 0, &attrs);
        assert_eq!(parse_record(&mut b, 700).unwrap().name, "report+.txt");
        // 只有 POSIX 与 DOS → 取 POSIX
        let mut attrs = attr_file_name(10, 0, 0, "posix名");
        attrs.extend(attr_file_name(10, 0, 2, "POSIX~1"));
        attrs.extend_from_slice(&0xFFFF_FFFFu32.to_le_bytes());
        let mut b = build_record(701, 1, 0, &attrs);
        assert_eq!(parse_record(&mut b, 701).unwrap().name, "posix名");
    }

    #[test]
    fn test_parse_record_skips() {
        let attrs = attr_file_name(5, 0, 3, "x.txt");
        // 元文件记录（<24 且非 5）
        let mut b = build_record(9, 1, 0, &attrs);
        assert!(parse_record(&mut b, 9).is_none());
        // 扩展记录（base != 0）
        let mut b = build_record(800, 1, 0x1234, &attrs);
        assert!(parse_record(&mut b, 800).is_none());
        // 未使用
        let mut b = build_record(801, 0, 0, &attrs);
        assert!(parse_record(&mut b, 801).is_none());
        // 记录 5（卷根）不作为条目（parent 引用 5 由构建层映射为根）
        let mut b = build_record(5, 3, 0, &attrs);
        assert!(parse_record(&mut b, 5).is_none());
        // 名字含 '/'（POSIX 不可达）
        let mut b = build_record(802, 1, 0, &attr_file_name(5, 0, 0, "a/b"));
        assert!(parse_record(&mut b, 802).is_none());
    }

    #[test]
    fn test_fixup_bad_us() {
        let attrs = attr_file_name(5, 0, 3, "x.txt");
        let mut b = build_record(900, 1, 0, &attrs);
        // 破坏扇区尾序列号 → fixup 校验失败
        b[510] ^= 0xFF;
        assert!(parse_record(&mut b, 900).is_none());
        // 未回填的原始记录：扇区尾是序列号，USA 校验通过且回填后解析正常
        let mut b2 = build_record(901, 1, 0, &attrs);
        let e = parse_record(&mut b2, 901).unwrap();
        assert_eq!(e.name, "x.txt");
    }

    #[test]
    fn test_filetime_to_unix() {
        assert_eq!(filetime_to_unix(11_644_473_600 * 10_000_000), 0);
        assert_eq!(filetime_to_unix(11_644_473_600 * 10_000_000 + 10_000_000), 1);
    }
}
