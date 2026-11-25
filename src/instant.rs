//! 秒搜（Everything 式即时搜索）
//!
//! 后端两级（见 docs/06-秒搜功能规格.md）：
//! 1. 自建全盘索引：NTFS 卷优先 MFT 直读（src/mft.rs，报告 §3.2；需管理员），
//!    非 NTFS / 非提权 / 自定义目录回退 FindFirstFile 递归枚举（报告 §3.5 文件夹
//!    索引模式）+ ReadDirectoryChangesW 增量。**不依赖任何第三方进程**（不借用
//!    Everything IPC——独立实现是硬性要求）。
//! 2. 旧递归搜索（tasks.rs）由 main.rs 直接降级，不在本模块。
//!
//! 索引构建完成后落盘到程序同级目录 wfm_index.bin，下次启动优先加载（卷序列号
//! 校验新鲜度），加载失败或环境变化自动重建。
//!
//! 查询执行复刻报告 §4.4 的核心结论：**紧凑全表线性扫描 + ASCII 大小写折叠快路径 +
//! 名字/大小/日期内联**，不做倒排索引。

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock, RwLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// 单次搜索结果上限（与 tasks::MAX_SEARCH_RESULTS 一致）
pub const MAX_RESULTS: usize = 5000;

// ============================================================
// 后端探测
// ============================================================

#[derive(PartialEq, Eq, Clone, Copy, Debug)]
pub enum Backend {
    /// 自建全盘索引已就绪
    OwnIndex,
    /// 索引未就绪 → 旧递归搜索
    Legacy,
}

static INDEX_READY: AtomicBool = AtomicBool::new(false);
static INDEX_PENDING: AtomicUsize = AtomicUsize::new(0);

pub fn index_ready() -> bool {
    INDEX_READY.load(Ordering::SeqCst)
}

pub fn detect_backend() -> Backend {
    if index_ready() {
        Backend::OwnIndex
    } else {
        Backend::Legacy
    }
}

// ============================================================
// 搜索结果
// ============================================================

#[derive(Debug, Clone)]
pub struct Hit {
    pub name: String,
    /// 完整路径（含文件名）
    pub path: String,
    pub dir: bool,
    /// 自建索引有值；IPC 结果由调用方补 stat（0 = 未知）
    pub size: u64,
    /// unix 秒；0 = 未知
    pub mtime: i64,
}

// ============================================================
// 查询语法（Everything 子集，报告 §4.2）
// ============================================================

const CMP_LT: u8 = 0;
const CMP_LE: u8 = 1;
const CMP_GT: u8 = 2;
const CMP_GE: u8 = 3;
const CMP_EQ: u8 = 4;

enum SizeSpec {
    /// (op, 字节值)
    Cmp(u8, u64),
    /// [min, max] 闭区间（常量档位，边界同报告 §4.2.5）
    Band(u64, u64),
}

enum DateSpec {
    /// [lo, hi) unix 秒（比较前缀折算为 Band）
    Band(i64, i64),
}

enum TermKind {
    /// 文件名匹配（已折叠；wild = 含通配符按整名 fnmatch）
    Name { pat: Vec<char>, wild: bool },
    /// 路径匹配（文件=父目录路径；pat 含 '\\' 时补全路径再试）
    Path { pat: Vec<char>, wild: bool, spans: bool },
    Exts(Vec<Vec<char>>),
    Size(SizeSpec),
    Date(DateSpec),
    /// true = 只文件，false = 只目录
    Scope(bool),
}

struct Term {
    neg: bool,
    kind: TermKind,
}

/// 解析结果：OR 备选组 × AND 词列表（`a|b c` → [[a],[b,c]]）
pub struct Query {
    alts: Vec<Vec<Term>>,
    /// 是否含 path: 词（扫描时才取父目录路径）
    has_path: bool,
}

fn fold_into(s: &str, out: &mut Vec<char>) {
    for c in s.chars() {
        if c.is_ascii() {
            // ASCII 快路径（报告 §4.4：fast_ascii_search 的意义所在）
            out.push(c.to_ascii_lowercase());
        } else {
            out.extend(c.to_lowercase());
        }
    }
}

fn fold_str(s: &str) -> Vec<char> {
    let mut b = Vec::new();
    fold_into(s, &mut b);
    b
}

fn has_wild(s: &str) -> bool {
    s.contains('*') || s.contains('?')
}

fn parse_size_val(v: &str) -> Option<u64> {
    let num: &str = v.split(|c: char| !(c.is_ascii_digit() || c == '.')).next()?;
    if num.is_empty() {
        return None;
    }
    let unit = &v[num.len()..];
    let n: f64 = num.parse().ok()?;
    let mul: u64 = match unit.trim().to_ascii_lowercase().as_str() {
        "" | "b" => 1,
        "kb" => 1024,
        "mb" => 1024 * 1024,
        "gb" => 1024 * 1024 * 1024,
        _ => return None,
    };
    Some((n * mul as f64) as u64)
}

fn split_op(v: &str) -> (u8, &str) {
    for (p, op) in [(">=", CMP_GE), ("<=", CMP_LE), (">", CMP_GT), ("<", CMP_LT), ("=", CMP_EQ)] {
        if let Some(rest) = v.strip_prefix(p) {
            return (op, rest);
        }
    }
    (CMP_EQ, v)
}

fn parse_size(v: &str) -> Option<SizeSpec> {
    let lv = v.to_ascii_lowercase();
    match lv.as_str() {
        "empty" => return Some(SizeSpec::Cmp(CMP_EQ, 0)),
        "tiny" => return Some(SizeSpec::Band(1, 10 * 1024)),
        "small" => return Some(SizeSpec::Band(10 * 1024, 100 * 1024)),
        "medium" => return Some(SizeSpec::Band(100 * 1024, 1024 * 1024)),
        "large" => return Some(SizeSpec::Band(1024 * 1024, 16 * 1024 * 1024)),
        "huge" => return Some(SizeSpec::Band(16 * 1024 * 1024, 128 * 1024 * 1024)),
        "gigantic" => return Some(SizeSpec::Cmp(CMP_GT, 128 * 1024 * 1024)),
        _ => {}
    }
    let (op, rest) = split_op(&lv);
    let val = parse_size_val(rest)?;
    Some(SizeSpec::Cmp(op, val))
}

use chrono::{Datelike, Duration as ChronoDur, Local, NaiveDate};

fn date_ts(d: NaiveDate) -> i64 {
    d.and_hms_opt(0, 0, 0)
        .unwrap()
        .and_local_timezone(Local)
        .single()
        .map(|t| t.timestamp())
        .unwrap_or(0)
}

fn parse_date(v: &str) -> Option<DateSpec> {
    let lv = v.to_ascii_lowercase();
    let now = Local::now().date_naive();
    let band: Option<(NaiveDate, NaiveDate)> = match lv.as_str() {
        "today" => Some((now, now.succ_opt().unwrap_or(now))),
        "yesterday" => Some((now.pred_opt().unwrap_or(now), now)),
        "thisweek" => {
            let mon = now - ChronoDur::days(now.weekday().num_days_from_monday() as i64);
            Some((mon, mon + ChronoDur::days(7)))
        }
        "thismonth" => {
            let first = NaiveDate::from_ymd_opt(now.year(), now.month(), 1).unwrap();
            let next = if now.month() == 12 {
                NaiveDate::from_ymd_opt(now.year() + 1, 1, 1).unwrap()
            } else {
                NaiveDate::from_ymd_opt(now.year(), now.month() + 1, 1).unwrap()
            };
            Some((first, next))
        }
        "thisyear" => Some((
            NaiveDate::from_ymd_opt(now.year(), 1, 1).unwrap(),
            NaiveDate::from_ymd_opt(now.year() + 1, 1, 1).unwrap(),
        )),
        _ => None,
    };
    if let Some((lo, hi)) = band {
        return Some(DateSpec::Band(date_ts(lo), date_ts(hi)));
    }
    // 显式日期：YYYY | YYYY-MM | YYYY-MM-DD（可带比较前缀）
    let (op, rest) = split_op(&lv);
    let parts: Vec<u32> = rest.split('-').map(|p| p.parse().unwrap_or(0)).collect();
    if parts.is_empty() || parts.contains(&0) || parts.len() > 3 {
        return None;
    }
    let y = parts[0] as i32;
    if !(1000..=9999).contains(&y) {
        return None;
    }
    let (lo, hi) = if parts.len() == 1 {
        (
            NaiveDate::from_ymd_opt(y, 1, 1).unwrap(),
            NaiveDate::from_ymd_opt(y + 1, 1, 1).unwrap(),
        )
    } else {
        let m = parts[1];
        if !(1..=12).contains(&m) {
            return None;
        }
        if parts.len() == 2 {
            let next = if m == 12 {
                NaiveDate::from_ymd_opt(y + 1, 1, 1).unwrap()
            } else {
                NaiveDate::from_ymd_opt(y, m + 1, 1).unwrap()
            };
            (NaiveDate::from_ymd_opt(y, m, 1).unwrap(), next)
        } else {
            let base = NaiveDate::from_ymd_opt(y, m, parts[2])?;
            (base, base.succ_opt().unwrap_or(base))
        }
    };
    let (lo, hi) = (date_ts(lo), date_ts(hi));
    Some(match op {
        CMP_LT => DateSpec::Band(0, lo),
        CMP_LE => DateSpec::Band(0, hi),
        CMP_GT => DateSpec::Band(hi, i64::MAX),
        CMP_GE => DateSpec::Band(lo, i64::MAX),
        _ => DateSpec::Band(lo, hi),
    })
}

/// 解析单个 token；无法识别的函数形 token 回退为字面量（Everything 同款语义）
fn parse_token(tok: &str) -> Option<Term> {
    let (neg, rest) = match tok.strip_prefix('!') {
        Some(r) => (true, r),
        None => (false, tok),
    };
    if rest.is_empty() {
        return None;
    }
    // 无副作用修饰符：剥离后剩余部分继续按词处理（报告 §4.2.3 修饰符子集）
    let mut rest2 = rest;
    for p in [
        "case:",
        "nocase:",
        "ascii:",
        "utf8:",
        "wholeword:",
        "wholewords:",
        "ww:",
        "noww:",
        "wholefilename:",
        "wfn:",
    ] {
        if let Some(r) = rest2.strip_prefix(p) {
            rest2 = r;
        }
    }
    if rest2.is_empty() {
        return None;
    }
    let kind = if let Some(v) = rest2.strip_prefix("ext:") {
        let exts: Vec<Vec<char>> = v
            .split([';', ','])
            .filter(|s| !s.is_empty())
            .map(fold_str)
            .collect();
        if exts.is_empty() {
            return None;
        }
        TermKind::Exts(exts)
    } else if let Some(v) = rest2.strip_prefix("size:") {
        match parse_size(v) {
            Some(s) => TermKind::Size(s),
            // 非法值回退字面量
            None => TermKind::Name { pat: fold_str(rest2), wild: has_wild(rest2) },
        }
    } else if let Some(v) = rest2
        .strip_prefix("dm:")
        .or_else(|| rest2.strip_prefix("datemodified:"))
    {
        match parse_date(v) {
            Some(d) => TermKind::Date(d),
            None => TermKind::Name { pat: fold_str(rest2), wild: has_wild(rest2) },
        }
    } else if rest2 == "file:" || rest2 == "files:" {
        TermKind::Scope(true)
    } else if rest2 == "folder:" || rest2 == "folders:" {
        TermKind::Scope(false)
    } else if let Some(v) = rest2.strip_prefix("path:") {
        TermKind::Path { wild: has_wild(v), spans: v.contains('\\'), pat: fold_str(v) }
    } else if let Some(v) = rest2.strip_prefix("startwith:") {
        let mut pat = fold_str(v);
        pat.push('*');
        TermKind::Name { wild: true, pat }
    } else if let Some(v) = rest2.strip_prefix("endwith:") {
        let mut pat = vec!['*'];
        pat.extend(fold_str(v));
        TermKind::Name { wild: true, pat }
    } else {
        TermKind::Name { pat: fold_str(rest2), wild: has_wild(rest2) }
    };
    Some(Term { neg, kind })
}

/// 解析查询：顶层 `|` 切 OR，空白切 AND
pub fn parse_query(q: &str) -> Query {
    let mut alts = Vec::new();
    let mut has_path = false;
    for alt in q.split('|') {
        let mut terms = Vec::new();
        for tok in alt.split_whitespace() {
            if let Some(t) = parse_token(tok) {
                if matches!(t.kind, TermKind::Path { .. }) {
                    has_path = true;
                }
                terms.push(t);
            }
        }
        if !terms.is_empty() {
            alts.push(terms);
        }
    }
    Query { alts, has_path }
}

