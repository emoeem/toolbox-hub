//! 「发现 / 仓库 / 已安装」界面的绘制。
//!
//! 只读 app::repo_view::RepositoryView 的状态，不做任何决定。布局与鼠标命中
//! 矩形共用 [rows_rect]，所以「看到的」和「点得到的」不会错位。

use ratatui::{
    layout::{Constraint, Layout, Rect},
    style::{Modifier, Style},
    text::{Line, Span, Text},
    widgets::{Block, Borders, Clear, Paragraph, Wrap},
};

use crate::{
    app::{
        App,
        repo_view::{Confirm, RepoMode, RepositoryView},
    },
    repository::{cache::CacheState, config::Trust},
    ui::{overlay, theme},
};

/// 详情区固定留几行。
const DETAIL_ROWS: u16 = 10;

/// 上下四段的切分（列表与详情共用，避免两处各算一遍）。
fn sections(area: Rect) -> std::rc::Rc<[Rect]> {
    Layout::vertical([
        Constraint::Length(2),
        Constraint::Length(1),
        Constraint::Min(3),
        Constraint::Length(DETAIL_ROWS.min(area.height / 2)),
    ])
    .split(area)
    .to_vec()
    .into()
}

/// 列表里可点区域（去掉边框行）。
pub fn rows_rect(area: Rect) -> Rect {
    let chunks = sections(area);
    let list = chunks[2];
    Rect {
        x: list.x,
        y: list.y.saturating_add(1),
        width: list.width,
        height: list.height.saturating_sub(2),
    }
}

pub fn draw(frame: &mut ratatui::Frame, app: &App, area: Rect) {
    let Some(view) = app.repository.as_ref() else {
        frame.render_widget(Paragraph::new("没有打开仓库界面"), area);
        return;
    };
    let chunks = sections(area);

    draw_modes(frame, view, chunks[0]);
    draw_hint(frame, view, chunks[1]);
    draw_list(frame, view, chunks[2]);
    draw_detail(frame, view, chunks[3]);

    if view.add.is_some() {
        draw_add_input(frame, view, area);
    }
    if let Some(confirm) = view.confirm.as_ref() {
        draw_confirm(frame, view, confirm, area);
    }
}

fn draw_modes(frame: &mut ratatui::Frame, view: &RepositoryView, area: Rect) {
    let mut spans = vec![Span::styled(
        " 工具仓库 ",
        Style::default()
            .fg(theme::BG)
            .bg(theme::PURPLE)
            .add_modifier(Modifier::BOLD),
    )];
    for mode in RepoMode::ALL {
        let count = match mode {
            RepoMode::Discover => view.hits.len(),
            RepoMode::Installed => view.installed.len(),
            RepoMode::Repositories => view.statuses.len(),
        };
        let style = if mode == view.mode {
            Style::default()
                .fg(theme::BG)
                .bg(theme::CYAN)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(theme::DIM)
        };
        spans.push(Span::raw(" "));
        spans.push(Span::styled(format!(" {} {count} ", mode.label()), style));
    }
    spans.push(Span::styled(
        "   （工具仓库，和 pacman/AUR 的软件包是两回事）",
        Style::default().fg(theme::FAINT),
    ));
    frame.render_widget(
        Paragraph::new(Line::from(spans)).block(
            Block::default()
                .borders(Borders::BOTTOM)
                .border_style(Style::default().fg(theme::FAINT)),
        ),
        area,
    );
}

fn draw_hint(frame: &mut ratatui::Frame, view: &RepositoryView, area: Rect) {
    let hint = view.mode.hint();
    let (text, style) = if let Some(busy) = view.busy.as_deref() {
        (format!("⟳ {busy}"), Style::default().fg(theme::YELLOW))
    } else if view.message.is_empty() {
        (hint.to_string(), Style::default().fg(theme::FAINT))
    } else {
        (
            format!("{}   ·   {hint}", view.message),
            Style::default().fg(theme::TEXT),
        )
    };
    frame.render_widget(Paragraph::new(Line::from(Span::styled(text, style))), area);
}

fn draw_list(frame: &mut ratatui::Frame, view: &RepositoryView, area: Rect) {
    let title = match view.mode {
        RepoMode::Discover if view.query.text().is_empty() => String::from(" 发现（全部） "),
        RepoMode::Discover => format!(" 发现 · 搜索「{}」 ", view.query.text()),
        RepoMode::Installed => String::from(" 已安装的工具包 "),
        RepoMode::Repositories => String::from(" 仓库 "),
    };
    let block = theme::panel(&title);

    if view.is_empty() {
        frame.render_widget(
            Paragraph::new(empty_text(view))
                .block(block)
                .wrap(Wrap { trim: true }),
            area,
        );
        return;
    }

    let height = area.height.saturating_sub(2).max(1) as usize;
    let start = view.viewport_start(height);

    let lines: Vec<Line> = list_lines(view)
        .into_iter()
        .enumerate()
        .skip(start)
        .take(height)
        .map(|(index, text)| {
            let style = if index == view.selected {
                Style::default()
                    .fg(theme::BG)
                    .bg(theme::CYAN)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(theme::TEXT)
            };
            Line::from(Span::styled(text, style))
        })
        .collect();
    frame.render_widget(Paragraph::new(Text::from(lines)).block(block), area);
}

