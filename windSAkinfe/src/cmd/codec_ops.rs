//! 编解码类命令：b64 / hash / replace。

use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use crate::algo::b64::{B64Config, Decoder, Encoder};
use crate::algo::hash::{self, Algo};
use crate::algo::regexlite::Regex;
use crate::cli::{B64Args, HashArgs, ReplaceArgs};
use crate::plan::{Fail, FailKind, Summary};
use crate::term::{self, JVal, Out};
use crate::util::{format_size, path_to_display};

use super::Global;

/// 一个文件的「算法 → 摘要」结果集
type HashRow = (PathBuf, Vec<(Algo, Vec<u8>)>);

// ---------------------------------------------------------------- b64

pub fn b64(a: &B64Args, _g: &Global, out: &mut Out) -> Summary {
    let mut sum = Summary::new();
    let cfg = B64Config { url_safe: a.url_safe, pad: !a.no_pad };

    let mut input: Box<dyn Read> = if a.input.as_os_str() == "-" {
        Box::new(std::io::stdin())
    } else {
        match fs::File::open(&a.input) {
            Ok(f) => Box::new(f),
            Err(e) => {
                sum.fail(Fail::io(a.input.clone(), e));
                return sum;
            }
        }
    };
    let mut sink: Box<dyn Write> = match &a.output {
        Some(p) => match fs::File::create(p) {
            Ok(f) => Box::new(std::io::BufWriter::new(f)),
            Err(e) => {
                sum.fail(Fail::io(p.clone(), e));
                return sum;
            }
        },
        None => Box::new(std::io::BufWriter::new(std::io::stdout())),
    };

    const CHUNK: usize = 48 * 1024;
    let mut buf = vec![0u8; CHUNK];
    let mut total_in = 0u64;
    let mut total_out = 0u64;
    // --wrap 的列计数
    let mut col = 0usize;
    let mut col_buf: Vec<u8> = Vec::new();
    let wrap = a.wrap;

    let emit = |bytes: &[u8], sink: &mut Box<dyn Write>, col: &mut usize, col_buf: &mut Vec<u8>, total: &mut u64| -> std::io::Result<()> {
        if wrap == 0 {
            *total += bytes.len() as u64;
            return sink.write_all(bytes);
        }
        for &b in bytes {
            col_buf.push(b);
            *col += 1;
            if *col >= wrap {
                col_buf.push(b'\n');
                *col = 0;
            }
        }
        if col_buf.len() >= 64 * 1024 {
            *total += col_buf.len() as u64;
            sink.write_all(col_buf)?;
            col_buf.clear();
        }
        Ok(())
    };

    if a.decode {
        let mut dec = Decoder::new(cfg);
        let mut text = Vec::new();
        loop {
            let n = match input.read(&mut buf) {
                Ok(n) => n,
                Err(e) => {
                    sum.fail(Fail::io(a.input.clone(), e));
                    return sum;
                }
            };
            if n == 0 {
                break;
            }
            total_in += n as u64;
            text.extend_from_slice(&buf[..n]);
            // 攒到 1MB 再解，兼顾内存与流式
            if text.len() >= 1 << 20 {
                let cleaned: Vec<u8> = text.clone();
                match dec.push(&cleaned) {
                    Ok(o) => {
                        total_out += o.len() as u64;
                        if let Err(e) = sink.write_all(&o) {
                            sum.fail(Fail::io(a.input.clone(), e));
                            return sum;
                        }
                    }
                    Err(e) => {
                        sum.fail(Fail { path: Some(a.input.clone()), kind: FailKind::Resource, msg: e });
                        return sum;
                    }
                }
                text.clear();
            }
        }
        match dec.push(&text) {
            Ok(o) => {
                total_out += o.len() as u64;
                if let Err(e) = sink.write_all(&o) {
                    sum.fail(Fail::io(a.input.clone(), e));
                    return sum;
                }
            }
            Err(e) => {
                sum.fail(Fail { path: Some(a.input.clone()), kind: FailKind::Resource, msg: e });
                return sum;
            }
        }
        match dec.finish() {
            Ok(o) => {
                total_out += o.len() as u64;
                let _ = sink.write_all(&o);
            }
            Err(e) => {
                sum.fail(Fail { path: Some(a.input.clone()), kind: FailKind::Resource, msg: e });
                return sum;
            }
        }
    } else {
        let mut enc = Encoder::new(cfg);
        loop {
            let n = match input.read(&mut buf) {
                Ok(n) => n,
                Err(e) => {
                    sum.fail(Fail::io(a.input.clone(), e));
                    return sum;
                }
            };
            if n == 0 {
                break;
            }
            total_in += n as u64;
            let v = enc.push(&buf[..n]);
            if let Err(e) = emit(&v, &mut sink, &mut col, &mut col_buf, &mut total_out) {
                sum.fail(Fail::io(a.input.clone(), e));
                return sum;
            }
        }
        let tail = enc.finish();
        if let Err(e) = emit(&tail, &mut sink, &mut col, &mut col_buf, &mut total_out) {
            sum.fail(Fail::io(a.input.clone(), e));
            return sum;
        }
        if wrap > 0 && col > 0 {
            col_buf.push(b'\n');
        }
        if !col_buf.is_empty() {
            total_out += col_buf.len() as u64;
            if let Err(e) = sink.write_all(&col_buf) {
                sum.fail(Fail::io(a.input.clone(), e));
                return sum;
            }
        }
        // 单行模式补一个结尾换行：与 base64 / gzip 等工具一致，
        // 否则输出到终端时提示符会紧贴正文。空输入不补。
        if wrap == 0 && total_out > 0 {
            if let Err(e) = sink.write_all(b"\n") {
                sum.fail(Fail::io(a.input.clone(), e));
                return sum;
            }
            total_out += 1;
        }
    }

    if let Err(e) = sink.flush() {
        sum.fail(Fail::io(a.input.clone(), e));
        return sum;
    }
    let _ = a.check;
    sum.ok = 1;
    if a.output.is_some() {
        out.note(term::dim(&format!(
            "{} → {}（{} → {}）",
            if a.decode { "解码" } else { "编码" },
            a.output.as_ref().map(|p| path_to_display(p)).unwrap_or_default(),
            format_size(total_in),
            format_size(total_out)
        )));
    }
    sum
}

