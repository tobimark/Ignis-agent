//! 启动欢迎面板：大型横幅 + 圆角双栏信息面板 + 欢迎语。
//!
//! 纯函数层：只读 [`ViewFacts`] 与终端尺寸，不碰终端/会话/IO；所有行都不含换行，宽度由
//! `visible_width` 控制。面板是**实时视图**（不进 scrollback）：首次提交/命令输出后自然消失，
//! Resize 时按新尺寸重新合成。
//!
//! 信息纪律：所有值来自 `ViewFacts`（真实注册表/技能目录/会话），不可用显示 `--`；Tools/Skills
//! 按真实名称分类/折叠，绝不硬编码不存在的功能。

use ignis_core::skills::SkillSource;

use crate::brand::banner;
use crate::brand::sidebar;
use crate::render::width::{char_width, truncate_to_width, visible_width};
use crate::render::{ColorDepth, palette};
use crate::tui::Size;
use crate::tui::view::{ContextUsage, ViewFacts};

/// 面板最大宽度：180 列终端上也不铺满整屏。
const MAX_PANEL_WIDTH: usize = 120;
/// 低于这个宽度不做面板（极简模式）。
const MIN_PANEL_WIDTH: usize = 48;
/// 两栏版式的最小面板宽度。
const TWO_COLUMN_MIN_WIDTH: usize = 72;
/// 左栏标签列宽（`Workspace` 9 列）。
const LABEL_WIDTH: usize = 9;
/// ANSI 重置（欢迎面板自带上色，行尾必须收干净）。
const RESET: &str = "\x1b[0m";

/// 合成欢迎面板（最多 `max_rows` 行）。
pub fn welcome_lines(facts: &ViewFacts, size: Size, max_rows: usize) -> Vec<String> {
    if max_rows == 0 || size.cols == 0 {
        return Vec::new();
    }
    let tier = banner::tier(size.cols, max_rows);
    let mut lines: Vec<String> = banner::lines(tier, facts.version, facts.depth, facts.theme)
        .into_iter()
        .map(|line| center(&line, size.cols))
        .collect();

    let remaining = max_rows.saturating_sub(lines.len());
    let width = panel_width(size.cols);
    if size.cols >= MIN_PANEL_WIDTH && remaining >= 7 {
        // 先给欢迎语/提示留位置；空间不足时至少保住欢迎语。
        let footer = if remaining >= 14 {
            2
        } else if remaining >= 9 {
            1
        } else {
            0
        };
        let budget = remaining.saturating_sub(footer);
        if let Some(panel) = panel_lines(facts, width, budget) {
            lines.extend(panel.into_iter().map(|line| center(&line, size.cols)));
        }
    }

    if lines.len() < max_rows && size.cols >= MIN_PANEL_WIDTH {
        lines.push(center(&welcome_line(facts), size.cols));
    }
    if lines.len() < max_rows && size.cols >= MIN_PANEL_WIDTH {
        lines.push(center(&tip_line(facts), size.cols));
    }

    // 没有面板/空间不足：极简 2–4 行关键状态。
    if lines.len() <= 1 && max_rows > 1 {
        lines.extend(minimal_lines(facts, size.cols, max_rows - lines.len()));
    }

    lines.truncate(max_rows);
    lines
}

/// 面板宽度：终端宽 - 边距，封顶 120。
fn panel_width(cols: usize) -> usize {
    cols.saturating_sub(2)
        .clamp(MIN_PANEL_WIDTH, MAX_PANEL_WIDTH)
}

