/// 工具箱的一级分类（域）。
///
/// 每个域下面可以挂一个或多个 [`crate::providers::Provider`]。
/// 当前只有 `Media` 域有真实 Provider（FFTools），其余域是待接入的空骨架：
/// 它们的工具数会是 0，UI 显示「Provider 待接入」，而不是假装有内容。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Domain {
    /// 音视频、字幕、转码 —— 当前由 FFTools Provider 提供。
    Media,
    /// 图像处理、批处理、格式转换。
    Image,
    /// 系统信息、电源、硬件。
    System,
    /// 下载、传输、网络诊断。
    Network,
    /// 开发工具、构建、仓库。
    Dev,
    /// 未归类的通用脚本。
    Tools,
}

impl Domain {
    /// 域的展示顺序，等于 Tabs 的顺序。
    pub const ALL: [Domain; 6] = [
        Domain::Media,
        Domain::Image,
        Domain::System,
        Domain::Network,
        Domain::Dev,
        Domain::Tools,
    ];

    /// Tabs 上的短标签。
    pub fn label(self) -> &'static str {
        match self {
            Domain::Media => "媒体",
            Domain::Image => "图像",
            Domain::System => "系统",
            Domain::Network => "网络",
            Domain::Dev => "开发",
            Domain::Tools => "工具",
        }
    }

    /// 空域提示与详情区用的一句话说明。
    pub fn description(self) -> &'static str {
        match self {
            Domain::Media => "音视频 / 字幕 / 转码",
            Domain::Image => "图像处理 / 批量转换",
            Domain::System => "系统信息 / 电源 / 硬件",
            Domain::Network => "下载 / 传输 / 网络诊断",
            Domain::Dev => "开发工具 / 构建 / 仓库",
            Domain::Tools => "未归类的通用脚本",
        }
    }

    /// 注解里可以写的英文标识（`# <名>:domain=system`）。
    pub fn id(self) -> &'static str {
        match self {
            Domain::Media => "media",
            Domain::Image => "image",
            Domain::System => "system",
            Domain::Network => "network",
            Domain::Dev => "dev",
            Domain::Tools => "tools",
        }
    }

    /// 解析注解里的 `domain=`：中文标签（`系统`）与英文标识（`system`）都接受，
    /// 大小写与首尾空白无关。解析不出来时返回 `None`，由调用方决定兜底策略 ——
    /// 一般不该因为写错一个域名就把工具丢掉。
    pub fn parse(raw: &str) -> Option<Self> {
        let needle = raw.trim().to_lowercase();
        Domain::ALL
            .into_iter()
            .find(|domain| needle == domain.label() || needle == domain.id())
    }

    /// 在 [`Domain::ALL`] 中的下标，用于排序和按键映射（`1`..=`6`）。
    pub fn index(self) -> usize {
        Domain::ALL
            .iter()
            .position(|candidate| *candidate == self)
            .unwrap_or(0)
    }

    /// 从 `1`..=`6` 的数字键解析域。
    pub fn from_digit(digit: char) -> Option<Self> {
        let index = digit.to_digit(10)? as usize;
        if (1..=Domain::ALL.len()).contains(&index) {
            Some(Domain::ALL[index - 1])
        } else {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::Domain;

    #[test]
    fn all_domains_have_distinct_labels_and_descriptions() {
        let labels: Vec<_> = Domain::ALL.iter().map(|d| d.label()).collect();
        for (i, label) in labels.iter().enumerate() {
            assert!(!label.is_empty());
            assert_eq!(
                labels.iter().position(|l| l == label),
                Some(i),
                "标签重复: {label}"
            );
        }
        assert!(Domain::ALL.iter().all(|d| !d.description().is_empty()));
    }

    #[test]
    fn index_matches_all_order() {
        for (i, domain) in Domain::ALL.iter().enumerate() {
            assert_eq!(domain.index(), i);
        }
    }

    #[test]
    fn digit_keys_map_to_all_domains_and_reject_the_rest() {
        assert_eq!(Domain::from_digit('1'), Some(Domain::Media));
        assert_eq!(Domain::from_digit('6'), Some(Domain::Tools));
        assert_eq!(Domain::from_digit('0'), None);
        assert_eq!(Domain::from_digit('7'), None);
        assert_eq!(Domain::from_digit('x'), None);
    }

    #[test]
    fn parse_accepts_chinese_labels_and_english_ids() {
        for domain in Domain::ALL {
            assert_eq!(Domain::parse(domain.label()), Some(domain));
            assert_eq!(Domain::parse(domain.id()), Some(domain));
            assert_eq!(Domain::parse(&domain.id().to_uppercase()), Some(domain));
            assert_eq!(
                Domain::parse(&format!("  {}  ", domain.label())),
                Some(domain)
            );
        }
        assert_eq!(Domain::parse("星际"), None);
        assert_eq!(Domain::parse(""), None);
    }

    #[test]
    fn ids_are_unique_and_do_not_collide_with_labels() {
        let ids: Vec<_> = Domain::ALL.iter().map(|domain| domain.id()).collect();
        for (i, id) in ids.iter().enumerate() {
            assert_eq!(ids.iter().position(|candidate| candidate == id), Some(i));
            // 一个域的英文 id 不能等于另一个域的中文标签，否则 parse 会有歧义。
            for domain in Domain::ALL {
                assert_ne!(*id, domain.label());
            }
        }
    }
}
