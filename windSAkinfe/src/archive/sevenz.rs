//! 7z 容器读写（7z 格式 0.4）自研实现。
//!
//! 写：单 Folder + 单个 LZMA2 coder（solid 归档）。
//! 读：支持 LZMA2 / LZMA1 / Copy 单 coder，多 substream 切分；加密头明确报错。

use std::io::{self, Write};

use crate::algo::hash::crc32;
use crate::codec::lzma::{lzma2_decompress, LzmaDecoder, LzmaProps};
use crate::codec::lzma_enc::lzma2_compress;
use crate::util::{read_varint7z, write_varint7z, ByteReader};

const SIGNATURE: [u8; 6] = [0x37, 0x7A, 0xBC, 0xAF, 0x27, 0x1C];

const K_END: u8 = 0x00;
const K_HEADER: u8 = 0x01;
const K_ARCHIVE_PROPS: u8 = 0x02;
const K_ADDITIONAL_STREAMS: u8 = 0x03;
const K_MAIN_STREAMS: u8 = 0x04;
const K_FILES_INFO: u8 = 0x05;
const K_PACK_INFO: u8 = 0x06;
const K_UNPACK_INFO: u8 = 0x07;
const K_SUBSTREAMS: u8 = 0x08;
const K_SIZE: u8 = 0x09;
const K_CRC: u8 = 0x0A;
const K_FOLDER: u8 = 0x0B;
const K_CODERS_UNPACK_SIZE: u8 = 0x0C;
const K_NUM_UNPACK_STREAM: u8 = 0x0D;
const K_EMPTY_STREAM: u8 = 0x0E;
const K_EMPTY_FILE: u8 = 0x0F;
const K_ANTI: u8 = 0x10;
const K_NAME: u8 = 0x11;
const K_MTIME: u8 = 0x14;
const K_WIN_ATTRS: u8 = 0x15;
const K_ENCODED_HEADER: u8 = 0x17;

const ID_LZMA2: [u8; 1] = [0x21];
const ID_LZMA1: [u8; 3] = [0x03, 0x01, 0x01];
const ID_COPY: [u8; 1] = [0x00];

pub fn bad(msg: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg.into())
}

// ---------------------------------------------------------------- 写

pub struct SevenZInput {
    pub name: String,
    pub data: Vec<u8>,
    pub is_dir: bool,
    pub mtime_filetime: Option<u64>,
}

fn write_bits(v: &[bool]) -> Vec<u8> {
    let mut out = vec![0u8; v.len().div_ceil(8)];
    for (i, &b) in v.iter().enumerate() {
        if b {
            out[i / 8] |= 1 << (7 - (i % 8));
        }
    }
    out
}

/// 写一个 solid 7z 归档。
pub fn write_7z<W: Write>(w: &mut W, entries: &[SevenZInput], level: u8) -> io::Result<()> {
    // 1) 拼接非空文件数据
    let mut solid: Vec<u8> = Vec::new();
    let mut sizes: Vec<u64> = Vec::new();
    let mut crcs: Vec<u32> = Vec::new();
    for e in entries {
        // 只有非空文件才产生 substream
        if e.is_dir || e.data.is_empty() {
            continue;
        }
        solid.extend_from_slice(&e.data);
        sizes.push(e.data.len() as u64);
        crcs.push(crc32(&e.data));
    }
    let solid_crc = crc32(&solid);
    let packed = if solid.is_empty() {
        // 空归档也写一个空 LZMA2 流（仅结束符）
        vec![0u8]
    } else {
        lzma2_compress(&solid, level, None)
    };

    let mut out: Vec<u8> = Vec::new();
    out.extend_from_slice(&SIGNATURE);
    out.push(0x00);
    out.push(0x04);
    out.extend_from_slice(&[0u8; 24]); // StartHeader 占位（CRC4 + Offset8 + Size8 + HdrCRC4）

    let pack_pos = 0u64;
    out.extend_from_slice(&packed);

    let header = build_header(
        entries,
        &sizes,
        &crcs,
        solid_crc,
        packed.len() as u64,
        solid.len() as u64,
    );
    let header_offset = out.len() as u64 - 32;
    let header_crc = crc32(&header);
    out.extend_from_slice(&header);

    // 回填签名头
    let mut sh = Vec::with_capacity(20);
    {
        let mut shcrc = Vec::new();
        shcrc.extend_from_slice(&header_offset.to_le_bytes());
        shcrc.extend_from_slice(&(header.len() as u64).to_le_bytes());
        shcrc.extend_from_slice(&header_crc.to_le_bytes());
        let c = crc32(&shcrc);
        sh.extend_from_slice(&c.to_le_bytes());
        sh.extend_from_slice(&shcrc);
    }
    out[8..32].copy_from_slice(&sh);
    debug_assert_eq!(pack_pos, 0);
    w.write_all(&out)?;
    w.flush()
}