/// 两栏面板：左 35% / 右 65%（含边框与列间分隔）。
fn panel_lines(facts: &ViewFacts, width: usize, budget: usize) -> Option<Vec<String>> {
    let inner_rows = budget.checked_sub(2)?;
    if inner_rows < 3 || width < MIN_PANEL_WIDTH {
        return None;
    }
    let title = format!(
        "IGNIS v{} · Ever-Living Fire. · {} Runtime",
        facts.version,
        runtime_title(facts)
    );
    let two_column = width >= TWO_COLUMN_MIN_WIDTH;
    let mut out = Vec::with_capacity(inner_rows + 2);
    out.push(top_border(&title, width, facts));
    if two_column {
        // 行格式：`│ {left} │ {right} │`（两侧各一个内边距），列间分隔 3 列。
        let left_width = ((width - 7) * 35 / 100).max(14);
        let right_width = width.checked_sub(7 + left_width)?;
        if right_width < 20 {
            return None;
        }
        let left = left_column(facts, left_width, inner_rows);
        let right = right_column(facts, right_width, inner_rows);
        let rows = left.len().max(right.len()).min(inner_rows);
        for index in 0..rows {
            let left_line = left.get(index).map(String::as_str).unwrap_or("");
            let right_line = right.get(index).map(String::as_str).unwrap_or("");
            out.push(two_columns(
                left_line,
                left_width,
                right_line,
                right_width,
                facts,
            ));
        }
    } else {
        let content = single_column(facts, width.saturating_sub(4), inner_rows);
        for line in content.into_iter().take(inner_rows) {
            out.push(one_column(&line, width, facts));
        }
    }
    out.push(bottom_border(width, facts));
    Some(out)
}

// ---- 左栏 ----

/// 左栏：火焰缩略图 + 当前环境信息。
fn left_column(facts: &ViewFacts, width: usize, max_rows: usize) -> Vec<String> {
    let info = info_lines(facts, width);
    // 火焰优先：至少留 3 行 + 1 行间隔；信息行按剩余空间裁剪（80×24 下 6 行信息全保留）。
    let info_budget = max_rows.saturating_sub(4).max(1);
    let info: Vec<String> = info.into_iter().take(info_budget).collect();
    let flame_rows = max_rows.saturating_sub(info.len() + 1).min(sidebar::ROWS);
    let mut out = Vec::new();
    if flame_rows >= 3 {
        let art = sidebar::rows(flame_rows, facts.depth);
        out.extend(art.into_iter().map(|line| center(&line, width)));
        out.push(String::new());
    }
    out.extend(info);
    out.truncate(max_rows);
    out
}

/// 环境信息：Model / Provider / Workspace / Session / Runtime / Memory。
fn info_lines(facts: &ViewFacts, width: usize) -> Vec<String> {
    let model = if facts.native {
        facts.model.clone()
    } else {
        "(kernel default)".to_string()
    };
    let provider = if facts.native {
        facts.provider.clone()
    } else {
        "--".to_string()
    };
    let workspace = middle_truncate(&facts.workspace, width.saturating_sub(LABEL_WIDTH + 1));
    let runtime = if facts.native {
        facts.kernel.clone()
    } else {
        format!("{} · external", facts.kernel)
    };
    let memory = facts
        .memory
        .map_or_else(|| "--".to_string(), |count| format!("{count} facts"));
    vec![
        field(facts, "Model", &model, width),
        field(facts, "Provider", &provider, width),
        field(facts, "Workspace", &workspace, width),
        field(facts, "Session", &facts.session, width),
        field(facts, "Runtime", &runtime, width),
        field(facts, "Memory", &memory, width),
    ]
}

/// `Label     value`：标签 dim、值普通；超宽按显示宽度截断。
fn field(facts: &ViewFacts, label: &str, value: &str, width: usize) -> String {
    let label = format!("{label:<LABEL_WIDTH$}");
    let value_width = width.saturating_sub(LABEL_WIDTH + 1);
    // 超宽值用省略号收尾，避免 `OpenAI-compati` 这种硬切看起来像坏数据。
    let value = if visible_width(value) > value_width {
        if value_width == 0 {
            String::new()
        } else {
            format!("{}…", truncate_to_width(value, value_width - 1))
        }
    } else {
        value.to_string()
    };
    if colored(facts) {
        let dim = palette(facts.theme, facts.depth).dim;
        format!("{dim}{label}{RESET} {value}")
    } else {
        format!("{label} {value}")
    }
}