// ---------- 匹配原语 ----------

/// 子串包含（两序列均为折叠后小写）；空词恒匹配
fn contains(hay: &[char], needle: &[char]) -> bool {
    if needle.is_empty() {
        return true;
    }
    if hay.len() < needle.len() {
        return false;
    }
    hay.windows(needle.len()).any(|w| w == needle)
}

/// 通配符匹配：`*` 任意长、`?` 单字符（经典双指针回溯）
fn wild_match(h: &[char], p: &[char]) -> bool {
    let (mut hi, mut pi) = (0usize, 0usize);
    let (mut star, mut mark): (usize, usize) = (usize::MAX, 0);
    while hi < h.len() {
        if pi < p.len() && (p[pi] == '?' || p[pi] == h[hi]) {
            pi += 1;
            hi += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star = pi;
            pi += 1;
            mark = hi;
        } else if star != usize::MAX {
            pi = star + 1;
            mark += 1;
            hi = mark;
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == '*' {
        pi += 1;
    }
    pi == p.len()
}

fn match_pat(hay: &[char], pat: &[char], wild: bool) -> bool {
    if wild {
        wild_match(hay, pat)
    } else {
        contains(hay, pat)
    }
}

fn size_match(spec: &SizeSpec, size: u64) -> bool {
    match spec {
        SizeSpec::Cmp(op, v) => match *op {
            CMP_LT => size < *v,
            CMP_LE => size <= *v,
            CMP_GT => size > *v,
            CMP_GE => size >= *v,
            _ => size == *v,
        },
        SizeSpec::Band(lo, hi) => size >= *lo && size <= *hi,
    }
}

fn date_match(spec: &DateSpec, mtime: i64) -> bool {
    match spec {
        DateSpec::Band(lo, hi) => mtime >= *lo && mtime < *hi,
    }
}

/// 从折叠名中取扩展名（不含 '.'；".gitignore" 这类点开头名返回空串切片语义见调用处）
fn ext_of_folded(name_f: &[char]) -> Option<&[char]> {
    let dot = name_f.iter().rposition(|&c| c == '.')?;
    Some(&name_f[dot + 1..])
}

impl Query {
    /// 评价一条索引记录。name_f：折叠后的名字；
    /// path_f：path: 词使用的路径（文件=父目录路径，目录=自身全路径），已折叠。
    fn eval(&self, name_f: &[char], path_f: Option<&[char]>, dir: bool, size: u64, mtime: i64) -> bool {
        'alts: for alt in &self.alts {
            for t in alt {
                let m = match &t.kind {
                    TermKind::Name { pat, wild } => match_pat(name_f, pat, *wild),
                    TermKind::Exts(exts) => exts
                        .iter()
                        .any(|e| ext_of_folded(name_f) == Some(e.as_slice())),
                    TermKind::Size(s) => size_match(s, size),
                    TermKind::Date(d) => date_match(d, mtime),
                    TermKind::Scope(file_only) => *file_only != dir,
                    TermKind::Path { pat, wild, spans } => {
                        let Some(pf) = path_f else { break 'alts };
                        let mut hit = match_pat(pf, pat, *wild);
                        if !hit && *spans {
                            // 词横跨目录名与文件名：补全路径再试
                            let mut full = pf.to_vec();
                            full.push('\\');
                            full.extend_from_slice(name_f);
                            hit = match_pat(&full, pat, *wild);
                        }
                        hit
                    }
                };
                if t.neg == m {
                    continue 'alts;
                }
            }
            return true;
        }
        false
    }
}

// ============================================================
// 自建全盘索引
// ============================================================

struct Entry {
    name: Box<str>,
    /// 父条目下标；卷根 = u32::MAX
    parent: u32,
    /// 同父兄弟链（增删 O(子项数)）
    sibling: u32,
    dir: bool,
    dead: bool,
    size: u64,
    /// unix 秒
    mtime: i64,
    /// File Reference Number 低 48 位（仅目录填值；0=未知）。
    /// USN 差量回放用 frn_dir（目录 FRN→下标）定位父目录；删除时联动清理。
    frn: u64,
}

struct Inner {
    entries: Vec<Entry>,
    /// 目录 idx → 首个子条目 idx
    first_child: HashMap<u32, u32>,
    /// 目录 idx → 全路径（盘根带尾部 `\`，其余不带）
    dir_paths: HashMap<u32, Arc<str>>,
    /// 全路径 → 目录 idx（增量事件定位）
    path_idx: HashMap<Arc<str>, u32>,
    /// 目录 FRN → 条目 idx（USN 差量回放定位父目录）
    frn_dir: HashMap<u64, u32>,
}

const NO_IDX: u32 = u32::MAX;

static INDEX: OnceLock<RwLock<Inner>> = OnceLock::new();

fn index_rw() -> &'static RwLock<Inner> {
    INDEX.get_or_init(|| {
        RwLock::new(Inner {
            entries: Vec::new(),
            first_child: HashMap::new(),
            dir_paths: HashMap::new(),
            path_idx: HashMap::new(),
            frn_dir: HashMap::new(),
        })
    })
}

fn join_path(parent: &str, name: &str) -> String {
    if parent.ends_with('\\') {
        format!("{parent}{name}")
    } else {
        format!("{parent}\\{name}")
    }
}

/// "D:\a\b" → "D:\a"；"D:\x" → "D:\"（盘根特判）
fn parent_of(full: &str) -> Option<&str> {
    let pos = full.rfind('\\')?;
    if pos == 2 && full.len() > 2 && full.as_bytes()[1] == b':' {
        Some(&full[..3])
    } else {
        Some(&full[..pos])
    }
}

fn base_name(full: &str) -> &str {
    match full.rfind('\\') {
        Some(p) => &full[p + 1..],
        None => full,
    }
}

fn mtime_secs(md: &std::fs::Metadata) -> i64 {
    md.modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// 插入条目（头插兄弟链）。idx 由 entries 长度决定。
/// 目录且 frn≠0 时同步登记 frn_dir（USN 差量父定位）。
fn push_entry(inner: &mut Inner, parent: u32, name: String, dir: bool, size: u64, mtime: i64, frn: u64) -> u32 {
    let idx = inner.entries.len() as u32;
    let sib = inner.first_child.get(&parent).copied().unwrap_or(NO_IDX);
    inner.first_child.insert(parent, idx);
    inner.entries.push(Entry {
        name: name.into_boxed_str(),
        parent,
        sibling: sib,
        dir,
        dead: false,
        size,
        mtime,
        frn,
    });
    if dir && frn != 0 {
        inner.frn_dir.insert(frn, idx);
    }
    idx
}

/// 登记一个根（盘根或自定义目录）。返回根条目 idx。
fn add_root(root_display: &str) -> u32 {
    let mut inner = index_rw().write().unwrap();
    let idx = push_entry(&mut inner, NO_IDX, root_display.to_string(), true, 0, 0, 0);
    let arc: Arc<str> = Arc::from(root_display);
    inner.dir_paths.insert(idx, Arc::clone(&arc));
    inner.path_idx.insert(arc, idx);
    drop(inner);
    roots_list().lock().unwrap().push((idx, root_display.to_string()));
    idx
}

/// 深度枚举插入 parent 下整棵子树（显式栈，不递归；跳过 junction/symlink）
fn walk_insert(parent_idx: u32, parent_path: &str) {
    const MAX_DEPTH: usize = 256;
    let mut stack: Vec<(u32, String, usize)> = vec![(parent_idx, parent_path.to_string(), 0)];
    while let Some((pidx, ppath, depth)) = stack.pop() {
        let mut batch: Vec<(String, bool, u64, i64, String)> = Vec::new();
        if let Ok(rd) = std::fs::read_dir(&ppath) {
            for e in rd.flatten() {
                // junction/符号链接不跟进（自引用环铁律）
                if e.file_type().is_ok_and(|ft| ft.is_symlink()) {
                    continue;
                }
                let name = e.file_name().to_string_lossy().into_owned();
                let Ok(md) = e.metadata() else { continue };
                let dir = md.is_dir();
                let size = if dir { 0 } else { md.len() };
                let mtime = mtime_secs(&md);
                let full = join_path(&ppath, &name);
                batch.push((name, dir, size, mtime, full));
            }
        }
        let mut next_stack: Vec<(u32, String, usize)> = Vec::new();
        {
            let mut inner = index_rw().write().unwrap();
            for (name, dir, size, mtime, full) in batch {
                let idx = push_entry(&mut inner, pidx, name, dir, size, mtime, 0);
                if dir {
                    let arc: Arc<str> = Arc::from(full.as_str());
                    inner.dir_paths.insert(idx, Arc::clone(&arc));
                    inner.path_idx.insert(arc, idx);
                    if depth + 1 < MAX_DEPTH {
                        next_stack.push((idx, full, depth + 1));
                    }
                }
            }
        }
        stack.extend(next_stack);
    }
}

/// 清空某目录条目的整个子树
fn remove_subtree(inner: &mut Inner, dir_idx: u32) {
    let mut stack = vec![dir_idx];
    while let Some(d) = stack.pop() {
        let mut cur = inner.first_child.get(&d).copied().unwrap_or(NO_IDX);
        while cur != NO_IDX {
            let e = &mut inner.entries[cur as usize];
            let (is_d, sib) = (e.dir, e.sibling);
            e.dead = true;
            e.sibling = NO_IDX;
            e.name = "".into();
            if is_d {
                let frn = inner.entries[cur as usize].frn;
                if frn != 0 {
                    inner.frn_dir.remove(&frn);
                }
                if let Some(p) = inner.dir_paths.remove(&(cur)) {
                    inner.path_idx.remove(&p);
                }
                stack.push(cur);
            }
            cur = sib;
        }
        inner.first_child.remove(&d);
    }
}

/// 从 parent 下摘除名为 name 的子项（目录连子树）。返回被摘除的 idx。
fn unlink_child(inner: &mut Inner, parent: u32, name: &str) -> Option<u32> {
    let mut prev = NO_IDX;
    let mut cur = inner.first_child.get(&parent).copied().unwrap_or(NO_IDX);
    let mut found = NO_IDX;
    while cur != NO_IDX {
        let e = &inner.entries[cur as usize];
        if !e.dead && e.name.as_ref() == name {
            found = cur;
            break;
        }
        prev = cur;
        cur = e.sibling;
    }
    if found == NO_IDX {
        return None;
    }
    let sib = inner.entries[found as usize].sibling;
    if prev == NO_IDX {
        if sib == NO_IDX {
            inner.first_child.remove(&parent);
        } else {
            inner.first_child.insert(parent, sib);
        }
    } else {
        inner.entries[prev as usize].sibling = sib;
    }
    let is_dir = inner.entries[found as usize].dir;
    // 目录从 frn_dir 摘除（USN 差量父定位映射），防 FRN 复用后悬空命中
    let removed_frn = inner.entries[found as usize].frn;
    if removed_frn != 0 {
        inner.frn_dir.remove(&removed_frn);
    }
    let e = &mut inner.entries[found as usize];
    e.dead = true;
    e.sibling = NO_IDX;
    e.name = "".into();
    if is_dir {
        if let Some(p) = inner.dir_paths.remove(&found) {
            inner.path_idx.remove(&p);
        }
        remove_subtree(inner, found);
    }
    Some(found)
}

/// 按名字找直接子项（不动死亡标记）
fn find_child(inner: &Inner, parent: u32, name: &str) -> Option<u32> {
    let mut cur = inner.first_child.get(&parent).copied().unwrap_or(NO_IDX);
    while cur != NO_IDX {
        let e = &inner.entries[cur as usize];
        if !e.dead && e.name.as_ref() == name {
            return Some(cur);
        }
        cur = e.sibling;
    }
    None
}

/// 增量事件应用（ReadDirectoryChangesW 事件分派，对应报告 §3.4 的 reason 分派）
/// root_display：根显示路径；rel：事件相对路径（可含中间目录）。
/// action：1=ADDED 2=REMOVED 3=MODIFIED 4=RENAMED_OLD 5=RENAMED_NEW
/// （RENAME 从简：OLD 删 + NEW 增；目录改名即子树重枚举）
fn apply_event(root_display: &str, rel: &str, action: u32) {
    let full = join_path(root_display, rel);
    let Some(parent_path) = parent_of(&full) else { return };
    let name = base_name(&full).to_string();
    match action {
        1 | 5 => {
            // ADDED / RENAMED_NEW：锁外取属性
            let wide: Vec<u16> = full.encode_utf16().chain(std::iter::once(0)).collect();
            let attrs = unsafe { win::GetFileAttributesW(wide.as_ptr()) };
            if attrs == win::INVALID_FILE_ATTRIBUTES {
                return;
            }
            let dir = attrs & win::FILE_ATTRIBUTE_DIRECTORY != 0;
            let (size, mtime) = if dir {
                (0, now_secs())
            } else {
                match std::fs::metadata(&full) {
                    Ok(md) => (md.len(), mtime_secs(&md)),
                    Err(_) => return,
                }
            };
            let mut inner = index_rw().write().unwrap();
            let Some(&pidx) = inner.path_idx.get(parent_path) else { return };
            if dir {
                // frn=0：RDC 事件拿不到记录号；该目录的 CREATE 记录会在下次
                // 启动 USN 差量中回放补齐（见 docs/08 §3.1）
                let idx = push_entry(&mut inner, pidx, name, true, 0, mtime, 0);
                let arc: Arc<str> = Arc::from(full.as_str());
                inner.dir_paths.insert(idx, Arc::clone(&arc));
                inner.path_idx.insert(arc, idx);
                drop(inner);
                // 新目录可能自带内容，枚举其子树
                walk_insert(idx, &full);
            } else {
                push_entry(&mut inner, pidx, name, false, size, mtime, 0);
            }
        }
        2 | 4 => {
            // REMOVED / RENAMED_OLD：摘除
            let mut inner = index_rw().write().unwrap();
            if let Some(&pidx) = inner.path_idx.get(parent_path) {
                unlink_child(&mut inner, pidx, &name);
            }
        }
        3 => {
            // MODIFIED（SIZE 变化）：补 stat
            let idx = {
                let inner = index_rw().read().unwrap();
                let Some(&pidx) = inner.path_idx.get(parent_path) else { return };
                match find_child(&inner, pidx, &name) {
                    Some(i) if !inner.entries[i as usize].dir => i,
                    _ => return,
                }
            };
            if let Ok(md) = std::fs::metadata(&full) {
                let mut inner = index_rw().write().unwrap();
                let e = &mut inner.entries[idx as usize];
                if !e.dead {
                    e.size = md.len();
                    e.mtime = mtime_secs(&md);
                }
            }
        }
        _ => {}
    }
}

/// 全量重扫某根（缓冲溢出 / 句柄失效后的恢复，对应报告 folder_rescan_if_full）。
/// 十九轮起升级为与差量失效重建共用实现：盘根 MFT 优先（保留 frn，差量链不断），
/// 枚举回退时差量停用。
fn rescan_volume(root_idx: u32, root_display: &str) {
    full_rescan_root(root_idx, root_display);
}

// ---------- 索引落盘（程序同级目录 wfm_index.bin） ----------

const IDX_MAGIC: &[u8; 8] = b"WFMIDX2\x00";
/// 条目数硬上限（防坏文件爆内存）
const IDX_MAX_ENTRIES: u32 = 64_000_000;

static ROOTS: OnceLock<Mutex<Vec<(u32, String)>>> = OnceLock::new();
static LAST_SAVE: AtomicI64 = AtomicI64::new(0);
static SAVE_LOCK: OnceLock<Mutex<()>> = OnceLock::new();
/// 各根 USN 差量进度（root_idx → (last_usn, journal_id)；(0,0) = 不参与差量）。
/// 快照落盘时逐根写入，启动加载后用于差量回放起点。
static USN_STATE: OnceLock<Mutex<HashMap<u32, (u64, u64)>>> = OnceLock::new();

fn usn_state() -> &'static Mutex<HashMap<u32, (u64, u64)>> {
    USN_STATE.get_or_init(|| Mutex::new(HashMap::new()))
}

fn roots_list() -> &'static Mutex<Vec<(u32, String)>> {
    ROOTS.get_or_init(|| Mutex::new(Vec::new()))
}