fn build_header(
    entries: &[SevenZInput],
    sizes: &[u64],
    crcs: &[u32],
    solid_crc: u32,
    pack_size: u64,
    unpack_size: u64,
) -> Vec<u8> {
    let mut h = Vec::new();
    h.push(K_HEADER);
    // ---- MainStreamsInfo
    h.push(K_MAIN_STREAMS);
    {
        // PackInfo
        h.push(K_PACK_INFO);
        write_varint7z(&mut h, 0); // packPos
        write_varint7z(&mut h, 1); // numPackStreams
        h.push(K_SIZE);
        write_varint7z(&mut h, pack_size);
        h.push(K_END);

        // UnPackInfo
        h.push(K_UNPACK_INFO);
        {
            h.push(K_FOLDER);
            write_varint7z(&mut h, 1); // numFolders
            h.push(0); // external
            // folder
            write_varint7z(&mut h, 1); // numCoders
            // flags: idSize=1 | hasAttrs(0x20)
            h.push(ID_LZMA2.len() as u8 | 0x20);
            h.extend_from_slice(&ID_LZMA2);
            // LZMA2 属性：1 字节字典大小编码
            write_varint7z(&mut h, 1);
            h.push(lzma2_dict_prop(1 << 20));

            h.push(K_CODERS_UNPACK_SIZE);
            write_varint7z(&mut h, unpack_size);

            h.push(K_CRC);
            h.push(1); // all defined
            h.extend_from_slice(&solid_crc.to_le_bytes());
        }
        h.push(K_END);

        // SubStreamsInfo
        if !sizes.is_empty() {
            h.push(K_SUBSTREAMS);
            h.push(K_NUM_UNPACK_STREAM);
            write_varint7z(&mut h, sizes.len() as u64);
            // 尺寸：最后一个可省略
            if sizes.len() > 1 {
                h.push(K_SIZE);
                for s in &sizes[..sizes.len() - 1] {
                    write_varint7z(&mut h, *s);
                }
            }
            h.push(K_CRC);
            h.push(1);
            for c in crcs {
                h.extend_from_slice(&c.to_le_bytes());
            }
            h.push(K_END);
        }
        h.push(K_END); // 结束 MainStreamsInfo
    }

    // ---- FilesInfo
    h.push(K_FILES_INFO);
    write_varint7z(&mut h, entries.len() as u64);
    {
        // 空流（目录或空文件）
        let empty: Vec<bool> = entries.iter().map(|e| e.is_dir || e.data.is_empty()).collect();
        if empty.iter().any(|&b| b) {
            let bits = write_bits(&empty);
            h.push(K_EMPTY_STREAM);
            write_varint7z(&mut h, bits.len() as u64);
            h.extend_from_slice(&bits);
            // 空文件中哪些是真文件（非目录）
            let empty_files: Vec<bool> = entries
                .iter()
                .filter(|e| e.is_dir || e.data.is_empty())
                .map(|e| !e.is_dir)
                .collect();
            if empty_files.iter().any(|&b| b) {
                let bits = write_bits(&empty_files);
                h.push(K_EMPTY_FILE);
                write_varint7z(&mut h, bits.len() as u64);
                h.extend_from_slice(&bits);
            }
        }

        // 名称（UTF-16LE，'\0' 结尾）
        // 分隔符必须写 '/'：7-Zip 实际产物如此，写成 '\\' 会被它当成非法字符消毒成 '_'。
        let mut names = Vec::new();
        names.push(0u8); // external = 0
        for e in entries {
            let n = e.name.replace('\\', "/");
            for u in n.encode_utf16() {
                names.extend_from_slice(&u.to_le_bytes());
            }
            names.extend_from_slice(&0u16.to_le_bytes());
        }
        h.push(K_NAME);
        write_varint7z(&mut h, names.len() as u64);
        h.extend_from_slice(&names);

        // 修改时间
        if entries.iter().any(|e| e.mtime_filetime.is_some()) {
            h.push(K_MTIME);
            let mut body = Vec::new();
            body.push(1); // all defined
            body.push(0); // external = 0
            let fallback = 0x01D0_0000_0000_0000u64; // 2020-01-01 附近
            for e in entries {
                body.extend_from_slice(&e.mtime_filetime.unwrap_or(fallback).to_le_bytes());
            }
            write_varint7z(&mut h, body.len() as u64);
            h.extend_from_slice(&body);
        }

        // Windows 属性
        {
            h.push(K_WIN_ATTRS);
            let mut body = Vec::new();
            body.push(1); // all defined
            body.push(0); // external
            for e in entries {
                let attr: u32 = if e.is_dir { 0x10 } else { 0x20 };
                body.extend_from_slice(&attr.to_le_bytes());
            }
            write_varint7z(&mut h, body.len() as u64);
            h.extend_from_slice(&body);
        }
    }
    h.push(K_END); // 结束 FilesInfo
    h.push(K_END); // 结束 Header
    h
}