// ---- 右栏 ----

/// 右栏：Available Tools / Available Skills / 统计摘要。
fn right_column(facts: &ViewFacts, width: usize, max_rows: usize) -> Vec<String> {
    let mut out = vec![header(facts, "Available Tools")];
    if facts.native {
        for (category, names) in group_tools(&facts.tools) {
            out.push(category_line(facts, category, &names, width));
        }
        if facts.tools.is_empty() {
            out.push(dim_line(facts, "  --"));
        }
    } else {
        out.push(dim_line(
            facts,
            &format!("  not exposed by {} kernel", facts.kernel),
        ));
    }

    out.push(String::new());
    out.push(header(facts, "Available Skills"));
    let (builtin, user) = group_skills(facts);
    if builtin.is_empty() && user.is_empty() {
        out.push(dim_line(facts, "  --"));
    } else {
        if !builtin.is_empty() {
            out.push(category_line(facts, "builtin", &builtin, width));
        }
        if !user.is_empty() {
            out.push(category_line(facts, "user", &user, width));
        }
    }

    out.push(String::new());
    out.push(dim_line(facts, &stats_line(facts, width)));
    out.truncate(max_rows);
    out
}

/// 工具按真实名称分类；顺序固定，未知工具落 `other`。
fn group_tools(tools: &[String]) -> Vec<(&'static str, Vec<String>)> {
    let order = ["filesystem", "shell", "network", "agent", "other"];
    order
        .iter()
        .filter_map(|category| {
            let names: Vec<String> = tools
                .iter()
                .filter(|name| tool_category(name) == *category)
                .cloned()
                .collect();
            (!names.is_empty()).then_some((*category, names))
        })
        .collect()
}

fn tool_category(name: &str) -> &'static str {
    match name {
        "fs_read" | "fs_list" | "fs_write" | "fs_edit" => "filesystem",
        "shell_run" => "shell",
        "http_fetch" => "network",
        "remember" | "load_skill" | "notify_user" | "schedule_create" => "agent",
        _ => "other",
    }
}

/// 技能按真实来源分组（内置/用户），保留目录里的名称顺序。
fn group_skills(facts: &ViewFacts) -> (Vec<String>, Vec<String>) {
    let mut builtin = Vec::new();
    let mut user = Vec::new();
    for meta in &facts.skills {
        match meta.source {
            SkillSource::Builtin => builtin.push(meta.name.clone()),
            SkillSource::User => user.push(meta.name.clone()),
        }
    }
    (builtin, user)
}

/// `  category: a, b, c +N more`：最多展示 4 个，剩余折叠。
fn category_line(facts: &ViewFacts, category: &str, names: &[String], width: usize) -> String {
    let prefix = format!("  {category}: ");
    let limit = 4.min(names.len());
    let mut chosen = 0usize;
    for count in (1..=limit).rev() {
        let suffix = if names.len() > count {
            format!(" +{} more", names.len() - count)
        } else {
            String::new()
        };
        let joined = names[..count].join(", ");
        if visible_width(&prefix) + visible_width(&joined) + visible_width(&suffix) <= width {
            chosen = count;
            break;
        }
    }
    if chosen == 0 && !names.is_empty() {
        // 一个名字都放不下：截断第一个名字，至少让用户看到类别存在。
        let available = width.saturating_sub(visible_width(&prefix) + 1);
        let first = truncate_to_width(&names[0], available);
        return dim_line(facts, &format!("{prefix}{first}…"));
    }
    let mut line = format!("{prefix}{}", names[..chosen].join(", "));
    if names.len() > chosen {
        line.push_str(&format!(" +{} more", names.len() - chosen));
    }
    dim_line(facts, &line)
}