fn index_file_path() -> Option<std::path::PathBuf> {
    let exe = std::env::current_exe().ok()?;
    Some(exe.parent()?.join("wfm_index.bin"))
}

/// 诊断日志落盘（exe 同级 wfm_index.log，>4MB 清空重写）。
/// explorer 启动时 stderr 不可见，索引构建/中断必须留档才能事后诊断。
pub fn flog_write(msg: &str) {
    eprintln!("{msg}");
    let Some(p) = index_file_path() else { return };
    let log = p.with_extension("log");
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    if let Ok(md) = std::fs::metadata(&log)
        && md.len() > 4 * 1024 * 1024
    {
        let _ = std::fs::remove_file(&log);
    }
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&log) {
        use std::io::Write as _;
        let _ = writeln!(f, "[{ts}] {msg}");
    }
}

/// 诊断日志宏：stderr + exe 同级 wfm_index.log
#[macro_export]
macro_rules! flog {
    ($($arg:tt)*) => {
        $crate::instant::flog_write(&format!($($arg)*))
    };
}

fn diagnostic_env() -> bool {
    std::env::var("WFM_INDEX_DRIVES")
        .map(|v| !v.trim().is_empty())
        .unwrap_or(false)
}

/// 全量落盘（根列表来自 ROOTS 登记）。诊断 env 模式跳过（沙箱树不覆盖真实索引）。
pub fn save_index() {
    if diagnostic_env() {
        return;
    }
    let Some(path) = index_file_path() else { return };
    save_index_to(&path);
}

/// 监控重扫后的限流落盘（≥60s 才写一次）。
/// 门禁：全量索引未就绪时禁止落盘——监控在构建中途重扫（如易变卷句柄立即失效）
/// 会把只有几个条目的半成品快照写盘，下次启动反被加载。
fn save_index_rate_limited() {
    if !INDEX_READY.load(Ordering::SeqCst) {
        return;
    }
    let now = now_secs();
    if now.saturating_sub(LAST_SAVE.load(Ordering::SeqCst)) < 60 {
        return;
    }
    save_index();
}

/// 快照写盘：活跃条目重映射下标后按 (parent, dir, size, mtime, name) 顺序编码。
/// 根列表按根条目下标排序，保证与保存序（entries 顺序）一致。
fn write_snapshot(path: &std::path::Path, roots: &[(u32, String)]) -> std::io::Result<()> {
    let inner = index_rw().read().unwrap();
    let mut map: HashMap<u32, u32> = HashMap::with_capacity(inner.entries.len());
    let mut live: Vec<&Entry> = Vec::new();
    for (i, e) in inner.entries.iter().enumerate() {
        if !e.dead {
            map.insert(i as u32, live.len() as u32);
            live.push(e);
        }
    }
    let mut roots: Vec<(u32, String)> = roots.to_vec();
    roots.sort_by_key(|(i, _)| *i);
    // 落盘守卫：盘根顶层为空 = 枚举异常（真 NTFS 盘至少有 System Volume
    // Information / $Recycle.Bin）。拒绝写入，保留旧的好快照。
    for (ridx, rp) in &roots {
        if is_drive_root(rp) {
            // 注意：write_snapshot 已持读锁，此处必须用 inner 内联计数，
            // 不能调 root_child_count（二次取读锁遇排队写者会死锁）
            let mut cur = inner.first_child.get(ridx).copied().unwrap_or(NO_IDX);
            let mut n = 0u32;
            while cur != NO_IDX {
                if !inner.entries[cur as usize].dead {
                    n += 1;
                }
                cur = inner.entries[cur as usize].sibling;
            }
            if n == 0 {
                crate::flog!("[秒搜] 落盘守卫：{rp} 顶层为空（枚举异常），本次跳过落盘");
                return Err(std::io::Error::other("drive root empty"));
            }
        }
    }
    let tmp = path.with_extension("bin.tmp");
    let mut w = std::io::BufWriter::new(std::fs::File::create(&tmp)?);
    use std::io::Write;
    w.write_all(IDX_MAGIC)?;
    w.write_all(&(roots.len() as u32).to_le_bytes())?;
    let usn_map = usn_state().lock().unwrap().clone();
    for (ridx, rp) in &roots {
        // 一致性自检：根条目名必须等于根路径
        debug_assert_eq!(live[map[ridx] as usize].name.as_ref(), rp.as_str());
        let b = rp.as_bytes();
        w.write_all(&(b.len() as u16).to_le_bytes())?;
        w.write_all(b)?;
        w.write_all(&crate::mft::volume_serial(rp).unwrap_or(0).to_le_bytes())?;
        // USN 差量进度（(0,0) = 该根不参与差量）
        let (lu, jid) = usn_map.get(ridx).copied().unwrap_or((0, 0));
        w.write_all(&lu.to_le_bytes())?;
        w.write_all(&jid.to_le_bytes())?;
    }
    w.write_all(&(live.len() as u32).to_le_bytes())?;
    for e in &live {
        let p = if e.parent == NO_IDX { NO_IDX } else { map[&e.parent] };
        w.write_all(&p.to_le_bytes())?;
        w.write_all(&[e.dir as u8])?;
        w.write_all(&e.size.to_le_bytes())?;
        w.write_all(&e.mtime.to_le_bytes())?;
        let nb = e.name.as_bytes();
        w.write_all(&(nb.len() as u16).to_le_bytes())?;
        w.write_all(nb)?;
        // 目录条目追加 frn（文件条目格式不变）
        if e.dir {
            w.write_all(&e.frn.to_le_bytes())?;
        }
    }
    w.flush()?;
    drop(w);
    drop(inner);
    // 防线：旧快照先转 .bak 再原子替换——坏快照（如 MFT 中断的半成品）不再
    // 永久毁掉上一份好快照（2026-11-16 事故：150MB 好快照被 16MB 半成品覆盖）
    let bak = path.with_extension("bak");
    if path.exists() {
        let _ = std::fs::rename(path, &bak);
    }
    // 原子替换（rename 带 MOVEFILE_REPLACE_EXISTING）
    std::fs::rename(&tmp, path)
}

fn save_index_to(path: &std::path::Path) {
    let _g = SAVE_LOCK.get_or_init(|| Mutex::new(())).lock().unwrap();
    let roots = roots_list().lock().unwrap().clone();
    if roots.is_empty() {
        return;
    }
    match write_snapshot(path, &roots) {
        Ok(()) => {
            LAST_SAVE.store(now_secs(), Ordering::SeqCst);
            crate::flog!("[秒搜] 索引已落盘 {}（{} 根）", path.display(), roots.len());
        }
        Err(e) => crate::flog!("[秒搜] 索引落盘失败：{e}"),
    }
}
/// 快照加载结果：索引 + (根下标, 根路径) 列表（保存序）+ 各根 USN 差量进度
type LoadResult = (Inner, Vec<(u32, String)>, HashMap<u32, (u64, u64)>);