/// LZMA2 字典大小 → 属性字节（dict = (2|(p&1)) << (p/2+11)）。
pub fn lzma2_dict_prop(dict_size: u64) -> u8 {
    for i in 0u8..40 {
        let size = (2u64 | (i as u64 & 1)) << (i as u64 / 2 + 11);
        if dict_size <= size {
            return i;
        }
    }
    40
}

// ---------------------------------------------------------------- 读

#[derive(Clone, Debug)]
pub struct SzEntry {
    pub name: String,
    pub size: u64,
    pub crc: Option<u32>,
    pub is_dir: bool,
    pub mtime: Option<u64>,
    /// 索引到解压后数据（同 folder 内偏移）
    stream_index: Option<usize>,
}

#[derive(Clone, Debug)]
struct Coder {
    id: Vec<u8>,
    #[allow(dead_code)]
    num_in: usize,
    num_out: usize,
    props: Vec<u8>,
}

#[derive(Clone, Debug)]
struct Folder {
    coders: Vec<Coder>,
    unpack_sizes: Vec<u64>,
    crc: Option<u32>,
    num_unpack_streams: usize,
    sub_sizes: Vec<u64>,
    sub_crcs: Vec<Option<u32>>,
}

pub struct SevenZArchive {
    data: Vec<u8>,
    pack_pos: u64,
    pack_sizes: Vec<u64>,
    folders: Vec<Folder>,
    pub entries: Vec<SzEntry>,
}

impl SevenZArchive {
    pub fn open(data: Vec<u8>) -> io::Result<Self> {
        if data.len() < 32 || data[..6] != SIGNATURE {
            return Err(bad("不是 7z 文件（签名不符）"));
        }
        let next_off = u64::from_le_bytes(data[12..20].try_into().unwrap());
        let next_size = u64::from_le_bytes(data[20..28].try_into().unwrap());
        let start = 32 + next_off as usize;
        if start + next_size as usize > data.len() {
            return Err(bad("7z 头偏移越界"));
        }
        let mut header = data[start..start + next_size as usize].to_vec();
        // 可能被压缩（kEncodedHeader）
        if header.first() == Some(&K_ENCODED_HEADER) {
            header = decode_encoded_header(&data, &header)?;
        }
        let mut ar = Self {
            data,
            pack_pos: 0,
            pack_sizes: Vec::new(),
            folders: Vec::new(),
            entries: Vec::new(),
        };
        ar.parse_header(&header)?;
        Ok(ar)
    }

    fn parse_header(&mut self, header: &[u8]) -> io::Result<()> {
        let mut r = ByteReader::new(header);
        let id = r.u8()?;
        if id != K_HEADER {
            return Err(bad(format!("7z 头起始属性非法：{id:#x}")));
        }
        loop {
            let id = r.u8()?;
            match id {
                K_END => break,
                K_ARCHIVE_PROPS => skip_props(&mut r)?,
                K_ADDITIONAL_STREAMS => {
                    // 解析并丢弃（仅支持单 folder 归档时跳过）
                    self.parse_streams_info(&mut r)?;
                }
                K_MAIN_STREAMS => {
                    self.parse_streams_info(&mut r)?;
                }
                K_FILES_INFO => {
                    self.parse_files_info(&mut r)?;
                }
                other => return Err(bad(format!("7z 头中出现未知属性 {other:#x}"))),
            }
        }
        Ok(())
    }

