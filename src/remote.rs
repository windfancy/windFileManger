//! 远程访问协议层：SMB / FTP / WebDAV / DLNA 只读浏览。
//!
//! 设计要点：
//! - 路径即 URL：`\\host[\share\…]`、`ftp://host[:port]/path`、
//!   `http(s)://host[:port]/path`（WebDAV）、`dlna://host[:port]/objectID`。
//!   条目的 path 字段携带完整 URL，双击目录即可继续应用内导航。
//! - 零重依赖：HTTP 用 std TcpStream 手写（含 chunked），HTTPS 用 rustls +
//!   全信任校验器（NAS 自签证书场景）；FTP/SMB 用 std + windows-sys 原生 API。
//! - 凭证：调用方从 config.remotes 按 host_key 匹配传入；SMB 走
//!   WNetAddConnection2W（等同 net use），FTP 走 USER/PASS，WebDAV 走 Basic。
//! - 第一版只读：浏览/导航/排序/预览元信息可用；写操作由 UI 层拦截。

use crate::fs::{self, RawEntry, CAT_FOLDER};
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpStream, ToSocketAddrs, UdpSocket};
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::Duration;

/// 登录凭证（user 为空 = 匿名/系统默认）
#[derive(Clone, Debug)]
pub struct Cred {
    pub user: String,
    pub pass: String,
}

pub fn is_remote(p: &str) -> bool {
    p.starts_with("\\\\")
        || ["ftp://", "http://", "https://", "dlna://"].iter().any(|s| p.starts_with(s))
}

/// URL 的身份键（用于凭证匹配）：`\\host` 或 `scheme://host:port`
pub fn host_key(url: &str) -> String {
    let u = url.trim();
    if u.starts_with("\\\\") {
        let h = u.trim_start_matches('\\').split(['\\', '/']).next().unwrap_or("");
        return format!(r"\\{}", h.to_lowercase());
    }
    match u.split_once("://") {
        Some((scheme, rest)) => {
            let authority = rest.split(['/', '?']).next().unwrap_or(rest).to_lowercase();
            format!("{}://{}", scheme.to_lowercase(), authority)
        }
        None => u.to_lowercase(),
    }
}

/// 协议分发列举
pub fn list(url: &str, cred: Option<&Cred>) -> Result<Vec<RawEntry>, String> {
    let u = url.trim();
    if u.starts_with("\\\\") {
        smb_list(u, cred)
    } else if u.starts_with("ftp://") {
        ftp_list(u, cred)
    } else if u.starts_with("http://") || u.starts_with("https://") {
        dav_list(u, cred)
    } else if u.starts_with("dlna://") {
        dlna_list(u)
    } else {
        Err(format!("不支持的地址：{u}"))
    }
}

/// 上一级 URL（本地同步计算；dlna 需网络查询走 dlna_parent）
pub fn parent_url(url: &str) -> Option<String> {
    let u = url.trim();
    if u.starts_with("\\\\") {
        // \\host\share\a\b -> …逐级剥到 \\host；\\host 无父级
        let trimmed = u.trim_end_matches(['\\', '/']);
        let parts: Vec<&str> =
            trimmed.trim_start_matches('\\').split(['\\', '/']).filter(|s| !s.is_empty()).collect();
        if parts.len() <= 1 {
            return None;
        }
        let parent_parts = &parts[..parts.len() - 1];
        let mut parent = String::from("\\\\");
        parent.push_str(&parent_parts.join("\\"));
        return Some(parent);
    }
    let (scheme, rest) = u.split_once("://")?;
    let (authority, path) = {
        let i = rest.find('/')?;
        (&rest[..i], &rest[i..])
    };
    let path = path.trim_end_matches('/');
    if path.is_empty() {
        return None; // 服务器根目录没有上一级
    }
    match path.rfind('/') {
        Some(i) if i > 0 => Some(format!("{scheme}://{authority}{}", &path[..i])),
        _ => Some(format!("{scheme}://{authority}")), // 到根
    }
}

/// 面包屑分段
pub fn crumbs_for(url: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let u = url.trim();
    if u.starts_with("\\\\") {
        let parts: Vec<&str> = u.trim_matches('\\').split('\\').filter(|s| !s.is_empty()).collect();
        if parts.is_empty() {
            return out;
        }
        let mut acc = format!(r"\\{}", parts[0]);
        out.push((parts[0].to_string(), acc.clone()));
        for p in &parts[1..] {
            acc.push('\\');
            acc.push_str(p);
            out.push((p.to_string(), acc.clone()));
        }
        return out;
    }
    let (scheme, rest) = match u.split_once("://") {
        Some(v) => v,
        None => return out,
    };
    let (authority, path) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, ""),
    };
    let root = format!("{scheme}://{authority}");
    if scheme == "dlna" {
        out.push((format!("DLNA {authority}"), root));
        // objectID 不含可读名称；当前容器名由列表页自行展示
        return out;
    }
    out.push((authority.to_string(), root.clone()));
    let mut acc = root;
    let mut skip_next_join = false;
    for seg in path.split('/') {
        if seg.is_empty() {
            continue;
        }
        let dec = percent_decode(seg);
        acc = if skip_next_join { acc.clone() } else { format!("{acc}/{}", seg) };
        skip_next_join = false;
        if acc.ends_with('/') {
            acc.pop();
            skip_next_join = true;
        }
        out.push((dec, acc.clone()));
    }
    out
}

/// 标签页标题用短名
pub fn display_label(url: &str) -> String {
    let u = url.trim().trim_end_matches(['\\', '/']);
    // dlna 的 objectID 不可读，直接用 host:port
    if let Some((scheme, rest)) = u.split_once("://")
        && scheme == "dlna" {
            let auth = rest.split(['/', '?']).next().unwrap_or(rest);
            return format!("DLNA {auth}");
        }
    if let Some(pos) = u.rfind(['\\', '/']) {
        let last = &u[pos + 1..];
        if !last.is_empty() {
            return percent_decode(last);
        }
    }
    if u.starts_with("\\\\") {
        return u.to_string();
    }
    match u.split_once("://") {
        Some((_s, rest)) => rest.split(['/', '?']).next().unwrap_or(rest).to_string(),
        None => u.to_string(),
    }
}

// ============ 通用小工具 ============

pub fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len()
            && let (Some(h), Some(l)) = (hex_val(b[i + 1]), hex_val(b[i + 2])) {
                out.push(h * 16 + l);
                i += 3;
                continue;
            }
        if b[i] == b'+' {
            out.push(b' '); // 查询串风格，尽力而为
        } else {
            out.push(b[i]);
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn hex_val(c: u8) -> Option<u8> {
    match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        b'A'..=b'F' => Some(c - b'A' + 10),
        _ => None,
    }
}

/// 路径段百分号编码（非 ASCII + 空格 + 保留字符）
pub fn percent_encode_path(s: &str) -> String {
    const SAFE: &[u8] = b"-_.~:@!$&'()*+,;=/"; // path 保留集（含分隔符）
    let mut out = String::with_capacity(s.len());
    for &b in s.as_bytes() {
        if b.is_ascii_alphanumeric() || SAFE.contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{:02X}", b));
        }
    }
    out
}

fn base64_encode(s: &str) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let b = s.as_bytes();
    let mut out = String::with_capacity(b.len().div_ceil(3) * 4);
    let chunks = b.as_chunks::<3>().0;
    for c in chunks {
        let n = ((c[0] as u32) << 16) | ((c[1] as u32) << 8) | (c[2] as u32);
        out.push(T[(n >> 18 & 63) as usize] as char);
        out.push(T[(n >> 12 & 63) as usize] as char);
        out.push(T[(n >> 6 & 63) as usize] as char);
        out.push(T[(n & 63) as usize] as char);
    }
    let rem = b.len() % 3;
    if rem > 0 {
        let last = &b[b.len() - rem..];
        let n = ((last[0] as u32) << 16) | ((last.get(1).copied().unwrap_or(0) as u32) << 8);
        out.push(T[(n >> 18 & 63) as usize] as char);
        out.push(T[(n >> 12 & 63) as usize] as char);
        out.push(if rem > 1 { T[(n >> 6 & 63) as usize] as char } else { '=' });
        out.push('=');
    }
    out
}

fn xml_unescape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(i) = rest.find('&') {
        out.push_str(&rest[..i]);
        let tail = &rest[i..];
        if let Some(semi) = tail.find(';') {
            let ent = &tail[1..semi];
            let rep = match ent {
                "lt" => Some('<'),
                "gt" => Some('>'),
                "amp" => Some('&'),
                "quot" => Some('"'),
                "apos" => Some('\''),
                _ => None,
            };
            match rep {
                Some(c) => {
                    out.push(c);
                    rest = &tail[semi + 1..];
                    continue;
                }
                None => {
                    if let Some(num) = ent.strip_prefix('#') {
                        let cp = if let Some(hex) = num.strip_prefix('x').or(num.strip_prefix('X')) {
                            u32::from_str_radix(hex, 16).ok()
                        } else {
                            num.parse::<u32>().ok()
                        };
                        if let Some(c) = cp.and_then(char::from_u32) {
                            out.push(c);
                            rest = &tail[semi + 1..];
                            continue;
                        }
                    }
                }
            }
        }
        out.push('&');
        rest = &tail[1..];
    }
    out.push_str(rest);
    out
}