/// 从落盘文件重建 Inner（含根路径与卷序列号校验；不匹配返回 None → 触发重建）。
fn load_inner_from(path: &std::path::Path, expected_roots: &[String]) -> Option<LoadResult> {
    let data = std::fs::read(path).ok()?;
    if data.len() < 12 || &data[..8] != IDX_MAGIC {
        return None;
    }
    let rd32 = |o: usize| -> Option<u32> {
        if o + 4 <= data.len() {
            Some(u32::from_le_bytes(data[o..o + 4].try_into().unwrap()))
        } else {
            None
        }
    };
    let rd64 = |o: usize| -> Option<u64> {
        if o + 8 <= data.len() {
            Some(u64::from_le_bytes(data[o..o + 8].try_into().unwrap()))
        } else {
            None
        }
    };
    let mut pos = 8usize;
    let root_cnt = rd32(pos)? as usize;
    pos += 4;
    if root_cnt != expected_roots.len() || root_cnt > 1024 {
        return None;
    }
    let mut saved_roots: Vec<(String, u32)> = Vec::with_capacity(root_cnt);
    let mut saved_usn: Vec<(u64, u64)> = Vec::with_capacity(root_cnt);
    // 期望根集合（保存序 = 根下标序，与 resolve_roots 的盘符序无关，必须按路径匹配）
    let expected_set: std::collections::HashSet<&str> =
        expected_roots.iter().map(|s| s.as_str()).collect();
    for _ in 0..root_cnt {
        if pos + 2 > data.len() {
            return None;
        }
        let nlen = u16::from_le_bytes(data[pos..pos + 2].try_into().ok()?) as usize;
        pos += 2;
        if pos + nlen + 4 + 16 > data.len() {
            return None;
        }
        let p = std::str::from_utf8(&data[pos..pos + nlen]).ok()?.to_string();
        pos += nlen;
        let serial = rd32(pos)?;
        pos += 4;
        // 新鲜度：根路径必须一致且卷序列号相同（文件动了位置/盘变了 → 重建）
        if !expected_set.contains(p.as_str()) {
            return None;
        }
        if crate::mft::volume_serial(&p).unwrap_or(0) != serial {
            return None;
        }
        // USN 差量进度（v3 新增）
        let lu = rd64(pos)?;
        pos += 8;
        let jid = rd64(pos)?;
        pos += 8;
        saved_roots.push((p, serial));
        saved_usn.push((lu, jid));
    }
    let cnt = rd32(pos)?;
    pos += 4;
    if cnt > IDX_MAX_ENTRIES {
        return None;
    }
    let mut entries: Vec<Entry> = Vec::with_capacity(cnt as usize);
    let mut first_child: HashMap<u32, u32> = HashMap::new();
    let mut frn_dir: HashMap<u64, u32> = HashMap::new();
    let mut root_idxs: Vec<u32> = Vec::new();
    for _ in 0..cnt as usize {
        // 每条固定 21 字节头 + 变长名
        let head = pos + 4 + 1 + 8 + 8 + 2;
        if head > data.len() {
            return None;
        }
        let parent = rd32(pos)?;
        pos += 4;
        let dir = data[pos] != 0;
        pos += 1;
        let size = rd64(pos)?;
        pos += 8;
        let mtime = u64::from_le_bytes(data[pos..pos + 8].try_into().ok()?) as i64;
        pos += 8;
        let nlen = u16::from_le_bytes(data[pos..pos + 2].try_into().ok()?) as usize;
        pos += 2;
        if pos + nlen > data.len() {
            return None;
        }
        let name = std::str::from_utf8(&data[pos..pos + nlen]).ok()?.to_string();
        pos += nlen;
        // 目录条目追加 frn（v3 新增）
        let frn = if dir {
            let f = rd64(pos)?;
            pos += 8;
            f
        } else {
            0
        };
        if parent != NO_IDX && parent >= cnt {
            return None; // 父下标越界
        }
        let idx = entries.len() as u32;
        if parent == NO_IDX {
            root_idxs.push(idx);
        }
        let sib = first_child.get(&parent).copied().unwrap_or(NO_IDX);
        first_child.insert(parent, idx);
        entries.push(Entry {
            name: name.into_boxed_str(),
            parent,
            sibling: sib,
            dir,
            dead: false,
            size,
            mtime,
            frn,
        });
        if dir && frn != 0 {
            frn_dir.insert(frn, idx);
        }
    }
    if root_idxs.len() != root_cnt {
        return None;
    }
    // 根条目名必须与保存的根路径一致（路径本身已通过 expected_set 校验）
    for (&ridx, (rp, _)) in root_idxs.iter().zip(saved_roots.iter()) {
        if entries[ridx as usize].name.as_ref() != rp.as_str() {
            return None;
        }
    }
    // 健全性抽查：对每个盘根顶层现枚举（~毫秒级），探测到若干条目而索引根下
    // 为空 → 快照陈旧/损坏（曾出现过 6 条目坏快照通过校验被加载的实例）→ 拒绝
    for (&ridx, (rp, _)) in root_idxs.iter().zip(saved_roots.iter()) {
        if !is_drive_root(rp) {
            continue;
        }
        let probe = std::fs::read_dir(rp)
            .map(|rd| rd.flatten().count())
            .unwrap_or(0);
        if probe >= 4 {
            let mut cur = first_child.get(&ridx).copied().unwrap_or(NO_IDX);
            let mut n = 0;
            while cur != NO_IDX {
                if !entries[cur as usize].dead {
                    n += 1;
                }
                cur = entries[cur as usize].sibling;
            }
            if n == 0 {
                eprintln!(
                    "[秒搜] 快照抽查失败：{rp} 顶层实际 {probe} 项而索引为空 → 拒绝加载，触发重建"
                );
                return None;
            }
        }
    }
    // 目录路径重建（从每个根 BFS）
    let mut dir_paths: HashMap<u32, Arc<str>> = HashMap::new();
    let mut path_idx: HashMap<Arc<str>, u32> = HashMap::new();
    let mut queue: Vec<(u32, String)> = root_idxs
        .iter()
        .zip(saved_roots.iter())
        .map(|(&i, (p, _))| (i, p.clone()))
        .collect();
    while let Some((didx, dpath)) = queue.pop() {
        let arc: Arc<str> = Arc::from(dpath.as_str());
        dir_paths.insert(didx, Arc::clone(&arc));
        path_idx.insert(arc, didx);
        let mut cur = first_child.get(&didx).copied().unwrap_or(NO_IDX);
        while cur != NO_IDX {
            let (name, is_dir, sib) = {
                let e = &entries[cur as usize];
                (e.name.to_string(), e.dir, e.sibling)
            };
            if is_dir {
                let full = join_path(&dpath, &name);
                queue.push((cur, full));
            }
            cur = sib;
        }
    }
    let roots_reg: Vec<(u32, String)> = root_idxs
        .iter()
        .copied()
        .zip(saved_roots.into_iter().map(|(p, _)| p))
        .collect();
    let usn_map: HashMap<u32, (u64, u64)> = root_idxs
        .into_iter()
        .zip(saved_usn)
        .collect();
    Some((
        Inner {
            entries,
            first_child,
            dir_paths,
            path_idx,
            frn_dir,
        },
        roots_reg,
        usn_map,
    ))
}

/// 启动时尝试加载落盘索引；成功后为每根启动监控线程 + USN 差量线程
fn load_index(roots: &[String]) -> bool {
    let Some(path) = index_file_path() else { return false };
    let (inner, roots_reg, usn_map) = match load_inner_from(path.as_path(), roots) {
        Some(r) => r,
        None => {
            // 主快照缺失/校验失败 → 尝试上一份备份（.bak 防线）
            let bak = path.with_extension("bak");
            match load_inner_from(&bak, roots) {
                Some(r) => {
                    crate::flog!("[秒搜] 主快照不可用，已从备份 {} 恢复", bak.display());
                    r
                }
                None => return false,
            }
        }
    };
    let n = inner.entries.len();
    {
        let mut g = index_rw().write().unwrap();
        *g = inner;
    }
    let map: HashMap<String, u32> = roots_reg
        .iter()
        .map(|(i, p)| (p.clone(), *i))
        .collect();
    *roots_list().lock().unwrap() = roots_reg.clone();
    // USN 差量进度入库
    *usn_state().lock().unwrap() = usn_map;
    // 先启动监控（RDC 句柄尽早打开，覆盖「打开后」窗口；catch-up 等它就绪）
    let mut catchups: Vec<(u32, String, u64, u64)> = Vec::new();
    for (r, idx) in map {
        let r2 = r.clone();
        std::thread::spawn(move || monitor_loop(idx, &r2));
        let (lu, jid) = usn_state().lock().unwrap().get(&idx).copied().unwrap_or((0, 0));
        if lu > 0 && jid != 0 && is_drive_root(&r) {
            catchups.push((idx, r, lu, jid));
        }
    }
    for (idx, r, lu, jid) in catchups {
        std::thread::spawn(move || catchup_root(idx, &r, lu, jid));
    }
    INDEX_READY.store(true, Ordering::SeqCst);
    crate::flog!("[秒搜] 索引落盘加载完成：{n} 条目");
    true
}

// ---------- 卷发现与启动 ----------

