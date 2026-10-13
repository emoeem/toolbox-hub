//! Shell 集成（widget）：把选中的命令**回填**到当前输入行。
//!
//! `toolbox-hub widget bash|zsh|fish` 打印一段 shell 脚本，`eval` 之后就多了一个
//! 快捷键（默认 `Ctrl-G`）。按下它打开选择器，选中的命令不是被执行，而是**变成
//! 你现在这一行输入**：回车之前你还能改，执行、写历史都由你的 shell 自己做。
//!
//! ## 三条约定（和 [`crate::components`] 同一套，破坏任何一条就没法用）
//!
//! 1. **stdout 只放最终命令**，界面一律画到 `/dev/tty`。`$(toolbox-hub widget pick)`
//!    拿到的必须是干干净净的一行，否则回填进去的就是一串转义序列。
//! 2. **退出码固定**：`0` 选中、`1` 用户取消、`2` 参数错或没有终端。
//! 3. **默认只回填，不执行**。直接执行需要显式打开（见 `--run`），而且即便打开了
//!    也走 argv，绝不拼 `sh -c` 字符串 —— 引号一拼错，命令的含义就变了。
//!
//! ## 为什么不是「喂给已安装的 register 命令」
//!
//! 回填而不是执行，用户始终在回路里：命令进了输入行，看得见、改得动、能按 Enter
//! 前再想一秒。这也让 shell 自己负责历史记录 —— 历史里出现的是真实命令，
//! 不是 `toolbox-hub widget` 这种东西。

use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    process::Command,
};

use ratatui::{
    crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers},
    layout::{Constraint, Layout, Rect},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, BorderType, Clear, Paragraph},
};

use crate::{
    app::TextInput,
    components::{Outcome, Session, UNSUPPORTED_CODE, draw_hint},
    ecosystem::{Recipe, RecipeParam, SafetyLevel},
    ui::{display_width, theme},
};

/// 选择器默认绑的按键（只用于文档与提示；真正的绑定写在生成的脚本里）。
pub const DEFAULT_KEY: &str = "Ctrl-G";

/// 找不到 featured 目录时的提示。
const FEATURED_ENV: &str = "TOOLBOX_HUB_FEATURED";

// ── 脚本生成 ────────────────────────────────────────────────────────────────

