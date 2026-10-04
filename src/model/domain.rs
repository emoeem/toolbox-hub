/// 工具箱的一级分类（域）。
///
/// 每个域下面可以挂一个或多个 [`crate::providers::Provider`]（FFTools 脚本 /
/// 本地注解脚本 / TOML manifest）。域本身只是分类：某个域暂时没有工具时，
/// UI 会显示「Provider 待接入」而不是假装有内容 —— 所以加一个域是安全的。
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
    /// pacman / AUR 包管理（安装、卸载、更新、清理、查信息）。
    Packages,
    /// 给自己打包：私有 Arch 仓库的构建、发布与维护（CI、Build Plan、修复中心…）。
    Packaging,
    /// 工具仓库：搜索 / 安装 / 更新远程的 ToolHub 工具包（和 pacman 无关）。
    Discover,
}

impl Domain {
    /// 域的展示顺序，等于 Tabs 的顺序。
    pub const ALL: [Domain; 9] = [
        Domain::Media,
        Domain::Image,
        Domain::System,
        Domain::Network,
        Domain::Dev,
        Domain::Tools,
        // 包管理放第 7；「打包」追加到最后 —— 新域不该动你已经记住的 1-7。
        Domain::Packages,
        Domain::Packaging,
        // 「发现」也追加到最后：新域不该动你已经记住的 1-8。
        Domain::Discover,
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
            Domain::Packages => "包管理",
            Domain::Packaging => "打包",
            Domain::Discover => "发现",
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
            Domain::Packages => "pacman / AUR 包管理",
            Domain::Packaging => "给自己的软件包打包 / 私有仓库 / CI",
            Domain::Discover => "工具仓库 / 搜索 / 安装 / 更新",
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
            Domain::Packages => "packages",
            Domain::Packaging => "packaging",
            Domain::Discover => "discover",
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

    /// 在 [`Domain::ALL`] 中的下标，用于排序和按键映射（`1`..=`8`）。
    pub fn index(self) -> usize {
        Domain::ALL
            .iter()
            .position(|candidate| *candidate == self)
            .unwrap_or(0)
    }

    /// 从 `1`..=`8` 的数字键解析域（超出域数量的数字返回 `None`）。
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
        for (index, domain) in Domain::ALL.iter().enumerate() {
            let digit = char::from_digit(index as u32 + 1, 10).expect("数字");
            assert_eq!(Domain::from_digit(digit), Some(*domain), "{digit}");
        }
        assert_eq!(Domain::from_digit('0'), None);
        // 现在正好 9 个域：单个数字 1-9 刚好用满。再加域就没有数字键可用了，
        // 那时要么改成 g+数字 之类的前缀，要么接受新域没有快捷键。
        assert_eq!(Domain::ALL.len(), 9);
        assert_eq!(Domain::from_digit('9'), Some(Domain::ALL[8]));
        if Domain::ALL.len() < 9 {
            let beyond = char::from_digit(Domain::ALL.len() as u32 + 1, 10).expect("数字");
            assert_eq!(Domain::from_digit(beyond), None);
        }
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