    fn parse_streams_info(&mut self, r: &mut ByteReader<'_>) -> io::Result<()> {
        loop {
            let id = r.u8()?;
            match id {
                K_END => break,
                K_PACK_INFO => {
                    self.pack_pos = read_varint7z(r)?;
                    let n = read_varint7z(r)? as usize;
                    loop {
                        let id2 = r.u8()?;
                        match id2 {
                            K_END => break,
                            K_SIZE => {
                                self.pack_sizes.clear();
                                for _ in 0..n {
                                    self.pack_sizes.push(read_varint7z(r)?);
                                }
                            }
                            K_CRC => {
                                let all = r.u8()?;
                                if all == 1 {
                                    r.take(4 * n)?;
                                } else {
                                    return Err(bad("7z PackInfo 变体 CRC 不支持"));
                                }
                            }
                            _ => return Err(bad("7z PackInfo 未知属性")),
                        }
                    }
                }
                K_UNPACK_INFO => {
                    loop {
                        let id2 = r.u8()?;
                        match id2 {
                            K_END => break,
                            K_FOLDER => {
                                let nf = read_varint7z(r)? as usize;
                                let external = r.u8()?;
                                if external != 0 {
                                    return Err(bad("7z 外部 folder 定义不支持"));
                                }
                                for _ in 0..nf {
                                    let f = parse_folder(r)?;
                                    self.folders.push(f);
                                }
                            }
                            K_CODERS_UNPACK_SIZE => {
                                for f in self.folders.iter_mut() {
                                    let mut sizes = Vec::new();
                                    for _ in 0..f.unpack_out_count() {
                                        sizes.push(read_varint7z(r)?);
                                    }
                                    f.unpack_sizes = sizes;
                                }
                            }
                            K_CRC => {
                                let all = r.u8()?;
                                if all == 1 {
                                    for f in self.folders.iter_mut() {
                                        f.crc = Some(r.u32le()?);
                                    }
                                } else {
                                    // 按位掩码
                                    let n = self.folders.len();
                                    let bits = r.take(n.div_ceil(8))?.to_vec();
                                    for (i, f) in self.folders.iter_mut().enumerate() {
                                        if bits[i / 8] & (1 << (7 - i % 8)) != 0 {
                                            f.crc = Some(r.u32le()?);
                                        }
                                    }
                                }
                            }
                            other => return Err(bad(format!("7z UnPackInfo 未知属性 {other:#x}"))),
                        }
                    }
                }
                K_SUBSTREAMS => {
                    self.parse_substreams(r)?;
                }
                other => return Err(bad(format!("7z StreamsInfo 未知属性 {other:#x}"))),
            }
        }
        Ok(())
    }

    fn parse_substreams(&mut self, r: &mut ByteReader<'_>) -> io::Result<()> {
        for f in self.folders.iter_mut() {
            f.num_unpack_streams = 1;
        }
        loop {
            let id = r.u8()?;
            match id {
                K_END => break,
                K_NUM_UNPACK_STREAM => {
                    for f in self.folders.iter_mut() {
                        f.num_unpack_streams = read_varint7z(r)? as usize;
                    }
                }
                K_SIZE => {
                    for f in self.folders.iter_mut() {
                        let total = f.total_unpack_size();
                        let mut acc = 0u64;
                        let mut v = Vec::new();
                        for _ in 0..f.num_unpack_streams.saturating_sub(1) {
                            let s = read_varint7z(r)?;
                            acc += s;
                            v.push(s);
                        }
                        v.push(total.saturating_sub(acc));
                        f.sub_sizes = v;
                    }
                }
                K_CRC => {
                    let all = r.u8()?;
                    for f in self.folders.iter_mut() {
                        let n = f.num_unpack_streams;
                        let mut crcs = vec![None; n];
                        if all == 1 {
                            for c in crcs.iter_mut() {
                                *c = Some(r.u32le()?);
                            }
                        } else {
                            let bits = r.take(n.div_ceil(8))?.to_vec();
                            for (i, c) in crcs.iter_mut().enumerate() {
                                if bits[i / 8] & (1 << (7 - i % 8)) != 0 {
                                    *c = Some(r.u32le()?);
                                }
                            }
                        }
                        f.sub_crcs = crcs;
                    }
                }
                _ => return Err(bad("7z SubStreamsInfo 未知属性")),
            }
        }
        // 补齐默认尺寸
        for f in self.folders.iter_mut() {
            if f.sub_sizes.is_empty() {
                if f.num_unpack_streams == 1 {
                    f.sub_sizes = vec![f.total_unpack_size()];
                } else {
                    return Err(bad("7z substream 尺寸缺失"));
                }
            }
            if f.sub_crcs.len() < f.sub_sizes.len() {
                // 单 substream 时可用 folder CRC
                if f.sub_sizes.len() == 1 {
                    f.sub_crcs = vec![f.crc];
                } else {
                    f.sub_crcs.resize(f.sub_sizes.len(), None);
                }
            }
        }
        Ok(())
    }

