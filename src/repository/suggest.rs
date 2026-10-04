//! 名字打错时的「你是不是想找 X」。
//!
//! 命令行里最常见的错误就是拼写。只说「没有叫「officia」的仓库」会把一次输入
//! 失误变成一次翻文档 —— 而候选名单就在手上，没道理不给。
//!
//! 原则：**只在足够近的时候才建议**。乱猜一个不相关的名字比不给建议更烦人，
//! 所以阈值卡得比较紧（前缀/包含，或编辑距离 ≤ 2）。

/// 给一个打错的词找最近的候选；找不到像样的就返回 `None`。
///
/// `candidates` 里已经等于 `needle` 的项会被忽略（调用方既然在报「找不到」，
/// 就不会有精确命中）。
pub fn did_you_mean<'a, I>(needle: &str, candidates: I) -> Option<String>
where
    I: IntoIterator<Item = &'a str>,
{
    let needle = needle.trim().to_lowercase();
    if needle.is_empty() {
        return None;
    }

    let mut best: Option<(u8, String)> = None;
    for candidate in candidates {
        let lower = candidate.to_lowercase();
        if lower == needle {
            continue;
        }
        let Some(score) = closeness(&needle, &lower) else {
            continue;
        };
        if best
            .as_ref()
            .is_none_or(|(best_score, _)| score < *best_score)
        {
            best = Some((score, candidate.to_string()));
        }
    }
    best.map(|(_, name)| format!("你是不是想找「{name}」？"))
}

/// 越小的分数越近；`None` 表示「不够近，别建议」。
fn closeness(needle: &str, candidate: &str) -> Option<u8> {
    let needle_len = needle.chars().count();

    // 前缀：用户多半是少打、多打了尾巴。两个字符就够 —— `my` → `my-tools`。
    if needle_len >= 2 && (candidate.starts_with(needle) || needle.starts_with(candidate)) {
        return Some(0);
    }
    // 包含：要三个字符起。否则 `a` 会蹭上 `official` 里那个 a —— 那不叫建议，叫噪音。
    if needle_len >= 3 && (candidate.contains(needle) || needle.contains(candidate)) {
        return Some(1);
    }
    // 太短的词不做编辑距离 —— 「oci」和「git」的距离也是 2，猜错比不猜更糟。
    if needle.chars().count() < 4 || candidate.chars().count() < 4 {
        return None;
    }
    let distance = edit_distance(needle, candidate);
    (distance <= 2).then_some(distance + 1)
}

/// 两个词的编辑距离（Levenshtein，两行滚动数组就够）。
fn edit_distance(left: &str, right: &str) -> u8 {
    let left: Vec<char> = left.chars().collect();
    let right: Vec<char> = right.chars().collect();
    if left.is_empty() {
        return right.len().min(u8::MAX as usize) as u8;
    }
    if right.is_empty() {
        return left.len().min(u8::MAX as usize) as u8;
    }

    let mut previous: Vec<usize> = (0..=right.len()).collect();
    let mut current = vec![0usize; right.len() + 1];
    for (i, a) in left.iter().enumerate() {
        current[0] = i + 1;
        for (j, b) in right.iter().enumerate() {
            let cost = usize::from(a != b);
            current[j + 1] = (previous[j] + cost)
                .min(previous[j + 1] + 1)
                .min(current[j] + 1);
        }
        std::mem::swap(&mut previous, &mut current);
    }
    previous[right.len()].min(u8::MAX as usize) as u8
}

#[cfg(test)]
mod tests {
    use super::*;

    const REPOS: &[&str] = &["official", "community", "my-tools"];

    #[test]
    fn a_one_letter_typo_is_suggested() {
        // 这正是实测报障的那一次：officia → official
        assert_eq!(
            did_you_mean("officia", REPOS.iter().copied()),
            Some(String::from("你是不是想找「official」？"))
        );
        assert_eq!(
            did_you_mean("oficial", REPOS.iter().copied()),
            Some(String::from("你是不是想找「official」？"))
        );
        assert_eq!(
            did_you_mean("communty", REPOS.iter().copied()),
            Some(String::from("你是不是想找「community」？"))
        );
    }

    #[test]
    fn a_prefix_or_extra_tail_is_suggested() {
        assert_eq!(
            did_you_mean("my", REPOS.iter().copied()),
            Some(String::from("你是不是想找「my-tools」？"))
        );
        assert_eq!(
            did_you_mean("my-tools-extra", REPOS.iter().copied()),
            Some(String::from("你是不是想找「my-tools」？"))
        );
    }

    /// 不像的名字不要乱猜 —— 猜错比不给建议更糟。
    #[test]
    fn an_unrelated_name_gets_no_suggestion() {
        assert_eq!(did_you_mean("gitlab", REPOS.iter().copied()), None);
        assert_eq!(did_you_mean("xyz", REPOS.iter().copied()), None);
        assert_eq!(did_you_mean("", REPOS.iter().copied()), None);
        // 单字母不该靠「子串」蹭上 official 里那个 a。
        assert_eq!(did_you_mean("a", REPOS.iter().copied()), None);
        assert_eq!(did_you_mean("l", REPOS.iter().copied()), None);
    }

    /// 精确命中不算「差一点」，不该给建议。
    #[test]
    fn an_exact_match_is_not_a_suggestion() {
        assert_eq!(did_you_mean("official", REPOS.iter().copied()), None);
        assert_eq!(did_you_mean("OFFICIAL", REPOS.iter().copied()), None);
    }

    /// 多个候选时给最近的那个。
    #[test]
    fn the_closest_candidate_wins() {
        let candidates = ["community", "communion", "comune"];
        assert_eq!(
            did_you_mean("communit", candidates.iter().copied()),
            Some(String::from("你是不是想找「community」？"))
        );
        // 精确命中要跳过；而剩下两个都离 `comune` 太远，宁可不说。
        assert_eq!(did_you_mean("comune", candidates.iter().copied()), None);
        // 两个都在射程内时，前缀更近的那个赢。
        assert_eq!(
            did_you_mean("communit", ["communion", "community"].iter().copied()),
            Some(String::from("你是不是想找「community」？"))
        );
    }

    #[test]
    fn edit_distance_is_sane() {
        assert_eq!(edit_distance("official", "officia"), 1);
        assert_eq!(edit_distance("", "abc"), 3);
        assert_eq!(edit_distance("abc", "abc"), 0);
        assert_eq!(edit_distance("kitten", "sitting"), 3);
    }
}