/// 大小写不敏感地找 XML 子元素文本（忽略命名空间前缀），返回解码后的文本
fn xml_child(text: &str, tag: &str) -> Option<String> {
    let low = text.to_ascii_lowercase();
    let tl = tag.to_ascii_lowercase();
    let mut from = 0;
    while let Some(rel) = low[from..].find('<') {
        let i = from + rel;
        let gt = low[i..].find('>')? + i;
        // 开标签名（<prefix:tag attrs...>）
        let open = &low[i + 1..gt];
        let name = open
            .split(|c: char| !(c.is_ascii_alphanumeric() || c == ':' || c == '-' || c == '_'))
            .next()
            .unwrap_or("");
        let local = name.rsplit(':').next().unwrap_or(name);
        if local == tl {
            // 自闭合
            if open.ends_with('/') {
                return Some(String::new());
            }
            // 找第一个同名闭标签（本文档无同名嵌套）
            let mut j = gt + 1;
            while let Some(rel2) = low[j..].find("</") {
                let ci = j + rel2;
                let cgt = low[ci..].find('>')? + ci;
                let cname = &low[ci + 2..cgt];
                let clocal = cname.rsplit(':').next().unwrap_or(cname);
                if clocal == tl {
                    return Some(xml_unescape(text[gt + 1..ci].trim()));
                }
                j = cgt + 1;
            }
            return None;
        }
        from = i + 1;
    }
    None
}

/// 开标签内属性值（大小写不敏感）
fn xml_attr(text: &str, attr: &str) -> Option<String> {
    let low = text.to_ascii_lowercase();
    let al = attr.to_ascii_lowercase();
    let pat = format!("{al}=\"");
    let idx = low.find(&pat)?;
    let after = &text[idx + pat.len()..];
    let end = after.find('"')?;
    Some(after[..end].to_string())
}

/// 列举所有 `<tag …>…</tag>` 块（忽略前缀），返回块文本（从 `<` 到闭标签结束）
fn xml_blocks(text: &str, tag: &str) -> Vec<String> {
    let mut out = Vec::new();
    let low = text.to_ascii_lowercase();
    let tl = tag.to_ascii_lowercase();
    let mut from = 0;
    while let Some(rel) = low[from..].find('<') {
        let i = from + rel;
        let Some(gt_rel) = low[i..].find('>') else { break };
        let gt = i + gt_rel;
        let open = &low[i + 1..gt];
        let name = open
            .split(|c: char| !(c.is_ascii_alphanumeric() || c == ':' || c == '-' || c == '_'))
            .next()
            .unwrap_or("");
        let local = name.rsplit(':').next().unwrap_or(name);
        if local == tl && !open.ends_with('/') {
            // 找闭标签（跳过嵌套同名：DIDL 中不存在）
            let mut j = gt + 1;
            let mut done = false;
            while let Some(rel2) = low[j..].find("</") {
                let ci = j + rel2;
                let Some(cgt_rel) = low[ci..].find('>') else { break };
                let cgt = ci + cgt_rel;
                let cname = &low[ci + 2..cgt];
                let clocal = cname.rsplit(':').next().unwrap_or(cname);
                if clocal == tl {
                    out.push(text[i..cgt + 1].to_string());
                    from = cgt + 1;
                    done = true;
                    break;
                }
                j = cgt + 1;
            }
            if !done {
                break;
            }
        } else {
            from = i + 1;
        }
    }
    out
}

fn month_of(m: &str) -> Option<u32> {
    const M: [&str; 12] =
        ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];
    M.iter().position(|x| x.eq_ignore_ascii_case(m)).map(|i| i as u32 + 1)
}

fn local_ts(y: i32, mo: u32, d: u32, h: u32, mi: u32, s: u32) -> Option<i64> {
    chrono::NaiveDate::from_ymd_opt(y, mo, d)?
        .and_hms_opt(h, mi, s)?
        .and_local_timezone(chrono::Local)
        .single()
        .map(|t| t.timestamp())
}

// ============ 极简 HTTP 客户端（http/https，供 WebDAV + DLNA 复用） ============

struct Url {
    https: bool,
    host: String,
    port: u16,
    /// 已编码的请求路径（含前导 /）
    path: String,
    /// scheme://host:port
    base: String,
}

fn parse_url(u: &str) -> Result<Url, String> {
    let (scheme, rest) =
        u.split_once("://").ok_or_else(|| format!("URL 缺少协议前缀：{u}"))?;
    let https = match scheme {
        "http" => false,
        "https" => true,
        _ => return Err(format!("不支持的协议：{scheme}")),
    };
    let (authority, path) = match rest.find('/') {
        Some(i) => (&rest[..i], rest[i..].to_string()),
        None => (rest, "/".to_string()),
    };
    let (host, port) = match authority.rsplit_once(':') {
        Some((h, p)) if p.chars().all(|c| c.is_ascii_digit()) && !p.is_empty() => {
            (h.to_string(), p.parse::<u16>().map_err(|_| "端口无效")?)
        }
        _ => (authority.to_string(), if https { 443 } else { 80 }),
    };
    if host.is_empty() {
        return Err("URL 缺少主机名".into());
    }
    Ok(Url { https, port, host, path: percent_encode_path(&path), base: format!("{scheme}://{authority}") })
}

struct HttpResp {
    status: u16,
    body: Vec<u8>,
}

impl HttpResp {
    fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }
}

/// 极简 HTTP/1.1 请求：Content-Length + Connection: close，支持 chunked 响应
fn http_request(
    method: &str,
    url: &str,
    extra_headers: &[(&str, &str)],
    body: &[u8],
    cred: Option<&Cred>,
) -> Result<HttpResp, String> {
    let u = parse_url(url)?;
    let addr = (u.host.as_str(), u.port)
        .to_socket_addrs()
        .map_err(|e| format!("DNS 解析失败：{e}"))?
        .next()
        .ok_or("DNS 解析失败：无地址")?;
    let tcp = TcpStream::connect_timeout(&addr, Duration::from_secs(6))
        .map_err(|e| format!("连接失败：{e}"))?;
    tcp.set_read_timeout(Some(Duration::from_secs(20))).ok();
    tcp.set_write_timeout(Some(Duration::from_secs(10))).ok();

    let mut stream: Box<dyn ReadWrite> = if u.https {
        Box::new(tls_stream(&u.host, tcp)?)
    } else {
        Box::new(tcp)
    };

    let mut req = format!(
        "{method} {} HTTP/1.1\r\nHost: {}:{}\r\nUser-Agent: windFileManger/0.2\r\nConnection: close\r\nContent-Length: {}\r\n",
        u.path, u.host, u.port, body.len()
    );
    if let Some(c) = cred.filter(|c| !c.user.is_empty()) {
        let token = base64_encode(&format!("{}:{}", c.user, c.pass));
        req.push_str(&format!("Authorization: Basic {token}\r\n"));
    }
    for (k, v) in extra_headers {
        req.push_str(&format!("{k}: {v}\r\n"));
    }
    req.push_str("\r\n");
    stream.write_all(req.as_bytes()).map_err(|e| format!("发送请求失败：{e}"))?;
    stream.write_all(body).map_err(|e| format!("发送请求体失败：{e}"))?;
    stream.flush().ok();

    let mut reader = BufReader::new(stream);
    // 跳过 1xx 临时响应
    let mut status = 0u16;
    for _ in 0..3 {
        let mut line = String::new();
        reader.read_line(&mut line).map_err(|e| format!("读取响应失败：{e}"))?;
        if line.is_empty() {
            return Err("连接被服务器关闭".into());
        }
        let code = line
            .split_whitespace()
            .nth(1)
            .and_then(|c| c.parse::<u16>().ok())
            .ok_or_else(|| format!("响应格式异常：{line}"))?;
        status = code;
        if code >= 200 {
            break;
        }
        // 1xx：丢弃其头部
        loop {
            let mut l = String::new();
            reader.read_line(&mut l).map_err(|e| e.to_string())?;
            if l == "\r\n" || l == "\n" || l.is_empty() {
                break;
            }
        }
    }

    // 头部
    let mut content_length: Option<usize> = None;
    let mut chunked = false;
    loop {
        let mut l = String::new();
        reader.read_line(&mut l).map_err(|e| format!("读取头部失败：{e}"))?;
        let t = l.trim_end();
        if t.is_empty() {
            break;
        }
        if let Some((k, v)) = t.split_once(':') {
            let k = k.trim().to_ascii_lowercase();
            let v = v.trim();
            if k == "content-length" {
                content_length = v.parse().ok();
            } else if k == "transfer-encoding" && v.to_ascii_lowercase().contains("chunked") {
                chunked = true;
            }
        }
    }

    let body = if chunked {
        read_chunked(&mut reader)?
    } else if let Some(n) = content_length {
        let mut buf = vec![0u8; n];
        reader.read_exact(&mut buf).map_err(|e| format!("读取响应体失败：{e}"))?;
        buf
    } else {
        let mut buf = Vec::new();
        reader.read_to_end(&mut buf).map_err(|e| format!("读取响应体失败：{e}"))?;
        buf
    };
    Ok(HttpResp { status, body })
}