fn list_lines(view: &RepositoryView) -> Vec<String> {
    match view.mode {
        RepoMode::Discover => view
            .hits
            .iter()
            .map(|hit| {
                format!(
                    "  {} {}  {}  {}  {}",
                    clip(&hit.id, 24),
                    clip(&hit.version, 10),
                    clip(hit.trust.label(), 4),
                    clip(hit.status(), 8),
                    clip(&hit.summary, 48),
                )
            })
            .collect(),
        RepoMode::Installed => view
            .installed
            .iter()
            .map(|package| {
                format!(
                    "  {} {}  {}  {:>3} 个文件  {}",
                    clip(&package.id, 24),
                    clip(&package.version, 10),
                    clip(package.trust().label(), 4),
                    package.file_count(),
                    clip(&package.repository_name, 26),
                )
            })
            .collect(),
        RepoMode::Repositories => view
            .statuses
            .iter()
            .map(|status| {
                let flag = if status.config.enabled { " " } else { "×" };
                let packages = if status.state.usable() {
                    format!("{} 个包", status.package_count)
                } else {
                    String::from("-")
                };
                format!(
                    "{flag}{} {} {}  {}  {}  {}",
                    status.state.marker(),
                    clip(&status.config.id, 18),
                    clip(status.trust.label(), 6),
                    clip(status.state.label(), 6),
                    clip(&packages, 8),
                    clip(&status.config.name, 26),
                )
            })
            .collect(),
    }
}

fn empty_text(view: &RepositoryView) -> String {
    match view.mode {
        RepoMode::Discover if !view.has_usable_index() => String::from(
            "还没有可用的索引。\n\n按 u 刷新，或切到「仓库」面板看状态。\n\
             离线时已经装好的工具照常可用 —— 只有搜索与安装新包需要网络。",
        ),
        RepoMode::Discover => String::from("没有匹配的包。\n\n换个关键词，或按 / 重新搜索。"),
        RepoMode::Installed => String::from(
            "还没装任何工具包。\n\n切到「发现」面板搜一个装上 —— 装好的动作会出现在它声明的域里。",
        ),
        RepoMode::Repositories => {
            String::from("还没有配置任何仓库。\n\n按 a 添加索引地址（URL / 本地路径 / file://…）。")
        }
    }
}