    fn parse_files_info(&mut self, r: &mut ByteReader<'_>) -> io::Result<()> {
        let num = read_varint7z(r)? as usize;
        let mut empty_stream = vec![false; num];
        let mut empty_file = vec![false; num];
        let mut names: Vec<String> = Vec::new();
        let mut mtimes: Vec<Option<u64>> = vec![None; num];
        let mut attrs: Vec<u32> = vec![0; num];

        loop {
            let id = r.u8()?;
            if id == K_END {
                break;
            }
            let size = read_varint7z(r)? as usize;
            let body = r.take(size)?;
            match id {
                K_EMPTY_STREAM => {
                    let flags = read_bits(body, num);
                    for (i, f) in flags.iter().enumerate() {
                        if i < num {
                            empty_stream[i] = *f;
                        }
                    }
                }
                K_EMPTY_FILE | K_ANTI => {
                    // 位向量长度 = 空流数量（不是文件总数）
                    let m = empty_stream.iter().filter(|&&b| b).count();
                    let flags = read_bits(body, m);
                    if id == K_EMPTY_FILE {
                        let mut k = 0usize;
                        for i in 0..num {
                            if empty_stream[i] {
                                empty_file[i] = flags.get(k).copied().unwrap_or(false);
                                k += 1;
                            }
                        }
                    }
                }
                K_NAME => {
                    let mut nr = ByteReader::new(body);
                    let external = nr.u8()?;
                    if external != 0 {
                        return Err(bad("7z 外部文件名不支持"));
                    }
                    let rest = nr.rest();
                    let mut cur: Vec<u16> = Vec::new();
                    let mut i = 0;
                    while i + 1 < rest.len() {
                        let u = u16::from_le_bytes([rest[i], rest[i + 1]]);
                        i += 2;
                        if u == 0 {
                            names.push(String::from_utf16_lossy(&cur).replace('\\', "/"));
                            cur.clear();
                        } else {
                            cur.push(u);
                        }
                    }
                    if !cur.is_empty() {
                        names.push(String::from_utf16_lossy(&cur).replace('\\', "/"));
                    }
                }
                K_MTIME => {
                    let mut mr = ByteReader::new(body);
                    let all = mr.u8()?;
                    let mut defined = vec![true; num];
                    if all == 0 {
                        let bits = mr.take(num.div_ceil(8))?.to_vec();
                        for (i, d) in defined.iter_mut().enumerate() {
                            *d = bits[i / 8] & (1 << (7 - i % 8)) != 0;
                        }
                    }
                    let external = mr.u8()?;
                    if external != 0 {
                        return Err(bad("7z 外部时间戳不支持"));
                    }
                    for (i, d) in defined.iter().enumerate() {
                        if *d {
                            let v = mr.u64le()?;
                            if i < num {
                                mtimes[i] = Some(v);
                            }
                        }
                    }
                }
                K_WIN_ATTRS => {
                    let mut ar = ByteReader::new(body);
                    let all = ar.u8()?;
                    let mut defined = vec![true; num];
                    if all == 0 {
                        let bits = ar.take(num.div_ceil(8))?.to_vec();
                        for (i, d) in defined.iter_mut().enumerate() {
                            *d = bits[i / 8] & (1 << (7 - i % 8)) != 0;
                        }
                    }
                    let external = ar.u8()?;
                    if external != 0 {
                        return Err(bad("7z 外部属性不支持"));
                    }
                    for (i, d) in defined.iter().enumerate() {
                        if *d {
                            let v = ar.u32le()?;
                            if i < num {
                                attrs[i] = v;
                            }
                        }
                    }
                }
                _ => { /* 跳过未知属性 */ }
            }
        }

        if names.len() < num {
            for i in names.len()..num {
                names.push(format!("file{i}"));
            }
        }

        // 分配 substream
        let mut entries = Vec::with_capacity(num);
        let mut fi = 0usize; // 非空流索引
        for i in 0..num {
            let is_dir = empty_stream[i] && !empty_file[i];
            let (size, crc, stream_index) = if empty_stream[i] {
                (0, None, None)
            } else {
                // 定位 folder / substream
                let mut acc = 0usize;
                let mut found = None;
                for (fidx, f) in self.folders.iter().enumerate() {
                    for (sidx, sz) in f.sub_sizes.iter().enumerate() {
                        if acc == fi {
                            found = Some((fidx, sidx, *sz, f.sub_crcs.get(sidx).copied().flatten()));
                        }
                        acc += 1;
                    }
                }
                match found {
                    Some((_, _, sz, c)) => (sz, c, Some(fi)),
                    None => (0, None, None),
                }
            };
            entries.push(SzEntry {
                name: names[i].clone(),
                size,
                crc,
                is_dir,
                mtime: mtimes[i],
                stream_index,
            });
            if !empty_stream[i] {
                fi += 1;
            }
        }
        self.entries = entries;
        Ok(())
    }