// ---------------------------------------------------------------- hash

pub fn hash_cmd(a: &HashArgs, _g: &Global, out: &mut Out) -> Summary {
    let mut sum = Summary::new();
    let algos = match parse_algos(&a.algo) {
        Ok(v) => v,
        Err(f) => {
            sum.fail(f);
            return sum;
        }
    };

    // 校验模式
    if let Some(manifest) = &a.check {
        return check_manifest(manifest, &algos[0], out, &mut sum);
    }

    // 展开目标
    let mut files: Vec<PathBuf> = Vec::new();
    for p in &a.paths {
        if p.as_os_str() == "-" {
            files.push(p.clone());
            continue;
        }
        match fs::metadata(p) {
            Ok(m) if m.is_dir() => {
                if a.recursive {
                    if let Ok(items) = super::walk_relative(p, usize::MAX, false) {
                        for (f, _, _, is_dir) in items {
                            if !is_dir {
                                files.push(f);
                            }
                        }
                    }
                } else {
                    sum.fail(Fail::usage(format!("{} 是目录，请加 --recursive", path_to_display(p))));
                }
            }
            Ok(_) => files.push(p.clone()),
            Err(e) => sum.fail(Fail::io(p.clone(), e)),
        }
    }
    if files.is_empty() {
        if sum.is_ok() {
            sum.fail(Fail::usage("没有可计算的输入"));
        }
        return sum;
    }

    let hash_stdin = files.iter().any(|p| p.as_os_str() == "-");
    let stdin_data = if hash_stdin {
        let mut b = Vec::new();
        match std::io::stdin().read_to_end(&mut b) {
            Ok(_) => Some(b),
            Err(e) => {
                sum.fail(Fail::io(PathBuf::from("-"), e));
                return sum;
            }
        }
    } else {
        None
    };

    let mut manifest_lines: Vec<String> = Vec::new();
    let mut computed: Vec<HashRow> = Vec::new();
    for f in &files {
        let mut row = Vec::new();
        for al in &algos {
            let digest = if f.as_os_str() == "-" {
                hash::hash_bytes(*al, stdin_data.as_deref().unwrap_or(&[]))
            } else {
                match hash::hash_file(*al, f) {
                    Ok(v) => v,
                    Err(e) => {
                        sum.fail(Fail::io(f.clone(), e));
                        continue;
                    }
                }
            };
            row.push((*al, digest));
        }
        if !row.is_empty() {
            computed.push((f.clone(), row));
        }
    }

    for (f, row) in &computed {
        for (al, digest) in row {
            let text = if a.base64 {
                String::from_utf8_lossy(&crate::algo::b64::encode(
                    digest,
                    B64Config { url_safe: false, pad: true },
                ))
                .to_string()
            } else if a.upper {
                crate::util::hex_upper(digest)
            } else {
                crate::util::hex(digest)
            };
            if algos.len() == 1 {
                out.raw(&format!("{text}  {}", path_to_display(f)));
            } else {
                out.raw(&format!("{} ({}) = {}", text, al.name(), path_to_display(f)));
            }
            out.json_item(JVal::Obj(vec![
                ("status".to_string(), JVal::s("ok")),
                ("action".to_string(), JVal::s("hash")),
                ("path".to_string(), JVal::s(path_to_display(f))),
                ("algo".to_string(), JVal::s(al.name())),
                ("digest".to_string(), JVal::s(text.clone())),
            ]));
            if !a.base64 {
                manifest_lines.push(format!("{text}  {}", path_to_display(f)));
            }
        }
        sum.ok += 1;
    }

    if let Some(w) = &a.write {
        let body = manifest_lines.join("\n") + "\n";
        match fs::write(w, body) {
            Ok(()) => out.note(term::dim(&format!("清单已写到 {}", path_to_display(w)))),
            Err(e) => sum.fail(Fail::io(w.clone(), e)),
        }
    }
    sum
}

