//! 精简正则引擎（`replace` 命令专用，零依赖）。
//!
//! 支持子集：
//! - 字面量、`.`（不匹配换行）
//! - 字符集 `[abc]` `[a-z]` `[^...]`，预定义类 `\d \D \w \W \s \S`
//! - 量词 `* + ? {n} {n,} {n,m}`，均可加 `?` 变非贪婪
//! - 分组 `( )` 捕获、`(?: )` 非捕获
//! - 选择 `|`
//! - 锚点 `^ $`，转义 `\.` 等
//!
//! 实现方式是「AST → 指令表 → 递归回溯」。回溯步数有上限，防止灾难性回溯卡死。

use std::fmt;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RegexError(pub String);

impl fmt::Display for RegexError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "正则语法错误：{}", self.0)
    }
}

impl std::error::Error for RegexError {}

fn err<T>(msg: impl Into<String>) -> Result<T, RegexError> {
    Err(RegexError(msg.into()))
}

// ---------------------------------------------------------------- AST

#[derive(Debug, Clone)]
enum Node {
    Lit(char),
    Any,
    Class(Class),
    AnchorStart,
    AnchorEnd,
    Group(Option<usize>, Box<Node>),
    Concat(Vec<Node>),
    /// 分支：第一个是首选（贪婪/正序）
    Alt(Vec<Node>),
    Repeat {
        node: Box<Node>,
        min: u32,
        max: u32,
        greedy: bool,
    },
}

const INF: u32 = u32::MAX;

#[derive(Debug, Clone, Default)]
struct Class {
    neg: bool,
    ranges: Vec<(char, char)>,
}

impl Class {
    fn new(neg: bool) -> Class {
        Class { neg, ranges: Vec::new() }
    }

    fn add(&mut self, lo: char, hi: char) {
        self.ranges.push((lo, hi));
    }

    fn matches(&self, c: char, ci: bool) -> bool {
        let probe = if ci && c.is_ascii() { c.to_ascii_lowercase() } else { c };
        let hit = self.ranges.iter().any(|&(lo, hi)| {
            if ci && lo.is_ascii() && hi.is_ascii() {
                let l = lo.to_ascii_lowercase();
                let h = hi.to_ascii_lowercase();
                l <= probe && probe <= h
            } else {
                lo <= probe && probe <= hi
            }
        });
        hit != self.neg
    }
}

// ---------------------------------------------------------------- 解析器

struct Parser<'a> {
    chars: Vec<char>,
    pos: usize,
    ngroup: usize,
    _src: &'a str,
}