trait ReadWrite: Read + Write {}
impl<T: Read + Write> ReadWrite for T {}

fn read_chunked(r: &mut impl Read) -> Result<Vec<u8>, String> {
    let mut reader = BufReader::new(r);
    let mut out = Vec::new();
    loop {
        let mut line = String::new();
        reader.read_line(&mut line).map_err(|e| e.to_string())?;
        let size = usize::from_str_radix(line.trim().split(';').next().unwrap_or("0"), 16)
            .map_err(|_| format!("chunk 大小异常：{line}"))?;
        if size == 0 {
            // 尾部头 + 结束行
            loop {
                let mut l = String::new();
                let n = reader.read_line(&mut l).map_err(|e| e.to_string())?;
                if n == 0 || l.trim().is_empty() {
                    break;
                }
            }
            return Ok(out);
        }
        let mut buf = vec![0u8; size];
        reader.read_exact(&mut buf).map_err(|e| e.to_string())?;
        out.extend_from_slice(&buf);
        let mut crlf = [0u8; 2];
        let _ = reader.read_exact(&mut crlf);
    }
}

/// rustls 全信任 TLS 流（NAS 自签证书场景）
fn tls_stream(host: &str, tcp: TcpStream) -> Result<impl Read + Write, String> {
    use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
    use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
    use rustls::{ClientConfig, ClientConnection, DigitallySignedStruct, SignatureScheme, StreamOwned};

    #[derive(Debug)]
    struct NoVerify;
    impl ServerCertVerifier for NoVerify {
        fn verify_server_cert(
            &self,
            _end_entity: &CertificateDer<'_>,
            _intermediates: &[CertificateDer<'_>],
            _server_name: &ServerName<'_>,
            _ocsp_response: &[u8],
            _now: UnixTime,
        ) -> Result<ServerCertVerified, rustls::Error> {
            Ok(ServerCertVerified::assertion())
        }
        fn verify_tls12_signature(
            &self,
            _message: &[u8],
            _cert: &CertificateDer<'_>,
            _dss: &DigitallySignedStruct,
        ) -> Result<HandshakeSignatureValid, rustls::Error> {
            Ok(HandshakeSignatureValid::assertion())
        }
        fn verify_tls13_signature(
            &self,
            _message: &[u8],
            _cert: &CertificateDer<'_>,
            _dss: &DigitallySignedStruct,
        ) -> Result<HandshakeSignatureValid, rustls::Error> {
            Ok(HandshakeSignatureValid::assertion())
        }
        fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
            vec![
                SignatureScheme::RSA_PKCS1_SHA256,
                SignatureScheme::RSA_PKCS1_SHA384,
                SignatureScheme::RSA_PKCS1_SHA512,
                SignatureScheme::RSA_PSS_SHA256,
                SignatureScheme::RSA_PSS_SHA384,
                SignatureScheme::RSA_PSS_SHA512,
                SignatureScheme::ECDSA_NISTP256_SHA256,
                SignatureScheme::ECDSA_NISTP384_SHA384,
                SignatureScheme::ED25519,
            ]
        }
    }

    let cfg = ClientConfig::builder()
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(NoVerify))
        .with_no_client_auth();
    let name = ServerName::try_from(host.to_string())
        .map_err(|_| "TLS 主机名无效".to_string())?;
    let conn = ClientConnection::new(std::sync::Arc::new(cfg), name)
        .map_err(|e| format!("TLS 初始化失败：{e}"))?;
    Ok(StreamOwned::new(conn, tcp))
}

fn http_err_hint(status: u16, body: &str) -> String {
    match status {
        401 => "需要账号密码（HTTP 401 未授权）".to_string(),
        403 => "服务器拒绝访问（HTTP 403）".to_string(),
        404 => "路径不存在（HTTP 404）".to_string(),
        500..=599 => {
            let t = body.trim();
            if t.is_empty() {
                format!("服务器错误（HTTP {status}）")
            } else {
                format!("服务器错误（HTTP {status}）：{}", t.chars().take(80).collect::<String>())
            }
        }
        _ => format!("HTTP {status}"),
    }
}

// ============ WebDAV ============

fn dav_list(url: &str, cred: Option<&Cred>) -> Result<Vec<RawEntry>, String> {
    let u = parse_url(url)?;
    let body = br#"<?xml version="1.0" encoding="utf-8"?>
<D:propfind xmlns:D="DAV:"><D:prop><D:displayname/><D:resourcetype/><D:getcontentlength/><D:getlastmodified/></D:prop></D:propfind>"#;
    let resp = http_request(
        "PROPFIND",
        url,
        &[("Depth", "1"), ("Content-Type", "application/xml")],
        body,
        cred,
    )?;
    if resp.status != 207 {
        return Err(http_err_hint(resp.status, &resp.text()));
    }
    let text = resp.text();
    let want_path = u.path.trim_end_matches('/').to_string();
    let want_dec = percent_decode(&want_path);
    let mut out = Vec::new();
    for block in xml_blocks(&text, "response") {
        let href = xml_child(&block, "href").unwrap_or_default();
        if href.is_empty() {
            continue;
        }
        let is_dir = block.to_ascii_lowercase().contains("collection");
        // 组装完整 URL
        let full = if href.starts_with("http://") || href.starts_with("https://") {
            href.clone()
        } else {
            format!("{}{}", u.base, href)
        };
        // 跳过自身
        let href_dec = percent_decode(&href);
        let href_norm = href_dec.trim_end_matches('/');
        if href_norm == want_path.trim_end_matches('/') || href_norm == want_dec.trim_end_matches('/') {
            continue;
        }
        let name = match xml_child(&block, "displayname").filter(|s| !s.is_empty()) {
            Some(d) => d,
            None => {
                let dec = percent_decode(href.trim_end_matches('/'));
                dec.rsplit('/').next().unwrap_or("").to_string()
            }
        };
        if name.is_empty() {
            continue;
        }
        let size: i64 = xml_child(&block, "getcontentlength")
            .and_then(|s| s.trim().parse().ok())
            .unwrap_or(0);
        let mtime = xml_child(&block, "getlastmodified")
            .and_then(|s| chrono::DateTime::parse_from_rfc2822(s.trim()).ok())
            .map(|t| t.timestamp())
            .unwrap_or(0);
        let ext = if is_dir { String::new() } else { fs::ext_of(&name) };
        let category = if is_dir { CAT_FOLDER } else { fs::category_for_ext(&ext) };
        out.push(RawEntry {
            path: if is_dir && !full.ends_with('/') { format!("{full}/") } else { full },
            name,
            is_dir,
            size: if is_dir { 0 } else { size },
            mtime,
            ext,
            category,
            hidden: false,
        });
    }
    if out.is_empty() {
        return Err("服务器返回空目录（或路径不存在）".into());
    }
    Ok(out)
}

// ============ FTP ============

struct FtpConn {
    reader: BufReader<TcpStream>,
    ctrl: TcpStream,
}

impl FtpConn {
    fn connect(host: &str, port: u16) -> Result<Self, String> {
        let addr = (host, port)
            .to_socket_addrs()
            .map_err(|e| format!("DNS 解析失败：{e}"))?
            .next()
            .ok_or("DNS 解析失败：无地址")?;
        let s = TcpStream::connect_timeout(&addr, Duration::from_secs(6))
            .map_err(|e| format!("连接失败：{e}"))?;
        s.set_read_timeout(Some(Duration::from_secs(20))).ok();
        let mut c = Self { reader: BufReader::new(s.try_clone().map_err(|e| e.to_string())?), ctrl: s };
        let (code, _) = c.read_reply()?;
        if code != 220 {
            return Err(format!("FTP 服务器拒绝连接（{code}）"));
        }
        Ok(c)
    }

    fn read_reply(&mut self) -> Result<(u16, String), String> {
        let mut first = String::new();
        self.reader.read_line(&mut first).map_err(|e| format!("连接中断：{e}"))?;
        if first.len() < 3 {
            return Err(format!("FTP 响应异常：{first}"));
        }
        let code: u16 = first[..3].parse().map_err(|_| format!("FTP 响应异常：{first}"))?;
        let mut text = first.clone();
        // 多行回复：NNN- … 直到 NNN 空格
        if first.as_bytes().get(3) == Some(&b'-') {
            let cont = format!("{code} ");
            loop {
                let mut l = String::new();
                self.reader.read_line(&mut l).map_err(|e| e.to_string())?;
                text.push_str(&l);
                if l.starts_with(&cont) || l.is_empty() {
                    break;
                }
            }
        }
        Ok((code, text))
    }

