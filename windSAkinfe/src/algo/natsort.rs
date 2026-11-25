//! 自然排序：数字段按数值比较，`img2 < img10`。

use std::cmp::Ordering;

/// 从 `start` 起取一段「同类型」的连续字符：数字段或非数字段。
/// 按字节切分是安全的：ASCII 数字只可能出现在字符边界上。
fn span(b: &[u8], start: usize) -> usize {
    let d = b[start].is_ascii_digit();
    let mut i = start;
    while i < b.len() && b[i].is_ascii_digit() == d {
        i += 1;
    }
    i
}

/// 零分配的自然序比较。
///
/// 排序时一次比较会调用几十万次，早期实现每次都比较两个字符串
/// 并 `Vec<(bool, String)>` 分词，10 万文件排序时分配开销成了主要瓶颈。
pub fn natural_cmp(a: &str, b: &str) -> Ordering {
    if a == b {
        return Ordering::Equal;
    }
    let (ab, bb) = (a.as_bytes(), b.as_bytes());
    let (mut i, mut j) = (0usize, 0usize);
    while i < ab.len() && j < bb.len() {
        let ad = ab[i].is_ascii_digit();
        let bd = bb[j].is_ascii_digit();
        let ai = span(ab, i);
        let bj = span(bb, j);
        let at = &a[i..ai];
        let bt = &b[j..bj];
        let c = if ad && bd {
            let a0 = at.trim_start_matches('0');
            let b0 = bt.trim_start_matches('0');
            a0.len()
                .cmp(&b0.len())
                .then_with(|| a0.cmp(b0))
                // 数值相等时，前导零少的在前（"1" < "01"）
                .then_with(|| at.len().cmp(&bt.len()))
        } else {
            at.cmp(bt)
        };
        if c != Ordering::Equal {
            return c;
        }
        i = ai;
        j = bj;
    }
    // 一方还有剩余段 ⇒ 它的段数更多（与原「token 数量比较」语义一致）
    (ab.len() - i).cmp(&(bb.len() - j))
}

pub fn natural_cmp_path(a: &std::path::Path, b: &std::path::Path) -> Ordering {
    natural_cmp(&a.to_string_lossy(), &b.to_string_lossy())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cmp::Ordering::*;

    #[test]
    fn numeric_order() {
        let mut v = vec!["img10.jpg", "img2.jpg", "img1.jpg", "img20.jpg"];
        v.sort_by(|a, b| natural_cmp(a, b));
        assert_eq!(v, vec!["img1.jpg", "img2.jpg", "img10.jpg", "img20.jpg"]);
    }

    #[test]
    fn leading_zero() {
        // 数值相等时前导零少的在前
        assert_eq!(natural_cmp("1", "01"), Less);
        assert_eq!(natural_cmp("01", "001"), Less);
        assert_eq!(natural_cmp("1", "1"), Equal);
    }

    #[test]
    fn chinese_segments() {
        assert_eq!(natural_cmp("第2卷", "第10卷"), Less);
        assert_eq!(natural_cmp("第10卷", "第10卷"), Equal);
    }

    #[test]
    fn mixed() {
        assert_eq!(natural_cmp("a1b2", "a1b10"), Less);
        assert_eq!(natural_cmp("a2b1", "a10b1"), Less);
    }

    #[test]
    fn big_numbers() {
        assert_eq!(natural_cmp("x999999999999999999999", "x1000000000000000000000"), Less);
    }
}