impl<'a> Parser<'a> {
    fn new(src: &'a str, _ci: bool) -> Parser<'a> {
        Parser { chars: src.chars().collect(), pos: 0, ngroup: 0, _src: src }
    }

    fn peek(&self) -> Option<char> {
        self.chars.get(self.pos).copied()
    }

    fn next(&mut self) -> Option<char> {
        let c = self.peek();
        if c.is_some() {
            self.pos += 1;
        }
        c
    }

    fn eat(&mut self, c: char) -> bool {
        if self.peek() == Some(c) {
            self.pos += 1;
            true
        } else {
            false
        }
    }

    fn parse(&mut self) -> Result<Node, RegexError> {
        let node = self.parse_alt()?;
        if self.pos < self.chars.len() {
            return err(format!("位置 {} 处有多余的 '{}'", self.pos, self.chars[self.pos]));
        }
        Ok(node)
    }

    fn parse_alt(&mut self) -> Result<Node, RegexError> {
        let mut branches = vec![self.parse_concat()?];
        while self.eat('|') {
            branches.push(self.parse_concat()?);
        }
        if branches.len() == 1 {
            Ok(branches.pop().unwrap())
        } else {
            Ok(Node::Alt(branches))
        }
    }

    fn parse_concat(&mut self) -> Result<Node, RegexError> {
        let mut items: Vec<Node> = Vec::new();
        while let Some(c) = self.peek() {
            if c == '|' || c == ')' {
                break;
            }
            let atom = self.parse_atom()?;
            let node = self.parse_quant(atom)?;
            items.push(node);
        }
        if items.is_empty() {
            // 空分支：匹配空串
            return Ok(Node::Concat(Vec::new()));
        }
        if items.len() == 1 {
            Ok(items.pop().unwrap())
        } else {
            Ok(Node::Concat(items))
        }
    }

    fn parse_quant(&mut self, atom: Node) -> Result<Node, RegexError> {
        let (min, max) = match self.peek() {
            Some('*') => {
                self.pos += 1;
                (0, INF)
            }
            Some('+') => {
                self.pos += 1;
                (1, INF)
            }
            Some('?') => {
                self.pos += 1;
                (0, 1)
            }
            Some('{') => {
                // 只有在形如 {n} / {n,} / {n,m} 时才当作量词，否则按字面量
                let save = self.pos;
                self.pos += 1;
                let mut a = String::new();
                while let Some(c) = self.peek() {
                    if c.is_ascii_digit() {
                        a.push(c);
                        self.pos += 1;
                    } else {
                        break;
                    }
                }
                let lo = a.parse::<u32>().ok();
                let hi = if self.eat(',') {
                    let mut b = String::new();
                    while let Some(c) = self.peek() {
                        if c.is_ascii_digit() {
                            b.push(c);
                            self.pos += 1;
                        } else {
                            break;
                        }
                    }
                    if b.is_empty() {
                        Some(INF)
                    } else {
                        Some(b.parse::<u32>().unwrap_or(INF))
                    }
                } else {
                    lo
                };
                if !self.eat('}') || lo.is_none() {
                    self.pos = save;
                    return Ok(atom);
                }
                let lo = lo.unwrap();
                let hi = hi.unwrap_or(lo);
                if hi != INF && hi < lo {
                    return err(format!("量词 {{{lo},{hi}}} 上界小于下界"));
                }
                (lo, hi)
            }
            _ => return Ok(atom),
        };
        let greedy = !self.eat('?');
        Ok(Node::Repeat { node: Box::new(atom), min, max, greedy })
    }

    fn parse_atom(&mut self) -> Result<Node, RegexError> {
        let c = match self.next() {
            Some(c) => c,
            None => return err("表达式意外结束"),
        };
        match c {
            '.' => Ok(Node::Any),
            '^' => Ok(Node::AnchorStart),
            '$' => Ok(Node::AnchorEnd),
            '(' => {
                // (?:...) 非捕获
                let cap = if self.peek() == Some('?') {
                    let save = self.pos;
                    self.pos += 1;
                    if self.eat(':') {
                        None
                    } else {
                        // (?i) 之类不支持，还原后按捕获组处理
                        self.pos = save;
                        self.ngroup += 1;
                        Some(self.ngroup)
                    }
                } else {
                    self.ngroup += 1;
                    Some(self.ngroup)
                };
                let inner = self.parse_alt()?;
                if !self.eat(')') {
                    return err("括号未闭合");
                }
                Ok(Node::Group(cap, Box::new(inner)))
            }
            ')' => err("多余的 ')'"),
            '[' => {
                let cls = self.parse_class()?;
                Ok(Node::Class(cls))
            }
            '\\' => {
                let e = match self.next() {
                    Some(e) => e,
                    None => return err("反斜杠后缺少字符"),
                };
                Ok(match e {
                    'd' => {
                        let mut c = Class::new(false);
                        c.add('0', '9');
                        Node::Class(c)
                    }
                    'D' => {
                        let mut c = Class::new(true);
                        c.add('0', '9');
                        Node::Class(c)
                    }
                    'w' => {
                        let mut c = Class::new(false);
                        c.add('a', 'z');
                        c.add('A', 'Z');
                        c.add('0', '9');
                        c.add('_', '_');
                        Node::Class(c)
                    }
                    'W' => {
                        let mut c = Class::new(true);
                        c.add('a', 'z');
                        c.add('A', 'Z');
                        c.add('0', '9');
                        c.add('_', '_');
                        Node::Class(c)
                    }
                    's' => {
                        let mut c = Class::new(false);
                        for ch in [' ', '\t', '\n', '\r', '\u{0b}', '\u{0c}'] {
                            c.add(ch, ch);
                        }
                        Node::Class(c)
                    }
                    'S' => {
                        let mut c = Class::new(true);
                        for ch in [' ', '\t', '\n', '\r', '\u{0b}', '\u{0c}'] {
                            c.add(ch, ch);
                        }
                        Node::Class(c)
                    }
                    'n' => Node::Lit('\n'),
                    'r' => Node::Lit('\r'),
                    't' => Node::Lit('\t'),
                    'f' => Node::Lit('\u{0c}'),
                    'v' => Node::Lit('\u{0b}'),
                    '0' => Node::Lit('\0'),
                    'u' => Node::Lit(self.parse_unicode_escape()?),
                    other => Node::Lit(other),
                })
            }
            '*' | '+' | '?' => err(format!("量词 '{c}' 前面没有可重复的内容")),
            '{' => {
                // 不是量词位置，按字面量
                Ok(Node::Lit('{'))
            }
            other => Ok(Node::Lit(other)),
        }
    }

    /// 解析 `\uXXXX` 与 `\u{XXXXXX}`。
    fn parse_unicode_escape(&mut self) -> Result<char, RegexError> {
        let mut hex = String::new();
        if self.eat('{') {
            while let Some(c) = self.peek() {
                if c == '}' {
                    break;
                }
                hex.push(c);
                self.pos += 1;
            }
            if !self.eat('}') {
                return err("\\u{ 未闭合");
            }
        } else {
            for _ in 0..4 {
                match self.next() {
                    Some(c) => hex.push(c),
                    None => return err("\\uXXXX 位数不足"),
                }
            }
        }
        let v = u32::from_str_radix(&hex, 16)
            .map_err(|_| RegexError(format!("\\u{hex} 不是合法的十六进制")))?;
        char::from_u32(v).ok_or_else(|| RegexError(format!("非法 Unicode 码点 {v:#x}")))
    }

    fn parse_class(&mut self) -> Result<Class, RegexError> {        let neg = self.eat('^');
        let mut cls = Class::new(neg);
        let mut first = true;
        loop {
            let c = match self.next() {
                Some(c) => c,
                None => return err("字符集未闭合"),
            };
            if c == ']' && !first {
                return Ok(cls);
            }
            first = false;
            // 转义类
            let lo = if c == '\\' {
                match self.next() {
                    Some('d') => {
                        cls.add('0', '9');
                        continue;
                    }
                    Some('w') => {
                        cls.add('a', 'z');
                        cls.add('A', 'Z');
                        cls.add('0', '9');
                        cls.add('_', '_');
                        continue;
                    }
                    Some('s') => {
                        for ch in [' ', '\t', '\n', '\r', '\u{0b}', '\u{0c}'] {
                            cls.add(ch, ch);
                        }
                        continue;
                    }
                    Some('n') => '\n',
                    Some('r') => '\r',
                    Some('t') => '\t',
                    Some(other) => other,
                    None => return err("字符集内反斜杠后缺字符"),
                }
            } else {
                c
            };
            // 区间 a-z
            if self.peek() == Some('-') && self.chars.get(self.pos + 1).copied() != Some(']') {
                self.pos += 1;
                let mut hi = self.next().unwrap_or(lo);
                if hi == '\\' {
                    hi = self.next().unwrap_or(lo);
                    hi = match hi {
                        'n' => '\n',
                        'r' => '\r',
                        't' => '\t',
                        o => o,
                    };
                }
                if hi < lo {
                    return err(format!("字符区间 {lo}-{hi} 顺序颠倒"));
                }
                cls.add(lo, hi);
            } else {
                cls.add(lo, lo);
            }
        }
    }
}

// ---------------------------------------------------------------- 指令表

#[derive(Debug, Clone)]
enum Inst {
    Char(char),
    Any,
    Class(usize),
    Split(usize, usize),
    Jmp(usize),
    Save(usize),
    Begin,
    End,
    Match,
}

struct Prog {
    insts: Vec<Inst>,
    classes: Vec<Class>,
    ncap: usize,
}

fn compile(node: &Node) -> Prog {
    let mut insts = Vec::new();
    let mut classes = Vec::new();
    // 整体匹配区间是 0 号捕获组：Save(0) ... Save(1)
    insts.push(Inst::Save(0));
    emit(node, &mut insts, &mut classes);
    insts.push(Inst::Save(1));
    insts.push(Inst::Match);
    Prog { insts, classes, ncap: 0 }
}

fn emit(node: &Node, out: &mut Vec<Inst>, classes: &mut Vec<Class>) {
    match node {
        Node::Lit(c) => out.push(Inst::Char(*c)),
        Node::Any => out.push(Inst::Any),
        Node::Class(c) => {
            classes.push(c.clone());
            out.push(Inst::Class(classes.len() - 1));
        }
        Node::AnchorStart => out.push(Inst::Begin),
        Node::AnchorEnd => out.push(Inst::End),
        Node::Group(cap, inner) => {
            if let Some(n) = cap {
                out.push(Inst::Save(2 * n));
            }
            emit(inner, out, classes);
            if let Some(n) = cap {
                out.push(Inst::Save(2 * n + 1));
            }
        }
        Node::Concat(items) => {
            for it in items {
                emit(it, out, classes);
            }
        }
        Node::Alt(branches) => {
            // 逐分支 split 串起来
            let mut jumps = Vec::new();
            for (i, b) in branches.iter().enumerate() {
                if i + 1 == branches.len() {
                    emit(b, out, classes);
                } else {
                    let split_at = out.len();
                    out.push(Inst::Split(0, 0));
                    let body_at = out.len();
                    emit(b, out, classes);
                    let jmp_at = out.len();
                    out.push(Inst::Jmp(0));
                    jumps.push(jmp_at);
                    let next_at = out.len();
                    out[split_at] = Inst::Split(body_at, next_at);
                }
            }
            let end = out.len();
            for j in jumps {
                out[j] = Inst::Jmp(end);
            }
        }
        Node::Repeat { node, min, max, greedy } => {
            for _ in 0..*min {
                emit(node, out, classes);
            }
            if *max == INF {
                // L: Split(body, after) ; body ; Jmp L
                let l = out.len();
                out.push(Inst::Split(0, 0));
                let body_at = out.len();
                emit(node, out, classes);
                out.push(Inst::Jmp(l));
                let after = out.len();
                out[l] = if *greedy {
                    Inst::Split(body_at, after)
                } else {
                    Inst::Split(after, body_at)
                };
            } else {
                let extra = max.saturating_sub(*min);
                let mut splits = Vec::new();
                for _ in 0..extra {
                    let s = out.len();
                    out.push(Inst::Split(0, 0));
                    splits.push(s);
                    emit(node, out, classes);
                }
                let after = out.len();
                for s in splits {
                    out[s] = if *greedy {
                        Inst::Split(s + 1, after)
                    } else {
                        Inst::Split(after, s + 1)
                    };
                }
            }
        }
    }
}

// ---------------------------------------------------------------- 匹配器

pub struct Regex {
    prog: Prog,
    ci: bool,
    multi_line: bool,
    src: String,
    /// 回溯步数上限
    budget: u32,
}

/// 回溯用的显式栈帧。
///
/// 用 `Vec` 而不是递归：灾难性回溯（例如 `(a*)*b` 匹配一长串 a）时递归深度
/// 会随步数线性增长，必然爆掉线程栈（实测 STATUS_STACK_OVERFLOW）。显式栈
/// 把深度搬到堆上，再靠 `budget` 兜住总步数即可安全退出。
struct Frame {
    pc: usize,
    pos: usize,
    /// 进入该帧时 undo 日志的长度，回溯时按此长度回滚捕获组
    undo: usize,
}

impl Regex {
    pub fn new(pattern: &str) -> Result<Regex, RegexError> {
        Regex::with_options(pattern, false, false)
    }