    fn cmd(&mut self, line: &str) -> Result<(u16, String), String> {
        self.ctrl
            .write_all(format!("{line}\r\n").as_bytes())
            .map_err(|e| format!("发送命令失败：{e}"))?;
        self.read_reply()
    }

    fn login(&mut self, cred: Option<&Cred>) -> Result<(), String> {
        let (user, pass) = match cred {
            Some(c) if !c.user.is_empty() => (c.user.clone(), c.pass.clone()),
            _ => ("anonymous".into(), "anonymous@".into()),
        };
        let (code, msg) = self.cmd(&format!("USER {user}"))?;
        if code == 230 {
            return Ok(());
        }
        if code != 331 && code != 220 {
            return Err(format!("FTP 登录失败（{code}）：{}", first_line(&msg)));
        }
        let (code, msg) = self.cmd(&format!("PASS {pass}"))?;
        if code == 230 || code == 202 {
            Ok(())
        } else {
            Err(format!("FTP 登录失败（{code}）：账号或密码错误（{}）", first_line(&msg)))
        }
    }

    /// EPSV 优先，PASV 回退
    fn pasv(&mut self) -> Result<(String, u16), String> {
        let (code, msg) = self.cmd("EPSV")?;
        if code == 229 {
            // (|||port|)
            if let Some(a) = msg.find('|') {
                let rest = &msg[a..];
                let digits: String = rest.chars().filter(|c| c.is_ascii_digit()).collect();
                if let Ok(p) = digits.parse() {
                    return Ok((String::new(), p)); // host 同控制连接
                }
            }
        }
        let (code, msg) = self.cmd("PASV")?;
        if code != 227 {
            return Err(format!("被动模式协商失败（{code}）"));
        }
        let nums: Vec<u16> = msg
            .matches(|c: char| c.is_ascii_digit() || c == ',')
            .collect::<String>()
            .split(',')
            .filter_map(|s| s.trim().parse().ok())
            .collect();
        if nums.len() < 6 {
            return Err("PASV 响应格式异常".into());
        }
        let ip = format!("{}.{}.{}.{}", nums[0], nums[1], nums[2], nums[3]);
        Ok((ip, nums[4] * 256 + nums[5]))
    }

    /// 数据连接上执行列表命令，返回原始文本。
    /// 先 CWD 进目录（避免 LIST 带参数的兼容性问题），MLSD 失败后同一会话内
    /// 回退 LIST（每次重开数据连接，响应序列自洽）。
    /// 注意：EPSV 不带 IP，地址取控制连接的 IP 但必须替换成数据端口。
    fn list_at(&mut self, path: &str) -> Result<String, String> {
        let (code, msg) = self.cmd(&format!("CWD {path}"))?;
        if code >= 400 {
            return Err(format!("进入目录失败（{code}）：{}", first_line(&msg)));
        }
        let mut last_err = String::new();
        for cmd in ["MLSD", "LIST"] {
            let (ip, port) = self.pasv()?;
            let addr = if ip.is_empty() {
                let mut a = self.ctrl.peer_addr().map_err(|e| e.to_string())?;
                a.set_port(port);
                a
            } else {
                (ip.as_str(), port)
                    .to_socket_addrs()
                    .map_err(|e| e.to_string())?
                    .next()
                    .ok_or("数据连接地址无效")?
            };
            let data = match TcpStream::connect_timeout(&addr, Duration::from_secs(8)) {
                Ok(s) => s,
                Err(e) => {
                    last_err = format!("数据连接失败：{e}");
                    continue;
                }
            };
            data.set_read_timeout(Some(Duration::from_secs(25))).ok();
            let (code, msg) = self.cmd(cmd)?;
            if code >= 400 {
                last_err = format!("{cmd} 被拒绝（{code}）：{}", first_line(&msg));
                continue; // 试下一种命令
            }
            let mut buf = Vec::new();
            BufReader::new(data)
                .read_to_end(&mut buf)
                .map_err(|e| format!("读取列表失败：{e}"))?;
            let _ = self.read_reply(); // 226 传输完成
            return Ok(String::from_utf8_lossy(&buf).into_owned());
        }
        Err(last_err)
    }
}

fn first_line(s: &str) -> String {
    s.lines().next().unwrap_or("").trim().to_string()
}

fn ftp_list(url: &str, cred: Option<&Cred>) -> Result<Vec<RawEntry>, String> {
    // 独立解析（parse_url 仅接受 http/https）
    let rest = url.trim().trim_start_matches("ftp://").trim_start_matches("ftps://");
    let (hostport, path) = match rest.find('/') {
        Some(i) => (&rest[..i], rest[i..].to_string()),
        None => (rest, "/".to_string()),
    };
    if hostport.is_empty() {
        return Err("FTP 地址缺少主机名".into());
    }
    let (host, port) = match hostport.rsplit_once(':') {
        Some((h, p)) if !p.is_empty() && p.chars().all(|c| c.is_ascii_digit()) => {
            (h.to_string(), p.parse::<u16>().map_err(|_| "端口无效")?)
        }
        _ => (hostport.to_string(), 21),
    };
    let u = parse_url(&format!("http://{host}:{port}{path}"))?; // 复用 path 归一化
    let mut conn = FtpConn::connect(&u.host, u.port)?;
    conn.login(cred)?;
    conn.cmd("TYPE I")?;
    let raw_text = conn.list_at(&u.path)?;
    let raw = if raw_text.contains("type=dir;") || raw_text.contains("type=file;") {
        parse_mlsd(&raw_text)
    } else {
        parse_unix_list(&raw_text)
    };
    let base = format!("ftp://{}:{}", u.host, u.port);
    Ok(raw
        .into_iter()
        .map(|mut e| {
            e.path = format!("{}{}", base.trim_end_matches('/'), enc_join(&u.path, &e.name));
            e
        })
        .collect())
}

fn enc_join(dir: &str, name: &str) -> String {
    let d = dir.trim_end_matches('/');
    format!("{}/{}", d, percent_encode_path(name))
}

/// MLSD（RFC 3659）解析
fn parse_mlsd(text: &str) -> Vec<RawEntry> {
    let mut out = Vec::new();
    for line in text.lines() {
        let line = line.trim_end_matches('\r');
        if line.is_empty() {
            continue;
        }
        let Some(sp) = line.find(' ') else { continue };
        let (facts, name) = (&line[..sp], &line[sp + 1..]);
        if name.is_empty() || name == "." || name == ".." {
            continue;
        }
        let mut is_dir = false;
        let mut size = 0i64;
        let mut mtime = 0i64;
        for f in facts.split(';') {
            let f = f.trim();
            if let Some((k, v)) = f.split_once('=') {
                match k.to_ascii_lowercase().as_str() {
                    "type" => is_dir = v.eq_ignore_ascii_case("dir"),
                    "size" => size = v.parse().unwrap_or(0),
                    "modify" => {
                        mtime = parse_compact_time(v);
                    }
                    _ => {}
                }
            }
        }
        let ext = if is_dir { String::new() } else { fs::ext_of(name) };
        let category = if is_dir { CAT_FOLDER } else { fs::category_for_ext(&ext) };
        out.push(RawEntry {
            name: name.to_string(),
            path: String::new(),
            is_dir,
            size: if is_dir { 0 } else { size },
            mtime,
            ext,
            category,
            hidden: name.starts_with('.'),
        });
    }
    out
}

/// 20260330124400（可带毫秒/时区尾巴）→ 本地 epoch
fn parse_compact_time(s: &str) -> i64 {
    let d: String = s.chars().filter(|c| c.is_ascii_digit()).collect();
    if d.len() < 8 {
        return 0;
    }
    let y: i32 = d[0..4].parse().unwrap_or(1970);
    let mo: u32 = d[4..6].parse().unwrap_or(1);
    let dd: u32 = d[6..8].parse().unwrap_or(1);
    let (h, mi, sec) = if d.len() >= 14 {
        (
            d[8..10].parse().unwrap_or(0),
            d[10..12].parse().unwrap_or(0),
            d[12..14].parse().unwrap_or(0),
        )
    } else {
        (0, 0, 0)
    };
    local_ts(y, mo, dd, h, mi, sec).unwrap_or(0)
}

