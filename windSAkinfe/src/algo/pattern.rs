//! 通配符模式：`*`（捕获）、`**`、`?`、`[a-z]`、`{a,b}`，支持大小写策略。
//!
//! `*` 与 `?` 不跨越路径分隔符；`**` 跨目录；`**/` 可匹配零层目录。

#[derive(Clone, Debug)]
enum Ast {
    Lit(char),
    Star(usize),
    Any,
    DoubleStar,
    DoubleStarSlash,
    Class { neg: bool, items: Vec<(char, char)> },
    Alt(Vec<Vec<Ast>>),
}

#[derive(Clone, Debug)]
enum Tok {
    Lit(char),
    Star(usize),
    Any,
    DoubleStar,
    DoubleStarSlash,
    Class { neg: bool, items: Vec<(char, char)> },
}

#[derive(Debug, Clone)]
pub struct Pattern {
    branches: Vec<Vec<Tok>>,
    ncaptures: usize,
    case_sensitive: bool,
    source: String,
}

impl Pattern {
    pub fn new(pat: &str, case_sensitive: bool) -> Result<Pattern, String> {
        let chars: Vec<char> = pat.chars().collect();
        let mut groups = 0usize;
        let mut i = 0usize;
        let ast = parse_seq(&chars, &mut i, false, &mut groups)?;
        if i != chars.len() {
            return Err(format!("模式未闭合（多余 `}}`）：{pat}"));
        }
        let ncap = groups;
        let branches = expand(&ast);
        Ok(Pattern { branches, ncaptures: ncap, case_sensitive, source: pat.to_string() })
    }

    /// 便捷构造：默认按平台决定大小写敏感性（Windows 不敏感）。
    pub fn platform(pat: &str) -> Result<Pattern, String> {
        Pattern::new(pat, !cfg!(windows))
    }

    pub fn source(&self) -> &str {
        &self.source
    }

    pub fn ncaptures(&self) -> usize {
        self.ncaptures
    }

    pub fn is_match(&self, s: &str) -> bool {
        self.captures(s).is_some()
    }

    /// 返回捕获组（长度 = ncaptures，未参与的组为空串）。
    pub fn captures(&self, s: &str) -> Option<Vec<String>> {
        let chars: Vec<char> = s.chars().collect();
        for br in &self.branches {
            let mut caps = vec![None; self.ncaptures];
            let mut budget = 200_000u32;
            if match_toks(br, &chars, 0, &mut caps, self.case_sensitive, &mut budget) {
                return Some(caps.into_iter().map(|c| c.unwrap_or_default()).collect());
            }
        }
        None
    }
}

fn eq_char(a: char, b: char, ci: bool) -> bool {
    if ci {
        a == b
    } else {
        a.eq_ignore_ascii_case(&b) || a.to_lowercase().eq(b.to_lowercase())
    }
}