    /// 解压全部条目数据（按 entry 顺序；目录返回空）。
    pub fn extract_all(&self) -> io::Result<Vec<Vec<u8>>> {
        let mut out: Vec<Vec<u8>> = self.entries.iter().map(|_| Vec::new()).collect();
        for (fidx, f) in self.folders.iter().enumerate() {
            let decoded = self.decode_folder(fidx)?;
            // 切分 substream
            let mut off = 0usize;
            let mut sidx = 0usize;
            for (i, e) in self.entries.iter().enumerate() {
                if e.is_dir || e.stream_index.is_none() {
                    continue;
                }
                if sidx < f.sub_sizes.len() {
                    let sz = f.sub_sizes[sidx] as usize;
                    if off + sz <= decoded.len() {
                        out[i] = decoded[off..off + sz].to_vec();
                    }
                    off += sz;
                    sidx += 1;
                }
            }
            let _ = f;
        }
        Ok(out)
    }

    fn decode_folder(&self, idx: usize) -> io::Result<Vec<u8>> {
        let f = self.folders.get(idx).ok_or_else(|| bad("folder 索引越界"))?;
        if f.coders.len() != 1 {
            return Err(bad(format!(
                "7z 多 coder 链（{} 个，含 BCJ 等过滤器）暂不支持",
                f.coders.len()
            )));
        }
        let c = &f.coders[0];
        // 收集打包数据
        let mut packed = Vec::new();
        let mut pos = self.pack_pos;
        for (i, sz) in self.pack_sizes.iter().enumerate() {
            if i == 0 {
                pos = self.pack_pos;
            }
            let start = 32 + pos as usize;
            if start + *sz as usize > self.data.len() {
                return Err(bad("7z 打包数据越界"));
            }
            packed.extend_from_slice(&self.data[start..start + *sz as usize]);
            pos += *sz;
        }
        if c.id == ID_LZMA2 {
            let out = lzma2_decompress(&packed, Some(f.total_unpack_size() as usize))?;
            Ok(out)
        } else if c.id == ID_LZMA1 {
            if c.props.len() < 5 {
                return Err(bad("7z LZMA1 属性不完整"));
            }
            let props = LzmaProps::from_byte(c.props[0])?;
            let total = f.total_unpack_size() as usize;
            let mut out = Vec::with_capacity(total);
            let mut dec = LzmaDecoder::new(props);
            dec.decode_chunk(&packed, &mut out, total, 0)?;
            Ok(out)
        } else if c.id == ID_COPY {
            Ok(packed)
        } else {
            Err(bad(format!(
                "7z 不支持的 coder：{}",
                c.id.iter().map(|b| format!("{b:02x}")).collect::<String>()
            )))
        }
    }
}

impl Folder {
    fn unpack_out_count(&self) -> usize {
        self.coders.iter().map(|c| c.num_out).sum()
    }
    fn total_unpack_size(&self) -> u64 {
        // 最后一个 coder 的输出即最终大小
        self.unpack_sizes.last().copied().unwrap_or(0)
    }
}