/// 统计摘要：真实计数；读不到记忆时显示 `--`。
fn stats_line(facts: &ViewFacts, width: usize) -> String {
    let tools = if facts.native {
        format!("{} tools", facts.tools.len())
    } else {
        "tools not exposed".to_string()
    };
    let memory = facts.memory.map_or_else(
        || "-- memories".to_string(),
        |count| format!("{count} memories"),
    );
    // 窄栏用短提示，避免 80 列下 `/help for commands` 被切掉尾巴。
    let help = if width >= 60 {
        "/help for commands"
    } else {
        "/help"
    };
    let line = format!(
        "{tools} · {} skills · {memory} · {help}",
        facts.skills.len()
    );
    truncate_to_width(&line, width).to_string()
}

// ---- 单栏/极简 ----

/// 单栏（60–71 列）：环境 + 工具 + 技能各压成一行。
fn single_column(facts: &ViewFacts, width: usize, max_rows: usize) -> Vec<String> {
    let model = if facts.native {
        format!("{} · {}", facts.model, facts.provider)
    } else {
        "(kernel default)".to_string()
    };
    let mut out = Vec::new();
    // 单栏也保留火焰：4 行缩略图 + 一行间隔，正好和 6 行信息一起装进 11 行。
    if max_rows >= 9 {
        let art = sidebar::rows(4, facts.depth);
        out.extend(art.into_iter().map(|line| center(&line, width)));
        out.push(String::new());
    }
    let memory = facts
        .memory
        .map_or_else(|| "--".to_string(), |count| format!("{count} facts"));
    let tools = tool_summary(facts, width);
    let skills = skill_summary(facts, width);
    let stats = format!(
        "{} tools · {} skills · {memory} · /help",
        facts.tools.len(),
        facts.skills.len()
    );
    out.extend([
        field(facts, "Model", &model, width),
        field(
            facts,
            "Workspace",
            &middle_truncate(&facts.workspace, width.saturating_sub(LABEL_WIDTH + 1)),
            width,
        ),
        field(
            facts,
            "Session",
            &format!("{} · {} · {memory}", facts.session, facts.kernel),
            width,
        ),
        dim_line(facts, &tools),
        dim_line(facts, &skills),
        dim_line(facts, &stats),
    ]);
    out.truncate(max_rows);
    out
}

fn tool_summary(facts: &ViewFacts, width: usize) -> String {
    if !facts.native {
        return format!("Tools: not exposed by {} kernel", facts.kernel);
    }
    let groups = group_tools(&facts.tools);
    let summary = groups
        .iter()
        .map(|(category, names)| format!("{category}: {}", names.join(", ")))
        .collect::<Vec<_>>()
        .join(" · ");
    truncate_to_width(&format!("Tools: {summary}"), width).to_string()
}

fn skill_summary(facts: &ViewFacts, width: usize) -> String {
    let (builtin, user) = group_skills(facts);
    let mut parts = Vec::new();
    if !builtin.is_empty() {
        parts.push(format!("builtin: {}", builtin.join(", ")));
    }
    if !user.is_empty() {
        parts.push(format!("user: {}", user.join(", ")));
    }
    let summary = if parts.is_empty() {
        "--".to_string()
    } else {
        parts.join(" · ")
    };
    truncate_to_width(&format!("Skills: {summary}"), width).to_string()
}

/// 极简（<48 列或没有面板空间）：只留模型/上下文/统计/提示。
fn minimal_lines(facts: &ViewFacts, cols: usize, max_rows: usize) -> Vec<String> {
    let model = if facts.native {
        format!("{} · {}", facts.model, facts.provider)
    } else {
        format!("{} · (kernel default)", facts.kernel)
    };
    let context = facts
        .context
        .map_or_else(|| "ctx -/?".to_string(), context_short);
    let stats = format!(
        "{} tools · {} skills · {}",
        facts.tools.len(),
        facts.skills.len(),
        facts.memory.map_or_else(
            || "-- memories".to_string(),
            |count| format!("{count} memories")
        )
    );
    let mut lines = vec![
        truncate_to_width(&model, cols).to_string(),
        truncate_to_width(&format!("{context} · {stats}"), cols).to_string(),
        truncate_to_width(&welcome_line(facts), cols).to_string(),
        truncate_to_width(&tip_line(facts), cols).to_string(),
    ];
    lines.truncate(max_rows);
    lines
}

