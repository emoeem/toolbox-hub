//! ToolHub Repository —— 工具 / 脚本 / manifest 的仓库。
//!
//! 和软件包中心不是一回事：那边是 Arch 的 pacman/AUR，这边是工具箱自己的工具
//! 仓库。两者共用的只有「先给你看清楚要干什么，再动手」这条纪律。
//!
//! | 模块 | 职责 |
//! | --- | --- |
//! | index | 索引格式（外部输入，版本化、向前兼容） |
//! | config | 有哪些仓库、开没开、信任等级 |
//! | cache | 索引缓存 + 条件抓取（离线优先） |
//! | installed | 已安装包的账本（卸载与更新靠它，不靠猜路径） |
//! | install | 计划 / 校验 / 安装 / 卸载 / 更新 |
//! | service | 服务层：CLI 与 TUI 共用的唯一入口 |
//! | worker | 把慢活搬到后台线程（UI 线程绝不联网、绝不哈希） |
//! | author | **作者工具**：脚手架 / 校验 / 可复现打包（给写插件的人） |
//! | paths | 安装路径安全（目录穿越、绝对路径…） |
//! | version | 版本比较（判断有没有新版） |

pub mod author;
pub mod cache;
pub mod cli;
pub mod config;
pub mod index;
pub mod install;
pub mod installed;
pub mod paths;
pub mod service;
pub mod version;
pub mod worker;

pub use service::Service;