/// 生成 shell 集成脚本。`bin` 是脚本里要调用的可执行文件（默认 `toolbox-hub`，
/// 开发时可以用 `--bin ./target/debug/toolbox-hub`）。
pub fn script(shell: &str, bin: &str) -> Result<String, String> {
    let bin = quote_bin(bin);
    let text = match shell {
        "bash" => format!(
            r#"# toolbox-hub 的 shell 集成（bash）
# 装法（~/.bashrc）：
#   eval "$({bin} widget bash)"
#
# 之后按 {key}: 打开选择器，选中的命令会**填到当前输入行**，
# 回车之前你还能改；执行与历史都交给 bash 自己。
#
# 需要 bash >= 4.0（bind -x 与 READLINE_LINE）。

toolbox_hub_widget() {{
    local line
    line="$({bin} widget pick)" || return $?
    READLINE_LINE="$line"
    READLINE_POINT=${{#READLINE_LINE}}
}}

bind -x '"\C-g": toolbox_hub_widget'
"#,
            key = DEFAULT_KEY
        ),
        "zsh" => format!(
            r#"# toolbox-hub 的 shell 集成（zsh）
# 装法（~/.zshrc）：
#   eval "$({bin} widget zsh)"
#
# 之后按 {key}: 打开选择器，选中的命令会**填到当前输入行**，
# 回车之前你还能改；执行与历史都交给 zsh 自己。

toolbox_hub_widget() {{
    local line
    line="$({bin} widget pick)" || return $?
    LBUFFER="${{LBUFFER}}${{line}}"
    zle reset-prompt
}}

zle -N toolbox_hub_widget
bindkey '^G' toolbox_hub_widget
"#,
            key = DEFAULT_KEY
        ),
        "fish" => format!(
            r#"# toolbox-hub 的 shell 集成（fish）
# 装法（~/.config/fish/config.fish）：
#   {bin} widget fish | source
#
# 之后按 {key}: 打开选择器，选中的命令会**填到当前输入行**，
# 回车之前你还能改；执行与历史都交给 fish 自己。

function toolbox_hub_widget
    set -l line ({bin} widget pick)
    or return $status
    commandline -r -- "$line"
    commandline -f repaint
end

bind \cg toolbox_hub_widget
# 再绑一份 vi 的 insert 模式：emacs 模式下用不到（默认 keymap 就是它），
# 但 vi 模式里只用上面那条是**按不出效果**的。同配置里的 atuin / fzf 也是两条都绑。
bind -M insert \cg toolbox_hub_widget
"#,
            key = DEFAULT_KEY
        ),
        other => {
            return Err(format!("不支持的 shell：{other}（有 bash / zsh / fish）"));
        }
    };
    Ok(text)
}

/// 脚本里调用哪个可执行文件。
///
/// 带路径的（`./target/debug/toolbox-hub`）原样写进去、保持不带引号，
/// 因为 shell 函数里它得能被当命令调用；含空格的路径本来就该先装好再配。
fn quote_bin(bin: &str) -> String {
    let trimmed = bin.trim();
    if trimmed.is_empty() {
        String::from("toolbox-hub")
    } else {
        trimmed.to_string()
    }
}

// ── 命令回填 ────────────────────────────────────────────────────────────────

/// 把一个词渲染成 shell 里**字面量**写法。
///
/// 只做一件事：让「这个词」在 shell 里重新解析回同一个字符串。安全的字符集之外的
/// 一律单引号包起来 —— 单引号里除了单引号本身什么都不需要转义，所以 `'\''` 是最短
/// 的正确写法。`~`、`$`、`*` 都落在引号里，不会被意外展开。
pub fn quote(word: &str) -> String {
    if word.is_empty() {
        return String::from("''");
    }
    let plain = word.chars().all(|c| {
        c.is_ascii_alphanumeric()
            || matches!(c, '-' | '_' | '.' | '/' | ':' | '=' | '@' | '+' | ',' | '%')
    });
    if plain {
        return word.to_string();
    }
    format!("'{}'", word.replace('\'', r"'\''"))
}

/// 把参数模板里的 `{名字}` 换成值。第二个返回值表示「还有没填的占位符」——
/// 没填的写成 `<名字>`，调用方据此决定是【留着让人改】还是【拒绝执行】。
fn substituted(arg: &str, values: &BTreeMap<String, String>) -> (String, bool) {
    let mut out = String::new();
    let mut rest = arg;
    let mut unfilled = false;
    while let Some(open) = rest.find('{') {
        out.push_str(&rest[..open]);
        let after = &rest[open + 1..];
        let Some(close) = after.find('}') else {
            // 花括号不闭合：原样留着，交给 Recipe 校验去报错。
            out.push_str(rest);
            return (out, unfilled);
        };
        let key = &after[..close];
        match values.get(key).filter(|value| !value.is_empty()) {
            Some(value) => out.push_str(value),
            None => {
                out.push('<');
                out.push_str(key);
                out.push('>');
                unfilled = true;
            }
        }
        rest = &after[close + 1..];
    }
    out.push_str(rest);
    (out, unfilled)
}

/// 渲染成可以贴进 shell 的一行。
///
/// 填好的值一律按**字面量**引用（`quote`），没填的占位符留下 `<名字>` 且不引用：
/// 看得见、改得动，比塞一个空字符串强 —— 空字符串会让命令在语法上「像是对的」
/// 而语义是错的。
pub fn render_line(program: &str, args: &[String], values: &BTreeMap<String, String>) -> String {
    let mut parts = vec![quote(program)];
    for arg in args {
        let (text, unfilled) = substituted(arg, values);
        if unfilled {
            parts.push(text);
        } else {
            parts.push(quote(&text));
        }
    }
    parts.join(" ")
}

/// 直接执行时用的 argv。**和 [`render_line`] 共用同一套替换**，
/// 区别只有一个：这里还留着没填的占位符就拒绝执行 —— shell 会把它当字面量跑掉。
pub fn render_argv(
    program: &str,
    args: &[String],
    values: &BTreeMap<String, String>,
) -> Result<Vec<String>, String> {
    let mut out = vec![program.to_string()];
    for arg in args {
        let (text, unfilled) = substituted(arg, values);
        if unfilled {
            return Err(format!("参数还没填完，拒绝直接执行：{arg}"));
        }
        out.push(text);
    }
    Ok(out)
}

// ── 精选库定位 ──────────────────────────────────────────────────────────────

/// 候选目录按顺序试，谁先带着 `index.json` 谁就是答案。
///
/// 抽成纯函数是为了能测：`--dir` > `TOOLBOX_HUB_FEATURED` > 当前目录 >
/// 可执行文件旁边的 `share/toolbox-hub` > `~/.local/share/toolbox-hub`。
fn featured_candidates(
    explicit: Option<&str>,
    env: Option<&str>,
    cwd: Option<&Path>,
    exe_dir: Option<&Path>,
    home: Option<&Path>,
) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = vec![];
    let mut push = |path: PathBuf| {
        if !out.contains(&path) {
            out.push(path);
        }
    };
    if let Some(dir) = explicit.filter(|d| !d.trim().is_empty()) {
        push(PathBuf::from(dir));
    }
    if let Some(dir) = env.filter(|d| !d.trim().is_empty()) {
        push(PathBuf::from(dir));
    }
    if let Some(dir) = cwd {
        push(dir.join("registry/featured"));
    }
    if let Some(dir) = exe_dir {
        push(dir.join("../share/toolbox-hub/featured"));
        push(dir.join("share/toolbox-hub/featured"));
    }
    if let Some(home) = home {
        push(home.join(".local/share/toolbox-hub/featured"));
    }
    out
}

fn find_featured_dir(explicit: Option<&str>) -> Result<PathBuf, String> {
    let env = std::env::var(FEATURED_ENV).ok();
    let cwd = std::env::current_dir().ok();
    let exe = std::env::current_exe().ok();
    let exe_dir = exe.as_deref().and_then(Path::parent);
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let candidates = featured_candidates(
        explicit,
        env.as_deref(),
        cwd.as_deref(),
        exe_dir,
        home.as_deref(),
    );
    for candidate in &candidates {
        if candidate.join("index.json").is_file() {
            return Ok(candidate.clone());
        }
    }
    Err(format!(
        "找不到 featured 索引。用 --dir 指定，或把 {FEATURED_ENV} 指向它（试过 {}）",
        candidates
            .iter()
            .map(|path| path.display().to_string())
            .collect::<Vec<_>>()
            .join("、")
    ))
}

/// 读 featured 索引里的全部 Recipe（按索引顺序，索引本身已排序）。
fn load_recipes(dir: &Path) -> Result<Vec<Recipe>, String> {
    let index: serde_json::Value =
        serde_json::from_slice(&std::fs::read(dir.join("index.json")).map_err(|e| e.to_string())?)
            .map_err(|e| e.to_string())?;
    let rows = index["recipes"].as_array().ok_or("索引缺少 recipes")?;
    let mut out = vec![];
    for row in rows {
        let Some(file) = row["file"].as_str() else {
            continue;
        };
        let bytes = std::fs::read(dir.join(file)).map_err(|e| e.to_string())?;
        out.push(serde_json::from_slice(&bytes).map_err(|e| format!("{file}：{e}"))?);
    }
    Ok(out)
}

// ── 交互 ────────────────────────────────────────────────────────────────────

/// 过滤：空串全都要；否则按标题 / id / 命令行做**大小写不敏感的子串**匹配。
fn matches(recipe: &Recipe, needle: &str) -> bool {
    let needle = needle.trim().to_lowercase();
    if needle.is_empty() {
        return true;
    }
    let haystack = format!(
        "{} {} {} {}",
        recipe.title,
        recipe.id,
        recipe.command,
        recipe.args.join(" ")
    )
    .to_lowercase();
    haystack.contains(&needle)
}

fn visible_indices(recipes: &[Recipe], needle: &str) -> Vec<usize> {
    recipes
        .iter()
        .enumerate()
        .filter(|(_, recipe)| matches(recipe, needle))
        .map(|(index, _)| index)
        .collect()
}

/// 安全等级的标记与配色（选择器里一眼看出哪些是踩下去要小心的）。
fn safety_badge(safety: &SafetyLevel) -> (&'static str, ratatui::style::Color) {
    match safety {
        SafetyLevel::Safe => ("只读", theme::GREEN),
        SafetyLevel::Caution => ("谨慎", theme::YELLOW),
        SafetyLevel::Destructive => ("破坏性", theme::RED),
    }
}

/// 选中之后的结果：既能回填（默认），也能直接执行（显式 `--run`）。
struct Selection {
    id: String,
    safety: SafetyLevel,
    line: String,
    argv: Result<Vec<String>, String>,
}

/// 打开选择器，返回选中项；`Ok(None)` = 用户取消。
fn pick_line(recipes: &[Recipe]) -> Result<Option<Selection>, String> {
    let Some(mut session) = Session::open()? else {
        return Err(String::from(
            "不是交互式终端：widget 要有人点。脚本里请直接调 `widget render`",
        ));
    };

    let mut filter = TextInput::new();
    let mut cursor = 0usize;

    let chosen = loop {
        let visible = visible_indices(recipes, filter.text());
        cursor = cursor.min(visible.len().saturating_sub(1));
        session.draw(|frame, area| draw_picker(frame, area, recipes, &visible, cursor, &filter))?;
        if !session.poll(200)? {
            continue;
        }
        let Event::Key(key) = session.read()? else {
            continue;
        };
        match key.code {
            KeyCode::Esc => break None,
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => break None,
            KeyCode::Char('q') if filter.is_empty() && key.modifiers.is_empty() => break None,
            KeyCode::Enter => {
                if let Some(index) = visible.get(cursor) {
                    break Some(recipes[*index].clone());
                }
            }
            KeyCode::Up | KeyCode::Char('k') if key.modifiers.is_empty() && filter.is_empty() => {
                cursor = cursor.saturating_sub(1);
            }
            KeyCode::Down | KeyCode::Char('j') if key.modifiers.is_empty() && filter.is_empty() => {
                if cursor + 1 < visible.len() {
                    cursor += 1;
                }
            }
            KeyCode::Tab => {
                if !visible.is_empty() {
                    cursor = (cursor + 1) % visible.len();
                }
            }
            KeyCode::BackTab => {
                if !visible.is_empty() {
                    cursor = cursor.checked_sub(1).unwrap_or(visible.len() - 1);
                }
            }
            other => {
                if edit_input(&mut filter, KeyEvent { code: other, ..key }) {
                    cursor = 0;
                }
            }
        }
    };

    let Some(recipe) = chosen else {
        return Ok(None);
    };

    // 有参数就一个一个问。回车留空 = 保留 `<名字>` 占位符，让人回去手填。
    let mut values = BTreeMap::new();
    for param in &recipe.params {
        match ask_param(&mut session, &recipe, param)? {
            Some(value) if !value.is_empty() => {
                values.insert(param.name.clone(), value);
            }
            Some(_) => {}
            None => return Ok(None),
        }
    }

    if recipe.safety == SafetyLevel::Destructive {
        eprintln!(
            "toolbox-hub: 注意：{} 标为破坏性命令，回车前请再读一遍这一行",
            recipe.id
        );
    }
    Ok(Some(Selection {
        id: recipe.id.clone(),
        safety: recipe.safety.clone(),
        line: render_line(&recipe.command, &recipe.args, &values),
        argv: render_argv(&recipe.command, &recipe.args, &values),
    }))
}

/// 单行编辑。返回 true 表示文本变了（调用方据此把光标挪回第一项）。
fn edit_input(input: &mut TextInput, key: KeyEvent) -> bool {
    match key.code {
        KeyCode::Char(ch) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
            input.insert(ch);
            true
        }
        KeyCode::Backspace => {
            input.backspace();
            true
        }
        KeyCode::Delete => {
            input.delete();
            true
        }
        KeyCode::Left => {
            input.left();
            false
        }
        KeyCode::Right => {
            input.right();
            false
        }
        KeyCode::Home => {
            input.home();
            false
        }
        KeyCode::End => {
            input.end();
            false
        }
        KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            input.clear();
            true
        }
        _ => false,
    }
}