fn context_short(context: ContextUsage) -> String {
    format!(
        "ctx {}{}/{}",
        if context.estimated { "≈" } else { "" },
        context.used_word(),
        context.window_word()
    )
}

// ---- 边框与颜色 ----

fn two_columns(
    left: &str,
    left_width: usize,
    right: &str,
    right_width: usize,
    facts: &ViewFacts,
) -> String {
    let left = pad_to(left, left_width);
    let right = pad_to(right, right_width);
    if colored(facts) {
        let dim = palette(facts.theme, facts.depth).dim;
        format!("{dim}│{RESET} {left} {dim}│{RESET} {right} {dim}│{RESET}")
    } else {
        format!("│ {left} │ {right} │")
    }
}

fn one_column(content: &str, width: usize, facts: &ViewFacts) -> String {
    let inner = width.saturating_sub(4);
    let content = pad_to(truncate_to_width(content, inner), inner);
    if colored(facts) {
        let dim = palette(facts.theme, facts.depth).dim;
        format!("{dim}│{RESET} {content} {dim}│{RESET}")
    } else {
        format!("│ {content} │")
    }
}

fn top_border(title: &str, width: usize, facts: &ViewFacts) -> String {
    let inner = width.saturating_sub(2);
    let max_title = inner.saturating_sub(3);
    let title = truncate_to_width(title, max_title);
    let fill = inner.saturating_sub(3 + visible_width(title));
    if colored(facts) {
        let slots = palette(facts.theme, facts.depth);
        format!(
            "{dim}╭─ {RESET}{bold}{brand}{title}{RESET}{dim} {}╮{RESET}",
            "─".repeat(fill),
            dim = slots.dim,
            bold = slots.bold,
            brand = slots.brand
        )
    } else {
        format!("╭─ {title} {}╮", "─".repeat(fill))
    }
}

fn bottom_border(width: usize, facts: &ViewFacts) -> String {
    let inner = width.saturating_sub(2);
    if colored(facts) {
        let dim = palette(facts.theme, facts.depth).dim;
        format!("{dim}╰{}╯{RESET}", "─".repeat(inner))
    } else {
        format!("╰{}╯", "─".repeat(inner))
    }
}

fn header(facts: &ViewFacts, text: &str) -> String {
    if colored(facts) {
        let slots = palette(facts.theme, facts.depth);
        format!("{}{}{}{}", slots.bold, slots.accent, text, RESET)
    } else {
        text.to_string()
    }
}

fn dim_line(facts: &ViewFacts, text: &str) -> String {
    if colored(facts) {
        let dim = palette(facts.theme, facts.depth).dim;
        format!("{dim}{text}{RESET}")
    } else {
        text.to_string()
    }
}

fn welcome_line(facts: &ViewFacts) -> String {
    if colored(facts) {
        let slots = palette(facts.theme, facts.depth);
        format!(
            "{bold}{brand}Fire, With Intent.{RESET}",
            bold = slots.bold,
            brand = slots.brand
        )
    } else {
        "Fire, With Intent.".to_string()
    }
}

fn tip_line(facts: &ViewFacts) -> String {
    if colored(facts) {
        let slots = palette(facts.theme, facts.depth);
        format!(
            "{brand}✦{RESET}{dim} Tip: Type /help to explore commands.{RESET}",
            brand = slots.brand,
            dim = slots.dim
        )
    } else {
        "✦ Tip: Type /help to explore commands.".to_string()
    }
}

fn colored(facts: &ViewFacts) -> bool {
    facts.color && facts.depth != ColorDepth::None
}

fn runtime_title(facts: &ViewFacts) -> String {
    if facts.native {
        "Native".to_string()
    } else {
        match facts.kernel.as_str() {
            "codex" => "Codex".to_string(),
            "claude" => "Claude".to_string(),
            other => {
                let mut chars = other.chars();
                match chars.next() {
                    Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
                    None => "External".to_string(),
                }
            }
        }
    }
}