    pub fn with_options(pattern: &str, ci: bool, multi_line: bool) -> Result<Regex, RegexError> {
        let mut p = Parser::new(pattern, ci);
        let ast = p.parse()?;
        let mut prog = compile(&ast);
        prog.ncap = p.ngroup;
        Ok(Regex {
            prog,
            ci,
            multi_line,
            src: pattern.to_string(),
            budget: 500_000,
        })
    }

    pub fn pattern(&self) -> &str {
        &self.src
    }

    pub fn ncaptures(&self) -> usize {
        self.prog.ncap
    }

    fn start_anchored(&self) -> bool {
        // insts[0] 是整体的 Save(0)，锚点在其后
        matches!(self.prog.insts.get(1), Some(Inst::Begin))
    }

    /// 从 `from` 开始找第一个匹配，返回捕获区间。
    fn search_at(&self, text: &[char], from: usize) -> Option<Vec<Option<usize>>> {
        // 带 ^ 锚点且非多行模式时，只可能从行首（0）匹配
        let anchored = self.start_anchored() && !self.multi_line;
        let mut start = from;
        loop {
            let mut caps = vec![None; 2 * (self.prog.ncap + 1)];
            if self.run(text, start, &mut caps) {
                return Some(caps);
            }
            if anchored || start >= text.len() {
                return None;
            }
            start += 1;
        }
    }