/// 问一个参数。`Ok(None)` = 用户取消（Esc）。
fn ask_param(
    session: &mut Session,
    recipe: &Recipe,
    param: &RecipeParam,
) -> Result<Option<String>, String> {
    let mut input = TextInput::new();
    loop {
        session.draw(|frame, area| draw_param(frame, area, recipe, param, &input))?;
        if !session.poll(200)? {
            continue;
        }
        let Event::Key(key) = session.read()? else {
            continue;
        };
        match key.code {
            KeyCode::Esc => return Ok(None),
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => return Ok(None),
            KeyCode::Enter => return Ok(Some(input.text().to_string())),
            other => {
                edit_input(&mut input, KeyEvent { code: other, ..key });
            }
        }
    }
}

// ── 画 ──────────────────────────────────────────────────────────────────────

fn draw_picker(
    frame: &mut ratatui::Frame,
    area: Rect,
    recipes: &[Recipe],
    visible: &[usize],
    cursor: usize,
    filter: &TextInput,
) {
    let rows = Layout::vertical([
        Constraint::Min(1),
        Constraint::Length(3),
        Constraint::Length(1),
    ])
    .split(area);

    let Some(list_area) = rows.first().copied() else {
        return;
    };
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(theme::BORDER))
        .title(Span::styled(
            format!(" 选择命令（{} / {}） ", visible.len(), recipes.len()),
            Style::default().fg(theme::PURPLE),
        ));
    let inner = block.inner(list_area);
    frame.render_widget(block, list_area);

    // 选中项永远留在可视区里：列表比屏幕高时不能把人选丢在屏幕外。
    let height = inner.height.max(1) as usize;
    let first = cursor
        .saturating_sub(height / 2)
        .min(visible.len().saturating_sub(height));
    let mut lines = vec![];
    for (offset, index) in visible.iter().skip(first).take(height).enumerate() {
        let row = first + offset;
        let recipe = &recipes[*index];
        let (badge, color) = safety_badge(&recipe.safety);
        let selected = row == cursor;
        let marker = if selected { "▸ " } else { "  " };
        let command = command_preview(recipe);
        let title_style = if selected {
            Style::default()
                .fg(theme::TEXT)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(theme::TEXT)
        };
        lines.push(Line::from(vec![
            Span::styled(
                marker,
                Style::default().fg(if selected {
                    theme::PURPLE
                } else {
                    theme::FAINT
                }),
            ),
            Span::styled(format!("{:<22}", truncate(&recipe.title, 22)), title_style),
            Span::styled(format!("{badge} "), Style::default().fg(color)),
            Span::styled(command, Style::default().fg(theme::DIM)),
        ]));
    }
    frame.render_widget(Paragraph::new(lines), inner);

    if let Some(filter_area) = rows.get(1).copied() {
        frame.render_widget(
            Paragraph::new(filter_line(filter)).block(
                Block::bordered()
                    .border_type(BorderType::Rounded)
                    .border_style(Style::default().fg(theme::BORDER))
                    .title(Span::styled(" 过滤 ", Style::default().fg(theme::DIM))),
            ),
            filter_area,
        );
    }

    if let Some(hint_area) = rows.get(2).copied() {
        draw_hint(
            frame,
            hint_area,
            &format!(
                "↑↓ 选择 · Enter 选中 · 打字过滤 · Ctrl+U 清空 · Esc 取消（回填到输入行后按 {DEFAULT_KEY} 再叫一次）"
            ),
        );
    }
}