/// UNIX ls -l / Windows DOS 双格式解析
fn parse_unix_list(text: &str) -> Vec<RawEntry> {
    let mut out = Vec::new();
    for line in text.lines() {
        let line = line.trim_end_matches('\r');
        let f: Vec<&str> = line.split_whitespace().collect();
        if f.is_empty() || f[0] == "total" {
            continue;
        }
        // DOS 风格优先判断：03-01-2020  10:00AM  <DIR>  name（字段数少于 UNIX）
        if f[0].matches('-').count() == 2 && f[0].matches('/').count() == 0 && f[0].len() >= 8
        {
            if let Some(e) = parse_dos_line(&f, line) {
                out.push(e);
            }
            continue;
        }
        // UNIX 风格：drwx------ 1 owner group size Mon day time/year name…
        let perm = f[0];
        if !(perm.starts_with('-') || perm.starts_with('d') || perm.starts_with('l')) {
            continue;
        }
        // 标准布局：perm links owner group size month day time name
        if f.len() < 9 {
            continue;
        }
        let is_dir = perm.starts_with('d');
        let size: i64 = f[4].parse().unwrap_or(0);
        let name = f[8..].join(" ");
        if name.is_empty() || name == "." || name == ".." {
            continue;
        }
        let mtime = parse_ls_time(f[5], f[6], f[7]);
        let ext = if is_dir { String::new() } else { fs::ext_of(&name) };
        let category = if is_dir { CAT_FOLDER } else { fs::category_for_ext(&ext) };
        let hidden = name.starts_with('.');
        out.push(RawEntry {
            name,
            path: String::new(),
            is_dir,
            size: if is_dir { 0 } else { size },
            mtime,
            ext,
            category,
            hidden,
        });
    }
    out
}

fn parse_dos_line(f: &[&str], _line: &str) -> Option<RawEntry> {
    // 03-01-2020 10:00AM <DIR> name / 03-01-2020 10:00AM 123 name
    if f.len() < 4 {
        return None;
    }
    let dparts: Vec<&str> = f[0].split('-').collect();
    if dparts.len() != 3 {
        return None;
    }
    let mo: u32 = dparts[0].parse().ok()?;
    let dd: u32 = dparts[1].parse().ok()?;
    let y: i32 = dparts[2].parse().ok()?;
    let t = f[1];
    let (hh, mi) = parse_ampm(t);
    let mtime = local_ts(y, mo, dd, hh, mi, 0).unwrap_or(0);
    let is_dir = f[2].eq_ignore_ascii_case("<DIR>") || f[2].contains("DIR");
    let name = f[3..].join(" ");
    if name.is_empty() {
        return None;
    }
    let size: i64 = if is_dir { 0 } else { f[2].parse().unwrap_or(0) };
    let ext = if is_dir { String::new() } else { fs::ext_of(&name) };
    let category = if is_dir { CAT_FOLDER } else { fs::category_for_ext(&ext) };
    Some(RawEntry {
        name,
        path: String::new(),
        is_dir,
        size,
        mtime,
        ext,
        category,
        hidden: false,
    })
}

fn parse_ampm(t: &str) -> (u32, u32) {
    let t = t.trim().trim_end_matches(['A', 'P', 'M', 'a', 'p', 'm']);
    let mut it = t.split(':');
    let mut hh: u32 = it.next().and_then(|x| x.parse().ok()).unwrap_or(0);
    let mi: u32 = it.next().and_then(|x| x.parse().ok()).unwrap_or(0);
    let lower = t.to_ascii_lowercase();
    if lower.ends_with('p') && hh < 12 {
        hh += 12;
    }
    (hh, mi)
}

fn parse_ls_time(mon: &str, day: &str, tv: &str) -> i64 {
    use chrono::Datelike;
    let Some(mo) = month_of(mon) else { return 0 };
    let dd: u32 = day.parse().unwrap_or(1);
    let now = chrono::Local::now();
    if tv.contains(':') {
        let mut it = tv.split(':');
        let h: u32 = it.next().and_then(|x| x.parse().ok()).unwrap_or(0);
        let mi: u32 = it.next().and_then(|x| x.parse().ok()).unwrap_or(0);
        local_ts(now.year(), mo, dd, h, mi, 0).unwrap_or(0)
    } else {
        let y: i32 = tv.parse().unwrap_or(now.year());
        local_ts(y, mo, dd, 0, 0, 0).unwrap_or(0)
    }
}

// ============ SMB ============

const ERROR_ALREADY_ASSIGNED: u32 = 81;
const ERROR_DEVICE_ALREADY_REMEMBERED: u32 = 85;

/// 设备无关连接（等同 net use \\host\share /user:x pass）；已有会话视为成功
fn smb_ensure_session(remote: &str, cred: Option<&Cred>) -> Result<(), String> {
    use windows_sys::Win32::NetworkManagement::WNet::{NETRESOURCEW, WNetAddConnection2W};
    let (user, pass) = match cred {
        Some(c) if !c.user.is_empty() => (c.user.clone(), c.pass.clone()),
        _ => return Ok(()), // 无凭证：交给系统当前会话
    };
    let mut wide: Vec<u16> = remote.encode_utf16().chain(std::iter::once(0)).collect();
    let u16user: Vec<u16> = user.encode_utf16().chain(std::iter::once(0)).collect();
    let u16pass: Vec<u16> = pass.encode_utf16().chain(std::iter::once(0)).collect();
    let res = NETRESOURCEW {
        dwScope: 0,
        dwType: 1, // RESOURCETYPE_DISK
        dwDisplayType: 0,
        dwUsage: 0,
        lpLocalName: std::ptr::null_mut(),
        lpRemoteName: wide.as_mut_ptr(),
        lpComment: std::ptr::null_mut(),
        lpProvider: std::ptr::null_mut(),
    };
    let rc = unsafe {
        WNetAddConnection2W(
            &res,
            u16pass.as_ptr(),
            u16user.as_ptr(),
            0,
        )
    };
    match rc {
        0 | ERROR_ALREADY_ASSIGNED | ERROR_DEVICE_ALREADY_REMEMBERED => Ok(()),
        1219 => Ok(()), // 同一用户多重会话冲突：现有会话可用
        c => Err(format!(
            "SMB 认证失败（错误 {c}）——请删除该条目后重新添加并填写正确的账号密码"
        )),
    }
}

fn smb_list(url: &str, cred: Option<&Cred>) -> Result<Vec<RawEntry>, String> {
    let parts: Vec<&str> = url.trim_matches('\\').split('\\').filter(|s| !s.is_empty()).collect();
    match parts.len() {
        0 => Err("SMB 地址缺少主机名".into()),
        1 => {
            // \\host → 共享枚举
            let host = parts[0];
            smb_ensure_session(&format!(r"\\{host}\IPC$"), cred)?;
            smb_enum_shares(host)
        }
        _ => {
            let host = parts[0];
            let share = parts[1];
            let p = PathBuf::from(url);
            match list_unc(&p) {
                Ok(v) => Ok(v),
                Err(e) => {
                    // 尝试建立/刷新会话后重试一次
                    smb_ensure_session(&format!(r"\\{host}\{share}"), cred)?;
                    list_unc(&p).map_err(|_| format!("无法访问共享：{e}"))
                }
            }
        }
    }
}

fn list_unc(path: &Path) -> std::io::Result<Vec<RawEntry>> {
    use std::os::windows::fs::MetadataExt;
    const HIDDEN_OR_SYSTEM: u32 = 0x2 | 0x4;
    let mut out = Vec::new();
    for entry in std::fs::read_dir(path)? {
        let Ok(entry) = entry else { continue };
        let Ok(md) = entry.metadata() else { continue };
        let hidden = md.file_attributes() & HIDDEN_OR_SYSTEM != 0;
        let is_dir = md.is_dir();
        let name = entry.file_name().to_string_lossy().into_owned();
        let ext = if is_dir { String::new() } else { fs::ext_of(&name) };
        let category = if is_dir { CAT_FOLDER } else { fs::category_for_ext(&ext) };
        let mtime = md
            .modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        out.push(RawEntry {
            path: entry.path().to_string_lossy().into_owned(),
            name,
            is_dir,
            size: if is_dir { 0 } else { md.len() as i64 },
            mtime,
            ext,
            category,
            hidden,
        });
    }
    Ok(out)
}