fn wide0(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// 解析要索引的根列表：env（诊断钩子）> config > 全部固定盘
fn resolve_roots(cfg_drives: &[String]) -> Vec<String> {
    if let Ok(env) = std::env::var("WFM_INDEX_DRIVES") {
        let list: Vec<String> = env
            .split([',', ';'])
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();
        if !list.is_empty() {
            return list;
        }
    }
    if !cfg_drives.is_empty() {
        return cfg_drives.to_vec();
    }
    let mut out = Vec::new();
    let mask = unsafe { win::GetLogicalDrives() };
    for i in 0..26u32 {
        if mask & (1 << i) != 0 {
            let letter = (b'A' + i as u8) as char;
            let root = format!("{letter}:\\");
            let wide = wide0(&root);
            // DRIVE_FIXED = 3（windows-sys 0.59 未导出该常量，自补）
            if unsafe { win::GetDriveTypeW(wide.as_ptr()) } == 3 {
                out.push(root);
            }
        }
    }
    out
}

/// 启动：每根一个线程（构建 + 增量监控）。cfg_drives 来自 config.index_drives。
pub fn startup(cfg_drives: &[String]) {
    // 根显示名统一归一化：盘根带尾部反斜杠
    let roots: Vec<String> = resolve_roots(cfg_drives)
        .into_iter()
        .map(|r| {
            if r.len() == 2 && r.as_bytes()[1] == b':' {
                format!("{r}\\")
            } else {
                r
            }
        })
        .collect();
    if roots.is_empty() {
        INDEX_READY.store(true, Ordering::SeqCst);
        return;
    }
    INDEX_PENDING.store(roots.len(), Ordering::SeqCst);
    let env_override = diagnostic_env();
    std::thread::spawn(move || {
        // 优先加载落盘索引（卷序列号校验）；诊断 env 模式强制重建
        if !env_override {
            let t0 = Instant::now();
            if load_index(&roots) {
                crate::flog!("[秒搜] 索引加载耗时 {:?}", t0.elapsed());
                return;
            }
        }
        // 重建：每根一线程并行
        for root in roots {
            std::thread::spawn(move || {
                build_and_monitor(root);
            });
        }
    });
}

fn is_drive_root(display: &str) -> bool {
    display.len() == 3
        && display.as_bytes()[1] == b':'
        && display.as_bytes()[2] == b'\\'
}

/// 根条目的活跃子项数（落盘守卫 / 枚举异常检测共用）
fn root_child_count(ridx: u32) -> u32 {
    let inner = index_rw().read().unwrap();
    let mut cur = inner.first_child.get(&ridx).copied().unwrap_or(NO_IDX);
    let mut n = 0u32;
    while cur != NO_IDX {
        if !inner.entries[cur as usize].dead {
            n += 1;
        }
        cur = inner.entries[cur as usize].sibling;
    }
    n
}

/// MFT 直读结果装配进索引。孤儿条目（父记录缺失，如 $Extend 子项）自动剔除。
fn build_from_mft(root_idx: u32, display: &str, raws: Vec<crate::mft::RawEntry>) {
    let n = raws.len();
    let mut slot_of_rec: HashMap<u64, u32> = HashMap::with_capacity(n);
    for (s, r) in raws.iter().enumerate() {
        slot_of_rec.entry(r.rec).or_insert(s as u32);
    }
    // 可达性：从卷根（parent 引用 5）沿父子图 BFS
    let mut children: Vec<Vec<u32>> = vec![Vec::new(); n];
    let mut root_slots: Vec<u32> = Vec::new();
    for (s, r) in raws.iter().enumerate() {
        if r.parent == 5 {
            root_slots.push(s as u32);
        } else if let Some(&ps) = slot_of_rec.get(&r.parent) {
            children[ps as usize].push(s as u32);
        }
    }
    let mut alive = vec![false; n];
    let mut stack = root_slots;
    while let Some(s) = stack.pop() {
        if alive[s as usize] {
            continue;
        }
        alive[s as usize] = true;
        for &c in &children[s as usize] {
            stack.push(c);
        }
    }
    // 各活跃槽位的秩（条目会与其他根的构建线程并发 push 进全局共享表，
    // 本地 0 基编号必须经 base 偏移翻译，否则父链接指到别的盘的条目）
    let mut rank_of = vec![0u32; n];
    let mut rk = 0u32;
    for s in 0..n {
        if alive[s] {
            rank_of[s] = rk;
            rk += 1;
        }
    }
    // 整个 push 阶段持写锁：保证 base+rank 与实际下标一一对应
    let mut inner = index_rw().write().unwrap();
    let base = inner.entries.len() as u32;
    for s in 0..n {
        if !alive[s] {
            continue;
        }
        let r = &raws[s];
        let parent_idx = if r.parent == 5 {
            root_idx
        } else {
            base + rank_of[slot_of_rec[&r.parent] as usize]
        };
        debug_assert_eq!(inner.entries.len() as u32, base + rank_of[s]);
        // 只给目录登记 frn（USN 差量只需父定位；文件删除按父FRN+名字定位）
        let frn = if r.dir { r.rec } else { 0 };
        push_entry(&mut inner, parent_idx, r.name.clone(), r.dir, r.size, r.mtime, frn);
    }
    drop(inner);
    // 目录路径重建：从根沿兄弟链 BFS（目录全部登记 dir_paths/path_idx，供
    // path: 查询与增量事件定位；文件路径查询时按父目录路径现算）
    let mut queue: Vec<(u32, String)> = vec![(root_idx, display.to_string())];
    while let Some((didx, dpath)) = queue.pop() {
        let mut inner = index_rw().write().unwrap();
        let mut cur = inner.first_child.get(&didx).copied().unwrap_or(NO_IDX);
        let mut next_level: Vec<(u32, String)> = Vec::new();
        while cur != NO_IDX {
            let (name, is_dir, sib) = {
                let e = &inner.entries[cur as usize];
                (e.name.to_string(), e.dir, e.sibling)
            };
            if is_dir {
                let full = join_path(&dpath, &name);
                let arc: Arc<str> = Arc::from(full.as_str());
                inner.dir_paths.insert(cur, Arc::clone(&arc));
                inner.path_idx.insert(arc, cur);
                next_level.push((cur, full));
            }
            cur = sib;
        }
        drop(inner);
        queue.extend(next_level);
    }
}

fn build_and_monitor(root: String) {
    // 根显示名：盘根带尾部反斜杠，自定义目录原样
    let display = if root.len() == 2 && root.as_bytes()[1] == b':' {
        format!("{root}\\")
    } else {
        root.clone()
    };
    let t0 = Instant::now();
    let root_idx = add_root(&display);
    // 盘根：MFT/枚举之前先取 journal 进度（差量起点；宁可重复不漏，规格 §3.4）。
    // MFT 构建成功且卷根非空 → 保留进度；枚举回退 → (0,0) 停用差量。
    if is_drive_root(&display) {
        let row = query_usn_row(&display).unwrap_or((0, 0));
        usn_state().lock().unwrap().insert(root_idx, row);
    }
    // NTFS 盘根优先 MFT 直读（需管理员）；失败回退文件夹枚举（报告 §3.2/§3.5）
    let mut mode = "文件夹枚举";
    if is_drive_root(&display) {
        match crate::mft::enum_mft(&display) {
            Some(raws) => {
                build_from_mft(root_idx, &display, raws);
                // 自愈：MFT 解析异常时卷根零子项（正常 NTFS 盘至少有 SVI/$Recycle.Bin），
                // 回退文件夹枚举，不让单个卷拖死全局索引与落盘
                if root_child_count(root_idx) == 0 {
                    crate::flog!("[秒搜] {display} MFT 直读结果异常（卷根为空），回退文件夹枚举");
                    walk_insert(root_idx, &display);
                    mode = "文件夹枚举(MFT回退)";
                } else {
                    mode = "MFT直读";
                }
            }
            None => walk_insert(root_idx, &display),
        }
    } else {
        walk_insert(root_idx, &display);
    }
    if mode != "MFT直读" {
        // 枚举回退：无 frn，差量无法定位父目录 → 停用
        usn_state().lock().unwrap().insert(root_idx, (0, 0));
    }
    if is_drive_root(&display) {
        let empty = root_child_count(root_idx);
        if empty == 0 {
            eprintln!(
                "[秒搜] 警告：{display} 枚举结果为空（异常，正常 NTFS 盘至少含 SVI/$Recycle.Bin）"
            );
        }
    }
    let n = index_rw().read().map(|i| i.entries.len()).unwrap_or(0);
    crate::flog!("[秒搜] 索引完成 {display}（{mode}）：{n} 条目，耗时 {:?}", t0.elapsed());
    if INDEX_PENDING.fetch_sub(1, Ordering::SeqCst) == 1 {
        INDEX_READY.store(true, Ordering::SeqCst);
        save_index();
        crate::flog!("[秒搜] 全部索引就绪");
    }
    monitor_loop(root_idx, &display);
}

/// ReadDirectoryChangesW 阻塞监控循环；任何失败（溢出/句柄失效）→ 全量重扫后重开
fn monitor_loop(root_idx: u32, display: &str) {
    let root_wide = wide0(display);
    loop {
        let handle = unsafe {
            win::CreateFileW(
                root_wide.as_ptr(),
                win::FILE_LIST_DIRECTORY,
                win::FILE_SHARE_READ | win::FILE_SHARE_WRITE | win::FILE_SHARE_DELETE,
                std::ptr::null(),
                win::OPEN_EXISTING,
                win::FILE_FLAG_BACKUP_SEMANTICS,
                std::ptr::null_mut(),
            )
        };
        if handle == win::INVALID_HANDLE_VALUE {
            std::thread::sleep(Duration::from_secs(5));
            continue;
        }
        // 句柄就绪标记：USN 差量线程等此标记后才回放（覆盖窗口衔接，规格 §3.3.1）
        mon_open().lock().unwrap().insert(root_idx, true);
        // 8MB 事件缓冲（Everything 默认 12MB）
        let mut buf = vec![0u8; 8 * 1024 * 1024];
        loop {
            let mut ret = 0u32;
            let ok = unsafe {
                win::ReadDirectoryChangesW(
                    handle,
                    buf.as_mut_ptr() as *mut core::ffi::c_void,
                    buf.len() as u32,
                    1, // 递归子树
                    win::FILE_NOTIFY_CHANGE_FILE_NAME | win::FILE_NOTIFY_CHANGE_SIZE,
                    &mut ret,
                    std::ptr::null_mut(),
                    None,
                )
            };
            if ok == 0 {
                break;
            }
            if ret == 0 {
                continue;
            }
            for (rel, action) in parse_notify_events(&buf[..ret as usize]) {
                apply_event(display, &rel, action);
            }
        }
        unsafe { win::CloseHandle(handle) };
        // 只有 ReadDirectoryChangesW 失败（溢出/句柄失效）才会走到这里
        crate::flog!("[秒搜] {display} 监控中断，全量重扫");
        rescan_volume(root_idx, display);
        save_index_rate_limited();
        std::thread::sleep(Duration::from_secs(1));
    }
}

/// 解析 FILE_NOTIFY_INFORMATION 链 → (相对路径, action)
fn parse_notify_events(data: &[u8]) -> Vec<(String, u32)> {
    let mut out = Vec::new();
    let mut off = 0usize;
    while off + 12 <= data.len() {
        let next = u32::from_le_bytes(data[off..off + 4].try_into().unwrap()) as usize;
        let action = u32::from_le_bytes(data[off + 4..off + 8].try_into().unwrap());
        let nlen = u32::from_le_bytes(data[off + 8..off + 12].try_into().unwrap()) as usize;
        let name_end = (off + 12 + nlen).min(data.len());
        let mut u16s = Vec::with_capacity(nlen / 2);
        let mut i = off + 12;
        while i + 2 <= name_end {
            u16s.push(u16::from_le_bytes(data[i..i + 2].try_into().unwrap()));
            i += 2;
        }
        let rel = String::from_utf16_lossy(&u16s);
        if !rel.is_empty() {
            out.push((rel, action));
        }
        if next == 0 {
            break;
        }
        off += next;
    }
    out
}

// ============================================================
// USN Journal 启动差量（docs/08-增量索引更新规格.md §3.3）
// ============================================================

/// USN Reason 标志位（windows-sys 0.59 未导出，自补；Microsoft docs: Change Journals）
const USN_REASON_FILE_CREATE: u32 = 0x0000_0100;
const USN_REASON_FILE_DELETE: u32 = 0x0000_0200;
const USN_REASON_DATA_OVERWRITE: u32 = 0x0000_0001;
const USN_REASON_DATA_EXTEND: u32 = 0x0000_0002;
const USN_REASON_RENAME_OLD_NAME: u32 = 0x0000_2000;
const USN_REASON_RENAME_NEW_NAME: u32 = 0x0000_4000;

/// FRN 有效位：低 48 位（高 16 位是序号）。与 MFT 直读的 RawEntry.rec 对齐。
const FRN_MASK: u64 = 0x0000_FFFF_FFFF_FFFF;

/// RDC 监控句柄就绪标记（root_idx → true）。差量必须等监控打开后再回放，
/// 保证两个覆盖窗口衔接无洞（规格 §3.3.1）
static MON_OPEN: OnceLock<Mutex<HashMap<u32, bool>>> = OnceLock::new();

fn mon_open() -> &'static Mutex<HashMap<u32, bool>> {
    MON_OPEN.get_or_init(|| Mutex::new(HashMap::new()))
}

/// 重扫代数计数器（rescan_volume/rebuild_root 递增；差量线程发现变化即放弃）
static RESCAN_GEN: AtomicU64 = AtomicU64::new(0);

mod usn_win {
    pub use windows_sys::Win32::Foundation::{
        CloseHandle, GENERIC_READ, HANDLE, INVALID_HANDLE_VALUE,
    };
    pub use windows_sys::Win32::Storage::FileSystem::{
        CreateFileW, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
    };
    pub use windows_sys::Win32::System::IO::DeviceIoControl;
    pub use windows_sys::Win32::System::Ioctl::{
        FSCTL_QUERY_USN_JOURNAL, FSCTL_READ_USN_JOURNAL, READ_USN_JOURNAL_DATA_V0,
        USN_JOURNAL_DATA_V0,
    };
}

/// 单条 USN_RECORD_V2 解析结果
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UsnEvent {
    pub frn: u64,
    pub pfrn: u64,
    pub name: String,
    pub reason: u32,
    pub attrs: u32,
}

/// 解析 USN 输出缓冲（8 字节 NextUsn + V2 记录链；V2 固定头 60 字节 packed）。
/// 手工解字节，不依赖结构体内存布局；非 V2 记录跳过但推进偏移（终止性保证）。
fn parse_usn_records(data: &[u8]) -> (i64, Vec<UsnEvent>) {
    let mut out = Vec::new();
    if data.len() < 8 {
        return (0, out);
    }
    let next = i64::from_le_bytes(data[..8].try_into().unwrap());
    let mut off = 8usize;
    while off + 60 <= data.len() {
        let reclen = u32::from_le_bytes(data[off..off + 4].try_into().unwrap()) as usize;
        if reclen < 60 || off + reclen > data.len() {
            break;
        }
        let major = u16::from_le_bytes(data[off + 4..off + 6].try_into().unwrap());
        if major == 2 {
            let frn = u64::from_le_bytes(data[off + 8..off + 16].try_into().unwrap());
            let pfrn = u64::from_le_bytes(data[off + 16..off + 24].try_into().unwrap());
            let reason = u32::from_le_bytes(data[off + 40..off + 44].try_into().unwrap());
            let attrs = u32::from_le_bytes(data[off + 52..off + 56].try_into().unwrap());
            let nlen = u16::from_le_bytes(data[off + 56..off + 58].try_into().unwrap()) as usize;
            let noff = u16::from_le_bytes(data[off + 58..off + 60].try_into().unwrap()) as usize;
            let end = (off + noff + nlen).min(off + reclen);
            let mut u16s = Vec::with_capacity(nlen / 2);
            let mut i = off + noff;
            while i + 2 <= end {
                u16s.push(u16::from_le_bytes(data[i..i + 2].try_into().unwrap()));
                i += 2;
            }
            out.push(UsnEvent {
                frn: frn & FRN_MASK,
                pfrn: pfrn & FRN_MASK,
                name: String::from_utf16_lossy(&u16s),
                reason,
                attrs,
            });
        }
        off += reclen;
    }
    (next, out)
}

/// 打开卷设备（\\.\C:，GENERIC_READ；需管理员）
fn open_volume_handle(display: &str) -> Option<usn_win::HANDLE> {
    if display.len() < 2 || display.as_bytes()[1] != b':' {
        return None;
    }
    let vol = format!(r"\\.\{}", &display[..2]);
    let wide = wide0(&vol);
    let h = unsafe {
        usn_win::CreateFileW(
            wide.as_ptr(),
            usn_win::GENERIC_READ,
            usn_win::FILE_SHARE_READ | usn_win::FILE_SHARE_WRITE,
            std::ptr::null(),
            usn_win::OPEN_EXISTING,
            0,
            std::ptr::null_mut(),
        )
    };
    if h == usn_win::INVALID_HANDLE_VALUE {
        None
    } else {
        Some(h)
    }
}