fn draw_param(
    frame: &mut ratatui::Frame,
    area: Rect,
    recipe: &Recipe,
    param: &RecipeParam,
    input: &TextInput,
) {
    let width = area.width.saturating_sub(4).clamp(24, 72);
    let height = 5u16.min(area.height);
    let popup = Rect::new(
        area.x + (area.width.saturating_sub(width)) / 2,
        area.y + (area.height.saturating_sub(height)) / 2,
        width,
        height,
    );
    frame.render_widget(Clear, popup);
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(theme::PURPLE))
        .title(Span::styled(
            format!(" {} · 参数 {} ", recipe.id, param.name),
            Style::default().fg(theme::PURPLE),
        ));
    let inner = block.inner(popup);
    frame.render_widget(block, popup);

    let hint = if param.required {
        "回车留空 = 保留 <占位符>，之后再手填"
    } else {
        "可选参数，回车跳过"
    };
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled("> ", Style::default().fg(theme::PURPLE)),
            Span::styled(
                input.split_at_cursor().0.to_string(),
                Style::default().fg(theme::TEXT),
            ),
            Span::styled("█", Style::default().fg(theme::PURPLE)),
            Span::styled(
                input.split_at_cursor().1.to_string(),
                Style::default().fg(theme::TEXT),
            ),
            Span::styled(format!("  {hint}"), Style::default().fg(theme::FAINT)),
        ])),
        inner,
    );
}