/// NetShareEnum level 1：仅列磁盘共享（排除打印/IPC/特殊）
fn smb_enum_shares(host: &str) -> Result<Vec<RawEntry>, String> {
    use windows_sys::Win32::NetworkManagement::NetManagement::NetApiBufferFree;
    use windows_sys::Win32::Storage::FileSystem::{NetShareEnum, SHARE_INFO_1};
    let wide: Vec<u16> = format!(r"\\{host}")
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    let mut buf: *mut u8 = std::ptr::null_mut();
    let mut read = 0u32;
    let mut total = 0u32;
    let mut resume = 0u32;
    let rc = unsafe {
        NetShareEnum(
            wide.as_ptr(),
            1,
            &mut buf,
            0xFFFFFFFF, // MAX_PREFERRED_LENGTH
            &mut read,
            &mut total,
            &mut resume,
        )
    };
    if rc != 0 {
        return Err(match rc {
            5 => "共享枚举被拒绝（错误 5）：请确认条目带有正确的账号密码".to_string(),
            1326 => "SMB 登录失败：账号或密码错误".to_string(),
            c => format!("共享枚举失败（错误 {c}）"),
        });
    }
    let mut out = Vec::new();
    unsafe {
        let infos = buf as *const SHARE_INFO_1;
        for i in 0..read as usize {
            let si = &*infos.add(i);
            let t = si.shi1_type;
            // 磁盘共享：低位类型 0，非特殊（高位标志排除 IPC$/打印/管理共享）
            if (t & 0x3) != 0 || (t & 0x8000_0000) != 0 {
                continue;
            }
            let name = if si.shi1_netname.is_null() {
                continue;
            } else {
                let len = (0..).take_while(|&k| *si.shi1_netname.add(k) != 0).count();
                String::from_utf16_lossy(std::slice::from_raw_parts(si.shi1_netname, len))
            };
            out.push(RawEntry {
                path: format!(r"\\{host}\{name}"),
                name,
                is_dir: true,
                size: 0,
                mtime: 0,
                ext: String::new(),
                category: CAT_FOLDER,
                hidden: false,
            });
        }
        NetApiBufferFree(buf as *mut _);
    }
    if out.is_empty() {
        return Err("主机上没有可用的磁盘共享（或访问被拒绝）".into());
    }
    Ok(out)
}

// ============ DLNA ============

/// host:port → ContentDirectory 控制地址（进程级缓存）
static DLNA_CACHE: LazyLock<Mutex<HashMap<String, String>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

fn dlna_list(url: &str) -> Result<Vec<RawEntry>, String> {
    let rest = url.trim().trim_start_matches("dlna://");
    let (hostport, objid) = match rest.find('/') {
        Some(i) => (&rest[..i], rest[i + 1..].trim_end_matches('/')),
        None => (rest, ""),
    };
    if hostport.is_empty() {
        return Err("DLNA 地址缺少主机".into());
    }
    let objid = if objid.is_empty() { "0" } else { objid };
    let control = dlna_resolve(hostport)?;
    let didl = dlna_browse(&control, objid, 300)?;
    Ok(parse_didl(&didl, hostport))
}

/// BrowseMetadata 取父容器 objectID（nav_up 用，后台线程调用）
pub fn dlna_parent(url: &str) -> Option<String> {
    let rest = url.trim().trim_start_matches("dlna://");
    let (hostport, objid) = match rest.find('/') {
        Some(i) => (&rest[..i], rest[i + 1..].trim_end_matches('/')),
        None => (rest, ""),
    };
    if hostport.is_empty() || objid.is_empty() || objid == "0" {
        return None;
    }
    let control = dlna_resolve(hostport).ok()?;
    let didl = dlna_browse_meta(&control, objid).ok()?;
    let block = xml_blocks(&didl, "container")
        .into_iter()
        .chain(xml_blocks(&didl, "item"))
        .next()?;
    let parent = xml_attr(&block, "parentID")?;
    Some(format!("dlna://{hostport}/{parent}"))
}

fn dlna_resolve(hostport: &str) -> Result<String, String> {
    if let Some(c) = DLNA_CACHE.lock().unwrap().get(hostport) {
        return Ok(c.clone());
    }
    // 1) SSDP 发现（2.5 秒窗口）
    if let Ok(Some(ctrl)) = ssdp_find(hostport, 2500) {
        DLNA_CACHE.lock().unwrap().insert(hostport.to_string(), ctrl.clone());
        return Ok(ctrl);
    }
    // 2) 回退：常见端口直探标准 MiniDLNA 路径
    let ip = hostport.split(':').next().unwrap_or(hostport);
    for port in [8200u16, 49152, 5000, 5001, 8201, 8000] {
        let addr = match (ip, port).to_socket_addrs() {
            Ok(mut it) => match it.next() {
                Some(a) => a,
                None => continue,
            },
            Err(_) => continue,
        };
        if TcpStream::connect_timeout(&addr, Duration::from_millis(700)).is_err() {
            continue;
        }
        let base = format!("http://{ip}:{port}");
        for path in ["/Control/ContentDirectory", "/control/ContentDirectory"] {
            let ctrl = format!("{base}{path}");
            if dlna_browse(&ctrl, "0", 1).is_ok() {
                DLNA_CACHE.lock().unwrap().insert(hostport.to_string(), ctrl.clone());
                return Ok(ctrl);
            }
        }
        // description.xml → 解析控制 URL
        if let Ok(resp) = http_request("GET", &format!("{base}/description.xml"), &[], b"", None)
            && resp.status == 200
                && let Some(ctrl) = parse_device_desc(&resp.text(), &base)
                    && dlna_browse(&ctrl, "0", 1).is_ok() {
                        DLNA_CACHE.lock().unwrap().insert(hostport.to_string(), ctrl.clone());
                        return Ok(ctrl);
                    }
    }
    Err("未发现 DLNA 服务器（SSDP 无响应，常见端口探测失败）".into())
}

/// SSDP M-SEARCH 多播发现，按 IP 过滤；返回 ContentDirectory 控制 URL
fn ssdp_find(hostport: &str, window_ms: u64) -> Result<Option<String>, String> {
    let ip = hostport.split(':').next().unwrap_or(hostport);
    let sock = UdpSocket::bind("0.0.0.0:0").map_err(|e| e.to_string())?;
    sock.set_read_timeout(Some(Duration::from_millis(400))).ok();
    let msg = "M-SEARCH * HTTP/1.1\r\nHOST: 239.255.255.250:1900\r\nMAN: \"ssdp:discover\"\r\nMX: 2\r\nST: ssdp:all\r\n\r\n";
    for _ in 0..2 {
        let _ = sock.send_to(msg.as_bytes(), "239.255.255.250:1900");
    }
    let mut locations: Vec<String> = Vec::new();
    let deadline = std::time::Instant::now() + Duration::from_millis(window_ms);
    while std::time::Instant::now() < deadline {
        let mut buf = [0u8; 4096];
        match sock.recv_from(&mut buf) {
            Ok((n, _from)) => {
                let text = String::from_utf8_lossy(&buf[..n]).to_ascii_lowercase();
                if (text.contains("contentdirectory") || text.contains("mediaserver"))
                    && let Some(i) = text.find("location:") {
                        let loc = text[i + 9..].lines().next().unwrap_or("").trim();
                        if !loc.is_empty() {
                            locations.push(loc.to_string());
                        }
                    }
            }
            Err(_) => continue,
        }
    }
    locations.sort();
    locations.dedup();
    for loc in &locations {
        // 按 IP 过滤
        if let Ok(u) = parse_url(loc)
            && u.host != ip && !ip.is_empty() {
                continue;
            }
        if let Ok(resp) = http_request("GET", loc, &[], b"", None)
            && resp.status == 200
                && let Some(ctrl) = parse_device_desc(&resp.text(), loc) {
                    return Ok(Some(ctrl));
                }
    }
    Ok(None)
}

/// 设备描述 XML → ContentDirectory 控制 URL（处理 URLBase/相对路径）
fn parse_device_desc(xml: &str, loc: &str) -> Option<String> {
    let url_base = xml_child(xml, "URLBase").filter(|s| !s.is_empty());
    for svc in xml_blocks(xml, "service") {
        let stype = xml_child(&svc, "serviceType").unwrap_or_default();
        if !stype.to_ascii_lowercase().contains("contentdirectory") {
            continue;
        }
        let curl = xml_child(&svc, "controlURL")?;
        if curl.starts_with("http://") || curl.starts_with("https://") {
            return Some(curl);
        }
        let base_host = match url_base.filter(|b| b.starts_with("http")) {
            Some(b) => b,
            None => {
                // location 的 scheme://host:port
                {
                    let s = loc.find("://")?;
                    let rest = &loc[s + 3..];
                    match rest.find('/') {
                        Some(i) => loc[..s + 3 + i].to_string(),
                        None => loc.to_string(),
                    }
                }
            }
        };
        return Some(if curl.starts_with('/') {
            format!("{base_host}{curl}")
        } else {
            format!("{base_host}/{curl}")
        });
    }
    None
}

fn soap_body(action: &str, objid: &str, flag: &str, count: u32) -> String {
    format!(
        r#"<?xml version="1.0" encoding="utf-8"?>
<s:Envelope xmlns:s="http://schemas.xmlsoap.org/soap/envelope/" s:encodingStyle="http://schemas.xmlsoap.org/soap/encoding/">
<s:Body><u:{action} xmlns:u="urn:schemas-upnp-org:service:ContentDirectory:1">
<ObjectID>{objid}</ObjectID><BrowseFlag>{flag}</BrowseFlag><Filter>*</Filter><StartingIndex>0</StartingIndex><RequestedCount>{count}</RequestedCount><SortCriteria></SortCriteria>
</u:{action}></s:Body></s:Envelope>"#
    )
}