pub fn hash(a: &HashArgs, g: &Global, out: &mut Out) -> Summary {
    hash_cmd(a, g, out)
}

fn parse_algos(spec: &str) -> Result<Vec<Algo>, Fail> {
    let mut out = Vec::new();
    for part in spec.split(',') {
        let p = part.trim();
        if p.is_empty() {
            continue;
        }
        match Algo::parse(p) {
            Some(al) => {
                if !out.contains(&al) {
                    out.push(al);
                }
            }
            None => {
                let all: Vec<&str> = Algo::ALL.iter().map(|a| a.name()).collect();
                return Err(Fail::usage(format!(
                    "未知算法：{p}（支持 {}）",
                    all.join(", ")
                )));
            }
        }
    }
    if out.is_empty() {
        return Err(Fail::usage("至少指定一个算法"));
    }
    Ok(out)
}

fn check_manifest(path: &Path, algo: &Algo, out: &mut Out, sum: &mut Summary) -> Summary {
    let text = match fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) => {
            sum.fail(Fail::io(path.to_path_buf(), e));
            return std::mem::take(sum);
        }
    };
    let expect_len = algo.out_len() * 2;
    let mut bad = 0usize;
    for (lineno, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        // 两种格式：`hash  path`（sha256sum）与 `path  hash`
        let (digest, name) = match line.split_once(char::is_whitespace) {
            Some((a, b)) => {
                let a = a.trim();
                let b = b.trim().trim_start_matches('*');
                if a.len() == expect_len && a.chars().all(|c| c.is_ascii_hexdigit()) {
                    (a.to_string(), b.to_string())
                } else {
                    // 反向：最后一个空白分隔的是 hash
                    match line.rsplit_once(char::is_whitespace) {
                        Some((p, h)) => {
                            let h = h.trim();
                            if h.len() == expect_len && h.chars().all(|c| c.is_ascii_hexdigit()) {
                                (h.to_string(), p.trim().to_string())
                            } else {
                                out.raw(&format!("{} 第 {} 行格式无法识别", path_to_display(path), lineno + 1));
                                bad += 1;
                                continue;
                            }
                        }
                        None => {
                            bad += 1;
                            continue;
                        }
                    }
                }
            }
            None => {
                bad += 1;
                continue;
            }
        };
        let p = PathBuf::from(&name);
        if !p.exists() {
            out.raw(&format!("{} {}", term::yellow("MISSING"), name));
            bad += 1;
            sum.fail(Fail { path: Some(p), kind: FailKind::Corrupt, msg: "文件缺失".into() });
            continue;
        }
        match hash::hash_file(*algo, &p) {
            Ok(v) => {
                let got = crate::util::hex(&v);
                if got.eq_ignore_ascii_case(&digest) {
                    out.raw(&format!("{} {}", term::green("OK"), name));
                    sum.ok += 1;
                } else {
                    out.raw(&format!("{} {}（期望 {digest}，实际 {got}）", term::red("FAILED"), name));
                    bad += 1;
                    sum.fail(Fail { path: Some(p), kind: FailKind::Corrupt, msg: "哈希不匹配".into() });
                }
            }
            Err(e) => sum.fail(Fail::io(p, e)),
        }
    }
    if bad == 0 {
        out.note(term::dim(&format!("全部通过（{} 项）", sum.ok)));
    } else {
        out.note(term::dim(&format!("{} 项通过，{bad} 项失败", sum.ok)));
    }
    std::mem::take(sum)
}

// ---------------------------------------------------------------- replace