fn filter_line(input: &TextInput) -> Line<'static> {
    let (before, after) = input.split_at_cursor();
    Line::from(vec![
        Span::styled(" > ", Style::default().fg(theme::PURPLE)),
        Span::styled(before.to_string(), Style::default().fg(theme::TEXT)),
        Span::styled("█", Style::default().fg(theme::PURPLE)),
        Span::styled(after.to_string(), Style::default().fg(theme::TEXT)),
    ])
}

/// 列表里显示的命令预览：占位符写成 `<名字>`，参数不展示具体值（还没问）。
fn command_preview(recipe: &Recipe) -> String {
    let empty = BTreeMap::new();
    render_line(&recipe.command, &recipe.args, &empty)
}

/// 按**显示宽度**截断（中文占 2 列，按字符数截会错位）。
fn truncate(text: &str, width: usize) -> String {
    if usize::from(display_width(text)) <= width {
        return text.to_string();
    }
    let mut out = String::new();
    let mut used = 0usize;
    for ch in text.chars() {
        let w = usize::from(display_width(&ch.to_string()));
        if used + w > width.saturating_sub(1) {
            break;
        }
        out.push(ch);
        used += w;
    }
    out.push('…');
    out
}

// ── CLI ─────────────────────────────────────────────────────────────────────

/// `toolbox-hub widget …` 的入口。返回进程退出码（见模块头的约定）。
pub fn run_cli(args: &[String]) -> i32 {
    match args.first().map(String::as_str) {
        Some("bash") | Some("zsh") | Some("fish") => {
            let shell = args[0].as_str();
            let bin = flag_value(args, "--bin").unwrap_or_else(|| String::from("toolbox-hub"));
            match script(shell, &bin) {
                Ok(text) => {
                    print!("{text}");
                    0
                }
                Err(error) => {
                    eprintln!("toolbox-hub: {error}");
                    UNSUPPORTED_CODE
                }
            }
        }
        Some("pick") => {
            let run = args.iter().any(|arg| arg == "--run");
            match pick_cli(args, run) {
                Ok(code) => code,
                Err(error) => {
                    eprintln!("toolbox-hub: {error}");
                    UNSUPPORTED_CODE
                }
            }
        }
        Some("render") => {
            let run = args.iter().any(|arg| arg == "--run");
            match render_cli(args, run) {
                Ok(code) => code,
                Err(error) => {
                    eprintln!("toolbox-hub: {error}");
                    UNSUPPORTED_CODE
                }
            }
        }
        Some("--help" | "-h" | "help") => {
            print!("{USAGE}");
            0
        }
        _ => {
            eprint!("{USAGE}");
            UNSUPPORTED_CODE
        }
    }
}

