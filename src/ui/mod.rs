//! 渲染层：只读 [`App`]，不修改状态。
//!
//! 布局自上而下固定五段：
//!
//! ```text
//! ┌ 顶部栏 ─── 标题 / Provider / 计数 / 心跳 ─── 搜索框 ─┐
//! │ 域 Tabs：媒体 图像 系统 网络 开发 工具              │
//! │ 二级筛选（仅当当前域有分类时出现）                   │
//! │ 工具表格                                            │
//! │ 当前工具详情                                        │
//! └ 状态栏 + 快捷键 ────────────────────────────────────┘
//! ```

pub mod theme;

mod detail;
mod files;
mod footer;
mod form;
mod header;
mod help;
mod history;
mod packages;
mod picker;
mod run;
mod table;
mod tabs;
mod viewer;

use ratatui::{
    layout::{Constraint, Layout, Rect},
    style::{Modifier, Style},
    text::{Line, Span, Text},
    widgets::{Block, Paragraph},
};

use std::path::Path;

use crate::app::{App, Scope};

/// 把 `$HOME` 缩写成 `~`，让长路径在状态行与预览里放得下。
pub(crate) fn short_path(path: &Path) -> String {
    let text = path.display().to_string();
    let Some(home) = std::env::var_os("HOME") else {
        return text;
    };
    let home = home.to_string_lossy().to_string();
    if home.is_empty() {
        return text;
    }
    if text == home {
        return String::from("~");
    }
    match text.strip_prefix(&format!("{home}/")) {
        Some(rest) => format!("~/{rest}"),
        None => text,
    }
}

pub fn draw(frame: &mut ratatui::Frame, app: &mut App) {
    let area = frame.area();
    frame.render_widget(Block::default().style(Style::default().bg(theme::BG)), area);

    // 顶部栏 4 行（边框 + 标题 + 搜索/提示）、域 Tabs 2 行、二级筛选 2 行、
    // 表格、详情、状态栏。低于最低高度就无法表达两级层级，宁可给提示也不挤成一团。
    //
    // 跨域搜索时也要留出这一条：它显示的是「命中落在哪些域」，比分类条更重要。
    // 参数表单模式下则整条省掉，把行数让给字段。
    let show_sub = app.viewer.is_none()
        && app.history.is_none()
        && app.files.is_none()
        && app.packages.is_none()
        && app.picker.is_none()
        && app.form.is_none()
        && (app.is_global_search() || app.scope != Scope::All || app.has_sub_tabs());
    // 24 行是最常见的终端默认高度，所以布局按它来卡：
    // 顶栏 4 + 域 2 (+ 二级筛选 2) + 表格 ≥6 + 详情 8 + 底栏 2 = 24（带筛选行时）。
    // 实测过：早先要求 26 行，80×24 的终端直接显示「终端窗口太小」，等于不能用。
    let (table_min, detail_height) = if area.height >= 30 { (9, 10) } else { (6, 8) };
    let min_height = if show_sub { 24 } else { 22 };
    if area.height < min_height || area.width < 72 {
        draw_too_small(frame, area, min_height);
        return;
    }

    let mut constraints = vec![Constraint::Length(4), Constraint::Length(2)];
    if show_sub {
        constraints.push(Constraint::Length(2));
    }
    constraints.push(Constraint::Min(table_min));
    constraints.push(Constraint::Length(detail_height));
    constraints.push(Constraint::Length(2));

    let chunks = Layout::vertical(constraints).split(area);
    let mut rows = chunks.iter().copied();

    let (Some(header), Some(domains)) = (rows.next(), rows.next()) else {
        return;
    };
    header::draw(frame, app, header);
    tabs::draw_domains(frame, app, domains);

    if show_sub && let Some(sub) = rows.next() {
        if app.is_global_search() || app.scope != Scope::All {
            tabs::draw_scope(frame, app, sub);
        } else {
            tabs::draw_sub(frame, app, sub);
        }
    }

    if let (Some(list), Some(detail), Some(footer)) = (rows.next(), rows.next(), rows.next()) {
        // 输出视图占满表格 + 详情两块；表单模式顶掉表格与详情；
        // 都没有就是普通的列表 + 详情。
        if app.viewer.is_some() {
            viewer::draw(frame, app, list.union(detail));
        } else if app.history.is_some() {
            history::draw(frame, app, list.union(detail));
        } else if app.packages.is_some() {
            packages::draw(frame, app, list.union(detail));
        } else if app.files.is_some() {
            files::draw(frame, app, list.union(detail));
        } else if app.picker.is_some() {
            picker::draw(frame, app, list.union(detail));
        } else if app.form.is_some() {
            form::draw_fields(frame, app, list);
            form::draw_preview(frame, app, detail);
        } else {
            table::draw(frame, app, list);
            // 有任务在跑就用「执行中」面板顶掉详情区：列表照常可用，进度也看得见。
            if app.running.is_some() {
                run::draw(frame, app, detail);
            } else {
                detail::draw(frame, app, detail);
            }
        }
        footer::draw(frame, app, footer);

        // 帮助屏最后画 —— 它是盖在所有东西上面的一层。
        help::draw(frame, app);
    }
}