pub fn replace(a: &ReplaceArgs, g: &Global, out: &mut Out) -> Summary {
    let mut sum = Summary::new();
    let re = if a.literal {
        None
    } else {
        match Regex::with_options(&a.pattern, a.ignore_case, true) {
            Ok(r) => Some(r),
            Err(e) => {
                sum.fail(Fail::usage(e.to_string()));
                return sum;
            }
        }
    };
    let ci = cfg!(windows);
    let mut exc = Vec::new();
    for p in &a.exclude {
        if let Ok(x) = crate::algo::pattern::Pattern::new(p, ci) {
            exc.push(x);
        }
    }
    let mut inc = Vec::new();
    for p in &a.include {
        if let Ok(x) = crate::algo::pattern::Pattern::new(p, ci) {
            inc.push(x);
        }
    }
    // 目录遍历时用：排除优先，其次 include（为空即全通过），文件名与相对路径都算
    let keep = |rel: &str, base: &str| -> bool {
        if exc.iter().any(|x| x.is_match(rel) || x.is_match(base)) {
            return false;
        }
        inc.is_empty() || inc.iter().any(|x| x.is_match(rel) || x.is_match(base))
    };

    // 收集目标文件（先展开通配：`replace 'src/*.rs' ...`）
    let targets = match super::expand_targets(&a.paths, cfg!(windows)) {
        Ok(t) => t,
        Err(f) => {
            sum.fail(f);
            return sum;
        }
    };
    let mut files: Vec<PathBuf> = Vec::new();
    for p in &targets {
        match fs::metadata(p) {
            Ok(m) if m.is_dir() => {
                if let Ok(items) = super::walk_relative(p, usize::MAX, false) {
                    for (f, rel, _, is_dir) in items {
                        let base = f.file_name().map(|s| s.to_string_lossy().to_string()).unwrap_or_default();
                        if is_dir || !keep(&rel, &base) {
                            continue;
                        }
                        files.push(f);
                    }
                }
            }
            Ok(_) => files.push(p.clone()),
            Err(e) => sum.fail(Fail::io(p.clone(), e)),
        }
    }
    if files.is_empty() {
        out.raw("没有目标文件");
        return sum;
    }

    let mut total_hits = 0usize;
    let mut changed = 0usize;
    for f in &files {
        let raw = match fs::read(f) {
            Ok(b) => b,
            Err(e) => {
                sum.fail(Fail::io(f.clone(), e));
                continue;
            }
        };
        if !a.binary && raw[..raw.len().min(8192)].contains(&0) {
            sum.skipped += 1;
            out.warn(format!("{} 是二进制文件，已跳过", path_to_display(f)));
            continue;
        }
        // 编码：UTF-8 优先；失败则按 Latin-1 字节直通（保证 round-trip 无损）
        let (text, was_utf8) = match String::from_utf8(raw.clone()) {
            Ok(s) => (s, true),
            Err(_) => (raw.iter().map(|&b| b as char).collect::<String>(), false),
        };

        let (new_text, hits) = match &re {
            Some(r) => {
                let n = r.count_matches(&text);
                (r.replace_all(&text, &a.replacement), n)
            }
            None => crate::algo::regexlite::literal_replace_all(&text, &a.pattern, &a.replacement),
        };
        total_hits += hits;

        if hits == 0 {
            continue;
        }
        out.raw(&format!(
            "{}  {} 处匹配",
            path_to_display(f),
            hits
        ));
        out.json_item(JVal::Obj(vec![
            ("status".to_string(), JVal::s("ok")),
            ("action".to_string(), JVal::s("replace")),
            ("path".to_string(), JVal::s(path_to_display(f))),
            ("matches".to_string(), JVal::n(hits as i64)),
            ("written".to_string(), JVal::Bool(a.write && !g.dry_run)),
        ]));
        sum.ok += 1;

        if !a.write || g.dry_run {
            continue;
        }
        let bytes: Vec<u8> = if was_utf8 {
            new_text.into_bytes()
        } else {
            // 回到字节：Latin-1 映射位 <= 0xFF，直接截取低字节
            new_text.chars().map(|c| c as u32 as u8).collect()
        };
        if let Some(ext) = &a.backup {
            let bak = PathBuf::from(format!("{}.{}", f.to_string_lossy(), ext.trim_start_matches('.')));
            if let Err(e) = fs::copy(f, &bak) {
                sum.fail(Fail::io(bak, e));
                continue;
            }
        }
        match fs::write(f, &bytes) {
            Ok(()) => changed += 1,
            Err(e) => sum.fail(Fail::io(f.clone(), e)),
        }
    }

    if !a.write {
        out.note(term::dim(&format!(
            "共 {} 处匹配，{} 个文件受影响（加 --write 才会落盘）",
            total_hits, sum.ok
        )));
    } else if !g.dry_run {
        out.note(term::dim(&format!("共替换 {} 处，写回 {} 个文件", total_hits, changed)));
    } else {
        out.note(term::dim(&format!("预演：共 {} 处匹配，未写盘", total_hits)));
    }
    sum
}