    /// 显式栈回溯执行器。命中时 `caps` 为最终捕获位置。
    fn run(&self, text: &[char], start: usize, caps: &mut [Option<usize>]) -> bool {
        let mut stack: Vec<Frame> = vec![Frame { pc: 0, pos: start, undo: 0 }];
        let mut undo: Vec<(usize, Option<usize>)> = Vec::new();
        let mut steps: u32 = 0;
        while let Some(frame) = stack.pop() {
            while undo.len() > frame.undo {
                let (n, old) = undo.pop().unwrap();
                caps[n] = old;
            }
            let mut pc = frame.pc;
            let mut pos = frame.pos;
            loop {
                steps += 1;
                if steps > self.budget {
                    return false;
                }
                match &self.prog.insts[pc] {
                    Inst::Match => return true,
                    Inst::Char(c) => match text.get(pos) {
                        Some(&t) if char_eq(*c, t, self.ci) => {
                            pos += 1;
                            pc += 1;
                        }
                        _ => break,
                    },
                    Inst::Any => match text.get(pos) {
                        Some(&t) if t != '\n' => {
                            pos += 1;
                            pc += 1;
                        }
                        _ => break,
                    },
                    Inst::Class(idx) => match text.get(pos) {
                        Some(&t) if self.prog.classes[*idx].matches(t, self.ci) => {
                            pos += 1;
                            pc += 1;
                        }
                        _ => break,
                    },
                    Inst::Begin => {
                        let ok = pos == 0 || (self.multi_line && text[pos - 1] == '\n');
                        if ok {
                            pc += 1;
                        } else {
                            break;
                        }
                    }
                    Inst::End => {
                        let ok = pos == text.len() || (self.multi_line && text[pos] == '\n');
                        if ok {
                            pc += 1;
                        } else {
                            break;
                        }
                    }
                    Inst::Jmp(t) => pc = *t,
                    Inst::Split(a, b) => {
                        if stack.len() > 400_000 {
                            return false;
                        }
                        // 优先分支继续走，另一分支入栈待回溯
                        stack.push(Frame { pc: *b, pos, undo: undo.len() });
                        pc = *a;
                    }
                    Inst::Save(n) => {
                        undo.push((*n, caps[*n]));
                        caps[*n] = Some(pos);
                        pc += 1;
                    }
                }
            }
        }
        false
    }