const USAGE: &str = "\
用法：
  toolbox-hub widget bash|zsh|fish [--bin 可执行文件]   打印 shell 集成脚本
  toolbox-hub widget pick [--dir featured 目录] [--run]  打开选择器，把命令写到 stdout
  toolbox-hub widget render <RecipeID> [key=value …] [--run]  非交互：渲染一行命令

退出码：0 选中 / 1 取消 / 2 参数错或没有终端。
stdout 只有最终命令，界面画在 /dev/tty。
--run 是显式开启的「直接执行」：不打印命令，改用 argv 起进程（绝不拼 sh -c）。";

fn flag_value(args: &[String], flag: &str) -> Option<String> {
    args.windows(2)
        .find(|pair| pair[0] == flag)
        .map(|pair| pair[1].clone())
}

/// 选中之后按 `run` 决定：打印一行，还是真的执行。
fn finish(selection: Selection, run: bool) -> Result<i32, String> {
    if run {
        // `--run` 已经是显式选择，但破坏性 Recipe 再多一道：直接执行破坏性动作，
        // 和 `recipe run` 走同一套授权（环境变量授权 + 显式开启）。
        if selection.safety == SafetyLevel::Destructive
            && std::env::var("TOOLBOX_HUB_CONFIRM_DESTRUCTIVE").as_deref() != Ok("1")
        {
            return Err(format!(
                "{} 标为破坏性；直接执行需要 TOOLBOX_HUB_CONFIRM_DESTRUCTIVE=1",
                selection.id
            ));
        }
        let argv = selection.argv?;
        let Some((program, rest)) = argv.split_first() else {
            return Err(String::from("空 argv，拒绝执行"));
        };
        return run_with_argv(program, rest);
    }
    println!("{}", selection.line);
    Ok(Outcome::Ok.code())
}

fn pick_cli(args: &[String], run: bool) -> Result<i32, String> {
    let explicit = flag_value(args, "--dir");
    let dir = find_featured_dir(explicit.as_deref())?;
    let recipes = load_recipes(&dir)?;
    if recipes.is_empty() {
        return Err(format!("{} 里没有任何 Recipe", dir.display()));
    }
    match pick_line(&recipes)? {
        Some(selection) => finish(selection, run),
        None => Ok(Outcome::Cancelled.code()),
    }
}

fn render_cli(args: &[String], run: bool) -> Result<i32, String> {
    let id = args
        .get(1)
        .ok_or("用法：widget render <RecipeID> [key=value …]")?;
    if id.starts_with("--") {
        return Err(String::from("widget render 需要一个 RecipeID"));
    }
    let explicit = flag_value(args, "--dir");
    let dir = find_featured_dir(explicit.as_deref())?;
    let recipes = load_recipes(&dir)?;
    let recipe = recipes
        .iter()
        .find(|recipe| &recipe.id == id)
        .ok_or_else(|| format!("没有这条 Recipe：{id}（目录 {}）", dir.display()))?;

    let mut values = BTreeMap::new();
    for arg in args.iter().skip(2) {
        if arg.starts_with("--") {
            continue;
        }
        let (key, value) = arg
            .split_once('=')
            .ok_or_else(|| format!("参数要写成 key=value：{arg}"))?;
        if !recipe.params.iter().any(|param| param.name == key) {
            return Err(format!("这条 Recipe 没有参数 {key}"));
        }
        values.insert(key.to_string(), value.to_string());
    }

    finish(
        Selection {
            id: recipe.id.clone(),
            safety: recipe.safety.clone(),
            line: render_line(&recipe.command, &recipe.args, &values),
            argv: render_argv(&recipe.command, &recipe.args, &values),
        },
        run,
    )
}