// ---- 文本工具 ----

fn center(line: &str, cols: usize) -> String {
    let width = visible_width(line);
    if width >= cols {
        return truncate_to_width(line, cols).to_string();
    }
    let pad = (cols - width) / 2;
    format!("{}{line}", " ".repeat(pad))
}

fn pad_to(line: &str, width: usize) -> String {
    let line = truncate_to_width(line, width);
    let visible = visible_width(line);
    if visible >= width {
        line.to_string()
    } else {
        format!("{line}{}", " ".repeat(width - visible))
    }
}

/// 长路径中间截断：保留开头与结尾，中间一个 `…`。
fn middle_truncate(text: &str, width: usize) -> String {
    if visible_width(text) <= width {
        return text.to_string();
    }
    if width <= 1 {
        return truncate_to_width(text, width).to_string();
    }
    // 头尾各占一半：既不只剩开头，也不只剩结尾（长路径要能认出两端）。
    let max_tail = (width.saturating_sub(2) / 2).max(1);
    let mut tail = String::new();
    let mut tail_width = 0usize;
    for ch in text.chars().rev() {
        let w = char_width(ch);
        if tail_width + w > max_tail {
            break;
        }
        tail.insert(0, ch);
        tail_width += w;
    }
    let head_width = width.saturating_sub(1 + tail_width);
    let head = truncate_to_width(text, head_width);
    format!("{head}…{tail}")
}

#[cfg(test)]
mod tests {
    use ignis_core::config::{OutputMode, Theme};
    use ignis_core::skills::{SkillMeta, SkillSource};
    use ignis_runtime::caps::ModelCapabilities;

    use crate::render::ColorDepth;
    use crate::render::width::{truncate_to_width, visible_width};
    use crate::tui::Size;
    use crate::tui::view::{ContextUsage, ViewFacts, content_rows, estimate_tokens_from_chars};

    use super::{middle_truncate, welcome_lines};

    fn facts() -> ViewFacts {
        let skills = vec![
            SkillMeta {
                name: "project-analyzer".into(),
                description: "Analyze a project".into(),
                source: SkillSource::Builtin,
            },
            SkillMeta {
                name: "report-generator".into(),
                description: "Generate a report".into(),
                source: SkillSource::Builtin,
            },
            SkillMeta {
                name: "server-health".into(),
                description: "Check a server".into(),
                source: SkillSource::Builtin,
            },
            SkillMeta {
                name: "team-notes".into(),
                description: "Team notes".into(),
                source: SkillSource::User,
            },
        ];
        ViewFacts {
            version: env!("CARGO_PKG_VERSION"),
            workspace: "/home/tobi/workspace/ignis-agent".to_string(),
            kernel: "native".to_string(),
            session: "01a1534c-9cac".to_string(),
            mode: OutputMode::Normal,
            interaction_mode: crate::commands::InteractionMode::Normal,
            theme: Theme::Dark,
            color: false,
            depth: ColorDepth::None,
            budget: 200_000,
            history_messages: 2,
            stats: None,
            usage: None,
            native: true,
            model: "gpt-4o-mini".to_string(),
            provider: "OpenAI".to_string(),
            capabilities: ModelCapabilities::static_resolve(&ignis_runtime::ModelConfig::default()),
            context: Some(ContextUsage::new(
                estimate_tokens_from_chars(12_000),
                true,
                Some(128_000),
            )),
            tools: vec![
                "fs_read".into(),
                "fs_list".into(),
                "fs_write".into(),
                "fs_edit".into(),
                "shell_run".into(),
                "http_fetch".into(),
                "remember".into(),
                "load_skill".into(),
                "notify_user".into(),
                "schedule_create".into(),
            ],
            skills,
            memory: Some(7),
        }
    }