/// 查询 journal 元数据 → (journal_id, next_usn, first_usn)
fn query_journal(h: usn_win::HANDLE) -> Option<(u64, i64, i64)> {
    let mut out: usn_win::USN_JOURNAL_DATA_V0 = unsafe { std::mem::zeroed() };
    let mut ret = 0u32;
    let ok = unsafe {
        usn_win::DeviceIoControl(
            h,
            usn_win::FSCTL_QUERY_USN_JOURNAL,
            std::ptr::null(),
            0,
            &mut out as *mut _ as *mut core::ffi::c_void,
            std::mem::size_of::<usn_win::USN_JOURNAL_DATA_V0>() as u32,
            &mut ret,
            std::ptr::null_mut(),
        )
    };
    if ok == 0 {
        return None;
    }
    Some((out.UsnJournalID, out.NextUsn, out.FirstUsn))
}

/// 查询该根的 USN 差量进度行（重扫/构建前调用：MFT 读取之前取 NextUsn，
/// 宁可下次回放重复也不漏——见规格 §3.4）
fn query_usn_row(display: &str) -> Option<(u64, u64)> {
    let h = open_volume_handle(display)?;
    let r = query_journal(h);
    unsafe { usn_win::CloseHandle(h) };
    r.map(|(jid, next, _)| (next.max(0) as u64, jid))
}

/// 读取从 `from` 起当前可用的全部记录 → (Usn 终点, 事件列表)。BytesToWaitFor=0 立即返回。
fn read_usn_records(
    h: usn_win::HANDLE,
    jid: u64,
    from: i64,
    buf: &mut [u8],
) -> Option<(i64, Vec<UsnEvent>)> {
    let inp = usn_win::READ_USN_JOURNAL_DATA_V0 {
        StartUsn: from,
        ReasonMask: 0xFFFF_FFFF,
        ReturnOnlyOnClose: 0,
        Timeout: 0,
        BytesToWaitFor: 0,
        UsnJournalID: jid,
    };
    let mut ret = 0u32;
    let ok = unsafe {
        usn_win::DeviceIoControl(
            h,
            usn_win::FSCTL_READ_USN_JOURNAL,
            &inp as *const _ as *const core::ffi::c_void,
            std::mem::size_of::<usn_win::READ_USN_JOURNAL_DATA_V0>() as u32,
            buf.as_mut_ptr() as *mut core::ffi::c_void,
            buf.len() as u32,
            &mut ret,
            std::ptr::null_mut(),
        )
    };
    if ok == 0 {
        return None;
    }
    Some(parse_usn_records(&buf[..ret as usize]))
}

/// 差量事件应用（幂等，规格 §3.6）：
/// - 增（CREATE/RENAME_NEW）：已存在则更新属性并补登记 frn（防重复 push），
///   否则 push；目录另登记路径映射并重枚举子树（改名目录的子项无独立记录）。
/// - 删（DELETE/RENAME_OLD）：父 FRN + 名字定位 unlink。
/// - 数据变化（DATA_*）：已有文件 stat 刷新 size/mtime。
///
/// 返回四元组 (adds, removes, refreshes, skips)。
fn usn_apply(evs: &[UsnEvent]) -> (usize, usize, usize, usize) {
    let mut adds = 0usize;
    let mut removes = 0usize;
    let mut refreshes = 0usize;
    let mut skips = 0usize;
    for ev in evs {
        if ev.name.is_empty() {
            skips += 1;
            continue;
        }
        let create = ev.reason & (USN_REASON_FILE_CREATE | USN_REASON_RENAME_NEW_NAME) != 0;
        let delete = ev.reason & (USN_REASON_FILE_DELETE | USN_REASON_RENAME_OLD_NAME) != 0;
        let data_chg = ev.reason & (USN_REASON_DATA_OVERWRITE | USN_REASON_DATA_EXTEND) != 0;
        if !create && !delete && !data_chg {
            continue;
        }
        // 父目录定位（FRN 低 48 位）；不在索引内（walk 根/系统对象）→ 跳过
        let (pidx, ppath) = {
            let inner = index_rw().read().unwrap();
            match inner.frn_dir.get(&ev.pfrn) {
                Some(&i) => match inner.dir_paths.get(&i) {
                    Some(p) => (i, p.to_string()),
                    None => {
                        skips += 1;
                        continue;
                    }
                },
                None => {
                    skips += 1;
                    continue;
                }
            }
        };
        if delete {
            let mut inner = index_rw().write().unwrap();
            unlink_child(&mut inner, pidx, &ev.name);
            removes += 1;
            continue;
        }
        if create {
            let full = join_path(&ppath, &ev.name);
            let wide: Vec<u16> = full.encode_utf16().chain(std::iter::once(0)).collect();
            let attrs = unsafe { win::GetFileAttributesW(wide.as_ptr()) };
            if attrs == win::INVALID_FILE_ATTRIBUTES {
                skips += 1; // 已被再次删除（journal 顺序内后续 DELETE 记录会再清一次）
                continue;
            }
            let is_dir = attrs & win::FILE_ATTRIBUTE_DIRECTORY != 0;
            let (size, mtime) = if is_dir {
                (0, now_secs())
            } else {
                match std::fs::metadata(&full) {
                    Ok(md) => (md.len(), mtime_secs(&md)),
                    Err(_) => {
                        skips += 1;
                        continue;
                    }
                }
            };
            let mut inner = index_rw().write().unwrap();
            if let Some(i) = find_child(&inner, pidx, &ev.name) {
                // 幂等：运行期 RDC 已应用过/重复回放 → 更新属性 + 补 frn 登记
                let e = &mut inner.entries[i as usize];
                if !e.dead {
                    e.size = size;
                    e.mtime = mtime;
                    if is_dir && ev.frn != 0 {
                        e.frn = ev.frn;
                    }
                }
                if is_dir && ev.frn != 0 {
                    inner.frn_dir.insert(ev.frn, i);
                }
                adds += 1;
            } else {
                let frn = if is_dir { ev.frn } else { 0 };
                let idx = push_entry(&mut inner, pidx, ev.name.clone(), is_dir, size, mtime, frn);
                if is_dir {
                    let arc: Arc<str> = Arc::from(full.as_str());
                    inner.dir_paths.insert(idx, Arc::clone(&arc));
                    inner.path_idx.insert(arc, idx);
                    drop(inner);
                    // 改名目录的子项无独立 journal 记录 → 重枚举子树
                    walk_insert(idx, &full);
                }
                adds += 1;
            }
            continue;
        }
        // 数据变化：已有文件 stat 刷新
        if let Ok(md) = std::fs::metadata(join_path(&ppath, &ev.name)) {
            let mut inner = index_rw().write().unwrap();
            if let Some(i) = find_child(&inner, pidx, &ev.name) {
                let e = &mut inner.entries[i as usize];
                if !e.dead && !e.dir {
                    e.size = md.len();
                    e.mtime = mtime_secs(&md);
                    refreshes += 1;
                }
            }
        }
    }
    (adds, removes, refreshes, skips)
}

/// 差量失效（journal 裁剪/重建/读取失败）时整根重建：MFT 优先（frn 完整），
/// 枚举回退（差量停用）。与 rescan_volume 共用实现。
fn full_rescan_root(root_idx: u32, display: &str) {
    RESCAN_GEN.fetch_add(1, Ordering::SeqCst);
    // 重扫前先取 journal 进度（T_q < 重扫状态完成时刻 → 差量宁可重复不漏）
    let usn_row = if is_drive_root(display) { query_usn_row(display) } else { None };
    {
        let mut inner = index_rw().write().unwrap();
        remove_subtree(&mut inner, root_idx);
        // remove_subtree 清掉了根自身的登记，补回
        let arc: Arc<str> = Arc::from(display);
        inner.dir_paths.insert(root_idx, Arc::clone(&arc));
        inner.path_idx.insert(arc, root_idx);
    }
    let mut mode = "文件夹枚举";
    if is_drive_root(display) {
        match crate::mft::enum_mft(display) {
            Some(raws) => {
                build_from_mft(root_idx, display, raws);
                // MFT 解析异常（卷根为空）→ 清空回退枚举
                if root_child_count(root_idx) > 0 {
                    mode = "MFT直读";
                } else {
                    let mut inner = index_rw().write().unwrap();
                    remove_subtree(&mut inner, root_idx);
                    let arc: Arc<str> = Arc::from(display);
                    inner.dir_paths.insert(root_idx, Arc::clone(&arc));
                    inner.path_idx.insert(arc, root_idx);
                    drop(inner);
                    walk_insert(root_idx, display);
                }
            }
            None => walk_insert(root_idx, display),
        }
    } else {
        walk_insert(root_idx, display);
    }
    // frn 完整（MFT）→ 差量可继续；枚举回退 → 差量停用
    let row = if mode == "MFT直读" {
        usn_row.unwrap_or((0, 0))
    } else {
        (0, 0)
    };
    usn_state().lock().unwrap().insert(root_idx, row);
    crate::flog!("[秒搜] {display} 全量重扫完成（{mode}），USN 进度 {:?}", row);
}