fn dlna_soap(control: &str, objid: &str, flag: &str, count: u32) -> Result<String, String> {
    let body = soap_body("Browse", objid, flag, count);
    let resp = http_request(
        "POST",
        control,
        &[("SOAPACTION", "\"urn:schemas-upnp-org:service:ContentDirectory:1#Browse\""), ("Content-Type", "text/xml; charset=\"utf-8\"")],
        body.as_bytes(),
        None,
    )?;
    if resp.status != 200 {
        return Err(http_err_hint(resp.status, &resp.text()));
    }
    let text = resp.text();
    let result = xml_child(&text, "Result").ok_or("SOAP 响应缺少 Result")?;
    Ok(xml_unescape(&result))
}

fn dlna_browse(control: &str, objid: &str, count: u32) -> Result<String, String> {
    dlna_soap(control, objid, "BrowseDirectChildren", count)
}

fn dlna_browse_meta(control: &str, objid: &str) -> Result<String, String> {
    dlna_soap(control, objid, "BrowseMetadata", 1)
}

/// DIDL-Lite → 条目（container=目录，item=文件）
fn parse_didl(didl: &str, hostport: &str) -> Vec<RawEntry> {
    let mut out = Vec::new();
    let mut push = |block: &str, is_dir: bool| {
        let Some(id) = xml_attr(block, "id").filter(|s| !s.is_empty()) else {
            return;
        };
        let name = xml_child(block, "title").unwrap_or_else(|| "未命名".into());
        if name.is_empty() {
            return;
        }
        let size: i64 = xml_blocks(block, "res")
            .iter()
            .filter_map(|r| xml_attr(r, "size"))
            .find_map(|s| s.trim().parse().ok())
            .unwrap_or(0);
        let ext = if is_dir { String::new() } else { fs::ext_of(&name) };
        let category = if is_dir { CAT_FOLDER } else { fs::category_for_ext(&ext) };
        out.push(RawEntry {
            path: format!("dlna://{hostport}/{id}"),
            name,
            is_dir,
            size: if is_dir { 0 } else { size },
            mtime: 0,
            ext,
            category,
            hidden: false,
        });
    };
    for b in xml_blocks(didl, "container") {
        push(&b, true);
    }
    for b in xml_blocks(didl, "item") {
        push(&b, false);
    }
    out
}