    pub fn is_match(&self, text: &str) -> bool {
        let chars: Vec<char> = text.chars().collect();
        self.search_at(&chars, 0).is_some()
    }

    /// 返回所有匹配的捕获文本（0 号是整个匹配）。
    pub fn captures(&self, text: &str) -> Option<Vec<Option<String>>> {
        let chars: Vec<char> = text.chars().collect();
        let caps = self.search_at(&chars, 0)?;
        Some(caps_to_strings(&chars, &caps))
    }

    pub fn find_all(&self, text: &str) -> Vec<(usize, usize)> {
        let chars: Vec<char> = text.chars().collect();
        let mut out = Vec::new();
        let mut from = 0usize;
        while from <= chars.len() {
            match self.search_at(&chars, from) {
                Some(caps) => {
                    let (s, e) = (caps[0].unwrap_or(from), caps[1].unwrap_or(from));
                    out.push((s, e));
                    from = if e > s { e } else { e + 1 };
                }
                None => break,
            }
        }
        out
    }

    /// 替换全部匹配；`repl` 支持 `$0..$9` 与 `$$`。
    pub fn replace_all(&self, text: &str, repl: &str) -> String {
        let chars: Vec<char> = text.chars().collect();
        let mut out = String::with_capacity(text.len());
        let mut cursor = 0usize;
        let mut from = 0usize;
        while from <= chars.len() {
            let caps = match self.search_at(&chars, from) {
                Some(c) => c,
                None => break,
            };
            let s = caps[0].unwrap_or(from);
            let e = caps[1].unwrap_or(from);
            for ch in &chars[cursor..s] {
                out.push(*ch);
            }
            let strs = caps_to_strings(&chars, &caps);
            out.push_str(&expand(repl, &strs));
            cursor = e;
            from = if e > s { e } else { e + 1 };
            if from > chars.len() {
                break;
            }
        }
        for ch in &chars[cursor.min(chars.len())..] {
            out.push(*ch);
        }
        out
    }