/// 直接执行（**默认关闭**）。
///
/// 只有显式打开才会走这里，而且仍然用 argv 起进程 —— 回填那条路才是默认路径。
pub fn run_with_argv(program: &str, args: &[String]) -> Result<i32, String> {
    let status = Command::new(program)
        .args(args)
        .status()
        .map_err(|error| format!("启动 {program} 失败：{error}"))?;
    Ok(status.code().unwrap_or(1))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    fn recipe(id: &str, title: &str, safety: SafetyLevel) -> Recipe {
        Recipe {
            id: id.into(),
            title: title.into(),
            description: "d".into(),
            command: "ls".into(),
            args: args(&["-lah", "--", "{path}"]),
            params: vec![RecipeParam {
                name: "path".into(),
                description: "path".into(),
                required: true,
            }],
            safety,
            source: "test".into(),
            tags: vec![],
        }
    }

    // ── 脚本生成 ────────────────────────────────────────────────────────────

    #[test]
    fn every_supported_shell_gets_a_script_that_hooks_its_own_line_buffer() {
        let bash = script("bash", "toolbox-hub").expect("bash");
        assert!(bash.contains("bind -x"), "bash 靠 bind -x 绑键");
        assert!(bash.contains("READLINE_LINE"), "bash 回填用 READLINE_LINE");
        assert!(bash.contains("READLINE_POINT"), "光标要跟着挪到行尾");
        assert!(bash.contains("toolbox-hub widget pick"));

        let zsh = script("zsh", "toolbox-hub").expect("zsh");
        assert!(zsh.contains("zle -N"), "zsh 要先注册成 zle widget");
        assert!(zsh.contains("bindkey"));
        assert!(zsh.contains("LBUFFER"), "zsh 回填用 LBUFFER");

        let fish = script("fish", "toolbox-hub").expect("fish");
        assert!(fish.contains("function toolbox_hub_widget"));
        assert!(
            fish.contains("commandline -r"),
            "fish 回填用 commandline -r"
        );
        assert!(fish.contains("bind \\cg"), "fish 的 Ctrl-G 写作 \\cg");
        // vi 模式下只绑默认 keymap 是按不出效果的（emacs 模式下两条等价）。
        assert!(
            fish.contains("bind -M insert \\cg"),
            "fish 也要绑 insert 模式，否则 vi 用户按不出这个键"
        );
    }

    #[test]
    fn scripts_never_bake_in_shell_evaluation_of_the_result() {
        // 回填路径绝不出现 `sh -c` / `eval "$(...)"` 这类「把结果当代码跑」的写法：
        // 结果是**输入行文本**，不是要解释的脚本。
        for shell in ["bash", "zsh", "fish"] {
            let text = script(shell, "toolbox-hub").expect(shell);
            assert!(!text.contains("sh -c"), "{shell} 脚本不该拼 sh -c");
            assert!(
                !text.contains("eval \"$(toolbox-hub widget pick)\""),
                "{shell} 脚本不该直接 eval 选择结果"
            );
        }
    }

    #[test]
    fn an_unknown_shell_is_an_error_not_a_guess() {
        assert!(script("nushell", "toolbox-hub").is_err());
        assert!(script("", "toolbox-hub").is_err());
    }

    #[test]
    fn the_binary_path_in_the_script_can_be_overridden_for_development() {
        let text = script("bash", "./target/debug/toolbox-hub").expect("bash");
        assert!(text.contains("./target/debug/toolbox-hub widget pick"));
        // 空字符串退回默认值，而不是生成一条跑不起来的空命令。
        let fallback = script("bash", "   ").expect("bash");
        assert!(fallback.contains("toolbox-hub widget pick"));
    }

    #[test]
    fn generated_scripts_parse_in_their_own_shell() {
        // 语法错误只能由真正的 shell 发现。机器上没装就跳过（CI 的 Arch 容器里有 bash，
        // fish 不保证有），装了就必须过。
        let dir = std::env::temp_dir().join(format!("toolbox-hub-widget-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("mkdir");

        let checks: [(&str, &str, Vec<&str>); 2] = [
            ("bash", "bash", vec!["-n"]),
            ("fish", "fish", vec!["--no-execute"]),
        ];
        for (shell, program, flags) in checks {
            let text = script(shell, "toolbox-hub").expect(shell);
            let file = dir.join(format!("widget.{shell}"));
            std::fs::write(&file, &text).expect("write");
            let output = match Command::new(program).args(&flags).arg(&file).output() {
                Ok(output) => output,
                Err(_) => continue, // 没装这个 shell：跳过，不假装通过
            };
            assert!(
                output.status.success(),
                "{shell} 说生成的脚本语法有问题：\n{}\n{}",
                text,
                String::from_utf8_lossy(&output.stderr)
            );
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    // ── 引用与渲染 ──────────────────────────────────────────────────────────

    #[test]
    fn quoting_keeps_a_word_a_word() {
        assert_eq!(quote("ls"), "ls");
        assert_eq!(quote("-lah"), "-lah");
        assert_eq!(quote("/tmp/a.txt"), "/tmp/a.txt");
        // 空格、通配符、变量、波浪号都要变成字面量。
        assert_eq!(quote("my file.txt"), "'my file.txt'");
        assert_eq!(quote("*.mp4"), "'*.mp4'");
        assert_eq!(quote("$HOME"), "'$HOME'");
        assert_eq!(quote("~"), "'~'");
        assert_eq!(quote(""), "''");
        // 单引号本身：'\'' 是最短的正确写法。
        assert_eq!(quote("it's"), r"'it'\''s'");
    }

    #[test]
    fn rendered_line_quotes_values_and_keeps_unfilled_placeholders_visible() {
        let mut values = BTreeMap::new();
        values.insert("path".to_string(), "/home/emo/my docs".to_string());
        assert_eq!(
            render_line("ls", &args(&["-lah", "--", "{path}"]), &values),
            "ls -lah -- '/home/emo/my docs'"
        );

        // 没填的占位符要**看得见**：空字符串会让命令「语法对、语义错」。
        let empty = BTreeMap::new();
        assert_eq!(
            render_line("ls", &args(&["-lah", "--", "{path}"]), &empty),
            "ls -lah -- <path>"
        );
        // 显式填空串也一样留占位符，不会渲染出一个空参数。
        let mut blank = BTreeMap::new();
        blank.insert("path".to_string(), String::new());
        assert_eq!(
            render_line("ls", &args(&["-lah", "--", "{path}"]), &blank),
            "ls -lah -- <path>"
        );
    }

    #[test]
    fn placeholders_embedded_in_a_longer_argument_still_substitute() {
        let mut values = BTreeMap::new();
        values.insert("count".to_string(), "20".to_string());
        assert_eq!(
            render_line("sed", &args(&["-n", "1,{count}p", "{path}"]), &values),
            "sed -n 1,20p <path>"
        );
    }

    #[test]
    fn rendered_line_is_stable_for_the_shipped_registry() {
        // 拿真实的精选库跑一遍：渲染不能 panic，也不能把参数漏成空。
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("registry/featured");
        let recipes = load_recipes(&dir).expect("载入精选库");
        assert!(recipes.len() >= 30);
        let empty = BTreeMap::new();
        for recipe in &recipes {
            let line = render_line(&recipe.command, &recipe.args, &empty);
            assert!(line.starts_with(&recipe.command), "{}: {line}", recipe.id);
            for param in &recipe.params {
                assert!(
                    line.contains(&format!("<{}>", param.name)),
                    "{} 的 {} 应当留下占位符：{line}",
                    recipe.id,
                    param.name
                );
            }
        }
    }

    // ── 过滤 ────────────────────────────────────────────────────────────────

    #[test]
    fn filtering_is_case_insensitive_and_looks_at_title_id_and_command() {
        let list = vec![
            recipe("git-status", "查看 Git 工作区", SafetyLevel::Safe),
            recipe("disk-free", "磁盘剩余空间", SafetyLevel::Safe),
        ];
        assert_eq!(visible_indices(&list, ""), vec![0, 1]);
        assert_eq!(visible_indices(&list, "GIT"), vec![0], "标题/id 大小写无关");
        assert_eq!(visible_indices(&list, "disk"), vec![1]);
        assert_eq!(
            visible_indices(&list, "-lah"),
            vec![0, 1],
            "命令行也参与匹配"
        );
        assert!(visible_indices(&list, "没有这个").is_empty());
    }

    // ── 定位 ────────────────────────────────────────────────────────────────

    #[test]
    fn featured_dir_lookup_prefers_the_most_specific_source() {
        let candidates = featured_candidates(
            Some("/explicit"),
            Some("/env"),
            Some(Path::new("/cwd")),
            Some(Path::new("/opt/bin")),
            Some(Path::new("/home/emo")),
        );
        assert_eq!(candidates[0], PathBuf::from("/explicit"));
        assert_eq!(candidates[1], PathBuf::from("/env"));
        assert_eq!(candidates[2], PathBuf::from("/cwd/registry/featured"));
        assert!(candidates.contains(&PathBuf::from("/opt/bin/share/toolbox-hub/featured")));
        assert!(candidates.contains(&PathBuf::from(
            "/home/emo/.local/share/toolbox-hub/featured"
        )));

        // 空的显式值当作没给：`--dir ""` 不该把后面全部候选干掉。
        let without = featured_candidates(
            Some("  "),
            None,
            Some(Path::new("/cwd")),
            None,
            Some(Path::new("/home/emo")),
        );
        assert_eq!(without[0], PathBuf::from("/cwd/registry/featured"));
    }

    #[test]
    fn loading_the_shipped_registry_keeps_its_ids_unique() {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("registry/featured");
        let recipes = load_recipes(&dir).expect("载入精选库");
        let mut ids = recipes.iter().map(|r| r.id.clone()).collect::<Vec<_>>();
        let total = ids.len();
        ids.sort();
        ids.dedup();
        assert_eq!(ids.len(), total, "精选库里的 id 必须唯一");
    }
}