fn draw_detail(frame: &mut ratatui::Frame, view: &RepositoryView, area: Rect) {
    let block = theme::panel(" 详情 ");
    let mut lines: Vec<Line> = Vec::new();

    match view.mode {
        RepoMode::Discover => match view.selected_hit() {
            Some(hit) => {
                lines.push(Line::from(vec![
                    Span::styled("  ", Style::default()),
                    Span::styled(hit.id.clone(), Style::default().fg(theme::CYAN)),
                    Span::styled(
                        format!("  {}   {}", hit.version, hit.status()),
                        Style::default().fg(theme::TEXT),
                    ),
                ]));
                lines.push(Line::from(vec![
                    Span::styled("  仓库      ", Style::default().fg(theme::DIM)),
                    Span::styled(
                        hit.repository_name.clone(),
                        Style::default().fg(theme::TEXT),
                    ),
                    Span::styled(format!("  {}", hit.trust.label()), trust_color(hit.trust)),
                    Span::styled(
                        format!("   {}", hit.trust.meaning()),
                        Style::default().fg(theme::FAINT),
                    ),
                ]));
                lines.push(Line::from(vec![
                    Span::styled("  索引      ", Style::default().fg(theme::DIM)),
                    Span::styled(
                        format!("{} {}", hit.state.marker(), hit.state.label()),
                        state_color(hit.state),
                    ),
                    Span::styled("    完整性  ", Style::default().fg(theme::DIM)),
                    Span::styled(
                        if hit.has_hash {
                            "有 SHA-256 可核对"
                        } else {
                            "来源没给哈希"
                        },
                        Style::default().fg(if hit.has_hash {
                            theme::GREEN
                        } else {
                            theme::YELLOW
                        }),
                    ),
                ]));
                lines.push(Line::from(Span::styled(
                    format!("  {}", hit.summary),
                    Style::default().fg(theme::TEXT),
                )));
                if !hit.tags.is_empty() {
                    lines.push(Line::from(Span::styled(
                        format!("  标签      {}", hit.tags.join(" / ")),
                        Style::default().fg(theme::FAINT),
                    )));
                }
                lines.push(Line::from(Span::styled(
                    "  Enter 先看安装计划（来源 / 依赖 / 文件 / 哈希），确认之后才动手",
                    Style::default().fg(theme::FAINT),
                )));
            }
            None => lines.push(Line::from(Span::styled(
                "  选中一个包看详情",
                Style::default().fg(theme::FAINT),
            ))),
        },
        RepoMode::Installed => match view.selected_installed() {
            Some(package) => {
                lines.push(Line::from(vec![
                    Span::styled("  ", Style::default()),
                    Span::styled(package.id.clone(), Style::default().fg(theme::CYAN)),
                    Span::styled(
                        format!("  {}", package.version),
                        Style::default().fg(theme::TEXT),
                    ),
                ]));
                lines.push(Line::from(Span::styled(
                    format!(
                        "  来自 {} · 装于 {} · {} 个文件{}",
                        package.repository_name,
                        crate::repository::cli::relative_time(package.installed_at),
                        package.file_count(),
                        if package.has_executables() {
                            "（含可执行脚本）"
                        } else {
                            ""
                        }
                    ),
                    Style::default().fg(theme::TEXT),
                )));
                for file in package.files.iter().take(4) {
                    lines.push(Line::from(Span::styled(
                        format!("    · {}", file.path.display()),
                        Style::default().fg(theme::FAINT),
                    )));
                }
                lines.push(Line::from(Span::styled(
                    "  Delete 卸载（你改过的文件不会被删）",
                    Style::default().fg(theme::FAINT),
                )));
            }
            None => lines.push(Line::from(Span::styled(
                "  选中一个包看详情",
                Style::default().fg(theme::FAINT),
            ))),
        },
        RepoMode::Repositories => match view.selected_status() {
            Some(status) => {
                lines.push(Line::from(vec![
                    Span::styled("  ", Style::default()),
                    Span::styled(status.config.id.clone(), Style::default().fg(theme::CYAN)),
                    Span::styled(
                        format!("  {}", status.config.name),
                        Style::default().fg(theme::TEXT),
                    ),
                ]));
                lines.push(Line::from(Span::styled(
                    format!("  索引      {}", status.config.index),
                    Style::default().fg(theme::TEXT),
                )));
                lines.push(Line::from(vec![
                    Span::styled("  状态      ", Style::default().fg(theme::DIM)),
                    Span::styled(
                        format!("{} {}", status.state.marker(), status.state.label()),
                        state_color(status.state),
                    ),
                    Span::styled(
                        format!("   {} 个包", status.package_count),
                        Style::default().fg(theme::TEXT),
                    ),
                ]));
                lines.push(Line::from(vec![
                    Span::styled("  信任      ", Style::default().fg(theme::DIM)),
                    Span::styled(status.trust.label(), trust_color(status.trust)),
                    Span::styled(
                        format!("   {}", status.trust.meaning()),
                        Style::default().fg(theme::FAINT),
                    ),
                ]));
            }
            None => lines.push(Line::from(Span::styled(
                "  按 a 添加一个仓库",
                Style::default().fg(theme::FAINT),
            ))),
        },
    }

    for warning in view.warnings.iter().take(2) {
        lines.push(Line::from(Span::styled(
            format!("  ⚠ {warning}"),
            Style::default().fg(theme::YELLOW),
        )));
    }

    frame.render_widget(Paragraph::new(Text::from(lines)).block(block), area);
}