fn parse_seq(chars: &[char], i: &mut usize, stop_at_comma: bool, groups: &mut usize) -> Result<Vec<Ast>, String> {
    let mut out = Vec::new();
    while *i < chars.len() {
        let c = chars[*i];
        if stop_at_comma && (c == ',' || c == '}') {
            break;
        }
        match c {
            '\\' => {
                *i += 1;
                if *i >= chars.len() {
                    return Err("模式以孤立的反斜杠结尾".into());
                }
                out.push(Ast::Lit(chars[*i]));
                *i += 1;
            }
            '*' => {
                if chars.get(*i + 1) == Some(&'*') {
                    *i += 2;
                    // `**/` 视为可空目录段
                    if chars.get(*i) == Some(&'/') {
                        *i += 1;
                        out.push(Ast::DoubleStarSlash);
                    } else {
                        out.push(Ast::DoubleStar);
                    }
                } else {
                    out.push(Ast::Star(*groups));
                    *groups += 1;
                    *i += 1;
                }
            }
            '?' => {
                out.push(Ast::Any);
                *i += 1;
            }
            '[' => {
                *i += 1;
                let mut neg = false;
                if matches!(chars.get(*i), Some('!') | Some('^')) {
                    neg = true;
                    *i += 1;
                }
                let mut items = Vec::new();
                let mut first = true;
                loop {
                    if *i >= chars.len() {
                        return Err("字符集未闭合（缺少 `]`）".into());
                    }
                    let ch = chars[*i];
                    if ch == ']' && !first {
                        *i += 1;
                        break;
                    }
                    first = false;
                    if chars.get(*i + 1) == Some(&'-')
                        && chars.get(*i + 2).is_some_and(|c2| *c2 != ']')
                    {
                        items.push((ch, chars[*i + 2]));
                        *i += 3;
                    } else {
                        items.push((ch, ch));
                        *i += 1;
                    }
                }
                out.push(Ast::Class { neg, items });
            }
            '{' => {
                *i += 1;
                let mut branches = Vec::new();
                loop {
                    let seq = parse_seq(chars, i, true, groups)?;
                    branches.push(seq);
                    if *i >= chars.len() {
                        return Err("备选组未闭合（缺少 `}`）".into());
                    }
                    if chars[*i] == ',' {
                        *i += 1;
                        continue;
                    }
                    if chars[*i] == '}' {
                        *i += 1;
                        break;
                    }
                }
                out.push(Ast::Alt(branches));
            }
            '}' => return Err("多余的 `}`".into()),
            _ => {
                out.push(Ast::Lit(c));
                *i += 1;
            }
        }
    }
    Ok(out)
}

fn expand(nodes: &[Ast]) -> Vec<Vec<Tok>> {
    let mut out: Vec<Vec<Tok>> = vec![Vec::new()];
    for n in nodes {
        match n {
            Ast::Alt(branches) => {
                let mut next = Vec::new();
                for b in branches {
                    let sub = expand(b);
                    for o in &out {
                        for s in &sub {
                            let mut v = o.clone();
                            v.extend(s.iter().cloned());
                            next.push(v);
                        }
                    }
                }
                out = next;
            }
            Ast::Lit(c) => out.iter_mut().for_each(|v| v.push(Tok::Lit(*c))),
            Ast::Star(g) => out.iter_mut().for_each(|v| v.push(Tok::Star(*g))),
            Ast::Any => out.iter_mut().for_each(|v| v.push(Tok::Any)),
            Ast::DoubleStar => out.iter_mut().for_each(|v| v.push(Tok::DoubleStar)),
            Ast::DoubleStarSlash => out.iter_mut().for_each(|v| v.push(Tok::DoubleStarSlash)),
            Ast::Class { neg, items } => out.iter_mut().for_each(|v| {
                v.push(Tok::Class { neg: *neg, items: items.clone() });
            }),
        }
    }
    out
}