    /// 四种目标尺寸 + 两个边界：行数不超过内容区、行宽不超过终端、真实数据都在。
    #[test]
    fn welcome_fits_the_target_sizes_and_shows_real_data() {
        for (cols, rows) in [
            (80, 24),
            (100, 30),
            (140, 40),
            (180, 50),
            (60, 24),
            (40, 12),
        ] {
            let size = Size { cols, rows };
            let budget = content_rows(size, 1);
            let lines = welcome_lines(&facts(), size, budget);
            assert!(
                lines.len() <= budget,
                "{cols}x{rows}: {} > {budget}",
                lines.len()
            );
            for line in &lines {
                assert!(
                    visible_width(line) <= cols,
                    "{cols}x{rows}: 行宽超限 {}: {line:?}",
                    visible_width(line)
                );
                assert!(!line.contains('\n'), "{cols}x{rows}: {line:?}");
                assert!(!line.contains('\u{1b}'), "无色现场零 SGR: {line:?}");
            }
            let joined = lines.join("\n");
            assert!(joined.contains("IGNIS"), "{cols}x{rows}: {joined}");
            assert!(
                joined.contains("gpt-4o-mini") || cols < 48,
                "{cols}x{rows}: {joined}"
            );
            if cols >= 80 {
                assert!(joined.contains("fs_read"), "{cols}x{rows}: {joined}");
                assert!(
                    joined.contains("project-analyzer"),
                    "{cols}x{rows}: {joined}"
                );
                assert!(joined.contains("01a1534c-9cac"), "{cols}x{rows}: {joined}");
                assert!(joined.contains("7 facts"), "{cols}x{rows}: {joined}");
                assert!(joined.contains("╭"), "面板边框必须存在：{joined}");
            }
        }
    }

    /// 80×24 的面板有火焰、双栏、折叠统计；欢迎语与提示在末尾。
    #[test]
    fn welcome_at_80x24_has_the_flame_and_the_footer() {
        let size = Size { cols: 80, rows: 24 };
        let lines = welcome_lines(&facts(), size, content_rows(size, 1));
        let joined = lines.join("\n");
        assert!(
            joined.contains('█') || joined.contains('▀') || joined.contains('▄'),
            "火焰 Logo 必须保留：{joined}"
        );
        assert!(joined.contains("Available Tools"), "{joined}");
        assert!(joined.contains("Available Skills"), "{joined}");
        assert!(
            joined.contains("10 tools · 4 skills · 7 memories"),
            "{joined}"
        );
        assert!(
            joined.contains("Fire, With Intent."),
            "欢迎语必须在：{joined}"
        );
        assert!(joined.contains("✦ Tip: Type /help"), "提示必须在：{joined}");
        assert!(
            joined.contains("Ever-Living Fire."),
            "新的面板标题必须在：{joined}"
        );
        assert!(!joined.contains("Autonomous Agent"), "{joined}");
    }

    /// 外部内核：工具显示为未暴露，不伪造可用性。
    #[test]
    fn welcome_marks_tools_as_not_exposed_for_external_kernels() {
        let mut facts = facts();
        facts.kernel = "codex".into();
        facts.native = false;
        let size = Size {
            cols: 100,
            rows: 30,
        };
        let joined = welcome_lines(&facts, size, content_rows(size, 1)).join("\n");
        assert!(joined.contains("not exposed by codex kernel"), "{joined}");
        assert!(joined.contains("Codex Runtime"), "{joined}");
        assert!(!joined.contains("fs_read"), "{joined}");
    }

    /// 未知记忆：显示 `--`，不显示 0 或假数字。
    #[test]
    fn welcome_shows_dash_when_memory_is_unavailable() {
        let mut facts = facts();
        facts.memory = None;
        let size = Size {
            cols: 140,
            rows: 40,
        };
        let joined = welcome_lines(&facts, size, content_rows(size, 1)).join("\n");
        assert!(joined.contains("-- memories"), "{joined}");
    }