fn parse_folder(r: &mut ByteReader<'_>) -> io::Result<Folder> {
    let num_coders = read_varint7z(r)? as usize;
    if num_coders == 0 || num_coders > 32 {
        return Err(bad("7z coder 数量非法"));
    }
    let mut coders = Vec::with_capacity(num_coders);
    let mut total_in = 0usize;
    let mut total_out = 0usize;
    for _ in 0..num_coders {
        let flag = r.u8()?;
        let id_size = (flag & 0x0F) as usize;
        let is_complex = flag & 0x10 != 0;
        let has_attrs = flag & 0x20 != 0;
        let id = r.take(id_size)?.to_vec();
        let (num_in, num_out) = if is_complex {
            (read_varint7z(r)? as usize, read_varint7z(r)? as usize)
        } else {
            (1, 1)
        };
        let props = if has_attrs {
            let n = read_varint7z(r)? as usize;
            r.take(n)?.to_vec()
        } else {
            Vec::new()
        };
        total_in += num_in;
        total_out += num_out;
        coders.push(Coder { id, num_in, num_out, props });
    }
    if total_out == 1 {
        // 单输出：无需 bind pair
    } else {
        let pairs = total_out - 1;
        for _ in 0..pairs {
            read_varint7z(r)?;
            read_varint7z(r)?;
        }
    }
    let num_packed = total_in.saturating_sub(total_out.saturating_sub(1));
    for _ in 0..num_packed.saturating_sub(1) {
        read_varint7z(r)?;
    }
    Ok(Folder {
        coders,
        unpack_sizes: Vec::new(),
        crc: None,
        num_unpack_streams: 1,
        sub_sizes: Vec::new(),
        sub_crcs: Vec::new(),
    })
}

fn skip_props(r: &mut ByteReader<'_>) -> io::Result<()> {
    loop {
        let id = r.u8()?;
        if id == K_END {
            return Ok(());
        }
        let size = read_varint7z(r)? as usize;
        r.take(size)?;
    }
}

fn read_bits(data: &[u8], n: usize) -> Vec<bool> {
    let mut v = Vec::with_capacity(n);
    for i in 0..n {
        let byte = data.get(i / 8).copied().unwrap_or(0);
        v.push((byte >> (7 - (i % 8))) & 1 != 0);
    }
    v
}

/// 解压 `kEncodedHeader`：先用 StreamsInfo 描述的流解出真正的 Header。
fn decode_encoded_header(data: &[u8], header: &[u8]) -> io::Result<Vec<u8>> {
    // 复用归档解析器读 streams info
    let mut r = ByteReader::new(&header[1..]);
    let mut ar = SevenZArchive {
        data: data.to_vec(),
        pack_pos: 0,
        pack_sizes: Vec::new(),
        folders: Vec::new(),
        entries: Vec::new(),
    };
    ar.parse_streams_info(&mut r)?;
    if ar.folders.is_empty() {
        return Err(bad("7z 编码头缺少 folder"));
    }
    // 补齐默认 substream 尺寸
    for f in ar.folders.iter_mut() {
        if f.sub_sizes.is_empty() {
            f.sub_sizes = vec![f.total_unpack_size()];
        }
    }
    ar.decode_folder(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bits_roundtrip() {
        let v = vec![true, false, true, true, false, false, false, true, true];
        let b = write_bits(&v);
        assert_eq!(read_bits(&b, v.len()), v);
    }

    #[test]
    fn write_and_reopen() {
        let entries = vec![
            SevenZInput { name: "a.txt".into(), data: b"hello 7z writer".to_vec(), is_dir: false, mtime_filetime: None },
            SevenZInput { name: "dir".into(), data: Vec::new(), is_dir: true, mtime_filetime: None },
            SevenZInput { name: "empty.txt".into(), data: Vec::new(), is_dir: false, mtime_filetime: None },
            SevenZInput { name: "b.bin".into(), data: (0..5000u32).map(|i| (i % 251) as u8).collect(), is_dir: false, mtime_filetime: None },
        ];
        let mut buf = Vec::new();
        write_7z(&mut buf, &entries, 6).unwrap();
        let ar = SevenZArchive::open(buf).unwrap();
        assert_eq!(ar.entries.len(), 4);
        assert!(ar.entries[1].is_dir);
        assert_eq!(ar.entries[2].size, 0);
        let all = ar.extract_all().unwrap();
        assert_eq!(all[0], entries[0].data);
        assert_eq!(all[3], entries[3].data);
    }
}