/// 启动差量回放（规格 §3.3）。lu/jid 来自快照；完成时前推 USN_STATE 并落盘。
fn catchup_root(root_idx: u32, display: &str, lu: u64, jid: u64) {
    // 等 RDC 句柄就绪（监控线程先于本线程启动；≤5s 超时放弃，宁可差量缺失不阻塞）
    for _ in 0..500 {
        if *mon_open().lock().unwrap().get(&root_idx).unwrap_or(&false) {
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    let gen0 = RESCAN_GEN.load(Ordering::SeqCst);
    let Some(h) = open_volume_handle(display) else {
        crate::flog!("[USN] {display} 卷打开失败，跳过差量");
        return;
    };
    let Some((cur_jid, next, first)) = query_journal(h) else {
        unsafe { usn_win::CloseHandle(h) };
        crate::flog!("[USN] {display} journal 查询失败（非 NTFS/已禁用），重建该根");
        full_rescan_root(root_idx, display);
        return;
    };
    if cur_jid != jid || (lu as i64) < first || (lu as i64) > next {
        unsafe { usn_win::CloseHandle(h) };
        crate::flog!(
            "[USN] {display} journal 失效（id/裁剪越界），重建该根（saved={lu} first={first}）"
        );
        full_rescan_root(root_idx, display);
        return;
    }
    let mut buf = vec![0u8; 1024 * 1024];
    let mut from = lu as i64;
    let (mut adds, mut removes, mut refreshes, mut skips) = (0usize, 0usize, 0usize, 0usize);
    loop {
        match read_usn_records(h, cur_jid, from, &mut buf) {
            None => {
                unsafe { usn_win::CloseHandle(h) };
                crate::flog!("[USN] {display} journal 读取失败，重建该根");
                full_rescan_root(root_idx, display);
                return;
            }
            Some((end, evs)) => {
                if end <= from {
                    break; // 无新记录
                }
                let (a, r, rf, s) = usn_apply(&evs);
                adds += a;
                removes += r;
                refreshes += rf;
                skips += s;
                from = end;
                if RESCAN_GEN.load(Ordering::SeqCst) != gen0 {
                    unsafe { usn_win::CloseHandle(h) };
                    crate::flog!("[USN] {display} 差量中途发生重扫，放弃剩余差量");
                    return;
                }
                if from >= next {
                    break;
                }
            }
        }
    }
    unsafe { usn_win::CloseHandle(h) };
    usn_state().lock().unwrap().insert(root_idx, (next as u64, cur_jid));
    crate::flog!(
        "[USN] {display} 差量完成：+{adds} -{removes} ~{refreshes} skip={skips}（{lu}→{next}）"
    );
    // 差量结果前推落盘（启动场景 LAST_SAVE 必然超过限流窗口）
    save_index();
}

// ---------- 自建索引查询 ----------


pub fn index_query(q: &Query, max_results: usize) -> Vec<Hit> {
    let inner = index_rw().read().unwrap();
    let mut out: Vec<Hit> = Vec::new();
    let mut name_buf: Vec<char> = Vec::with_capacity(64);
    let mut path_buf: Vec<char> = Vec::with_capacity(128);
    for (idx, e) in inner.entries.iter().enumerate() {
        if out.len() >= max_results {
            break;
        }
        if e.dead {
            continue;
        }
        name_buf.clear();
        fold_into(&e.name, &mut name_buf);
        // path: 词需要的路径（文件=父目录路径=全路径去掉文件名；目录=自身全路径）
        let path_f = if q.has_path {
            let pidx = if e.dir { idx as u32 } else { e.parent };
            match inner.dir_paths.get(&pidx) {
                Some(p) => {
                    path_buf.clear();
                    fold_into(p, &mut path_buf);
                    Some(path_buf.as_slice())
                }
                None => None,
            }
        } else {
            None
        };
        if q.eval(&name_buf, path_f, e.dir, e.size, e.mtime) {
            let path = if e.dir {
                inner
                    .dir_paths
                    .get(&(idx as u32))
                    .map(|p| p.to_string())
                    .unwrap_or_else(|| e.name.to_string())
            } else {
                match inner.dir_paths.get(&e.parent) {
                    Some(p) => join_path(p, &e.name),
                    None => e.name.to_string(),
                }
            };
            out.push(Hit {
                name: e.name.to_string(),
                path,
                dir: e.dir,
                size: e.size,
                mtime: e.mtime,
            });
        }
    }
    out
}

// ============================================================
// Windows 导入（仅自建索引所需；不依赖任何第三方进程）
// ============================================================

mod win {
    pub use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
    pub use windows_sys::Win32::Storage::FileSystem::{
        CreateFileW, GetDriveTypeW, GetFileAttributesW, GetLogicalDrives, ReadDirectoryChangesW,
        FILE_ATTRIBUTE_DIRECTORY, FILE_FLAG_BACKUP_SEMANTICS, FILE_LIST_DIRECTORY,
        FILE_NOTIFY_CHANGE_FILE_NAME, FILE_NOTIFY_CHANGE_SIZE, FILE_SHARE_DELETE, FILE_SHARE_READ,
        FILE_SHARE_WRITE, INVALID_FILE_ATTRIBUTES, OPEN_EXISTING,
    };
}

// ============================================================
// 单元测试（索引测试合并为单测试函数：OnceLock 全局索引不跨测试清理）
// ============================================================

#[cfg(test)]
mod tests {
    use super::*;

    fn fold(s: &str) -> Vec<char> {
        fold_str(s)
    }

    // ---------- 匹配原语 ----------

    #[test]
    fn test_fold_ascii_fast_path() {
        assert_eq!(fold("Hello.TXT"), "hello.txt".chars().collect::<Vec<_>>());
        // 非 ASCII 完整折叠
        assert_eq!(fold("ÁBC"), "ábc".chars().collect::<Vec<_>>());
        assert_eq!(fold("文件"), "文件".chars().collect::<Vec<_>>());
    }

    #[test]
    fn test_contains() {
        let h = fold("MyReport2024.docx");
        assert!(contains(&h, &fold("report")));
        assert!(contains(&h, &fold("REPORT")));
        assert!(!contains(&h, &fold("reportx")));
        assert!(contains(&h, &[]));
    }

    #[test]
    fn test_wild_match() {
        let h = fold("setup.exe");
        assert!(wild_match(&h, &fold("*.exe")));
        assert!(wild_match(&h, &fold("set*.exe")));
        assert!(wild_match(&h, &fold("setup.ex?")));
        assert!(wild_match(&h, &fold("*")));
        assert!(wild_match(&h, &fold("s*p.e?e")));
        assert!(!wild_match(&h, &fold("setup")));
        assert!(!wild_match(&h, &fold("*.msi")));
        let cn = fold("报告2026.docx");
        assert!(wild_match(&cn, &fold("报告*")));
    }

    // ---------- 查询解析 ----------

    #[test]
    fn test_parse_query_structure() {
        // a|b c → 两个备选组
        let q = parse_query("a|b c");
        assert_eq!(q.alts.len(), 2);
        assert_eq!(q.alts[0].len(), 1);
        assert_eq!(q.alts[1].len(), 2);
        // ext / size / scope / neg
        let q = parse_query("ext:txt;md size:>1mb !folder:");
        assert_eq!(q.alts.len(), 1);
        assert_eq!(q.alts[0].len(), 3);
        assert!(q.alts[0][2].neg);
        assert!(matches!(q.alts[0][0].kind, TermKind::Exts(_)));
        assert!(matches!(q.alts[0][1].kind, TermKind::Size(_)));
        assert!(matches!(q.alts[0][2].kind, TermKind::Scope(false)));
        // path: 触发 has_path
        assert!(parse_query("path:code").has_path);
        assert!(!parse_query("code").has_path);
    }

    #[test]
    fn test_parse_size() {
        assert!(matches!(parse_size("empty"), Some(SizeSpec::Cmp(CMP_EQ, 0))));
        assert!(matches!(parse_size(">1mb"), Some(SizeSpec::Cmp(CMP_GT, 1_048_576))));
        assert!(matches!(parse_size("small"), Some(SizeSpec::Band(10_240, 102_400))));
        assert!(matches!(parse_size("2048"), Some(SizeSpec::Cmp(CMP_EQ, 2048))));
        assert!(parse_size("abc").is_none());
        // 非法值在 parse_token 层回退为字面量
        let q = parse_query("size:abc");
        assert!(matches!(q.alts[0][0].kind, TermKind::Name { .. }));
    }

    #[test]
    fn test_parse_date() {
        assert!(matches!(parse_date("today"), Some(DateSpec::Band(_, _))));
        assert!(matches!(parse_date("thismonth"), Some(DateSpec::Band(_, _))));
        assert!(matches!(parse_date(">2024-06-01"), Some(DateSpec::Band(_, i64::MAX))));
        assert!(parse_date("notadate").is_none());
    }

    // ---------- 自建索引（构建 / 查询 / 增量事件） ----------

    /// 造一棵已知树（唯一临时目录，测试结束清理）
    fn build_test_tree(tag: &str) -> (String, std::path::PathBuf) {
        let tmp = std::env::temp_dir().join(format!("wfm_inst_{}_{}", std::process::id(), tag));
        let _ = std::fs::remove_dir_all(&tmp);
        let alpha = tmp.join("alpha");
        let sub = alpha.join("sub1");
        std::fs::create_dir_all(&sub).unwrap();
        std::fs::write(tmp.join("root.txt"), vec![0u8; 5]).unwrap();
        std::fs::write(alpha.join("a1.txt"), vec![0u8; 10]).unwrap();
        std::fs::write(alpha.join("a2.TXT"), vec![0u8; 200]).unwrap();
        std::fs::write(sub.join("s1.log"), vec![0u8; 50000]).unwrap();
        let rp = tmp.to_string_lossy().into_owned();
        let idx = add_root(&rp);
        walk_insert(idx, &rp);
        (rp, tmp)
    }

    /// 按 path:<tag> 限定范围（全局索引跨测试共享，用唯一目录名隔离）
    fn scoped(tag: &str, rest: &str) -> Query {
        parse_query(&format!("path:{tag} {rest}"))
    }

    #[test]
    fn test_index_suite() {
        let tag = format!("s1_{}", std::process::id());
        let (rp, tmp) = build_test_tree(&tag);
        // 子串 / 大小写不敏感
        let hits = index_query(&scoped(&tag, "a1"), MAX_RESULTS);
        assert_eq!(hits.len(), 1, "a1: {hits:?}");
        assert!(hits[0].path.ends_with("\\alpha\\a1.txt"));
        assert_eq!(hits[0].size, 10);
        assert_eq!(index_query(&scoped(&tag, "A1"), MAX_RESULTS).len(), 1);
        // 通配符
        let hits = index_query(&scoped(&tag, "*.log"), MAX_RESULTS);
        assert_eq!(hits.len(), 1);
        assert!(hits[0].path.ends_with("s1.log"));
        // OR：a1 与 a2 均命中（注意 path: 限定只作用于 OR 左支，a2 全局唯一不跨树）
        assert_eq!(index_query(&scoped(&tag, "a1|a2"), MAX_RESULTS).len(), 2);
        // NOT
        let hits = index_query(&scoped(&tag, "txt !a1"), MAX_RESULTS);
        assert_eq!(hits.len(), 2, "txt !a1: {hits:?}"); // root.txt + a2.TXT
        // ext
        assert_eq!(index_query(&scoped(&tag, "ext:txt;md"), MAX_RESULTS).len(), 3);
        // size
        assert_eq!(index_query(&scoped(&tag, "size:>1kb"), MAX_RESULTS).len(), 1);
        assert_eq!(index_query(&scoped(&tag, "size:small"), MAX_RESULTS).len(), 1);
        // folder:/file:
        assert_eq!(index_query(&scoped(&tag, "folder:"), MAX_RESULTS).len(), 3); // root/alpha/sub1
        assert_eq!(index_query(&scoped(&tag, "file:"), MAX_RESULTS).len(), 4);
        // path: 直接子项（sub1 目录自身与其下文件都命中——与 Everything 全路径匹配语义一致）
        let hits = index_query(&scoped(&tag, "path:sub1"), MAX_RESULTS);
        assert_eq!(hits.len(), 2, "path:sub1: {hits:?}");
        assert!(hits.iter().all(|h| h.path.contains("sub1")));
        // path: 覆盖深层后代（父目录路径前缀语义）
        assert_eq!(index_query(&scoped(&tag, "path:alpha file:"), MAX_RESULTS).len(), 3);

        // ---- 增量事件 ----
        // ADDED 文件
        std::fs::write(tmp.join("new_file.dat"), vec![0u8; 42]).unwrap();
        apply_event(&rp, "new_file.dat", 1);
        let hits = index_query(&scoped(&tag, "new_file"), MAX_RESULTS);
        assert_eq!(hits.len(), 1, "added file: {hits:?}");
        assert_eq!(hits[0].size, 42);
        // ADDED 目录（带内容）
        let nd = tmp.join("newdir");
        std::fs::create_dir_all(&nd).unwrap();
        std::fs::write(nd.join("inner.txt"), vec![0u8; 7]).unwrap();
        apply_event(&rp, "newdir", 1);
        assert_eq!(index_query(&scoped(&tag, "inner"), MAX_RESULTS).len(), 1);
        // REMOVED
        std::fs::remove_file(tmp.join("new_file.dat")).unwrap();
        apply_event(&rp, "new_file.dat", 2);
        assert_eq!(index_query(&scoped(&tag, "new_file"), MAX_RESULTS).len(), 0);
        // RENAMED：OLD 删 + NEW 增
        std::fs::rename(&nd, tmp.join("renamed_dir")).unwrap();
        apply_event(&rp, "newdir", 4);
        apply_event(&rp, "renamed_dir", 5);
        let hits = index_query(&scoped(&tag, "inner"), MAX_RESULTS);
        assert_eq!(hits.len(), 1, "renamed: {hits:?}");
        assert!(hits[0].path.contains("renamed_dir"));
        // MODIFIED（size 更新）
        std::fs::write(tmp.join("root.txt"), vec![0u8; 12345]).unwrap();
        apply_event(&rp, "root.txt", 3);
        let hits = index_query(&scoped(&tag, "root.txt"), MAX_RESULTS);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].size, 12345);
        std::fs::remove_dir_all(&tmp).unwrap();
    }

    #[test]
    fn test_parent_of_drive_root() {
        assert_eq!(parent_of("D:\\a\\b.txt"), Some("D:\\a"));
        assert_eq!(parent_of("D:\\a.txt"), Some("D:\\"));
        assert_eq!(parent_of("C:\\Users\\x\\f"), Some("C:\\Users\\x"));
        assert_eq!(parent_of("rel"), None);
    }

    #[test]
    fn test_parse_notify_events() {
        // 两条记录的链
        let name1: Vec<u8> = "a.txt".encode_utf16().flat_map(|u| u.to_le_bytes()).collect();
        let name2: Vec<u8> = "b.log".encode_utf16().flat_map(|u| u.to_le_bytes()).collect();
        let mut data = Vec::new();
        let rec1_padded = (12 + name1.len()).div_ceil(4) * 4;
        data.extend_from_slice(&(rec1_padded as u32).to_le_bytes());
        data.extend_from_slice(&1u32.to_le_bytes());
        data.extend_from_slice(&(name1.len() as u32).to_le_bytes());
        data.extend_from_slice(&name1);
        data.resize(rec1_padded, 0);
        data.extend_from_slice(&0u32.to_le_bytes()); // 链尾
        data.extend_from_slice(&2u32.to_le_bytes());
        data.extend_from_slice(&(name2.len() as u32).to_le_bytes());
        data.extend_from_slice(&name2);
        let ev = parse_notify_events(&data);
        assert_eq!(ev.len(), 2);
        assert_eq!(ev[0], ("a.txt".to_string(), 1));
        assert_eq!(ev[1], ("b.log".to_string(), 2));
    }

    // ---------- 索引落盘往返 ----------

    #[test]
    fn test_index_persist_roundtrip() {
        // 独立命名的文件树（全局索引跨测试共享，避免与 test_index_suite 的
        // 无 path 限定 OR 分支互相串扰）
        let tag = format!("persist_{}", std::process::id());
        let tmp = std::env::temp_dir().join(format!("wfm_inst_{tag}"));
        let _ = std::fs::remove_dir_all(&tmp);
        let alpha = tmp.join("palpha");
        let sub = alpha.join("psub");
        std::fs::create_dir_all(&sub).unwrap();
        std::fs::write(tmp.join("proot.txt"), vec![0u8; 5]).unwrap();
        std::fs::write(alpha.join("p1.txt"), vec![0u8; 10]).unwrap();
        std::fs::write(alpha.join("p2.TXT"), vec![0u8; 200]).unwrap();
        std::fs::write(sub.join("p3.log"), vec![0u8; 50000]).unwrap();
        let rp = tmp.to_string_lossy().into_owned();
        let _idx = add_root(&rp);
        walk_insert(_idx, &rp);
        // 落盘到临时文件（绕开 exe 同级目录；根列表按根下标排序）
        let path = std::env::temp_dir().join(format!("wfm_idx_{tag}.bin"));
        let roots = roots_list().lock().unwrap().clone();
        write_snapshot(&path, &roots).unwrap();
        let mut expected: Vec<String> = {
            let mut v: Vec<(u32, String)> = roots.clone();
            v.sort_by_key(|(i, _)| *i);
            v.into_iter().map(|(_, p)| p).collect()
        };
        // 加载 → 结构一致
        let (inner, roots_reg, _usn_map) = load_inner_from(&path, &expected).expect("roundtrip load");
        assert!(inner.dir_paths.values().any(|p| p.as_ref() == rp.as_str()));
        assert!(inner
            .entries
            .iter()
            .any(|e| e.name.as_ref() == "p1.txt" && e.size == 10));
        assert!(roots_reg.iter().any(|(_, p)| p == &rp));
        // 目录路径 BFS 应覆盖子目录
        assert!(inner
            .dir_paths
            .values()
            .any(|p| p.ends_with("\\palpha\\psub")));
        // 根列表不匹配 → 拒绝加载（触发重建）
        expected[0] = "C:\\definitely_not_a_root\\".into();
        assert!(load_inner_from(&path, &expected).is_none());
        let _ = std::fs::remove_file(&path);
        std::fs::remove_dir_all(&tmp).unwrap();
    }

    // ---------- 十九轮：USN 差量 ----------

    /// 合成一条 V2 记录（name UTF-16，4 字节对齐）
    fn make_v2_rec(frn: u64, pfrn: u64, reason: u32, attrs: u32, name: &str, major: u16) -> Vec<u8> {
        let mut name_b: Vec<u8> = name.encode_utf16().flat_map(|u| u.to_le_bytes()).collect();
        let reclen = (60 + name_b.len()).div_ceil(4) * 4;
        let mut v = Vec::with_capacity(reclen);
        v.extend_from_slice(&(reclen as u32).to_le_bytes());
        v.extend_from_slice(&major.to_le_bytes());
        v.extend_from_slice(&2u16.to_le_bytes()); // minor
        v.extend_from_slice(&frn.to_le_bytes());
        v.extend_from_slice(&pfrn.to_le_bytes());
        v.extend_from_slice(&0i64.to_le_bytes()); // Usn
        v.extend_from_slice(&0i64.to_le_bytes()); // TimeStamp
        v.extend_from_slice(&reason.to_le_bytes());
        v.extend_from_slice(&0u32.to_le_bytes()); // SourceInfo
        v.extend_from_slice(&0u32.to_le_bytes()); // SecurityId
        v.extend_from_slice(&attrs.to_le_bytes());
        v.extend_from_slice(&(name_b.len() as u16).to_le_bytes());
        v.extend_from_slice(&60u16.to_le_bytes()); // FileNameOffset
        v.append(&mut name_b);
        v.resize(reclen, 0);
        v
    }

    #[test]
    fn test_parse_usn_records() {
        let mut data = Vec::new();
        data.extend_from_slice(&0x1234i64.to_le_bytes()); // NextUsn
        data.extend_from_slice(&make_v2_rec(
            0x0007_0000_0000_0123, // 高 16 位序号 + 低 48 位记录号
            5,
            USN_REASON_FILE_CREATE | 0x8000_0000,
            0x20,
            "a.txt",
            2,
        ));
        // 非 V2 记录：跳过但推进偏移
        data.extend_from_slice(&make_v2_rec(1, 1, 0, 0, "junk", 3));
        data.extend_from_slice(&make_v2_rec(9, 9, USN_REASON_FILE_DELETE, 0x10, "b.log", 2));
        let (next, evs) = parse_usn_records(&data);
        assert_eq!(next, 0x1234);
        assert_eq!(evs.len(), 2, "非 V2 记录应跳过：{evs:?}");
        // FRN 掩码：只留低 48 位
        assert_eq!(evs[0].frn, 0x0000_0000_0000_0123);
        assert_eq!(evs[0].pfrn, 5);
        assert_eq!(evs[0].name, "a.txt");
        assert_eq!(evs[1].name, "b.log");
        assert_eq!(evs[1].reason, USN_REASON_FILE_DELETE);
        // 截断缓冲不 panic
        let (n2, e2) = parse_usn_records(&data[..20]);
        assert_eq!(n2, 0x1234);
        assert!(e2.is_empty());
    }

    #[test]
    fn test_snapshot_v3_frn_usn_roundtrip() {
        let tag = format!("snap3_{}", std::process::id());
        let tmp = std::env::temp_dir().join(format!("wfm_inst_{tag}"));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(tmp.join("sd2")).unwrap();
        std::fs::write(tmp.join("sd2").join("sf.txt"), vec![0u8; 3]).unwrap();
        let rp = tmp.to_string_lossy().into_owned();
        let idx = add_root(&rp);
        // 根与子目录登记 frn（模拟 MFT 构建形态）+ USN 进度
        {
            let mut inner = index_rw().write().unwrap();
            inner.entries[idx as usize].frn = 0x7777;
            inner.frn_dir.insert(0x7777, idx);
            let sidx = push_entry(&mut inner, idx, "sd2".into(), true, 0, 0, 0x8888);
            let arc: Arc<str> = Arc::from(join_path(&rp, "sd2").as_str());
            inner.dir_paths.insert(sidx, Arc::clone(&arc));
            inner.path_idx.insert(arc, sidx);
        }
        usn_state().lock().unwrap().insert(idx, (0xDEAD_BEEF_1234, 0xABCD));
        // 落盘（全部根：全局索引与其他测试共享）→ 加载 → frn/frn_dir/USN 进度一致
        let path = std::env::temp_dir().join(format!("wfm_idx_{tag}.bin"));
        let roots_all = roots_list().lock().unwrap().clone();
        write_snapshot(&path, &roots_all).unwrap();
        let mut expected: Vec<String> = {
            let mut v = roots_all.clone();
            v.sort_by_key(|(i, _)| *i);
            v.into_iter().map(|(_, p)| p).collect()
        };
        let (inner, roots_reg, usn_map) =
            load_inner_from(&path, &expected).expect("v3 roundtrip load");
        // 加载后下标经重映射，按 frn 与名字断言（不依赖具体下标值）
        assert!(inner.frn_dir.contains_key(&0x7777));
        assert!(inner.frn_dir.values().any(|&i| {
            inner.entries[i as usize].name.as_ref() == "sd2"
                && inner.entries[i as usize].frn == 0x8888
        }));
        assert!(roots_reg.iter().any(|(_, p)| p == &rp));
        let my_loaded = roots_reg
            .iter()
            .find(|(_, p)| p == &rp)
            .map(|(i, _)| *i)
            .expect("本根在加载结果中");
        assert_eq!(usn_map.get(&my_loaded), Some(&(0xDEAD_BEEF_1234, 0xABCD)));
        // 根列表不匹配 → 拒绝加载（触发重建）
        expected[0] = "C:\\definitely_not_a_root\\".into();
        assert!(load_inner_from(&path, &expected).is_none());
        // 清理全局态，避免污染并发测试
        usn_state().lock().unwrap().remove(&idx);
        let _ = std::fs::remove_file(&path);
        std::fs::remove_dir_all(&tmp).unwrap();
    }

    #[test]
    fn test_usn_apply_idempotent() {
        let tag = format!("usnap_{}", std::process::id());
        let tmp = std::env::temp_dir().join(format!("wfm_inst_{tag}"));
        let _ = std::fs::remove_dir_all(&tmp);
        let rp = tmp.to_string_lossy().into_owned();
        let idx = add_root(&rp);
        // 根登记 frn（模拟 MFT 形态），使 USN 父定位可用
        {
            let mut inner = index_rw().write().unwrap();
            inner.entries[idx as usize].frn = 0xAAAA;
            inner.frn_dir.insert(0xAAAA, idx);
        }
        // 磁盘上真实创建（usn_apply 增路径要 stat）
        std::fs::create_dir_all(&tmp).unwrap();
        std::fs::write(tmp.join("u1.txt"), vec![0u8; 3]).unwrap();
        std::fs::create_dir_all(tmp.join("usub")).unwrap();
        std::fs::write(tmp.join("usub").join("f.txt"), vec![0u8; 7]).unwrap();
        let ev_file = UsnEvent {
            frn: 0xBBB1,
            pfrn: 0xAAAA,
            name: "u1.txt".into(),
            reason: USN_REASON_FILE_CREATE | 0x8000_0000,
            attrs: 0x20,
        };
        let ev_dir = UsnEvent {
            frn: 0xCCCC,
            pfrn: 0xAAAA,
            name: "usub".into(),
            reason: USN_REASON_FILE_CREATE | 0x8000_0000,
            attrs: 0x10,
        };
        let ev_child = UsnEvent {
            frn: 0xDDD1,
            pfrn: 0xCCCC, // 经目录事件登记的 frn 定位
            name: "f.txt".into(),
            reason: USN_REASON_FILE_CREATE | 0x8000_0000,
            attrs: 0x20,
        };
        usn_apply(&[ev_file.clone(), ev_dir, ev_child]);
        assert_eq!(index_query(&scoped(&tag, "u1"), MAX_RESULTS).len(), 1);
        assert_eq!(index_query(&scoped(&tag, "f.txt"), MAX_RESULTS).len(), 1);
        // 幂等回放：同事件重复应用 → 不重复 push
        let entries_before = index_rw().read().unwrap().entries.len();
        usn_apply(std::slice::from_ref(&ev_file));
        assert_eq!(index_rw().read().unwrap().entries.len(), entries_before);
        assert_eq!(index_query(&scoped(&tag, "u1"), MAX_RESULTS).len(), 1);
        // 删除（父 FRN + 名字定位）
        std::fs::remove_file(tmp.join("u1.txt")).unwrap();
        let ev_del = UsnEvent { reason: USN_REASON_FILE_DELETE, ..ev_file };
        usn_apply(&[ev_del]);
        assert_eq!(index_query(&scoped(&tag, "u1"), MAX_RESULTS).len(), 0);
        // 数据变化：size 刷新
        std::fs::write(tmp.join("usub").join("f.txt"), vec![0u8; 777]).unwrap();
        let ev_data = UsnEvent {
            frn: 0xDDD1,
            pfrn: 0xCCCC,
            name: "f.txt".into(),
            reason: USN_REASON_DATA_EXTEND,
            attrs: 0x20,
        };
        usn_apply(&[ev_data]);
        let hits = index_query(&scoped(&tag, "f.txt"), MAX_RESULTS);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].size, 777);
        std::fs::remove_dir_all(&tmp).unwrap();
    }

    /// 真实卷 journal 元数据诊断（非管理员/非 NTFS 时跳过）
    #[test]
    fn test_diag_real_journal() {
        let Some(h) = open_volume_handle("C:\\") else {
            eprintln!("skip: 卷打开失败（需管理员）");
            return;
        };
        let (jid, next, first) = match query_journal(h) {
            Some(r) => r,
            None => {
                unsafe { usn_win::CloseHandle(h) };
                eprintln!("skip: C: 无 USN journal（非 NTFS/已禁用）");
                return;
            }
        };
        assert_ne!(jid, 0, "journal id 不应为 0");
        assert!(next >= first, "next={next} first={first}");
        // 读取一批记录验证解析链路
        let mut buf = vec![0u8; 64 * 1024];
        let (end, evs) = read_usn_records(h, jid, first.max(0), &mut buf)
            .expect("journal 读取失败");
        assert!(end >= first);
        eprintln!("journal: id={jid:#x} next={next} 首批事件 {} 条（读到 {end}）", evs.len());
        unsafe { usn_win::CloseHandle(h) };
    }
}