    /// 窄栏的超长字段值用省略号收尾，不硬切。
    #[test]
    fn long_field_values_end_with_an_ellipsis() {
        let mut facts = facts();
        facts.provider = "OpenAI-compatible".into();
        let size = Size { cols: 80, rows: 24 };
        let joined = welcome_lines(&facts, size, content_rows(size, 1)).join("\n");
        assert!(joined.contains("OpenAI-compat…"), "{joined}");
    }

    /// 未知窗口/无用量：状态片段显示 `?`/`-`，欢迎面板不 panic。
    #[test]
    fn welcome_handles_unknown_context_window() {
        let mut facts = facts();
        facts.context = None;
        let size = Size { cols: 60, rows: 24 };
        let lines = welcome_lines(&facts, size, content_rows(size, 1));
        assert!(!lines.is_empty());
        assert!(lines.iter().all(|line| visible_width(line) <= 60));
    }

    /// 超长工具/技能列表：每行折叠，不无限增长。
    #[test]
    fn long_lists_are_folded_into_one_line_per_category() {
        let mut facts = facts();
        facts.tools = (0..40).map(|index| format!("fs_tool_{index}")).collect();
        facts.skills = (0..40)
            .map(|index| SkillMeta {
                name: format!("skill-{index}"),
                description: String::new(),
                source: SkillSource::User,
            })
            .collect();
        let size = Size {
            cols: 100,
            rows: 30,
        };
        let lines = welcome_lines(&facts, size, content_rows(size, 1));
        let joined = lines.join("\n");
        assert!(joined.contains("+"), "长列表必须折叠：{joined}");
        assert!(
            lines.len() <= content_rows(size, 1),
            "面板不能无限增长：{}",
            lines.len()
        );
    }

    /// 长路径中间截断：保留开头与结尾。
    #[test]
    fn middle_truncate_keeps_both_ends() {
        let path = "/home/tobi/workspace/ignis-agent/packages/runtime/src/caps.rs";
        let cut = middle_truncate(path, 30);
        assert!(visible_width(&cut) <= 30, "{cut:?}");
        assert!(cut.contains('…'), "{cut:?}");
        assert!(cut.starts_with("/home/"), "{cut:?}");
        assert!(cut.ends_with("caps.rs"), "{cut:?}");
        assert_eq!(middle_truncate("short", 30), "short");
        assert!(visible_width(truncate_to_width(path, 10)) <= 10);
    }

    /// 无色现场零 SGR；上色现场带品牌/渐变 SGR。
    #[test]
    fn color_is_gated_by_the_color_flag() {
        let size = Size {
            cols: 140,
            rows: 40,
        };
        let plain = welcome_lines(&facts(), size, content_rows(size, 1));
        assert!(plain.iter().all(|line| !line.contains('\u{1b}')));

        let mut colored = facts();
        colored.color = true;
        colored.depth = ColorDepth::True;
        let lines = welcome_lines(&colored, size, content_rows(size, 1));
        let joined = lines.join("\n");
        assert!(
            joined.contains("\u{1b}[38;2;"),
            "横幅渐变必须上色：{joined}"
        );
        assert!(joined.contains("\u{1b}["), "面板必须有 SGR：{joined}");
        for line in lines {
            assert!(visible_width(&line) <= 140);
        }
    }

    /// 用量/窗口的词形：精确不加 `≈`，估算加 `≈`，未知窗口 `?`。
    #[test]
    fn context_words_are_explicit_about_estimates_and_unknowns() {
        let exact = ContextUsage::new(12_500, false, Some(128_000));
        assert_eq!(exact.segment(10), "ctx 12.5K/128K [█░░░░░░░░░] 10%");
        let estimated = ContextUsage::new(12_500, true, Some(128_000));
        assert_eq!(estimated.segment(10), "ctx ≈12.5K/128K [█░░░░░░░░░] 10%");
        let unknown = ContextUsage::new(12_500, false, None);
        assert_eq!(unknown.segment(10), "ctx 12.5K/? [----------] --");
        assert_eq!(unknown.percent_word(), "--");
    }
}