fn match_toks(
    toks: &[Tok],
    s: &[char],
    si: usize,
    caps: &mut [Option<String>],
    ci: bool,
    budget: &mut u32,
) -> bool {
    if *budget == 0 {
        return false;
    }
    *budget -= 1;
    let Some((head, tail)) = toks.split_first() else {
        return si == s.len();
    };
    match head {
        Tok::Lit(c) => {
            si < s.len() && eq_char(s[si], *c, ci) && match_toks(tail, s, si + 1, caps, ci, budget)
        }
        Tok::Any => si < s.len() && s[si] != '/' && match_toks(tail, s, si + 1, caps, ci, budget),
        Tok::Class { neg, items } => {
            if si >= s.len() || s[si] == '/' {
                return false;
            }
            let ch = s[si];
            let hit = if ci {
                let lc = ch.to_ascii_lowercase();
                items.iter().any(|(lo, hi)| {
                    let l = lo.to_ascii_lowercase();
                    let h = hi.to_ascii_lowercase();
                    l <= lc && lc <= h
                })
            } else {
                items.iter().any(|(lo, hi)| lo <= &ch && &ch <= hi)
            };
            if hit == *neg {
                return false;
            }
            match_toks(tail, s, si + 1, caps, ci, budget)
        }
        Tok::DoubleStar => {
            for k in si..=s.len() {
                if match_toks(tail, s, k, caps, ci, budget) {
                    return true;
                }
            }
            false
        }
        Tok::DoubleStarSlash => {
            // 零层目录
            if match_toks(tail, s, si, caps, ci, budget) {
                return true;
            }
            for k in si + 1..=s.len() {
                if s[k - 1] == '/' && match_toks(tail, s, k, caps, ci, budget) {
                    return true;
                }
            }
            false
        }
        Tok::Star(g) => {
            for k in si..=s.len() {
                if k > si && s[k - 1] == '/' {
                    break;
                }
                let saved = caps[*g].take();
                caps[*g] = Some(s[si..k].iter().collect());
                if match_toks(tail, s, k, caps, ci, budget) {
                    return true;
                }
                caps[*g] = saved;
            }
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(s: &str) -> Pattern {
        Pattern::new(s, true).unwrap()
    }

    #[test]
    fn simple_star() {
        assert!(p("*.txt").is_match("a.txt"));
        assert!(p("*.txt").is_match(".txt"));
        assert!(!p("*.txt").is_match("a.txt.bak"));
        assert!(!p("*.txt").is_match("dir/a.txt"));
    }

    #[test]
    fn star_captures() {
        let pat = p("第*话-*.jpg");
        let caps = pat.captures("第03话-封面.jpg").unwrap();
        assert_eq!(capss(&caps), vec!["03", "封面"]);
        assert_eq!(pat.ncaptures(), 2);
        assert!(pat.captures("第03话.jpg").is_none());
    }

    fn capss(v: &[String]) -> Vec<&str> {
        v.iter().map(|s| s.as_str()).collect()
    }

    #[test]
    fn question_and_class() {
        assert!(p("a?c").is_match("abc"));
        assert!(!p("a?c").is_match("a/c"));
        assert!(p("[a-c]x").is_match("bx"));
        assert!(!p("[!a-c]x").is_match("bx"));
        assert!(p("[!a-c]x").is_match("dx"));
        assert!(p("file[0-9][0-9].log").is_match("file42.log"));
    }

    #[test]
    fn double_star() {
        assert!(p("**/*.rs").is_match("src/a.rs"));
        assert!(p("**/*.rs").is_match("a.rs"));
        assert!(p("**/*.rs").is_match("a/b/c.rs"));
        assert!(p("src/**/*.rs").is_match("src/a.rs"));
        assert!(p("src/**/*.rs").is_match("src/x/y/a.rs"));
        assert!(!p("src/**/*.rs").is_match("lib/a.rs"));
    }

    #[test]
    fn alternatives() {
        let pat = p("*.{jpg,png,gif}");
        assert!(pat.is_match("a.jpg"));
        assert!(pat.is_match("a.png"));
        assert!(!pat.is_match("a.bmp"));
        assert_eq!(pat.ncaptures(), 1);
        assert_eq!(capss(&pat.captures("x.png").unwrap()), vec!["x"]);
    }

    #[test]
    fn case_insensitive_mode() {
        assert!(Pattern::new("*.JPG", false).unwrap().is_match("a.jpg"));
        assert!(!Pattern::new("*.JPG", true).unwrap().is_match("a.jpg"));
    }

    #[test]
    fn escape_literals() {
        assert!(p("a\\*b").is_match("a*b"));
        assert!(!p("a\\*b").is_match("axxb"));
    }

    #[test]
    fn unclosed_errors() {
        assert!(Pattern::new("[abc", true).is_err());
        assert!(Pattern::new("{a,b", true).is_err());
        assert!(Pattern::new("abc\\", true).is_err());
    }
}
