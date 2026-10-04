//! 版本比较。
//!
//! Repository 包用普通的语义化版本（\`1.2.0\`），但也要能吃下打包风格的
//! \`1.2.0-1\`（Arch 的 pkgver-pkgrel）以及带预发布的 \`1.2.0-alpha.1\`。
//!
//! 规则刻意做得简单、可预测、可测：
//!
//! * 按 \`.\` / \`-\` / \`_\` 切成段；
//! * 纯数字段是数字段，其余是文字段；
//! * 数字段之间按数值比；文字段之间按字典序比；
//! * 数字段 **大于** 文字段 —— 这样 \`1.2.0\` 比 \`1.2.0-alpha\` 新（预发布更旧），
//!   而 \`1.2.0-1\` 比 \`1.2.0\` 新（打包修订号更旧）；
//! * 前缀相同时，缺的那一段按 0 补（\`1.2\` == \`1.2.0\`），
//!   但对比方是文字段时，**短的那个更新**（\`1.2\` > \`1.2-alpha\`）。
//!
//! 这套规则不是为了兼容某个包管理器的全部怪癖，而是为了让
//! 「索引里的版本 vs 本地已装的版本」这个唯一用途给出正确结论。

use std::cmp::Ordering;
use std::fmt;

/// 一个可比较的版本号。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Version {
    segments: Vec<Segment>,
    /// 原始写法，报错和展示时原样回显。
    raw: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum Segment {
    Number(u64),
    Text(String),
}

impl Version {
    /// 解析。空串、只有分隔符的串算无效。
    pub fn parse(raw: &str) -> Option<Self> {
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            return None;
        }
        // 构建元数据（\`+\` 之后）不参与比较。
        let comparable = trimmed.split('+').next().unwrap_or(trimmed);

        let mut segments = Vec::new();
        for piece in comparable.split(['.', '-', '_']) {
            if piece.is_empty() {
                continue;
            }
            match piece.parse::<u64>() {
                Ok(number) => segments.push(Segment::Number(number)),
                Err(_) => segments.push(Segment::Text(piece.to_lowercase())),
            }
        }
        if segments.is_empty() {
            return None;
        }
        Some(Self {
            segments,
            raw: trimmed.to_string(),
        })
    }

    #[allow(dead_code)] // 原样回显版本
    pub fn as_str(&self) -> &str {
        &self.raw
    }

    /// \`self\` 是否比 \`other\` 新。
    pub fn is_newer_than(&self, other: &Self) -> bool {
        self.cmp(other) == Ordering::Greater
    }
}

impl fmt::Display for Version {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.raw)
    }
}

impl Ord for Version {
    fn cmp(&self, other: &Self) -> Ordering {
        let length = self.segments.len().max(other.segments.len());
        for index in 0..length {
            let left = self.segments.get(index);
            let right = other.segments.get(index);
            let ordering = match (left, right) {
                (Some(a), Some(b)) => compare_segments(a, b),
                // 一边没有这一段：按「补齐」规则决定。
                (Some(Segment::Number(value)), None) => value.cmp(&0),
                (None, Some(Segment::Number(value))) => 0.cmp(value),
                // 短的那个到了「发布版」，比带预发布后缀的长版本新。
                (None, Some(Segment::Text(_))) => Ordering::Greater,
                (Some(Segment::Text(_)), None) => Ordering::Less,
                (None, None) => Ordering::Equal,
            };
            if ordering != Ordering::Equal {
                return ordering;
            }
        }
        Ordering::Equal
    }
}

impl PartialOrd for Version {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

fn compare_segments(left: &Segment, right: &Segment) -> Ordering {
    match (left, right) {
        (Segment::Number(a), Segment::Number(b)) => a.cmp(b),
        (Segment::Text(a), Segment::Text(b)) => a.cmp(b),
        // 数字段比文字段新：1.2.0 > 1.2.0-alpha
        (Segment::Number(_), Segment::Text(_)) => Ordering::Greater,
        (Segment::Text(_), Segment::Number(_)) => Ordering::Less,
    }
}

/// 两个版本串比较；任一侧解析不出来就返回 \`None\`（调用方据此决定怎么提示）。
pub fn compare(left: &str, right: &str) -> Option<Ordering> {
    Some(Version::parse(left)?.cmp(&Version::parse(right)?))
}

/// \`candidate\` 是不是比 \`current\` 新；任一解析不了就返回 \`false\`。
///
/// 解析不了时返回 \`false\` 是有意的：版本号写坏时**不**提示升级，
/// 免得把一个读不懂的版本号当成「有新版」去覆盖本地安装。
pub fn is_upgrade(candidate: &str, current: &str) -> bool {
    match (Version::parse(candidate), Version::parse(current)) {
        (Some(newer), Some(older)) => newer.is_newer_than(&older),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_versions_compare_by_number_not_by_text() {
        // 字符串比较会得出 "1.10.0" < "1.9.0"，数值比较不会。
        assert!(is_upgrade("1.10.0", "1.9.0"));
        assert!(!is_upgrade("1.9.0", "1.10.0"));
        assert!(is_upgrade("2.0.0", "1.99.99"));
    }

    #[test]
    fn a_missing_segment_is_zero_but_a_missing_prerelease_is_newer() {
        assert_eq!(compare("1.2", "1.2.0"), Some(Ordering::Equal));
        assert!(is_upgrade("1.2", "1.2.0-alpha"));
        assert!(!is_upgrade("1.2.0-alpha", "1.2"));
        assert!(is_upgrade("1.0.0", "0.9.9"));
    }

    #[test]
    fn prereleases_sort_below_their_release() {
        assert!(is_upgrade("1.0.0", "1.0.0-alpha"));
        assert!(is_upgrade("1.0.0-alpha.2", "1.0.0-alpha.1"));
        assert!(is_upgrade("1.0.0-beta", "1.0.0-alpha"));
    }

    #[test]
    fn packaging_revisions_are_numeric_segments() {
        // Arch 的 pkgver-pkgrel：1.4.0-2 比 1.4.0-1 新，1.4.0 则等于 1.4.0-0。
        assert!(is_upgrade("1.4.0-2", "1.4.0-1"));
        assert!(is_upgrade("1.4.0-1", "1.4.0"));
        assert_eq!(compare("1.4.0", "1.4.0-0"), Some(Ordering::Equal));
    }

    #[test]
    fn build_metadata_is_ignored() {
        assert_eq!(compare("1.2.3+abc", "1.2.3+def"), Some(Ordering::Equal));
    }

    #[test]
    fn unreadable_versions_never_claim_an_upgrade() {
        assert_eq!(Version::parse(""), None);
        assert_eq!(Version::parse("   "), None);
        assert_eq!(Version::parse("..-"), None);
        // 写坏的版本号比「不提示升级」，绝不覆盖本地安装。
        assert!(!is_upgrade("不是版本", "1.0.0"));
        assert!(!is_upgrade("1.0.0", ""));
        assert_eq!(compare("1.0.0", ""), None);
        assert_eq!(compare("", "1.0.0"), None);
        // 非空但只有一个分隔符也算读不懂
        assert_eq!(Version::parse("..."), None);
    }

    #[test]
    fn text_versions_still_compare_somehow() {
        assert!(is_upgrade("v2", "v1"));
        assert_eq!(compare("alpha", "alpha"), Some(Ordering::Equal));
        assert!(is_upgrade("beta", "alpha"));
    }
}