fn draw_too_small(frame: &mut ratatui::Frame, area: Rect, min_height: u16) {
    let text = Text::from(vec![
        Line::from(Span::styled(
            "Toolbox",
            Style::default()
                .fg(theme::PURPLE)
                .add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
        Line::from("终端窗口太小"),
        Line::from(format!(
            "当前: {} × {}   需要: ≥72 × ≥{}",
            area.width, area.height, min_height
        )),
        Line::from("请放大终端窗口后继续。"),
    ]);
    frame.render_widget(Paragraph::new(text).block(theme::panel(" Toolbox ")), area);
}

/// 用 `TestBackend` 真渲染一帧，断言「界面上到底画出了什么」。
///
/// 这一层测试直接覆盖那类只有跑起来才看得见的 bug ——
/// 例如顶部栏高度不足导致第二行被边框裁掉、空域没有提示文案。
#[cfg(test)]
mod tests {
    use std::{io, path::PathBuf};

    // 导入 Backend 才能调 TestBackend 的 `clear()`（它就是「物理屏被清空」那一步）。
    use ratatui::{
        Terminal,
        backend::{Backend, TestBackend},
    };

    use super::draw;
    use crate::{
        app::App,
        model::{Danger, Domain, RunMode, ToolDefinition},
        providers::{Discovery, Provider},
        registry::Registry,
    };

    /// 造一个假的 Provider。用它而不是直接塞工具集，是为了让 App 走完整的
    /// reload 流程 —— 顶部栏的 Provider 名、域计数、二级筛选都来自真实路径。
    fn provider(
        id: &'static str,
        label: &'static str,
        tools: Vec<ToolDefinition>,
    ) -> Box<dyn Provider> {
        struct Stub {
            id: &'static str,
            label: &'static str,
            tools: Vec<ToolDefinition>,
        }

        impl Provider for Stub {
            fn id(&self) -> &'static str {
                self.id
            }

            fn label(&self) -> &'static str {
                self.label
            }

            fn discover(&self) -> io::Result<Discovery> {
                Ok(Discovery::clean(self.tools.clone()))
            }
        }

        Box::new(Stub { id, label, tools })
    }

    fn tool(
        id: &str,
        name: &str,
        provider: &str,
        domain: Domain,
        tags: &[&str],
        summary: &str,
    ) -> ToolDefinition {
        ToolDefinition {
            id: id.to_string(),
            name: name.to_string(),
            provider: provider.to_string(),
            domain,
            tags: tags.iter().map(|tag| (*tag).to_string()).collect(),
            summary: summary.to_string(),
            input: None,
            output: None,
            features: None,
            requires: Vec::new(),
            missing_deps: Vec::new(),
            install_hint: None,
            action: None,
            mode: RunMode::Interactive,
            danger: Danger::Safe,
            path: PathBuf::from(format!("/tmp/{name}")),
            ready: true,
        }
    }

    /// 两个 Provider、两个域，与真实情况同构（FFTools 占媒体，本地脚本铺其它域），
    /// 这样宽度、域计数、二级筛选的断言才有意义。
    fn app() -> App {
        let mut registry = Registry::from_providers(vec![
            provider(
                "fftools",
                "FFTools",
                vec![
                    tool(
                        "fftools:trim-video",
                        "trim-video",
                        "FFTools",
                        Domain::Media,
                        &["编辑"],
                        "裁剪视频",
                    ),
                    tool(
                        "fftools:convert-media",
                        "convert-media",
                        "FFTools",
                        Domain::Media,
                        &["转码"],
                        "格式转换",
                    ),
                ],
            ),
            provider(
                "scripted",
                "本地脚本",
                vec![tool(
                    "scripted:sysinfo",
                    "sysinfo",
                    "本地脚本",
                    Domain::System,
                    &["信息"],
                    "系统信息",
                )],
            ),
        ]);
        let report = registry.reload();
        App::new(registry, PathBuf::from("/home/emo/.local/bin"), report)
    }

    /// 渲染一帧，逐单元拼出文本。
    ///
    /// 注意：CJK 是宽字符，占两个单元，缓冲区里第二个单元留的是默认空格，
    /// 所以原始结果里会出现「媒 体」这种空档。做 `contains` 断言请用
    /// [`render_compact`]。
    fn render(app: &mut App, width: u16, height: u16) -> String {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).expect("测试终端");
        terminal.draw(|frame| draw(frame, app)).expect("渲染一帧");

        let buffer = terminal.backend().buffer();
        let area = buffer.area();
        let mut text = String::new();
        for y in area.top()..area.bottom() {
            for x in area.left()..area.right() {
                text.push_str(buffer.cell((x, y)).map_or(" ", |cell| cell.symbol()));
            }
            text.push('\n');
        }
        text
    }

    /// 去掉全部空白后的渲染结果，用来绕开宽字符补齐留下的空档。
    fn render_compact(app: &mut App, width: u16, height: u16) -> String {
        render(app, width, height)
            .chars()
            .filter(|c| !c.is_whitespace())
            .collect()
    }

    /// 缓冲区里的文字（去掉空白，便于断言）。
    fn buffer_compact(terminal: &Terminal<TestBackend>) -> String {
        let buffer = terminal.backend().buffer();
        let area = buffer.area();
        let mut text = String::new();
        for y in area.top()..area.bottom() {
            for x in area.left()..area.right() {
                text.push_str(buffer.cell((x, y)).map_or(" ", |cell| cell.symbol()));
            }
        }
        text.chars().filter(|c| !c.is_whitespace()).collect()
    }

    /// 回归测试：这正是「跑完一个交互式脚本回来，界面只剩一行字」的真凶。
    ///
    /// ratatui 是双缓冲差分渲染：接管终端时 `LeaveAlternateScreen` 把**物理屏**清了，
    /// 但 ratatui 的内部缓冲还记着接管前那一帧。于是回来后的第一次 `draw` 认为
    /// 「屏幕没变化」，一个格子都不写 —— 用户看到的就是一片空白加底部一行状态。
    /// 修法是先 `Terminal::clear()` 丢掉旧帧（主循环里的 `take_full_redraw`）。
    #[test]
    fn a_wiped_screen_needs_a_clear_before_the_next_draw() {
        let mut app = app();
        // 尺寸要够大：太小的话 draw 会走「终端窗口太小」分支，压根不画表格。
        let mut terminal = Terminal::new(TestBackend::new(100, 30)).expect("测试终端");
        terminal.draw(|frame| draw(frame, &mut app)).expect("首帧");
        assert!(buffer_compact(&terminal).contains("工具"), "首帧应该有内容");

        // 模拟离开备用屏幕：物理屏被清空，ratatui 的旧帧缓冲还在。
        terminal.backend_mut().clear().expect("清空物理屏");

        // 直接重画：差分认为没变化，于是什么都不写 —— bug 复现。
        terminal.draw(|frame| draw(frame, &mut app)).expect("重画");
        let after_plain = buffer_compact(&terminal);
        println!("清空后直接重画，屏幕上剩下: {after_plain:?}");
        assert!(
            !after_plain.contains("工具"),
            "复现：屏幕被清空后直接重画不会恢复界面（这就是用户看到的那一屏）: {after_plain:?}"
        );

        // 先丢掉旧帧再画：整屏被重写。
        terminal.clear().expect("丢旧帧");
        terminal.draw(|frame| draw(frame, &mut app)).expect("重画");
        assert!(
            buffer_compact(&terminal).contains("工具"),
            "clear 之后必须恢复成完整界面"
        );
    }

    /// 主循环用它决定「这一帧要不要先 clear」。
    #[test]
    fn a_full_redraw_is_requested_once_and_taken_once() {
        let mut app = app();
        assert!(!app.take_full_redraw(), "默认不需要整屏重画");
        app.request_full_redraw();
        assert!(app.take_full_redraw(), "请求过就要取到");
        assert!(!app.take_full_redraw(), "取过一次就不该再重复整屏重画");
    }

    #[test]
    fn header_draws_both_rows_with_providers_and_counts() {
        let mut app = app();
        let text = render_compact(&mut app, 100, 30);

        assert!(text.contains("Toolbox"), "{text}");
        assert!(text.contains("Linux工具箱"), "{text}");
        assert!(
            text.contains("FFTools/本地脚本"),
            "顶部栏应列出全部已接入 Provider: {text}"
        );
        // 分母是全部工具数（媒体 2 + 系统 1），分子是当前域命中数。
        assert!(text.contains("2/3"), "顶部栏应显示命中/总数: {text}");
        // 曾经的 bug：顶部栏高度不足，第二行被边框整个裁掉。
        // 第二行现在显示「工作目录」（工具在哪儿执行/找文件）+「扫描」（脚本来自哪）。
        assert!(
            text.contains("扫描~/.local/bin"),
            "顶部栏第二行必须可见: {text}"
        );
        assert!(text.contains("工作目录"), "{text}");
        assert!(text.contains("d改目录"), "要提示这个目录改得动: {text}");
    }

    /// 工作目录决定了扫目录的脚本能不能找到文件，所以它必须一直看得见。
    #[test]
    fn the_header_shows_the_work_dir() {
        let mut app = app();
        app.work_dir = PathBuf::from("/home/emo/Pictures");

        let text = render_compact(&mut app, 100, 30);
        assert!(text.contains("工作目录"), "{text}");
        assert!(text.contains("Pictures"), "要显示当前工作目录: {text}");
    }

    /// 按 `d` 时的输入行。
    #[test]
    fn the_dir_input_line_shows_what_you_are_typing() {
        let mut app = app();
        app.open_dir_input();
        app.dir_input = Some(String::from("~/Pictures"));

        let text = render_compact(&mut app, 100, 30);
        assert!(text.contains("工作目录→"), "{text}");
        assert!(text.contains("~/Pictures"), "{text}");
        assert!(text.contains("Enter确认"), "要提示怎么确认: {text}");
    }

    /// 窄终端下 Provider 列表不该被右侧状态区挤掉。
    #[test]
    fn header_keeps_the_provider_list_visible_at_80_columns() {
        let mut app = app();
        let text = render_compact(&mut app, 80, 30);
        assert!(
            text.contains("FFTools/本地脚本"),
            "80 列下 Provider 列表必须完整可见: {text}"
        );
        assert!(text.contains("2/3"), "{text}");
    }

    #[test]
    fn search_mode_swaps_the_header_row_and_the_footer_hint() {
        let mut app = app();
        app.searching = true;
        app.query = "trim".to_string();
        app.apply_filter();

        let text = render_compact(&mut app, 100, 30);
        // 光标方块只可能来自搜索框，用它证明那一行确实画出来了。
        assert!(text.contains("/trim▌"), "搜索框与光标必须可见: {text}");
        assert!(text.contains("输入搜索"), "底部提示应切成搜索态: {text}");
        assert!(
            !text.contains("扫描/home/emo/.local/bin"),
            "搜索态不该再显示扫描提示: {text}"
        );
    }

    #[test]
    fn sub_tab_row_appears_only_for_domains_with_tags() {
        let mut app = app();
        // 「全部」只可能来自二级筛选条，不会出现在表格或详情里。
        assert!(
            render_compact(&mut app, 100, 30).contains("全部"),
            "有分类的域应显示筛选条"
        );

        // 系统域有工具也有分类：筛选条在，工具也在。
        app.switch_domain(Domain::System.index());
        let system = render_compact(&mut app, 100, 30);
        assert!(system.contains("全部"), "系统域应有筛选条: {system}");
        assert!(system.contains("sysinfo"), "{system}");

        // 图像域既没有工具也没有分类：筛选条消失，给出「待接入」说明。
        app.switch_domain(Domain::Image.index());
        let empty = render_compact(&mut app, 100, 30);
        assert!(!empty.contains("全部"), "空域不该有筛选条: {empty}");
        assert!(
            empty.contains("Provider待接入"),
            "空域要有「待接入」说明: {empty}"
        );
        assert!(empty.contains("图像"), "{empty}");
    }

    #[test]
    fn marked_queue_is_visible_in_the_footer() {
        let mut app = app();
        // 工具按名字排序，所以当前高亮项是 convert-media 而不是 trim-video：
        // 断言跟着 App 的实际选择走，不依赖注册顺序。
        let name = app.current().expect("应有高亮工具").name.clone();
        app.toggle_mark();

        let text = render_compact(&mut app, 100, 30);
        assert!(text.contains("队列1"), "标记后底部应显示队列数: {text}");
        assert!(
            text.contains(&format!("●{name}")),
            "表格里应有标记圆点: {text}"
        );
    }

    #[test]
    fn tiny_terminal_falls_back_to_a_hint_instead_of_panicking() {
        let mut app = app();
        let text = render_compact(&mut app, 60, 12);
        assert!(text.contains("终端窗口太小"), "{text}");
        assert!(text.contains("当前:60×12"), "{text}");
    }

    #[test]
    fn cross_domain_search_explains_where_the_hits_came_from() {
        let mut app = app();
        app.searching = true;
        // sysinfo 属于系统域，而当前停在媒体域：跨域搜索要能找到并说明。
        app.query = "sysinfo".to_string();
        app.apply_filter();

        let text = render_compact(&mut app, 100, 30);
        assert!(
            text.contains("跨域搜索命中1个"),
            "要说明这是跨域结果: {text}"
        );
        assert!(
            text.contains("sysinfo"),
            "跨域命中的工具要出现在表里: {text}"
        );
        // 表格多了一列「域」，跨域结果才辨认得出属于哪个域。
        assert!(text.contains("工具域Provider分类状态说明"), "{text}");
    }

    #[test]
    fn browsing_still_shows_the_domain_scoped_tag_row() {
        let mut app = app();
        let text = render_compact(&mut app, 100, 30);
        assert!(
            !text.contains("跨域搜索"),
            "没搜索时不该出现跨域说明: {text}"
        );
        assert!(text.contains("全部"), "浏览时显示二级筛选条: {text}");
    }

    /// 收藏的工具带星标；切到收藏视图后那一行要说明「跨所有域」和怎么切回去。
    #[test]
    fn favorites_show_a_star_and_the_view_row_explains_itself() {
        let mut app = app();
        app.state = crate::state::State::default();
        let id = app.registry.tools()[app.filtered[0]].id.clone();
        app.state.toggle_favorite(&id);
        app.apply_filter();

        let text = render_compact(&mut app, 100, 30);
        assert!(text.contains('★'), "收藏的工具要有星标: {text}");

        app.cycle_scope();
        let text = render_compact(&mut app, 100, 30);
        assert!(text.contains("★收藏"), "{text}");
        assert!(text.contains("跨所有域"), "视图是跨域的，要说清楚: {text}");
        assert!(text.contains("v切换视图"), "要提示怎么切回去: {text}");
        assert_eq!(app.filtered.len(), 1, "只剩收藏的那件");
    }

    /// 执行历史：时间 / 工具 / 结果 / 耗时 / 命令，外加「可以重跑」的提示。
    #[test]
    fn history_view_lists_past_runs() {
        let mut app = app();
        app.history = Some(crate::app::HistoryView {
            entries: vec![
                crate::history::Entry {
                    epoch: crate::history::now_epoch(),
                    tool_id: "manifest:jq-query".to_string(),
                    tool_name: "jq 查询 JSON".to_string(),
                    argv: vec![
                        "/usr/bin/jq".to_string(),
                        ".".to_string(),
                        "/tmp/x.json".to_string(),
                    ],
                    success: true,
                    millis: 1200,
                    values: std::collections::BTreeMap::new(),
                },
                crate::history::Entry {
                    epoch: crate::history::now_epoch(),
                    tool_id: "manifest:7z-extract".to_string(),
                    tool_name: "7z 解压".to_string(),
                    argv: vec!["/usr/bin/7z".to_string(), "x".to_string()],
                    success: false,
                    millis: 300,
                    values: std::collections::BTreeMap::new(),
                },
            ],
            selected: 0,
        });

        let text = render_compact(&mut app, 100, 30);
        assert!(text.contains("执行历史"), "{text}");
        assert!(text.contains("jq查询JSON"), "{text}");
        assert!(
            text.contains("成功") && text.contains("失败"),
            "两种结果都要能看出来: {text}"
        );
        assert!(text.contains("1.20s"), "耗时: {text}");
        assert!(text.contains("/tmp/x.json"), "命令要看得见: {text}");
        assert!(text.contains("Enter重跑"), "要提示能重跑: {text}");
    }

    /// 回归测试：80×24（最常见的终端默认尺寸）必须能正常渲染。
    ///
    /// 曾经要求 ≥26 行，于是 24 行的终端一打开就是「终端窗口太小」。
    #[test]
    fn a_standard_24_row_terminal_renders() {
        let mut standard = app();
        let text = render_compact(&mut standard, 80, 24);
        assert!(!text.contains("终端窗口太小"), "80×24 应该能用: {text}");
        assert!(text.contains("Toolbox"), "顶栏要在: {text}");
        assert!(text.contains("工具"), "表格要在: {text}");

        // 更矮就该明确说太小（而不是画出一堆挤压的块）
        let mut tiny = app();
        let text = render_compact(&mut tiny, 80, 18);
        assert!(text.contains("终端窗口太小"), "18 行确实放不下: {text}");
    }

    /// 执行中面板：命令、耗时、进度、输出尾巴、取消提示。
    #[test]
    fn the_running_panel_shows_command_progress_and_tail() {
        let mut app = app();
        // 造一个真在跑的任务（sleep 让它活着，跑完也不用管）
        let job = crate::runtime::spawn_captured(
            &PathBuf::from("/usr/bin/sleep"),
            &[String::from("5")],
            &PathBuf::from("/tmp"),
            "长任务",
        )
        .expect("spawn");
        app.running = Some(crate::app::RunningJobView::for_test(
            job,
            vec![
                (false, String::from("第一行输出")),
                (true, String::from("警告：第二行")),
            ],
            vec![
                (String::from("out_time"), String::from("00:00:03")),
                (String::from("speed"), String::from("1.5x")),
            ],
        ));

        // 探到总时长 → 该画进度条（out_time=3s / 总 10s = 30%）
        app.running.as_mut().expect("running").total_seconds = Some(10.0);

        let text = render_compact(&mut app, 100, 30);
        assert!(text.contains("执行中"), "{text}");
        assert!(text.contains("sleep5"), "要显示真正在跑的命令: {text}");
        assert!(
            text.contains("30.0%"),
            "explored 总时长就该有百分比: {text}"
        );
        assert!(text.contains('█'), "要有实心段: {text}");
        assert!(text.contains('░'), "要有空心段: {text}");
        assert!(text.contains("00:03/00:10"), "要显示 已完成/总时长: {text}");
        assert!(text.contains("已跑"), "要有耗时: {text}");
        assert!(
            text.contains("out_time=00:00:03"),
            "要显示结构化进度: {text}"
        );
        assert!(text.contains("speed=1.5x"), "{text}");
        assert!(text.contains("第一行输出"), "要有实时输出: {text}");
        assert!(text.contains("警告：第二行"), "stderr 也要显示: {text}");
        assert!(text.contains("q取消"), "要提示能取消: {text}");
    }

    /// 选文件：目录排在前面、文件带体积、底部讲清按键。
    #[test]
    fn the_picker_lists_files_and_explains_its_keys() {
        let base =
            std::env::temp_dir().join(format!("toolbox-hub-picker-ui-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(base.join("sub")).expect("mkdir");
        std::fs::write(base.join("clip.mp4"), vec![0u8; 2048]).expect("write");

        let mut app = app();
        app.picker = Some(crate::app::Picker::open(&base, 0, false, false));

        let text = render_compact(&mut app, 100, 30);
        assert!(text.contains("选文件"), "{text}");
        assert!(text.contains("sub/"), "目录要能看出来: {text}");
        assert!(text.contains("clip.mp4"), "{text}");
        assert!(text.contains("2.0KB"), "文件要显示体积: {text}");
        assert!(text.contains("Enter选中"), "要讲清按键: {text}");
        assert!(text.contains("打字过滤"), "{text}");

        let _ = std::fs::remove_dir_all(&base);
    }

    /// 帮助屏：把各模式的键位都列出来，而且盖在界面上面。
    #[test]
    fn the_help_overlay_lists_the_keys_of_every_mode() {
        let mut app = app();
        assert!(!app.is_help_open());
        app.open_help();

        // 用足够高的终端一次看全（矮终端要靠滚动，另有一条断言盯着）。
        // 帮助内容会随着功能长，所以这里给得宽一点。
        let text = render_compact(&mut app, 110, 100);
        for expected in [
            "帮助",
            "列表",
            "文件视图",
            "表单",
            "选文件",
            "执行中",
            "输出视图",
            "执行历史",
        ] {
            assert!(text.contains(expected), "帮助里缺「{expected}」: {text}");
        }
        assert!(text.contains("Ctrl-F"), "表单的挑文件键要写出来: {text}");
        assert!(
            text.contains("F看工作目录里的媒体文件"),
            "F 也要写进去: {text}"
        );
        assert!(text.contains("q关闭"), "要说明怎么关: {text}");
    }

    /// 文件视图：名字、体积、相对路径，以及「这里没有媒体文件」的明说。
    #[test]
    fn the_files_view_lists_media_files() {
        let base =
            std::env::temp_dir().join(format!("toolbox-hub-files-ui-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(base.join("子目录")).expect("mkdir");
        std::fs::write(base.join("clip.mp4"), vec![0u8; 2048]).expect("write");
        std::fs::write(base.join("子目录/music.m4a"), vec![0u8; 1024]).expect("write");

        let mut app = app();
        app.work_dir = std::fs::canonicalize(&base).expect("canonical");
        app.refresh_media();
        app.open_files();

        let text = render_compact(&mut app, 100, 30);
        assert!(text.contains("文件"), "{text}");
        assert!(text.contains("clip.mp4"), "{text}");
        assert!(text.contains("music.m4a"), "子目录里的也要列出来: {text}");
        assert!(text.contains("2.0KB"), "要显示体积: {text}");
        assert!(text.contains("2个媒体文件"), "{text}");
        assert!(
            text.contains("Enter切到该文件所在目录"),
            "要讲清 Enter 干什么: {text}"
        );

        // 头部同时也要能看到这个数
        app.close_files();
        let head = render_compact(&mut app, 100, 30);
        assert!(head.contains("2个媒体文件"), "头部要显示数量: {head}");

        let _ = std::fs::remove_dir_all(&base);
    }

    /// 输出视图：顶掉列表与详情，显示捕获到的内容与真正跑的命令。
    #[test]
    fn viewer_replaces_the_list_and_shows_captured_output() {
        let mut app = app();
        let captured = crate::runtime::Captured {
            label: "jq".to_string(),
            command: "/usr/bin/jq . /tmp/x.json".to_string(),
            stdout: "hello-output\n".to_string(),
            stderr: String::new(),
            status: Some(0),
            success: true,
            elapsed: std::time::Duration::from_millis(12),
            cancelled: false,
        };
        app.open_viewer(
            crate::app::Viewer::from_captured(std::slice::from_ref(&captured)).expect("有输出"),
        );

        let text = render_compact(&mut app, 100, 30);
        assert!(text.contains("hello-output"), "要看得到捕获的输出: {text}");
        assert!(text.contains("/usr/bin/jq"), "标题是真正跑的命令: {text}");
        assert!(text.contains("退出码0"), "状态行要有退出码: {text}");
        assert!(text.contains("输出视图"), "顶部要说明这是输出视图: {text}");
        assert!(text.contains("q关闭"), "底部要提示怎么关: {text}");
    }

    /// 表单模式：表格与详情区换成了字段列表与命令预览。
    #[test]
    fn form_mode_shows_fields_and_the_command_preview() {
        let action_tool = crate::providers::manifest::bundled_tools()
            .into_iter()
            .next()
            .expect("至少应有一个内置动作");
        let mut registry =
            Registry::from_providers(vec![provider("manifest", "Manifest", vec![action_tool])]);
        let report = registry.reload();
        let mut app = App::new(registry, PathBuf::from("/tmp/bin"), report);

        // 打开之前是普通列表
        let before = render_compact(&mut app, 110, 32);
        assert!(!before.contains("填写参数"), "{before}");
        assert!(!before.contains("将要执行"), "{before}");

        assert!(app.open_form(), "带参数的工具应该打开表单");
        app.form_activate(); // 进入「视频链接」输入态
        for ch in "https://example.com/v".chars() {
            app.form_push_char(ch);
        }
        app.form_end_edit();

        let text = render_compact(&mut app, 110, 32);
        assert!(text.contains("填写参数"), "表单模式要有提示: {text}");
        assert!(text.contains("将要执行"), "要有命令预览区: {text}");
        assert!(text.contains("视频链接*"), "必填字段要标星: {text}");
        assert!(text.contains("最好"), "Choice 字段显示选项标签: {text}");
        assert!(
            text.contains("--no-mtime"),
            "预览要显示真正会跑的命令: {text}"
        );
        assert!(
            text.contains("https://example.com/v"),
            "预览要带上填好的链接: {text}"
        );
        assert!(
            text.contains("不经过shell"),
            "要说明参数是逐个原样传递的: {text}"
        );
        // 扫目录的脚本找不到文件时，第一眼就该看见「在哪个目录里跑」。
        assert!(text.contains("目录"), "预览要写明工作目录: {text}");
        assert!(
            crate::ui::short_path(&app.work_dir)
                .chars()
                .filter(|c| !c.is_whitespace())
                .all(|c| text.contains(c)),
            "预览里的工作目录应是 {:?}: {text}",
            crate::ui::short_path(&app.work_dir)
        );
    }

    /// 缺依赖的工具，详情区要说清「缺什么」和「怎么装」。
    #[test]
    fn detail_shows_what_is_missing_and_how_to_install_it() {
        let mut exif = tool(
            "exif-clean",
            "exif-clean",
            "Manifest",
            Domain::Image,
            &["元数据"],
            "清理图片 EXIF",
        );
        exif.ready = false;
        exif.requires = vec!["exiftool".to_string()];
        exif.missing_deps = vec!["exiftool".to_string()];
        exif.install_hint = Some("sudo pacman -S perl-image-exiftool".to_string());

        let mut registry =
            Registry::from_providers(vec![provider("manifest", "Manifest", vec![exif])]);
        let report = registry.reload();
        let mut app = App::new(registry, PathBuf::from("/tmp/bin"), report);
        app.switch_domain(Domain::Image.index());

        let text = render_compact(&mut app, 100, 30);
        assert!(text.contains("✗exiftool"), "依赖行要标出缺失项: {text}");
        // 注意 render_compact 会去掉空格，这里断言不含空格的片段。
        assert!(
            text.contains("安装sudopacman-Sperl-image-exiftool"),
            "详情区要显示安装办法: {text}"
        );
    }

    /// 打印用：宽字符在缓冲区里占两格、第二格是补齐的空格，去掉它才像真终端里的样子。
    fn pretty(text: &str) -> String {
        fn is_wide(ch: char) -> bool {
            matches!(
                ch as u32,
                0x1100..=0x115F
                    | 0x2E80..=0xA4CF
                    | 0xAC00..=0xD7A3
                    | 0xF900..=0xFAFF
                    | 0xFE30..=0xFE6F
                    | 0xFF00..=0xFF60
                    | 0xFFE0..=0xFFE6
            )
        }

        let mut out = String::new();
        let mut pending_pad = false;
        for ch in text.chars() {
            if ch == '\n' {
                pending_pad = false;
                out.push(ch);
                continue;
            }
            if pending_pad && ch == ' ' {
                pending_pad = false;
                continue;
            }
            pending_pad = is_wide(ch);
            out.push(ch);
        }
        out
    }

    /// 用真实系统数据渲染一帧并打印，肉眼确认框架把东西整合成了什么样。
    ///
    /// 手动执行：`cargo test --offline -- --ignored --nocapture smoke_render`
    #[test]
    #[ignore = "读取真实 HOME 与已装应用，默认跳过"]
    fn smoke_render_real_registry() {
        let Some(home) = std::env::var_os("HOME") else {
            return;
        };
        let bin_dir = PathBuf::from(home).join(".local/bin");
        let (registry, report) = Registry::discover(&bin_dir);
        let count = registry.len();
        let mut app = App::new(registry, bin_dir, report);

        println!("{}", pretty(&render(&mut app, 108, 34)));
        println!("（共 {count} 个工具，仅打印初始画面：媒体域）");

        // 再渲染一帧跨域搜索：故意用一个宽泛的查询，展示「命中落在哪些域」那一行。
        app.searching = true;
        app.query = "i".to_string();
        app.apply_filter();
        println!("{}", pretty(&render(&mut app, 108, 34)));
        println!("（查询「i」的跨域结果，第二行是命中分布）");
    }
}