fn draw_confirm(frame: &mut ratatui::Frame, view: &RepositoryView, confirm: &Confirm, area: Rect) {
    let plan = &confirm.plan;
    let height = area.height.saturating_sub(2).clamp(8, 16);
    let width = area.width.saturating_sub(6).clamp(30, 92);
    // 宽高按终端面积夹（跟帮助页按内容夹不是一回事），居中的那一步才共用。
    let box_area = overlay::centered_rect(area, width, height);
    frame.render_widget(Clear, box_area);

    let action = if confirm.upgrading {
        "升级"
    } else {
        "安装"
    };
    let block = theme::panel(&format!(" {action} {} ", plan.name));

    let mut lines = vec![
        Line::from(vec![
            Span::styled("  版本      ", Style::default().fg(theme::DIM)),
            Span::styled(plan.version.clone(), Style::default().fg(theme::TEXT)),
            match &plan.installed_version {
                Some(installed) => Span::styled(
                    format!("   （本地是 {installed}）"),
                    Style::default().fg(theme::YELLOW),
                ),
                None => Span::raw(""),
            },
        ]),
        Line::from(vec![
            Span::styled("  来源      ", Style::default().fg(theme::DIM)),
            Span::styled(
                plan.repository_name.clone(),
                Style::default().fg(theme::TEXT),
            ),
            Span::styled(
                format!("   {}", plan.trust.label()),
                trust_color(plan.trust),
            ),
        ]),
        Line::from(Span::styled(
            format!("            {}", plan.trust.meaning()),
            Style::default().fg(theme::FAINT),
        )),
    ];

    if let Some(artifact) = &plan.artifact {
        lines.push(Line::from(Span::styled(
            match artifact.sha256() {
                Some(hash) => format!(
                    "  SHA-256   {}…（下载后会核对，不一致就拒绝安装）",
                    &hash[..16.min(hash.len())]
                ),
                None => String::from("  SHA-256   来源没有提供 —— 内容无法核对"),
            },
            Style::default().fg(if artifact.sha256().is_some() {
                theme::GREEN
            } else {
                theme::YELLOW
            }),
        )));
    }

    lines.push(Line::from(Span::styled(
        format!(
            "  文件      {} 个{}",
            plan.files.len(),
            if plan.has_executables() {
                "   ⚠ 包含可执行脚本"
            } else {
                ""
            }
        ),
        Style::default().fg(if plan.has_executables() {
            theme::YELLOW
        } else {
            theme::TEXT
        }),
    )));
    for file in plan.files.iter().take(3) {
        lines.push(Line::from(Span::styled(
            format!("    {} → {}", file.source, file.target.display()),
            Style::default().fg(theme::FAINT),
        )));
    }
    if plan.files.len() > 3 {
        lines.push(Line::from(Span::styled(
            format!("    …还有 {} 个", plan.files.len() - 3),
            Style::default().fg(theme::FAINT),
        )));
    }

    if !plan.dependencies.is_empty() {
        let deps = plan
            .dependencies
            .iter()
            .map(|dependency| {
                if plan.missing_dependencies.contains(dependency) {
                    format!("✗ {dependency}")
                } else {
                    format!("✓ {dependency}")
                }
            })
            .collect::<Vec<_>>()
            .join("  ");
        lines.push(Line::from(Span::styled(
            format!("  依赖      {deps}"),
            Style::default().fg(if plan.deps_ready() {
                theme::TEXT
            } else {
                theme::YELLOW
            }),
        )));
    }

    match confirm.pending_reason() {
        Some(reason) => lines.push(Line::from(Span::styled(
            format!("  ⚠ {reason} —— 再按一次 Enter 表示你接受"),
            Style::default().fg(theme::RED),
        ))),
        None => lines.push(Line::from(Span::styled(
            "  Enter 确认   Esc 取消",
            Style::default().fg(theme::FAINT),
        ))),
    }

    if let Some(busy) = view.busy.as_deref() {
        lines.push(Line::from(Span::styled(
            format!("  ⟳ {busy}"),
            Style::default().fg(theme::YELLOW),
        )));
    }

    frame.render_widget(
        Paragraph::new(Text::from(lines))
            .block(block)
            .wrap(Wrap { trim: false }),
        box_area,
    );
}

fn draw_add_input(frame: &mut ratatui::Frame, view: &RepositoryView, area: Rect) {
    let Some(add) = view.add.as_ref() else {
        return;
    };
    let width = area.width.saturating_sub(4).clamp(20, 90);
    let box_area = Rect {
        x: area.x + area.width.saturating_sub(width) / 2,
        y: area.y + area.height.saturating_sub(4),
        width,
        height: 3,
    };
    frame.render_widget(Clear, box_area);
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(" 索引地址  ", Style::default().fg(theme::DIM)),
            Span::styled(
                format!("{}▌", add.text.text()),
                Style::default()
                    .fg(theme::YELLOW)
                    .add_modifier(Modifier::BOLD),
            ),
        ]))
        .block(theme::panel(" 添加仓库 ")),
        box_area,
    );
}

fn state_color(state: CacheState) -> Style {
    match state {
        CacheState::Fresh => Style::default().fg(theme::GREEN),
        CacheState::Stale => Style::default().fg(theme::YELLOW),
        CacheState::Missing => Style::default().fg(theme::DIM),
        CacheState::Unavailable => Style::default().fg(theme::RED),
    }
}

fn trust_color(trust: Trust) -> Style {
    match trust {
        Trust::Trusted => Style::default().fg(theme::GREEN),
        Trust::Verified => Style::default().fg(theme::CYAN),
        Trust::Community => Style::default().fg(theme::YELLOW),
        Trust::Unknown => Style::default().fg(theme::RED),
    }
}

/// 按显示宽度截断（中文是两格）。
fn clip(text: &str, width: usize) -> String {
    let mut out = String::new();
    let mut used = 0usize;
    for ch in text.chars() {
        let ch_width = unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0);
        if used + ch_width > width {
            break;
        }
        out.push(ch);
        used += ch_width;
    }
    out
}