    /// 统计匹配次数（不关心位置）。
    pub fn count_matches(&self, text: &str) -> usize {
        self.find_all(text).len()
    }
}

fn char_eq(a: char, b: char, ci: bool) -> bool {
    if a == b {
        return true;
    }
    if !ci {
        return false;
    }
    a.to_lowercase().eq(b.to_lowercase())
}

fn caps_to_strings(chars: &[char], caps: &[Option<usize>]) -> Vec<Option<String>> {
    let mut out = Vec::with_capacity(caps.len() / 2);
    let mut i = 0;
    while i + 1 < caps.len() {
        out.push(match (caps[i], caps[i + 1]) {
            (Some(a), Some(b)) if a <= b => Some(chars[a..b].iter().collect()),
            _ => None,
        });
        i += 2;
    }
    out
}

/// 展开替换串里的 `$n` / `${n}` / `$$`。
fn expand(repl: &str, caps: &[Option<String>]) -> String {
    let mut out = String::with_capacity(repl.len());
    let cs: Vec<char> = repl.chars().collect();
    let mut i = 0;
    while i < cs.len() {
        if cs[i] == '$' && i + 1 < cs.len() {
            if cs[i + 1] == '$' {
                out.push('$');
                i += 2;
                continue;
            }
            if cs[i + 1] == '{' {
                let mut j = i + 2;
                let mut num = String::new();
                while j < cs.len() && cs[j] != '}' {
                    num.push(cs[j]);
                    j += 1;
                }
                if j < cs.len() {
                    if let Ok(n) = num.parse::<usize>() {
                        if let Some(Some(v)) = caps.get(n) {
                            out.push_str(v);
                        }
                        i = j + 1;
                        continue;
                    }
                }
            }
            if cs[i + 1].is_ascii_digit() {
                let n = cs[i + 1] as usize - '0' as usize;
                if let Some(Some(v)) = caps.get(n) {
                    out.push_str(v);
                }
                i += 2;
                continue;
            }
        }
        out.push(cs[i]);
        i += 1;
    }
    out
}

/// 字面量替换（`--literal` 模式），一次扫描，不解释 `$`。
pub fn literal_replace_all(text: &str, needle: &str, repl: &str) -> (String, usize) {
    if needle.is_empty() {
        return (text.to_string(), 0);
    }
    let mut out = String::with_capacity(text.len());
    let mut count = 0;
    let mut rest = text;
    while let Some(idx) = rest.find(needle) {
        out.push_str(&rest[..idx]);
        out.push_str(repl);
        rest = &rest[idx + needle.len()..];
        count += 1;
    }
    out.push_str(rest);
    (out, count)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn m(p: &str, t: &str) -> bool {
        Regex::new(p).unwrap().is_match(t)
    }

    #[test]
    fn literals_and_anchors() {
        assert!(m("abc", "xxabcxx"));
        assert!(!m("^abc", "xxabc"));
        assert!(m("^abc", "abcxx"));
        assert!(m("abc$", "xxabc"));
        assert!(!m("abc$", "abcxx"));
        assert!(m("^$", ""));
    }

    #[test]
    fn dot_and_class() {
        assert!(m("a.c", "abc"));
        assert!(!m("a.c", "a\nc"));
        assert!(m("[abc]+", "cab"));
        assert!(!m("[^abc]+$", "abc"));
        assert!(m("[0-9]{3}", "x123x"));
        assert!(!m("[0-9]{4}", "x123x"));
        assert!(m(r"\d+", "abc123"));
        assert!(m(r"\w+@\w+", "mail me: a_b@host"));
        assert!(m(r"\s", "a b"));
    }

    #[test]
    fn quantifiers() {
        assert!(m("ab*c", "ac"));
        assert!(m("ab*c", "abbbc"));
        assert!(!m("ab+c", "ac"));
        assert!(m("colou?r", "color"));
        assert!(m("colou?r", "colour"));
        assert!(m("a{2,3}", "aa"));
        assert!(m("x{2,}", "xxxxx"));
        // 贪婪 / 非贪婪
        let re = Regex::new("<(.+?)>").unwrap();
        let c = re.captures("<a><b>").unwrap();
        assert_eq!(c[1].as_deref(), Some("a"));
        let re = Regex::new("<(.+)>").unwrap();
        let c = re.captures("<a><b>").unwrap();
        assert_eq!(c[1].as_deref(), Some("a><b"));
    }

    #[test]
    fn alternation_and_groups() {
        assert!(m("cat|dog", "hotdog"));
        assert!(!m("^cat|dog$", "hotdogx"));
        let re = Regex::new(r"(\d{4})-(\d{2})-(\d{2})").unwrap();
        let c = re.captures("date 2024-05-06 end").unwrap();
        assert_eq!(c[1].as_deref(), Some("2024"));
        assert_eq!(c[2].as_deref(), Some("05"));
        assert_eq!(c[3].as_deref(), Some("06"));
        assert_eq!(re.ncaptures(), 3);
        assert!(m("(?:ab)+", "ababab"));
    }

    #[test]
    fn case_insensitive() {
        let re = Regex::with_options("hello", true, false).unwrap();
        assert!(re.is_match("HeLLo"));
        assert!(m("[a-z]+", "abc"));
    }

    #[test]
    fn replace_with_backrefs() {
        let re = Regex::new(r"(\w+)@(\w+)").unwrap();
        assert_eq!(re.replace_all("me@here", "$2 at $1"), "here at me");
        assert_eq!(re.replace_all("me@here", "$$1"), "$1");
        assert_eq!(re.replace_all("me@here", "${2}"), "here");
        let re = Regex::new(r"\d+").unwrap();
        assert_eq!(re.replace_all("a1b22c333", "N"), "aNbNcN");
        assert_eq!(re.count_matches("a1b22c333"), 3);
    }

    #[test]
    fn literal() {
        let (s, n) = literal_replace_all("a.b.c", ".", "-");
        assert_eq!((s.as_str(), n), ("a-b-c", 2));
        let (s, n) = literal_replace_all("aaa", "aa", "b");
        assert_eq!((s.as_str(), n), ("ba", 1));
        let (s, n) = literal_replace_all("abc", "", "x");
        assert_eq!((s.as_str(), n), ("abc", 0));
    }

    #[test]
    fn bad_patterns_report() {
        assert!(Regex::new("(abc").is_err());
        assert!(Regex::new("[abc").is_err());
        assert!(Regex::new("*abc").is_err());
        assert!(Regex::new("abc)").is_err());
        assert!(Regex::new("a{3,1}").is_err());
    }

    #[test]
    fn empty_pattern_matches() {
        let re = Regex::new("").unwrap();
        assert!(re.is_match(""));
        assert!(re.is_match("abc"));
    }

    #[test]
    fn backtracking_budget_terminates() {
        // 经典灾难性回溯用例，必须在预算内退出而不是卡死
        let re = Regex::new("(a*)*b").unwrap();
        let text = "a".repeat(40);
        assert!(!re.is_match(&text));
    }

    #[test]
    fn multibyte_text() {
        let re = Regex::new("中文(.)").unwrap();
        let c = re.captures("这是中文测试").unwrap();
        assert_eq!(c[1].as_deref(), Some("测"));
        let re = Regex::new("[一-龥]").unwrap();
        assert!(re.is_match("汉"));
    }
}