// ============ 测试 ============

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_is_remote() {
        assert!(is_remote(r"\\nas\media"));
        assert!(is_remote(r"\\192.168.1.16"));
        assert!(is_remote("ftp://h/"));
        assert!(is_remote("https://h:5006/"));
        assert!(is_remote("dlna://h"));
        assert!(!is_remote("C:\\Users"));
        assert!(!is_remote("shell:AppsFolder"));
    }

    #[test]
    fn test_host_key() {
        assert_eq!(host_key(r"\\NAS\media"), r"\\nas");
        assert_eq!(host_key("ftp://192.168.1.16/public/x"), "ftp://192.168.1.16");
        assert_eq!(host_key("https://192.168.1.16:5006/d/"), "https://192.168.1.16:5006");
        assert_eq!(host_key("dlna://192.168.1.16"), "dlna://192.168.1.16");
    }

    #[test]
    fn test_parent_url() {
        assert_eq!(parent_url(r"\\h\s\a\b"), Some(r"\\h\s\a".into()));
        assert_eq!(parent_url(r"\\h\s\a"), Some(r"\\h\s".into()));
        assert_eq!(parent_url(r"\\h\s"), Some(r"\\h".into()));
        assert_eq!(parent_url(r"\\h"), None);
        assert_eq!(parent_url("ftp://h:21/a/b"), Some("ftp://h:21/a".into()));
        assert_eq!(parent_url("ftp://h:21/a"), Some("ftp://h:21".into()));
        assert_eq!(parent_url("ftp://h:21/"), None);
        assert_eq!(parent_url("https://h:5006/d/e/"), Some("https://h:5006/d".into()));
    }

    #[test]
    fn test_crumbs() {
        let c = crumbs_for(r"\\h\share\sub");
        assert_eq!(c.len(), 3);
        assert_eq!(c[0], ("h".into(), r"\\h".into()));
        assert_eq!(c[1], ("share".into(), r"\\h\share".into()));
        assert_eq!(c[2], ("sub".into(), r"\\h\share\sub".into()));
        let c = crumbs_for("ftp://h:21/a/b");
        assert_eq!(c.len(), 3);
        assert_eq!(c[1], ("a".into(), "ftp://h:21/a".into()));
        let c = crumbs_for("dlna://h:8200/64");
        assert_eq!(c.len(), 1);
    }

    #[test]
    fn test_display_label() {
        assert_eq!(display_label("ftp://192.168.1.16:21/public"), "public");
        assert_eq!(display_label("ftp://192.168.1.16:21/"), "192.168.1.16:21");
        assert_eq!(display_label(r"\\h\share"), "share");
        assert_eq!(display_label("dlna://192.168.1.16:8200"), "DLNA 192.168.1.16:8200");
    }

    #[test]
    fn test_percent() {
        assert_eq!(percent_decode("%E6%9D%A5%E8%87%AA"), "来自");
        assert_eq!(percent_decode("a%20b"), "a b");
        assert_eq!(percent_encode_path("来自"), "%E6%9D%A5%E8%87%AA");
        assert_eq!(percent_encode_path("/a b"), "/a%20b");
        assert_eq!(percent_encode_path("/plain/"), "/plain/");
    }

    #[test]
    fn test_base64() {
        assert_eq!(base64_encode("admin:Xj50000t"), "YWRtaW46WGo1MDAwMHQ=");
        assert_eq!(base64_encode("a"), "YQ==");
        assert_eq!(base64_encode("ab"), "YWI=");
        assert_eq!(base64_encode("abc"), "YWJj");
    }

    #[test]
    fn test_xml_child() {
        let t = r#"<D:response><D:href>/a/</D:href><D:displayname>a</D:displayname></D:response>"#;
        assert_eq!(xml_child(t, "href").unwrap(), "/a/");
        assert_eq!(xml_child(t, "displayname").unwrap(), "a");
        assert!(xml_child(t, "missing").is_none());
        // 无前缀 + 嵌套其他标签
        let t2 = r#"<container id="1"><dc:title>Music</dc:title><upnp:class>x</upnp:class></container>"#;
        assert_eq!(xml_child(t2, "title").unwrap(), "Music");
    }

    #[test]
    fn test_xml_attr_and_blocks() {
        let t = r#"<DIDL-Lite><container id="64" parentID="0" childCount="2"><dc:title>Folders</dc:title></container><item id="9"><dc:title>a.mp3</dc:title><res size="1234" duration="0:03">url</res></item></DIDL-Lite>"#;
        let blocks = xml_blocks(t, "container");
        assert_eq!(blocks.len(), 1);
        assert_eq!(xml_attr(&blocks[0], "id").unwrap(), "64");
        let items = xml_blocks(t, "item");
        assert_eq!(items.len(), 1);
        assert_eq!(xml_attr(&items[0], "id").unwrap(), "9");
        assert_eq!(
            xml_blocks(&items[0], "res").iter().find_map(|r| xml_attr(r, "size")).unwrap(),
            "1234"
        );
    }

    #[test]
    fn test_xml_unescape() {
        assert_eq!(xml_unescape("&lt;a&gt;&amp;&quot;"), "<a>&\"");
        assert_eq!(xml_unescape("&#20013;"), "中");
        assert_eq!(xml_unescape("plain"), "plain");
    }

    #[test]
    fn test_parse_unix_list() {
        let t = "\
drwx------   1 1000     1001              126 Mar 30 12:44 139\r\n\
d---------   1 1000     1001                0 Nov 10 12:51 SMB\r\n\
-rw-r--r--   1 1000     1001           524288 Apr  2  2025 test file.bin\r\n";
        let v = parse_unix_list(t);
        assert_eq!(v.len(), 3);
        assert!(v[0].is_dir);
        assert_eq!(v[0].name, "139");
        assert_eq!(v[1].name, "SMB");
        assert!(!v[2].is_dir);
        assert_eq!(v[2].name, "test file.bin");
        assert_eq!(v[2].size, 524288);
        assert_eq!(v[2].ext, "bin");
        assert_eq!(v[0].category, CAT_FOLDER);
    }

    #[test]
    fn test_parse_mlsd() {
        let t = "type=dir;size=0;modify=20260330124400;UNIX.mode=0700; 139\r\ntype=file;size=42;modify=20250402090500; test.bin\r\n";
        let v = parse_mlsd(t);
        assert_eq!(v.len(), 2);
        assert!(v[0].is_dir);
        assert_eq!(v[0].name, "139");
        assert_eq!(v[1].size, 42);
        assert_eq!(v[1].ext, "bin");
        assert!(v[1].mtime > 0);
    }

    #[test]
    fn test_parse_dos_list() {
        let t = "03-01-2020  10:00AM    <DIR>          dira\r\n03-01-2020  11:30PM          1234 file b.txt\r\n";
        let v = parse_unix_list(t);
        assert_eq!(v.len(), 2);
        assert!(v[0].is_dir);
        assert_eq!(v[0].name, "dira");
        assert_eq!(v[1].name, "file b.txt");
        assert_eq!(v[1].size, 1234);
    }

    #[test]
    fn test_parse_propfind() {
        let t = r#"<?xml version="1.0" encoding="UTF-8"?><D:multistatus xmlns:D="DAV:"><D:response><D:href>/</D:href><D:propstat><D:prop><D:resourcetype><D:collection xmlns:D="DAV:"/></D:resourcetype><D:displayname></D:displayname><D:getlastmodified>Wed, 11 Nov 2026 13:34:56 GMT</D:getlastmodified></D:prop><D:status>HTTP/1.1 200 OK</D:status></D:propstat></D:response><D:response><D:href>/139/</D:href><D:propstat><D:prop><D:getlastmodified>Mon, 30 Mar 2026 12:44:01 GMT</D:getlastmodified><D:resourcetype><D:collection xmlns:D="DAV:"/></D:resourcetype><D:displayname>139</D:displayname></D:prop></D:propstat></D:response><D:response><D:href>/file.txt</D:href><D:propstat><D:prop><D:getcontentlength>42</D:getcontentlength><D:resourcetype></D:resourcetype><D:displayname>file.txt</D:displayname></D:prop></D:propstat></D:response></D:multistatus>"#;
        let blocks = xml_blocks(t, "response");
        assert_eq!(blocks.len(), 3);
        assert!(blocks[1].to_ascii_lowercase().contains("collection"));
        assert_eq!(xml_child(&blocks[1], "displayname").unwrap(), "139");
        let mt = xml_child(&blocks[1], "getlastmodified").unwrap();
        assert!(chrono::DateTime::parse_from_rfc2822(&mt).is_ok());
        assert_eq!(xml_child(&blocks[2], "getcontentlength").unwrap(), "42");
    }

    #[test]
    fn test_parse_didl() {
        let didl = "&lt;DIDL-Lite xmlns:dc=&quot;http://purl.org/dc/elements/1.1/&quot;&gt;\n&lt;container id=&quot;64&quot; parentID=&quot;0&quot; restricted=&quot;1&quot; searchable=&quot;1&quot; childCount=&quot;2&quot;&gt;&lt;dc:title&gt;Browse Folders&lt;/dc:title&gt;&lt;upnp:class&gt;object.container.storageFolder&lt;/upnp:class&gt;&lt;/container&gt;&lt;container id=&quot;1&quot; parentID=&quot;0&quot; childCount=&quot;7&quot;&gt;&lt;dc:title&gt;Music&lt;/dc:title&gt;&lt;/container&gt;&lt;/DIDL-Lite&gt;";
        let decoded = xml_unescape(didl);
        let v = parse_didl(&decoded, "192.168.1.16:8200");
        assert_eq!(v.len(), 2);
        assert!(v[0].is_dir);
        assert_eq!(v[0].name, "Browse Folders");
        assert_eq!(v[0].path, "dlna://192.168.1.16:8200/64");
        assert_eq!(v[1].name, "Music");
    }

    #[test]
    fn test_parse_device_desc() {
        let xml = r#"<?xml version="1.0"?><root xmlns="urn:schemas-upnp-org:device-1-0"><URLBase>http://192.168.1.16:8200</URLBase><device><friendlyName>my-nsa</friendlyName><serviceList><service><serviceType>urn:schemas-upnp-org:service:ContentDirectory:1</serviceType><serviceId>urn:upnp-org:serviceId:ContentDirectory</serviceId><controlURL>/Control/ContentDirectory</controlURL></service></serviceList></device></root>"#;
        assert_eq!(
            parse_device_desc(xml, "http://192.168.1.16:8200/description.xml").unwrap(),
            "http://192.168.1.16:8200/Control/ContentDirectory"
        );
    }

    #[test]
    fn test_parse_url() {
        let u = parse_url("https://192.168.1.16:5006/d/").unwrap();
        assert!(u.https);
        assert_eq!(u.port, 5006);
        assert_eq!(u.path, "/d/");
        assert!(parse_url("ftp://192.168.1.16").is_err()); // ftp 不走 parse_url
        let u = parse_url("http://192.168.1.16:5005/").unwrap();
        assert_eq!(u.port, 5005);
    }

    // ---------- 实测（NAS 真机）：cargo test -- --ignored --nocapture ----------
    // 凭证从环境变量读取：WFM_NAS_USER / WFM_NAS_PASS

    fn live_cred() -> Cred {
        Cred {
            user: std::env::var("WFM_NAS_USER").unwrap_or_default(),
            pass: std::env::var("WFM_NAS_PASS").unwrap_or_default(),
        }
    }

    #[test]
    #[ignore]
    fn live_smb_shares() {
        let cred = live_cred();
        let v = smb_list(r"\\192.168.1.16", Some(&cred)).expect("SMB 共享枚举失败");
        println!("SMB 共享 {} 个：{:?}", v.len(), v.iter().map(|e| e.name.clone()).collect::<Vec<_>>());
        assert!(!v.is_empty());
    }

    #[test]
    #[ignore]
    fn live_smb_share_dir() {
        let cred = live_cred();
        let v = smb_list(r"\\192.168.1.16\public", Some(&cred)).expect("SMB 共享目录读取失败");
        println!("\\\\192.168.1.16\\public {} 项：", v.len());
        for e in v.iter().take(10) {
            println!("  {} {}", if e.is_dir { "[D]" } else { "[F]" }, e.name);
        }
        assert!(!v.is_empty());
    }

    #[test]
    #[ignore]
    fn live_ftp_root() {
        let cred = live_cred();
        let v = ftp_list("ftp://192.168.1.16:21/", Some(&cred)).expect("FTP 列目录失败");
        println!("FTP 根目录 {} 项：", v.len());
        for e in v.iter().take(12) {
            println!("  {} {} ({})", if e.is_dir { "[D]" } else { "[F]" }, e.name, e.path);
        }
        assert!(!v.is_empty());
        // 子目录导航
        let sub = v.iter().find(|e| e.is_dir).expect("无子目录");
        let v2 = ftp_list(&sub.path, Some(&cred)).expect("FTP 子目录失败");
        println!("FTP {} {} 项", sub.path, v2.len());
    }

    #[test]
    #[ignore]
    fn live_webdav_root() {
        let cred = live_cred();
        let v = dav_list("https://192.168.1.16:5006/", Some(&cred)).expect("WebDAV 列目录失败");
        println!("WebDAV 根 {} 项：", v.len());
        for e in v.iter().take(12) {
            println!("  {} {} ({})", if e.is_dir { "[D]" } else { "[F]" }, e.name, e.path);
        }
        assert!(!v.is_empty());
    }

    #[test]
    #[ignore]
    fn live_webdav_401() {
        let r = dav_list("https://192.168.1.16:5006/", None);
        let err = r.expect_err("无凭证应当失败");
        println!("无凭证错误提示：{err}");
        assert!(err.contains("账号密码") || err.contains("401"));
    }

    #[test]
    #[ignore]
    fn live_dlna_browse() {
        let v = dlna_list("dlna://192.168.1.16:8200").expect("DLNA 浏览失败");
        println!("DLNA 根 {} 项：", v.len());
        for e in &v {
            println!("  {} {} ({})", if e.is_dir { "[D]" } else { "[F]" }, e.name, e.path);
        }
        assert!(!v.is_empty());
        // 进入子容器
        let sub = v.iter().find(|e| e.is_dir).expect("无容器");
        let v2 = dlna_list(&sub.path).expect("DLNA 子容器失败");
        println!("DLNA {} {} 项", sub.path, v2.len());
        assert!(!v2.is_empty());
    }

    #[test]
    #[ignore]
    fn live_dlna_parent() {
        // 先拿一个子容器再取父
        let v = dlna_list("dlna://192.168.1.16:8200").expect("DLNA 浏览失败");
        let sub = v.iter().find(|e| e.is_dir).expect("无容器");
        let parent = dlna_parent(&sub.path).expect("取父容器失败");
        println!("{} → {}", sub.path, parent);
        assert_eq!(parent, "dlna://192.168.1.16:8200/0");
    }

    #[test]
    #[ignore]
    fn live_dlna_resolve_fallback() {
        // 绕过 SSDP：清缓存后直探（验证回退路径）
        DLNA_CACHE.lock().unwrap().clear();
        let ctrl = dlna_resolve("192.168.1.16:8200").expect("解析控制 URL 失败");
        println!("控制 URL：{ctrl}");
        assert!(ctrl.contains("ContentDirectory"));
    }
}

