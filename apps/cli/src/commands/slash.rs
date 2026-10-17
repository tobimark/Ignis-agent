//! REPL 的斜杠命令：命令表、解析、纯渲染函数与分发。
//!
//! 三条纪律：
//! - **解析**只认「首词逐字命中命令表」：[`COMMANDS`] 之外的所有 `/…` 都是 [`Parsed::Unknown`]，
//!   由 REPL 打到 stderr（v0.1 里它们会被当成任务送给模型）。
//! - **渲染**是纯函数（名字/模式/用量 → 行）：文案、分组与对齐都写在它们里；[`dispatch`] 只做
//!   「读会话字段 → 调渲染函数 / 调 setter」的薄接线，不自己拼字符串。
//! - **文案**：本期新增的 REPL 界面文案一律英文；未知命令行、帮助条目与分组逐字钉住。

use std::path::Path;

use ignis_core::config::OutputMode;
use ignis_core::memory::FactScope;
use ignis_runtime::Usage;
use ignis_storage::{SessionSummary, TaskRecord};

use super::Session;
use super::kernel::{KERNELS, NATIVE_KERNEL, kernel_note};
use super::{memory, skills, task};
use crate::render::short_id;

/// 一条斜杠命令的行为（[`parse`] 查表命中后交给 [`dispatch`]）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Slash {
    /// `/exit`：退出 REPL。
    Exit,
    /// `/help`：打印命令表。
    Help,
    /// `/kernel [name]`：显示或切换执行内核。
    Kernel,
    /// `/model [name]`：显示或设置本会话的模型（仅 native 生效）。
    Model,
    /// `/status`：内核 / 模型 / 工作区 / 未读告警 / 最近一轮用量。
    Status,
    /// `/tasks [N]`：最近任务表（默认 10 条 + `ignis task show` 提示）。
    Tasks,
    /// `/skills`：技能目录（与 `ignis skills list` 同一份渲染）。
    Skills,
    /// `/memory …`：长期记忆（list/search/add/edit/delete，复用 `ignis memory …` 的口径）。
    Memory,
    /// `/details [minimal|normal|debug]`：显示或切换输出模式。
    Details,
    /// `/new`：开始新会话（只清会话指针与历史，不删数据）。
    New,
    /// `/sessions`：本工作区最近会话列表（当前会话带 `*`）。
    Sessions,
    /// `/resume <id>`：恢复本工作区的一条会话（id 可以是 ≥8 字符的唯一前缀）。
    Resume,
}

/// 一行输入的含义。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Parsed {
    /// 空行/纯空白：跳过，不作为任务（编辑器可能提交空串）。
    Empty,
    /// 首词是 `/` 但不认识：`String` 是首词去掉斜杠后的原文（大小写不归一，`/Help` 就是 `Help`）。
    Unknown(String),
    /// 认识命令：命令本身 + 按空白切分的参数。
    Command(Slash, Vec<String>),
    /// 其余输入：一次任务的内容（只裁首尾空白，内部换行保留）。
    Task(String),
}

/// 本地命令的结果：退出，或要逐行打印的输出。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Dispatch {
    /// `/exit`：REPL 退出。
    Exit,
    /// 命令的输出行（帮助 / 状态 / 回显）。
    Lines(Vec<String>),
}

/// Tab 补全的结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Completion {
    /// 唯一候选：整行替换成这条文本。
    Accepted(String),
    /// 多个候选：列出行、输入行不变。
    Listing(Vec<String>),
    /// 没有可补全的。
    None,
}

/// `/details` 非法取值的错误（逐字）。
const DETAILS_ERROR: &str = "details: mode must be minimal, normal or debug";
/// `/kernel` 参数多于一个的错误（逐字）。
const KERNEL_USAGE: &str = "kernel: usage: /kernel [native|codex|claude]";
/// `/model` 参数多于一个的错误：与 `/kernel` 同一形状（`[name]` 只有一个位置）。
const MODEL_USAGE: &str = "model: usage: /model [name]";
/// 会话级模型覆盖的持久化提示（`/model` 有无参两条路径共用同一份文案）。
const MODEL_NOTE: &str = "note: session override; persist with `ignis config set model.model`";
/// `/memory` 的形状不对时的用法行（未知子命令与 `list` 的非法旗标共用）。
const MEMORY_USAGE: &str = "usage: /memory <list|search|add|edit|delete>";
/// `/memory add` 缺参（`<scope> <key> <text>` 任一为空）时的用法行。
const MEMORY_ADD_USAGE: &str = "usage: /memory add <user|project|global> <key> <text>";
/// `/memory edit` 缺参（没有 `<text>`）时的用法行。
const MEMORY_EDIT_USAGE: &str = "usage: /memory edit <id> <text>";
/// `/memory delete` 缺参（没有 `<id>`）时的用法行。
const MEMORY_DELETE_USAGE: &str = "usage: /memory delete <id>";
/// `/tasks` 的条数不是正整数时的错误（`0`/负数/非数字/多余词同一条）。
const TASKS_LIMIT_ERROR: &str = "tasks: limit must be a positive number";
/// `/tasks` 无参时的条数（与帮助里的 `(default 10)` 同一口径）。
const TASKS_DEFAULT_LIMIT: usize = 10;
/// `/resume` 缺参或多参时的用法行（命令只收一个位置）。
const RESUME_USAGE: &str = "resume: usage: /resume <id>";

/// 帮助分组（顺序即 [`help_lines`] 的输出顺序）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Group {
    /// 会话面（`/new` `/sessions` `/resume`）。
    Session,
    /// 智能体本身的开关。
    Agent,
    /// 记忆面。
    Intelligence,
    /// 输入/输出与元命令。
    Interface,
}

impl Group {
    /// 组名（帮助里独占一行）。
    fn label(self) -> &'static str {
        match self {
            Group::Session => "SESSION",
            Group::Agent => "AGENT",
            Group::Intelligence => "INTELLIGENCE",
            Group::Interface => "INTERFACE",
        }
    }
}

/// 命令表的一行：命令、分组、名字、参数提示、一行描述与「有没有参数」。
struct Spec {
    /// 名字到 [`Slash`] 的映射（`parse` 查表后直接用它）。
    command: Slash,
    group: Group,
    /// 命令名（含斜杠，如 `/kernel`）。
    name: &'static str,
    /// 参数提示（`[name]`、`<list|search|add|edit|delete>`…）；无参数是空串。
    arg_hint: &'static str,
    /// 帮助里的那一行描述（英文）。
    summary: &'static str,
    /// 有参数：补全唯一命中时补一个空格（`/ke` → `/kernel `）。
    expects_args: bool,
}

/// 命令表：帮助与补全的唯一事实来源（条目、分组、参数提示与描述都只写一遍）。
///
/// 顺序即组内顺序（`SESSION` 组排在最前；`/context` 与 `/compact` 是 INTELLIGENCE 组的后续条目，
/// 由 Task 6 / Task 7 各自进表）。
const COMMANDS: [Spec; 12] = [
    Spec {
        command: Slash::New,
        group: Group::Session,
        name: "/new",
        arg_hint: "",
        summary: "Start a new session",
        expects_args: false,
    },
    Spec {
        command: Slash::Sessions,
        group: Group::Session,
        name: "/sessions",
        arg_hint: "",
        summary: "List recent sessions in this workspace",
        expects_args: false,
    },
    Spec {
        command: Slash::Resume,
        group: Group::Session,
        name: "/resume",
        arg_hint: "<id>",
        summary: "Resume a session from this workspace",
        expects_args: true,
    },
    Spec {
        command: Slash::Model,
        group: Group::Agent,
        name: "/model",
        arg_hint: "[name]",
        summary: "Show or set the model of this session",
        expects_args: true,
    },
    Spec {
        command: Slash::Status,
        group: Group::Agent,
        name: "/status",
        arg_hint: "",
        summary: "Show kernel, model, workspace, alerts and last usage",
        expects_args: false,
    },
    Spec {
        command: Slash::Tasks,
        group: Group::Agent,
        name: "/tasks",
        arg_hint: "[N]",
        summary: "List the most recent tasks (default 10)",
        expects_args: true,
    },
    Spec {
        command: Slash::Skills,
        group: Group::Agent,
        name: "/skills",
        arg_hint: "",
        summary: "List the skill catalog",
        expects_args: false,
    },
    Spec {
        command: Slash::Memory,
        group: Group::Intelligence,
        name: "/memory",
        arg_hint: "<list|search|add|edit|delete>",
        summary: "Read or edit long-term memory",
        expects_args: true,
    },
    Spec {
        command: Slash::Details,
        group: Group::Interface,
        name: "/details",
        arg_hint: "[minimal|normal|debug]",
        summary: "Show or switch the output mode",
        expects_args: true,
    },
    Spec {
        command: Slash::Exit,
        group: Group::Interface,
        name: "/exit",
        arg_hint: "",
        summary: "Leave the REPL",
        expects_args: false,
    },
    Spec {
        command: Slash::Help,
        group: Group::Interface,
        name: "/help",
        arg_hint: "",
        summary: "Show this list",
        expects_args: false,
    },
    Spec {
        command: Slash::Kernel,
        group: Group::Interface,
        name: "/kernel",
        arg_hint: "[name]",
        summary: "Show or switch the execution kernel (native|codex|claude)",
        expects_args: true,
    },
];

/// 命令名查表：逐字相等（`/Help` 不是 `/help`，也就不在表里）。
fn spec_by_name(name: &str) -> Option<&'static Spec> {
    COMMANDS.iter().find(|spec| spec.name == name)
}

/// 帮助/补全里一行的「名 + 参数提示」列。
fn spec_label(spec: &Spec) -> String {
    if spec.arg_hint.is_empty() {
        spec.name.to_string()
    } else {
        format!("{} {}", spec.name, spec.arg_hint)
    }
}

/// 组内对齐后的一行：`  {名 + 参数提示}  {描述}`（列间两个空格）。
fn spec_line(spec: &Spec, width: usize) -> String {
    let label = spec_label(spec);
    let pad = width.saturating_sub(label.chars().count());
    format!("  {label}{}  {}", " ".repeat(pad), spec.summary)
}

/// 一组条目按 `/help` 的格式落行：组名独占一行、组内按最宽对齐；没有条目的组不出现。
fn group_lines(specs: &[&Spec]) -> Vec<String> {
    let mut lines = Vec::new();
    for group in [
        Group::Session,
        Group::Agent,
        Group::Intelligence,
        Group::Interface,
    ] {
        let entries: Vec<&Spec> = specs
            .iter()
            .copied()
            .filter(|spec| spec.group == group)
            .collect();
        if entries.is_empty() {
            continue;
        }
        let width = entries
            .iter()
            .map(|spec| spec_label(spec).chars().count())
            .max()
            .unwrap_or(0);
        lines.push(group.label().to_string());
        lines.extend(entries.iter().map(|spec| spec_line(spec, width)));
    }
    lines
}

/// `/help` 的输出：四个分组与十二条命令（顺序与 [`COMMANDS`] 一致）。
pub fn help_lines() -> Vec<String> {
    group_lines(&COMMANDS.iter().collect::<Vec<_>>())
}

/// 分类一行输入。
pub fn parse(line: &str) -> Parsed {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return Parsed::Empty;
    }
    // `trim` 之后至少有一个词（`split_whitespace` 不会给空串）
    let mut words = trimmed.split_whitespace();
    let first = words.next().unwrap_or_default();
    let Some(name) = first.strip_prefix('/') else {
        // 不以 `/` 开头：整段（内部换行保留）就是任务内容
        return Parsed::Task(trimmed.to_string());
    };
    match spec_by_name(first) {
        Some(spec) => Parsed::Command(spec.command, words.map(str::to_string).collect()),
        None => Parsed::Unknown(name.to_string()),
    }
}

/// 未知命令的报错行（REPL 打到 stderr；**绝不**送给模型）。
pub fn unknown_command_line(name: &str) -> String {
    format!("Unknown command: /{name} — /help lists commands")
}

/// `/kernel` 的输出：当前内核名，非 native 时补上那行能力提示。
///
/// 无参显示与切换成功共用同一份文案（提示来自 [`kernel_note`]）：两处措辞分家的话，用户会以为
/// 切换没生效。
pub fn kernel_lines(kernel: &str) -> Vec<String> {
    let mut lines = vec![format!("kernel: {kernel}")];
    if let Some(note) = kernel_note(kernel) {
        lines.push(note);
    }
    lines
}

/// `/details` 的回显：当前（或刚切到的）输出模式。
pub fn details_lines(mode: OutputMode) -> Vec<String> {
    vec![format!("output mode: {}", mode_word(mode))]
}

/// 输出模式的三档字面量（与 `ui.output_mode` 的 serde 名一致）。
fn mode_word(mode: OutputMode) -> &'static str {
    match mode {
        OutputMode::Minimal => "minimal",
        OutputMode::Normal => "normal",
        OutputMode::Debug => "debug",
    }
}

/// 三档字面量 → 模式；非法值给 `None`（调用方给人话错误，不做静默回落）。
fn parse_mode(value: &str) -> Option<OutputMode> {
    match value {
        "minimal" => Some(OutputMode::Minimal),
        "normal" => Some(OutputMode::Normal),
        "debug" => Some(OutputMode::Debug),
        _ => None,
    }
}

/// `/model` 的输出：只有 native 才有 Ignis 这边的模型可言。
///
/// `overridden` 为真表示显示的值来自会话级覆盖（追加一行持久化提示）。「刚设置完」的回显由
/// [`dispatch`] 在 `model` 里拼上 `(this session)`——无参显示带覆盖时只追加提示，不带这个后缀。
pub fn model_lines(kernel: &str, model: &str, overridden: bool) -> Result<Vec<String>, String> {
    if kernel != NATIVE_KERNEL {
        return Err(format!(
            "model: not applicable — kernel {kernel} uses its own default model"
        ));
    }
    let mut lines = vec![format!("model: {model}")];
    if overridden {
        lines.push(MODEL_NOTE.to_string());
    }
    Ok(lines)
}

/// banner 与 `/status` 共用的会话行取值：`new` 或 13 字符短 id（与任务短 id 同一 helper——
/// 两处的「哪一段可复制去给 `/resume`」保持同一口径）。
pub fn session_label(current: Option<&str>) -> String {
    match current {
        Some(id) => short_id(id),
        None => "new".to_string(),
    }
}

/// `/status` 的输出：内核、模型、工作区、会话、未读告警、最近一轮用量（顺序固定）。
///
/// 非 native 的内核在 `kernel:` 行后追加能力提示行（与 `/kernel` 同一份文案），模型行给
/// `(kernel default)`——它们用各自 CLI 的默认模型，Ignis 这边的模型名对它们没有意义。
/// 会话行插在 `workspace:` 之后：`new` = 本进程还没落过会话行，否则是 13 字符短 id。
pub fn status_lines(
    kernel: &str,
    model: &str,
    workspace: &Path,
    session: Option<&str>,
    unread: Option<usize>,
    usage: Option<&Usage>,
) -> Vec<String> {
    let mut lines = vec![format!("kernel: {kernel}")];
    if let Some(note) = kernel_note(kernel) {
        lines.push(note);
    }
    lines.push(format!(
        "model: {}",
        if kernel == NATIVE_KERNEL {
            model
        } else {
            "(kernel default)"
        }
    ));
    lines.push(format!("workspace: {}", workspace.display()));
    lines.push(format!("session: {}", session_label(session)));
    lines.push(match unread {
        Some(count) => format!("unread alerts: {count}"),
        None => "unread alerts: unavailable".to_string(),
    });
    lines.push(match usage {
        Some(usage) => format!(
            "last turn usage: in={} out={} tokens ({})",
            usage.input_tokens,
            usage.output_tokens,
            if usage.estimated {
                "estimated"
            } else {
                "exact"
            }
        ),
        None => "last turn usage: -".to_string(),
    });
    lines
}

/// 补全一行输入。
///
/// 两个位置：行首命令名（唯一命中接受整行，有参数的命令补一个空格；多候选列出行），以及
/// `/kernel` 的第一个参数（`native|codex|claude`）。其余位置一律不动输入。
pub fn complete(buffer: &str) -> Completion {
    let text = buffer.trim_start();
    if !text.starts_with('/') {
        return Completion::None;
    }
    let Some((first, rest)) = text.split_once(char::is_whitespace) else {
        return complete_command_name(text);
    };
    // 空白之后还有第二个词：这个位置没有候选
    let argument = rest.trim_start();
    if argument.chars().any(char::is_whitespace) {
        return Completion::None;
    }
    match spec_by_name(first) {
        Some(spec) if spec.command == Slash::Kernel => complete_kernel_value(spec.name, argument),
        // 已知命令但参数面还没有候选（`/memory` 的子命令补全留待后续阶段），未知命令更没有
        _ => Completion::None,
    }
}

/// 行首命令名的补全：唯一命中接受，多个候选按 `/help` 的格式列出。
fn complete_command_name(word: &str) -> Completion {
    if let Some(spec) = spec_by_name(word) {
        // 已经是完整命令名：有参数就补一个空格，其余没什么可补
        return if spec.expects_args {
            Completion::Accepted(format!("{word} "))
        } else {
            Completion::None
        };
    }
    let matches: Vec<&Spec> = COMMANDS
        .iter()
        .filter(|spec| spec.name.starts_with(word))
        .collect();
    match matches.as_slice() {
        [] => Completion::None,
        [only] => Completion::Accepted(spec_accept(only)),
        many => Completion::Listing(group_lines(many)),
    }
}

/// 唯一命中时替换整行的文本：有参数的命令补一个空格（`/ke` → `/kernel `）。
fn spec_accept(spec: &Spec) -> String {
    if spec.expects_args {
        format!("{} ", spec.name)
    } else {
        spec.name.to_string()
    }
}

/// `/kernel` 第一个参数的取值候选：三个内核名（唯一命中接受整条命令）。
fn complete_kernel_value(name: &str, prefix: &str) -> Completion {
    let matches: Vec<&str> = KERNELS
        .iter()
        .copied()
        .filter(|value| value.starts_with(prefix) && *value != prefix)
        .collect();
    match matches.as_slice() {
        [] => Completion::None,
        [only] => Completion::Accepted(format!("{name} {only}")),
        // 取值没有描述列：列的是可直接接受的完整命令行（与 `/help` 的行同型）
        many => Completion::Listing(
            many.iter()
                .map(|value| format!("  {name} {value}"))
                .collect(),
        ),
    }
}

/// 渲染助手给的是「带尾换行的整段文本」：REPL 要的是逐行（`lines` 不吐尾换行后面那个空行）。
fn lines_of(text: &str) -> Vec<String> {
    text.lines().map(str::to_string).collect()
}

/// 既有助手的结果 → REPL 的输出行：`anyhow` 的人话错误取顶层文本（REPL 打 `error: {…}`），
/// 整段文本落成逐行。
fn helper_lines(out: anyhow::Result<String>) -> Result<Vec<String>, String> {
    out.map(|text| lines_of(&text))
        .map_err(|error| error.to_string())
}

/// `/memory …` 的输出行：参数在这里解析，数据面全部落回 `commands::memory` 的既有助手
/// （五条子命令的回执与报错文案只有那一份）。
///
/// 无参等价于 `list`；`search`/`add`/`edit` 的多词参数用一个空格拼回（解析层已按空白切分）；
/// `add` 的 `project` 绑定**当前会话工作区**——REPL 里没有 `--workspace`/配置可解析，会话的
/// 工作区就是用户此刻所在的那一份（`Session::new` 已经校验过它是目录）。
fn memory_lines(session: &Session, args: &[String]) -> Result<Vec<String>, String> {
    let home = session.home();
    let Some((sub, rest)) = args.split_first() else {
        // 无参 = `list`（只看 active）
        return helper_lines(memory::list_at(home, false));
    };
    match sub.as_str() {
        // `--all` 是 `list` 唯一的可选旗标：别的取值按形状不对处理（与未知子命令同一句用法行）
        "list" => match rest {
            [] => helper_lines(memory::list_at(home, false)),
            [all] if all == "--all" => helper_lines(memory::list_at(home, true)),
            _ => Err(MEMORY_USAGE.to_string()),
        },
        // 检索词为空时由助手报「检索词不能为空」（不在 REPL 侧另写一份）
        "search" => helper_lines(memory::search_at(home, &rest.join(" "))),
        "add" => match rest {
            [scope, key, text @ ..] if !text.is_empty() => {
                let scope = memory::parse_scope(scope).map_err(|error| error.to_string())?;
                let workspace = match scope {
                    FactScope::Project => Some(session.workspace()),
                    FactScope::User | FactScope::Global => None,
                };
                helper_lines(memory::add_at(home, scope, workspace, key, &text.join(" ")))
            }
            _ => Err(MEMORY_ADD_USAGE.to_string()),
        },
        "edit" => match rest {
            [id, text @ ..] if !text.is_empty() => {
                let id = memory::parse_id(id).map_err(|error| error.to_string())?;
                helper_lines(memory::edit_at(home, id, &text.join(" ")))
            }
            _ => Err(MEMORY_EDIT_USAGE.to_string()),
        },
        "delete" => match rest {
            [id] => {
                let id = memory::parse_id(id).map_err(|error| error.to_string())?;
                helper_lines(memory::delete_at(home, id))
            }
            // 少一个 id 或多一个词都是同一条用法行（`delete` 只收一个位置）
            _ => Err(MEMORY_DELETE_USAGE.to_string()),
        },
        _ => Err(MEMORY_USAGE.to_string()),
    }
}

/// `/tasks [N]` 的条数：无参给默认值，有参必须是正整数（`0`/负数/非数字/多余词同一条错误）。
fn tasks_limit(args: &[String]) -> Result<usize, String> {
    match args {
        [] => Ok(TASKS_DEFAULT_LIMIT),
        [raw] => match raw.parse::<usize>() {
            Ok(limit) if limit > 0 => Ok(limit),
            _ => Err(TASKS_LIMIT_ERROR.to_string()),
        },
        _ => Err(TASKS_LIMIT_ERROR.to_string()),
    }
}

/// `/tasks` 的输出：最近任务表 + 一行 `ignis task show` 提示。
///
/// 空表只给一句 `(no tasks)`（空表头与提示对没有任何任务的人没有信息量）；有行时表与
/// `ignis task list` 逐行同形（同一个 [`task::render_list`]），提示带上**最近一条**的短 id——
/// 表的第一列就是它，`ignis task show` 接受这种 ≥8 字符的唯一前缀，可以直接复制。
pub fn tasks_lines(rows: &[TaskRecord]) -> Vec<String> {
    let Some(latest) = rows.first() else {
        return vec!["(no tasks)".to_string()];
    };
    let mut lines = lines_of(&task::render_list(rows));
    lines.push(format!("hint: ignis task show {}", short_id(&latest.id)));
    lines
}

/// `/sessions` 的输出：空表给一句 `(no sessions)`；否则 4 列表格（ID / MESSAGES / UPDATED / TITLE）。
///
/// 当前会话的 ID 单元格加 `*` 前缀；ID 是 13 字符短 id（与任务短 id 同一 helper，可直接复制给
/// `/resume`）。`MESSAGES` 是库里 canonical 消息的**原始条数**（含已被压缩覆盖、不在有效历史里的
/// 前缀，Task 2 的口径）；`UPDATED` 用 [`task::format_time`]；`TITLE` 原样（库里已经 ≤40 字符）。
pub fn sessions_lines(rows: &[SessionSummary], current: Option<&str>) -> Vec<String> {
    if rows.is_empty() {
        return vec!["(no sessions)".to_string()];
    }
    let mut table = vec![vec![
        "ID".to_string(),
        "MESSAGES".to_string(),
        "UPDATED".to_string(),
        "TITLE".to_string(),
    ]];
    for row in rows {
        let mark = if current == Some(row.session.id.as_str()) {
            "*"
        } else {
            ""
        };
        table.push(vec![
            format!("{mark}{}", short_id(&row.session.id)),
            row.message_count.to_string(),
            task::format_time(row.session.updated_at),
            row.session.title.clone(),
        ]);
    }
    lines_of(&task::render_table(&table))
}

/// `/resume` 成功的回显行：`resumed session {short13} ({n} messages)`。
///
/// `id` 传**解析出的整条会话 id**（参数可能只是它的一段前缀）：短 id 因此与 `/sessions` 的第一列
/// 逐字一致，用户下一步就认得是哪条。
pub fn resume_lines(id: &str, messages: usize) -> Vec<String> {
    vec![format!(
        "resumed session {} ({messages} messages)",
        short_id(id)
    )]
}

/// `/new` 的回显行（逐字）。
pub fn new_session_line() -> String {
    "new session".to_string()
}

/// 执行一条本地命令：读会话字段 → 调渲染函数 / 调 setter。
///
/// 错误一律人话（`String`）并原样交回 REPL（它打 `error: {…}`）；生产路径无 panic。
pub fn dispatch(
    session: &mut Session,
    command: Slash,
    args: &[String],
) -> Result<Dispatch, String> {
    match command {
        Slash::Exit => Ok(Dispatch::Exit),
        Slash::Help => Ok(Dispatch::Lines(help_lines())),
        Slash::Kernel => match args {
            // 无参显示当前内核；有参先校验可用再切换（错误原样交回，不动内核）
            [] => Ok(Dispatch::Lines(kernel_lines(session.kernel()))),
            [name] => {
                let kernel = session.set_kernel(name)?;
                Ok(Dispatch::Lines(kernel_lines(kernel)))
            }
            _ => Err(KERNEL_USAGE.to_string()),
        },
        Slash::Details => {
            // 有参：整体（含多余的空格与词）必须是合法字面量；非法值报错且不切换
            if !args.is_empty() {
                let raw = args.join(" ");
                let Some(mode) = parse_mode(&raw) else {
                    return Err(DETAILS_ERROR.to_string());
                };
                session.set_output_mode(mode);
            }
            Ok(Dispatch::Lines(details_lines(session.output_mode())))
        }
        Slash::Model => {
            let lines = match args {
                [] => model_lines(
                    session.kernel(),
                    session.model(),
                    session.model_override.is_some(),
                )?,
                [name] => {
                    // 先渲染后落状态：非 native 内核的 not-applicable 必须先于 set_model，
                    // 否则被拒的命令会留下幽灵覆盖（切回 native 后 `/status` 与任务都会用它）。
                    // 设置回显带上 `(this session)`：这次覆盖只影响本会话，持久化要另走 config set
                    let lines =
                        model_lines(session.kernel(), &format!("{name} (this session)"), true)?;
                    session.set_model(name);
                    lines
                }
                _ => return Err(MODEL_USAGE.to_string()),
            };
            Ok(Dispatch::Lines(lines))
        }
        Slash::Status => Ok(Dispatch::Lines(status_lines(
            session.kernel(),
            session.model(),
            session.workspace(),
            session.current_session(),
            session.unread_alerts(),
            session.last_usage(),
        ))),
        // 三个会话命令：渲染与状态变更都落到 `Session` 的方法与纯渲染函数，这里只做接线。
        Slash::New => {
            session.start_new_session();
            Ok(Dispatch::Lines(vec![new_session_line()]))
        }
        Slash::Sessions => Ok(Dispatch::Lines(sessions_lines(
            &session.sessions()?,
            session.current_session(),
        ))),
        Slash::Resume => match args {
            [arg] => {
                let messages = session.resume_session(arg)?;
                // 成功路径刚切过指针：回显用的是解析出的整条 id（参数可能只是一段前缀）
                let id = session.current_session().unwrap_or(arg);
                Ok(Dispatch::Lines(resume_lines(id, messages)))
            }
            // 少于/多于一个位置都是用法错误（不查库、不动状态）
            _ => Err(RESUME_USAGE.to_string()),
        },
        // 三个复用面命令：数据面取会话字段（`/tasks` 的库、`/skills` 的目录）或落回既有命令
        // 助手（`/memory` 的读写与回执），渲染与文案都不在这里另写一份。
        Slash::Memory => Ok(Dispatch::Lines(memory_lines(session, args)?)),
        // 多余参数不解释（与 `/status`、`/help` 同口径）；`reload` 属 P4
        Slash::Skills => Ok(Dispatch::Lines(lines_of(&skills::render(session.skills())))),
        Slash::Tasks => {
            let limit = tasks_limit(args)?;
            let rows = session
                .db()
                .list_tasks(limit)
                .map_err(|error| error.to_string())?;
            Ok(Dispatch::Lines(tasks_lines(&rows)))
        }
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::Path;
    use std::sync::Arc;

    use ignis_core::config::{Config, OutputMode};
    use ignis_core::memory::{FactScope, MemoryStore};
    use ignis_core::skills::SkillCatalog;
    use ignis_core::task::TaskKind;
    use ignis_runtime::test_support::{ScriptStep, ScriptedModel, ScriptedModelHandle};
    use ignis_runtime::{ChatMessage, Role, Usage};
    use ignis_storage::{
        Db, NewSession, NewSessionMessage, NewTask, SessionRecord, SessionSummary,
    };

    use super::*;
    use crate::commands::fixtures::{session_fixture, session_fixture_with_config};
    use crate::commands::skills;

    /// 分发一条命令并取出它的输出行；退出类命令不该走这里。
    fn dispatch_lines(session: &mut Session, command: Slash, args: &[String]) -> Vec<String> {
        match dispatch(session, command, args).expect("命令应成功") {
            Dispatch::Lines(lines) => lines,
            Dispatch::Exit => panic!("{command:?} 不该退出"),
        }
    }

    /// 取出 `complete` 的候选行；别的形态直接失败（并把实际值打出来）。
    fn listing(buffer: &str) -> Vec<String> {
        match complete(buffer) {
            Completion::Listing(lines) => lines,
            other => panic!("{buffer:?} 应该有候选：{other:?}"),
        }
    }

    /// 一条命令行 → [`dispatch`] 的实参（`parse` 做的也是按空白切分）。
    fn args(words: &[&str]) -> Vec<String> {
        words.iter().map(|word| word.to_string()).collect()
    }

    /// 一行拆成单元格：表格用两个空格分隔，列宽不足处补空格（列宽由最宽单元格决定）。
    fn cells(line: &str) -> Vec<&str> {
        line.split("  ")
            .map(str::trim)
            .filter(|cell| !cell.is_empty())
            .collect()
    }

    /// `2026-10-15T12:00:00Z` 的 epoch 毫秒（与 `task`/`memory` 侧测试同源）。
    const STARTED_AT: i64 = 1_792_065_600_000;

    /// 往临时 home 的库里插一条任务（`/tasks` 读的就是这份库）。
    fn insert_task(home: &Path, id: &str, started_at: i64) {
        Db::open(&home.join("ignis.db"))
            .unwrap()
            .insert_task(&NewTask {
                id: id.to_string(),
                kind: "run".into(),
                prompt: "修复配置路径".into(),
                kernel: "native".into(),
                workspace: "/tmp/project".into(),
                started_at,
                session_id: None,
            })
            .unwrap();
    }

    /// 会话行夹具：`/sessions` 的表格只读会话记录与消息条数。
    fn summary(id: &str, title: &str, updated_at: i64, message_count: usize) -> SessionSummary {
        SessionSummary {
            session: SessionRecord {
                id: id.to_string(),
                title: title.to_string(),
                workspace: "/tmp/ws".into(),
                kernel: "native".into(),
                model: None,
                created_at: updated_at,
                updated_at,
            },
            message_count,
        }
    }

    /// 往临时 home 的库里插一条会话行（`/sessions` 与 `/resume` 读的就是这份库）。
    fn insert_session(session: &Session, id: &str, title: &str, at: i64) {
        session
            .db()
            .insert_session(&NewSession {
                id: id.to_string(),
                title: title.to_string(),
                workspace: session.workspace().display().to_string(),
                kernel: "native".into(),
                model: None,
                created_at: at,
            })
            .unwrap();
    }

    /// 追加一轮 canonical 消息（user + assistant），并把 `updated_at` 刷成 `at`。
    fn append_turn(session: &Session, id: &str, prompt: &str, reply: &str, at: i64) {
        session
            .db()
            .append_session_messages(
                id,
                &[
                    NewSessionMessage {
                        role: "user".into(),
                        content: prompt.into(),
                    },
                    NewSessionMessage {
                        role: "assistant".into(),
                        content: reply.into(),
                    },
                ],
                at,
            )
            .unwrap();
    }

    /// 指向脚本化假模型的 native 配置（`/new` 的用例要先真的跑一轮把会话落库）。
    fn scripted_config(model: &ScriptedModelHandle) -> Config {
        Config {
            api_key: Some("test-key".into()),
            model: ignis_runtime::ModelConfig {
                base_url: model.base_url(),
                model: "test-model".into(),
                ..ignis_runtime::ModelConfig::default()
            },
            ..Config::default()
        }
    }

    /// 解析表：空行、认识命令、未知命令与任务各一条路。
    #[test]
    fn parse_table_covers_commands_tasks_and_unknown() {
        // 空行/纯空白（编辑器可能提交空串）：跳过
        assert_eq!(parse(""), Parsed::Empty);
        assert_eq!(parse("\n"), Parsed::Empty);
        assert_eq!(parse("   \n"), Parsed::Empty);

        // 认识命令：参数按空白切分（多余空白归一）
        assert_eq!(parse("/exit\n"), Parsed::Command(Slash::Exit, Vec::new()));
        assert_eq!(parse("/help"), Parsed::Command(Slash::Help, Vec::new()));
        assert_eq!(parse("/kernel"), Parsed::Command(Slash::Kernel, Vec::new()));
        assert_eq!(
            parse("  /kernel   codex  \n"),
            Parsed::Command(Slash::Kernel, vec!["codex".to_string()])
        );
        // 多出的词进 args（v0.1 里这是「一条任务」；现在由 dispatch 给人话错误）
        assert_eq!(
            parse("/kernel codex now\n"),
            Parsed::Command(Slash::Kernel, vec!["codex".to_string(), "now".to_string()])
        );

        // 逐字小写匹配：`/Help` 是未知命令（首词去掉斜杠后原样交回）
        assert_eq!(parse("/unknown\n"), Parsed::Unknown("unknown".to_string()));
        assert_eq!(parse("/Help\n"), Parsed::Unknown("Help".to_string()));

        // 其它输入是任务：只裁首尾空白，内部换行保留
        assert_eq!(
            parse("把 README 翻译成中文\n"),
            Parsed::Task("把 README 翻译成中文".to_string())
        );
        assert_eq!(
            parse("  第一行\n第二行  \n"),
            Parsed::Task("第一行\n第二行".to_string())
        );
    }

    /// `/help` 的形状：组名独占一行、每个条目恰好一行、描述逐字。
    #[test]
    fn help_lines_lists_every_command_in_its_group() {
        let lines = help_lines();

        // 组名独占一行、按固定顺序（`SESSION` 组在本任务落地，排在 `AGENT` 之前）
        let groups: Vec<&str> = lines
            .iter()
            .filter(|line| !line.starts_with("  "))
            .map(String::as_str)
            .collect();
        assert_eq!(
            groups,
            ["SESSION", "AGENT", "INTELLIGENCE", "INTERFACE"],
            "{lines:?}"
        );

        // 命令表里的每个条目在帮助里恰好出现一次（含参数提示、以描述收尾）
        for spec in &COMMANDS {
            let label = spec_label(spec);
            let hits: Vec<&String> = lines
                .iter()
                .filter(|line| line.starts_with(&format!("  {label}")))
                .collect();
            assert_eq!(hits.len(), 1, "{} 应恰好出现一次：{lines:?}", spec.name);
            assert!(hits[0].ends_with(spec.summary), "{:?}", hits[0]);
        }

        // 分组对齐逐字钉住：AGENT 组最宽是 `/model [name]`（13 列），列间两个空格
        let agent = lines
            .iter()
            .position(|line| line == "AGENT")
            .expect("AGENT 组标题");
        assert_eq!(
            &lines[agent + 1..agent + 5],
            [
                "  /model [name]  Show or set the model of this session",
                "  /status        Show kernel, model, workspace, alerts and last usage",
                "  /tasks [N]     List the most recent tasks (default 10)",
                "  /skills        List the skill catalog",
            ]
        );
        // 单条目的组：列宽就是它自己（参数提示与描述之间仍是两个空格）
        let intelligence = lines
            .iter()
            .position(|line| line == "INTELLIGENCE")
            .expect("INTELLIGENCE 组标题");
        assert_eq!(
            lines[intelligence + 1],
            "  /memory <list|search|add|edit|delete>  Read or edit long-term memory"
        );
    }

    /// `/help` 的 SESSION 组：组序在最前、三条新条目逐字、列宽由最宽的 `/resume <id>` 决定。
    #[test]
    fn help_lines_list_the_new_commands_in_their_groups() {
        let lines = help_lines();

        // 分组顺序固定：`SESSION` 排在最前
        let groups: Vec<&str> = lines
            .iter()
            .filter(|line| !line.starts_with("  "))
            .map(String::as_str)
            .collect();
        assert_eq!(
            groups,
            ["SESSION", "AGENT", "INTELLIGENCE", "INTERFACE"],
            "{lines:?}"
        );

        // 三条新条目的「名 + 参数提示」与描述逐字；`/resume <id>` 的 11 列参与组内对齐
        let session = lines
            .iter()
            .position(|line| line == "SESSION")
            .expect("SESSION 组标题");
        assert_eq!(
            &lines[session + 1..session + 4],
            [
                "  /new          Start a new session",
                "  /sessions     List recent sessions in this workspace",
                "  /resume <id>  Resume a session from this workspace",
            ]
        );
        // SESSION 组之后紧接 AGENT：组里没有多余的行
        assert_eq!(lines[session + 4], "AGENT", "{lines:?}");

        // 每个新条目在帮助里恰好出现一次（与既有用例同一口径）
        for spec in COMMANDS.iter().filter(|spec| spec.group == Group::Session) {
            let label = spec_label(spec);
            let hits: Vec<&String> = lines
                .iter()
                .filter(|line| line.starts_with(&format!("  {label}")))
                .collect();
            assert_eq!(hits.len(), 1, "{} 应恰好出现一次：{lines:?}", spec.name);
            assert!(hits[0].ends_with(spec.summary), "{:?}", hits[0]);
        }
    }

    /// 帮助里绝不出现后续阶段（P4）的命令——P3 的会话命令如今是合法条目。
    #[test]
    fn help_lines_never_mentions_later_phases() {
        let help = help_lines().join("\n");
        for later in ["/learn", "/reload"] {
            assert!(
                !help.contains(later),
                "{later} 不该出现在 P3 的帮助里：{help}"
            );
        }
    }

    /// `/sessions` 的表：空表一句、表头逐字、当前会话的 ID 格带 `*`、列宽对齐且行尾不留空格。
    #[test]
    fn sessions_lines_matrix() {
        // 空库：不画空表头
        assert_eq!(sessions_lines(&[], None), ["(no sessions)"]);

        let newer = "019a1b2c-2222-7f00-8000-000000000002";
        let older = "019a1b2c-1111-7f00-8000-000000000001";
        let rows = [
            summary(newer, "修复配置路径", STARTED_AT + 1_000, 4),
            summary(older, "你好", STARTED_AT, 2),
        ];

        let lines = sessions_lines(&rows, Some(older));
        assert_eq!(lines.len(), 3, "{lines:?}");
        assert_eq!(cells(&lines[0]), ["ID", "MESSAGES", "UPDATED", "TITLE"]);
        // 新近在前；`*` 只在当前会话（第二条）的 ID 格里
        assert_eq!(cells(&lines[1])[0], &newer[..13]);
        assert_eq!(cells(&lines[1])[1], "4");
        assert_eq!(cells(&lines[1])[2], "2026-10-15T12:00:01Z");
        assert_eq!(cells(&lines[1])[3], "修复配置路径");
        assert_eq!(cells(&lines[2])[0], format!("*{}", &older[..13]));
        assert_eq!(cells(&lines[2])[1], "2");
        assert_eq!(cells(&lines[2])[2], "2026-10-15T12:00:00Z");
        assert_eq!(cells(&lines[2])[3], "你好");
        // UPDATED 是 `task::format_time` 的口径（`YYYY-MM-DDTHH:MM:SSZ`）
        assert_eq!(cells(&lines[2])[2].len(), 20, "{lines:?}");
        // 行尾不留空格（`render_table` 的既有口径）
        for line in &lines {
            assert_eq!(line.trim_end(), line, "{line:?}");
        }

        // 没有当前会话：谁都不带 `*`
        let plain = sessions_lines(&rows, None);
        assert!(!plain.iter().any(|line| line.contains('*')), "{plain:?}");
    }

    /// `/resume <前缀>` 的真值表：唯一命中装载、撞车报候选（新近在前、最多 `CANDIDATES` 条）、
    /// 过短/非法/空串/空库都是同一句 `session not found`。
    #[test]
    fn resume_prefix_matrix() {
        let (_dir, mut session) = session_fixture("slash-resume-prefix", "native");
        let first = "019a1b2c-1aaa-7f00-8000-000000000001";
        let second = "019a1b2c-1bbb-7f00-8000-000000000002";

        // 空库：合法前缀也只是一句 not found
        assert_eq!(
            session.resume_session("019a1b2c").unwrap_err(),
            "session not found: 019a1b2c"
        );

        // 有且只有一条：短前缀唯一命中，历史装载、指针切过去、返回条数
        insert_session(&session, first, "第一段", STARTED_AT);
        append_turn(&session, first, "你好", "答复一", STARTED_AT + 1);
        assert_eq!(session.resume_session("019a1b2c").unwrap(), 2);
        assert_eq!(session.current_session(), Some(first));
        assert_eq!(session.history().len(), 2);
        assert_eq!(session.history()[0].content, "你好");

        // 再来一条同前缀的：`019a1b2c-1` 命中两条 → 歧义错误列出两个短 id（新近在前）
        insert_session(&session, second, "第二段", STARTED_AT + 2_000);
        append_turn(&session, second, "再问", "答复二", STARTED_AT + 2_001);
        let error = session.resume_session("019a1b2c-1").unwrap_err();
        assert_eq!(
            error,
            format!(
                "ambiguous session id: 019a1b2c-1（候选：{}, {}）",
                &second[..13],
                &first[..13]
            )
        );
        assert_eq!(session.current_session(), Some(first), "歧义不该改指针");

        // 更长的前缀唯一命中：切到第二条，历史换成它的
        assert_eq!(session.resume_session("019a1b2c-1bbb").unwrap(), 2);
        assert_eq!(session.current_session(), Some(second));
        assert_eq!(session.history()[0].content, "再问");

        // 过短 / 非法字符 / 空串：同一句 not found（准入闸门不过就不查库），指针不动
        for bad in ["short", "ZZZZ", ""] {
            assert_eq!(
                session.resume_session(bad).unwrap_err(),
                format!("session not found: {bad}"),
                "{bad:?}"
            );
        }
        assert_eq!(
            session.current_session(),
            Some(second),
            "失败的恢复不该动指针"
        );

        // 候选上限：12 条同前缀的会话只列出最新的 `CANDIDATES` 条（新近在前）
        let (_dir, mut many) = session_fixture("slash-resume-candidates", "native");
        for index in 0..12 {
            insert_session(
                &many,
                &format!("019a1b2c-{index:04x}-7f00-8000-000000000000"),
                "批量",
                STARTED_AT + index as i64,
            );
        }
        let error = many.resume_session("019a1b2c").unwrap_err();
        let candidates: Vec<&str> = error
            .strip_prefix("ambiguous session id: 019a1b2c（候选：")
            .and_then(|rest| rest.strip_suffix('）'))
            .unwrap_or_else(|| panic!("{error}"))
            .split(", ")
            .collect();
        assert_eq!(candidates.len(), task::CANDIDATES, "{error}");
        assert_eq!(candidates[0], "019a1b2c-000b", "{error}");
        assert!(!error.contains("019a1b2c-0000"), "{error}");
        assert!(!error.contains("019a1b2c-0001"), "{error}");
    }

    /// `/sessions` 经 dispatch：空库一句、有库按表格、当前会话带 `*`、条数上限 10 条。
    #[test]
    fn dispatch_sessions_lists_the_workspace() {
        let (_dir, mut session) = session_fixture("slash-sessions-dispatch", "native");
        assert_eq!(
            dispatch_lines(&mut session, Slash::Sessions, &[]),
            ["(no sessions)"]
        );

        let first = "019a1b2c-1111-7f00-8000-000000000001";
        let second = "019a1b2c-2222-7f00-8000-000000000002";
        insert_session(&session, first, "第一段", STARTED_AT);
        insert_session(&session, second, "第二段", STARTED_AT + 1_000);
        append_turn(&session, second, "第二句", "答复二", STARTED_AT + 1_001);
        // 先把当前会话切到第二条：表里它带 `*`，MESSAGES 是库里的原始条数
        assert_eq!(session.resume_session("019a1b2c-2222").unwrap(), 2);

        let lines = dispatch_lines(&mut session, Slash::Sessions, &[]);
        assert_eq!(lines.len(), 3, "{lines:?}");
        assert_eq!(cells(&lines[0]), ["ID", "MESSAGES", "UPDATED", "TITLE"]);
        assert_eq!(cells(&lines[1])[0], format!("*{}", &second[..13]));
        assert_eq!(cells(&lines[1])[1], "2");
        assert_eq!(cells(&lines[1])[3], "第二段");
        assert_eq!(cells(&lines[2])[0], &first[..13]);
        assert_eq!(cells(&lines[2])[1], "0");

        // 条数上限 10：再插 10 条更旧的，表里仍只有 10 行数据（最旧的两条被截掉）
        for index in 0..10 {
            insert_session(
                &session,
                &format!("019a1b2c-3{index:03x}-7f00-8000-000000000000"),
                "更旧",
                STARTED_AT - 10_000 - index as i64,
            );
        }
        let lines = dispatch_lines(&mut session, Slash::Sessions, &[]);
        assert_eq!(lines.len(), 11, "{lines:?}");
        assert_eq!(cells(&lines[1])[0], format!("*{}", &second[..13]));
        for oldest in ["019a1b2c-3008", "019a1b2c-3009"] {
            assert!(
                !lines.iter().any(|line| line.contains(oldest)),
                "第 11、12 新的两条不该出现：{lines:?}"
            );
        }
    }

    /// `/resume` 经 dispatch：装载有效历史、换指针、回显解析出的短 id（参数是前缀也一样）。
    #[test]
    fn dispatch_resume_loads_the_history_and_switches_the_pointer() {
        let (_dir, mut session) = session_fixture("slash-resume-dispatch", "native");
        let first = "019a1b2c-1111-7f00-8000-000000000001";
        let second = "019a1b2c-2222-7f00-8000-000000000002";
        insert_session(&session, first, "第一段", STARTED_AT);
        insert_session(&session, second, "第二段", STARTED_AT + 1_000);
        append_turn(&session, first, "你好", "答复一", STARTED_AT + 1);
        append_turn(&session, second, "再问", "答复二", STARTED_AT + 1_001);

        let lines = dispatch_lines(&mut session, Slash::Resume, &args(&["019a1b2c-1111"]));
        assert_eq!(lines, ["resumed session 019a1b2c-1111 (2 messages)"]);
        assert_eq!(session.current_session(), Some(first));
        let expected = vec![
            ChatMessage {
                role: Role::User,
                content: "你好".into(),
                tool_call_id: None,
                tool_calls: Vec::new(),
            },
            ChatMessage {
                role: Role::Assistant,
                content: "答复一".into(),
                tool_call_id: None,
                tool_calls: Vec::new(),
            },
        ];
        assert_eq!(session.history(), expected.as_slice(), "有效历史逐条相等");

        // 换一条：历史与指针一起换过去
        let lines = dispatch_lines(&mut session, Slash::Resume, &args(&["019a1b2c-2222"]));
        assert_eq!(lines, ["resumed session 019a1b2c-2222 (2 messages)"]);
        assert_eq!(session.current_session(), Some(second));
        assert_eq!(session.history()[0].content, "再问");
        assert_eq!(session.history()[1].content, "答复二");

        // 失败原样交回（REPL 打 `error: {…}`），且不改状态
        for (words, expected) in [
            (
                args(&["019a1b2c"]),
                "ambiguous session id: 019a1b2c（候选：019a1b2c-2222, 019a1b2c-1111）".to_string(),
            ),
            (args(&["short"]), "session not found: short".to_string()),
        ] {
            let error = dispatch(&mut session, Slash::Resume, &words).unwrap_err();
            assert_eq!(error, expected, "{words:?}");
            assert_eq!(
                session.current_session(),
                Some(second),
                "失败的恢复不该动指针"
            );
        }

        // 缺参 / 多参：用法错误（不查库、不动状态）
        for words in [args(&[]), args(&["a", "b"])] {
            let error = dispatch(&mut session, Slash::Resume, &words).unwrap_err();
            assert_eq!(error, "resume: usage: /resume <id>", "{words:?}");
            assert_eq!(session.current_session(), Some(second));
        }
    }

    /// `/new` 经 dispatch：清指针/历史/最近用量，旧会话一行不删；三个会话命令都不碰模型。
    #[tokio::test]
    async fn dispatch_new_clears_the_session_state() {
        let model = ScriptedModel::start(vec![ScriptStep::Text("答复一".into())]);
        let (_dir, mut session) =
            session_fixture_with_config("slash-new-dispatch", "native", scripted_config(&model));
        session
            .run_task("你好".into(), TaskKind::Repl)
            .await
            .expect("脚本化模型应答应成功");
        let old = session
            .current_session()
            .expect("首个 REPL 任务应建会话")
            .to_string();
        assert!(!session.history().is_empty());
        assert!(session.last_usage().is_some());
        assert_eq!(model.requests().len(), 1, "那一轮真的碰过模型");

        let lines = dispatch_lines(&mut session, Slash::New, &[]);
        assert_eq!(lines, ["new session"]);
        assert_eq!(session.current_session(), None, "指针回到「还没绑会话」");
        assert!(session.history().is_empty());
        assert_eq!(session.last_usage(), None);

        // 旧会话一行不动（不删数据），只是不再带 `*`
        let listed = dispatch_lines(&mut session, Slash::Sessions, &[]);
        assert_eq!(listed.len(), 2, "{listed:?}");
        assert_eq!(cells(&listed[1])[0], &old[..13], "旧会话还在，且没有 `*`");
        assert_eq!(cells(&listed[1])[1], "2");

        // `/resume` 能回到它；`/new` / `/sessions` / `/resume` 都没有再碰模型
        let resumed = dispatch_lines(&mut session, Slash::Resume, &args(&[&old[..8]]));
        assert_eq!(
            resumed,
            [format!("resumed session {} (2 messages)", &old[..13])]
        );
        assert_eq!(session.current_session(), Some(old.as_str()));
        assert_eq!(model.requests().len(), 1, "会话命令不碰模型");
    }

    /// 未知命令的报错行逐字钉住（REPL 打到 stderr，不送模型）。
    #[test]
    fn unknown_command_line_is_pinned() {
        assert_eq!(
            unknown_command_line("nope"),
            "Unknown command: /nope — /help lists commands"
        );
        // 名字原样（大小写不归一）：`/Help` 报的就是 `/Help`
        assert_eq!(
            unknown_command_line("Help"),
            "Unknown command: /Help — /help lists commands"
        );
    }

    /// `/details` 的回显与字面量反解：三档各一条，非法值给 `None`。
    #[test]
    fn details_lines_and_mode_lookup() {
        assert_eq!(details_lines(OutputMode::Minimal), ["output mode: minimal"]);
        assert_eq!(details_lines(OutputMode::Normal), ["output mode: normal"]);
        assert_eq!(details_lines(OutputMode::Debug), ["output mode: debug"]);

        for (name, mode) in [
            ("minimal", OutputMode::Minimal),
            ("normal", OutputMode::Normal),
            ("debug", OutputMode::Debug),
        ] {
            assert_eq!(parse_mode(name), Some(mode), "{name}");
        }
        for bad in ["", "verbose", "Normal", " debug", "normal "] {
            assert_eq!(parse_mode(bad), None, "{bad:?}");
        }
    }

    /// `/model` 只对 native 有意义：两个外部内核两种形态都是同一句 not-applicable。
    #[test]
    fn model_lines_are_native_only() {
        // native 无覆盖：单行
        assert_eq!(
            model_lines("native", "test-model", false).unwrap(),
            ["model: test-model"]
        );
        // native 有覆盖：追加持久化提示（同一份 note）
        assert_eq!(
            model_lines("native", "test-model-2", true).unwrap(),
            [
                "model: test-model-2",
                "note: session override; persist with `ignis config set model.model`",
            ]
        );
        // 有参切换的回显由调用方拼上 `(this session)`（无参显示不带这个后缀）
        assert_eq!(
            model_lines("native", "test-model-2 (this session)", true).unwrap()[0],
            "model: test-model-2 (this session)"
        );

        // 非 native：两种形态（有/无覆盖）都是同一句人话错误
        for kernel in ["codex", "claude"] {
            for overridden in [false, true] {
                let error = model_lines(kernel, "test-model", overridden).unwrap_err();
                assert_eq!(
                    error,
                    format!("model: not applicable — kernel {kernel} uses its own default model")
                );
            }
        }
    }

    /// `/status` 的真值表：六行固定顺序（会话行插在 workspace 之后），非 native 追加能力提示行。
    #[test]
    fn status_lines_matrix() {
        let path = Path::new("/tmp/ws");
        let usage = Usage {
            input_tokens: 1234,
            output_tokens: 567,
            estimated: false,
        };
        let estimated = Usage {
            input_tokens: 12,
            output_tokens: 3,
            estimated: true,
        };
        let session_id = "019a1b2c-3d4e-7f00-8000-000000000001";

        assert_eq!(
            status_lines("native", "test-model", path, None, Some(2), Some(&usage)),
            [
                "kernel: native",
                "model: test-model",
                "workspace: /tmp/ws",
                "session: new",
                "unread alerts: 2",
                "last turn usage: in=1234 out=567 tokens (exact)",
            ]
        );

        // 告警计数读失败 / 本会话还没跑过任务
        assert_eq!(
            status_lines("native", "test-model", path, None, None, None),
            [
                "kernel: native",
                "model: test-model",
                "workspace: /tmp/ws",
                "session: new",
                "unread alerts: unavailable",
                "last turn usage: -",
            ]
        );

        // 会话命中：13 字符短 id（与任务短 id 同一 helper），就插在 workspace 之后
        let with_session = status_lines("native", "test-model", path, Some(session_id), None, None);
        assert_eq!(with_session[2], "workspace: /tmp/ws");
        assert_eq!(with_session[3], "session: 019a1b2c-3d4e");

        // 估算用量按 `(estimated)` 标出；0 条未读照实显示
        let lines = status_lines(
            "native",
            "test-model",
            path,
            Some(session_id),
            Some(0),
            Some(&estimated),
        );
        assert_eq!(lines.len(), 6, "{lines:?}");
        assert_eq!(lines[3], "session: 019a1b2c-3d4e");
        assert_eq!(lines[4], "unread alerts: 0");
        assert_eq!(lines[5], "last turn usage: in=12 out=3 tokens (estimated)");

        // 非 native：kernel 行后是能力提示行，模型行给 (kernel default)
        let codex = status_lines("codex", "test-model", path, None, Some(1), None);
        assert_eq!(codex.len(), 7, "{codex:?}");
        assert_eq!(codex[0], "kernel: codex");
        assert_eq!(codex[1], kernel_note("codex").unwrap());
        assert_eq!(codex[2], "model: (kernel default)");
        assert_eq!(codex[3], "workspace: /tmp/ws");
        assert_eq!(codex[4], "session: new");
        assert_eq!(codex[5], "unread alerts: 1");
        assert_eq!(codex[6], "last turn usage: -");
    }

    /// banner 与 `/status` 共用的会话行取值：没有会话是 `new`，有就是 13 字符短 id。
    #[test]
    fn session_label_pins_new_and_the_thirteen_char_short_id() {
        assert_eq!(session_label(None), "new");
        assert_eq!(
            session_label(Some("019a1b2c-3d4e-7f00-8000-000000000001")),
            "019a1b2c-3d4e"
        );
        // 短于 13 字符的 id 原样返回（与 `short_id` 同一口径）
        assert_eq!(session_label(Some("short")), "short");
    }

    /// Tab 补全：唯一前缀接受、歧义列出、`/kernel` 的取值候选、其它位置不动。
    #[test]
    fn complete_accepts_unique_prefixes_and_lists_ambiguities() {
        // 唯一命中：需要参数的命令补一个空格，不需要的原样
        assert_eq!(complete("/he"), Completion::Accepted("/help".to_string()));
        assert_eq!(
            complete("/ke"),
            Completion::Accepted("/kernel ".to_string())
        );
        // 已经完整：要参数就补空格，不要参数就没什么可补
        assert_eq!(complete("/help"), Completion::None);
        assert_eq!(
            complete("/kernel"),
            Completion::Accepted("/kernel ".to_string())
        );

        // 有歧义：与 `/help` 同型（组名独占一行、组内对齐）
        let ambiguous = listing("/m");
        assert_eq!(
            ambiguous,
            [
                "AGENT",
                "  /model [name]  Show or set the model of this session",
                "INTELLIGENCE",
                "  /memory <list|search|add|edit|delete>  Read or edit long-term memory",
            ]
        );
        // 前缀 `/` 命中全部九条：与 `/help` 逐行相等
        assert_eq!(listing("/"), help_lines());

        // `/kernel` 的第一个参数位置：取值候选（唯一命中接受整条命令）
        assert_eq!(
            complete("/kernel n"),
            Completion::Accepted("/kernel native".to_string())
        );
        assert_eq!(
            listing("/kernel "),
            ["  /kernel native", "  /kernel codex", "  /kernel claude",]
        );
        assert_eq!(
            listing("/kernel c"),
            ["  /kernel codex", "  /kernel claude"]
        );

        // 空输入、非命令输入、未知命令、第二个参数之后、已经完整的取值：一律不动
        for buffer in [
            "",
            "你好",
            "/nope",
            "/kernel native",
            "/kernel codex now",
            "/status x",
            "/he x",
        ] {
            assert_eq!(complete(buffer), Completion::None, "{buffer:?}");
        }
    }

    /// `/details`：有参切换会话模式、无参回显当前、非法值报错且不切换。
    #[test]
    fn dispatch_details_switches_the_session_mode() {
        let (_dir, mut session) = session_fixture("slash-details", "native");
        assert_eq!(session.output_mode(), OutputMode::Normal, "默认跟随配置");

        let lines = dispatch_lines(&mut session, Slash::Details, &["debug".to_string()]);
        assert_eq!(lines, ["output mode: debug"]);
        assert_eq!(session.output_mode(), OutputMode::Debug);

        // 无参：回显当前（刚切过的值）
        assert_eq!(
            dispatch_lines(&mut session, Slash::Details, &[]),
            ["output mode: debug"]
        );

        // 非法值（含多余词）：报错、模式不动
        for bad in [
            vec!["verbose".to_string()],
            vec!["minimal normal".to_string()],
        ] {
            let error = dispatch(&mut session, Slash::Details, &bad).unwrap_err();
            assert_eq!(error, "details: mode must be minimal, normal or debug");
            assert_eq!(session.output_mode(), OutputMode::Debug, "非法值不该切模式");
        }
    }

    /// `/model`：原生内核设会话级覆盖并回显；两个外部内核报 not-applicable。
    #[test]
    fn dispatch_model_sets_a_session_override() {
        let (_dir, mut session) = session_fixture("slash-model", "native");
        let configured = session.model().to_string();

        let lines = dispatch_lines(&mut session, Slash::Model, &["test-model-2".to_string()]);
        assert_eq!(lines[0], "model: test-model-2 (this session)");
        assert_eq!(
            lines[1],
            "note: session override; persist with `ignis config set model.model`"
        );
        assert_eq!(session.model(), "test-model-2");
        assert_ne!(session.model(), configured, "覆盖值优先于配置");

        // 无参：显示生效值 + 同一份 note（不带 `(this session)` 后缀）
        assert_eq!(
            dispatch_lines(&mut session, Slash::Model, &[]),
            [
                "model: test-model-2",
                "note: session override; persist with `ignis config set model.model`",
            ]
        );

        // 外部内核：不适用（**不静默接受一个不生效的覆盖**）
        let (_dir, mut codex) = session_fixture("slash-model-codex", "codex");
        let kernel_default = codex.model().to_string();

        // 有参：先校验（渲染）后落状态——错误路径必须把会话留在原样
        let error = dispatch(&mut codex, Slash::Model, &["gpt-5".to_string()]).unwrap_err();
        assert_eq!(
            error,
            "model: not applicable — kernel codex uses its own default model"
        );
        assert_eq!(codex.model(), kernel_default, "被拒的覆盖不该落进会话");

        // 无参：同一句错误
        let error = dispatch(&mut codex, Slash::Model, &[]).unwrap_err();
        assert_eq!(
            error,
            "model: not applicable — kernel codex uses its own default model"
        );
        assert_eq!(codex.model(), kernel_default);

        // 幽灵覆盖最迟在切回 native 时暴露：`/status` 与后续任务都拿 `model()` 当生效值
        dispatch_lines(&mut codex, Slash::Kernel, &["native".to_string()]);
        assert!(codex.is_native());
        assert_eq!(
            codex.model(),
            kernel_default,
            "切回 native 后仍是配置里的模型，不是被拒的名字"
        );
        assert_eq!(
            dispatch_lines(&mut codex, Slash::Model, &[]),
            [format!("model: {kernel_default}")],
            "显示不带 note，说明会话里没有覆盖"
        );
    }

    /// `/model` 的参数个数先于内核判定：`/model a b` 在任何内核上都是 usage，且不改状态。
    #[test]
    fn dispatch_model_rejects_extra_arguments_on_every_kernel() {
        for kernel in ["native", "codex", "claude"] {
            let (_dir, mut session) = session_fixture("slash-model-usage", kernel);
            let before = session.model().to_string();

            let error = dispatch(
                &mut session,
                Slash::Model,
                &["a".to_string(), "b".to_string()],
            )
            .unwrap_err();

            assert_eq!(error, "model: usage: /model [name]", "{kernel}");
            assert_eq!(session.model(), before, "{kernel}: usage 错误不该改状态");
        }
    }

    /// `/kernel`：无参显示、有参切换（失败原样交回、内核不动）、多参给 usage。
    #[test]
    fn dispatch_kernel_keeps_the_current_kernel_on_a_bad_name() {
        let (_dir, mut session) = session_fixture("slash-kernel", "codex");

        // 无参：当前内核 + 能力提示行
        assert_eq!(
            dispatch_lines(&mut session, Slash::Kernel, &[]),
            kernel_lines("codex")
        );
        assert!(!session.is_native());

        // 有参且可用：切换（夹具里有 Ignis 凭据，native 可切）
        assert_eq!(
            dispatch_lines(&mut session, Slash::Kernel, &["native".to_string()]),
            ["kernel: native"]
        );
        assert_eq!(session.kernel(), "native");
        assert!(session.is_native());

        // 未知名字：错误原样交回，内核不动
        let error = dispatch(&mut session, Slash::Kernel, &["gemini".to_string()]).unwrap_err();
        assert!(error.contains("未知内核 'gemini'"), "{error}");
        assert_eq!(session.kernel(), "native");

        // 多于一个参数：usage（也不动内核）
        let error = dispatch(
            &mut session,
            Slash::Kernel,
            &["codex".to_string(), "now".to_string()],
        )
        .unwrap_err();
        assert_eq!(error, "kernel: usage: /kernel [native|codex|claude]");
        assert_eq!(session.kernel(), "native");
    }

    /// `/help` 与 `/exit`：本地动作，不碰模型、不改会话状态。
    #[test]
    fn dispatch_help_and_exit_are_local() {
        let (_dir, mut session) = session_fixture("slash-help-exit", "native");

        assert_eq!(
            dispatch(&mut session, Slash::Help, &[]).unwrap(),
            Dispatch::Lines(help_lines())
        );
        assert_eq!(
            dispatch(&mut session, Slash::Exit, &[]).unwrap(),
            Dispatch::Exit
        );
        assert_eq!(session.kernel(), "native");
        assert_eq!(session.output_mode(), OutputMode::Normal);
    }

    /// `/memory` 无参 = `list`：与 `ignis memory list` 同一张表（同一个 `render_facts`）。
    #[test]
    fn memory_list_shows_the_active_facts_table() {
        let (_dir, mut session) = session_fixture("slash-memory-list", "native");

        // 空库：只剩表头（列名逐字）
        assert_eq!(
            dispatch_lines(&mut session, Slash::Memory, &[]),
            ["ID  SCOPE  KEY  TEXT  UPDATED"]
        );

        // 写入一条：无参与 `list` 是同一条路（表头 + 一行一条 active 事实）
        let added = dispatch_lines(
            &mut session,
            Slash::Memory,
            &args(&["add", "user", "pkg_manager", "使用 uv 管理依赖"]),
        );
        assert_eq!(added, ["已记住（user/pkg_manager）：使用 uv 管理依赖"]);

        for command in [args(&[]), args(&["list"])] {
            let lines = dispatch_lines(&mut session, Slash::Memory, &command);
            assert_eq!(lines.len(), 2, "{command:?}: {lines:?}");
            assert_eq!(
                cells(&lines[0]),
                ["ID", "SCOPE", "KEY", "TEXT", "UPDATED"],
                "{command:?}: {lines:?}"
            );
            let row = cells(&lines[1]);
            assert_eq!(row[0], "1", "{command:?}: {lines:?}");
            assert_eq!(row[1], "user", "{command:?}: {lines:?}");
            assert_eq!(row[2], "pkg_manager", "{command:?}: {lines:?}");
            assert_eq!(row[3], "使用 uv 管理依赖", "{command:?}: {lines:?}");
            // UPDATED 是当前时间：只钉形状（`YYYY-MM-DDTHH:MM:SSZ`）
            assert_eq!(row[4].len(), 20, "{command:?}: {lines:?}");
            assert!(row[4].ends_with('Z'), "{command:?}: {lines:?}");
        }
    }

    /// `/memory add|search|edit|delete` 全部走既有助手：回执、检索、改写与删除落在同一个库上。
    #[test]
    fn memory_add_search_edit_delete_go_through_the_store() {
        let (dir, mut session) = session_fixture("slash-memory-crud", "native");

        // add：多词文本用一个空格拼回，回执逐字复用 `memory add`
        let added = dispatch_lines(
            &mut session,
            Slash::Memory,
            &args(&["add", "user", "pkg_manager", "使用", "uv", "管理依赖"]),
        );
        assert_eq!(added, ["已记住（user/pkg_manager）：使用 uv 管理依赖"]);

        // search：命中（表头 + 一行）；多个检索词也用一个空格拼回
        for words in [&["search", "uv"][..], &["search", "使用", "uv"][..]] {
            let hit = dispatch_lines(&mut session, Slash::Memory, &args(words));
            assert_eq!(hit.len(), 2, "{words:?}: {hit:?}");
            assert!(hit[1].contains("使用 uv 管理依赖"), "{words:?}: {hit:?}");
        }
        // 没有命中：只剩表头（与 `ignis memory search` 一致）
        assert_eq!(
            dispatch_lines(&mut session, Slash::Memory, &args(&["search", "poetry"])).len(),
            1
        );

        // edit：id 就是列表的第一列，文本多词拼回
        let store = MemoryStore::new(
            Arc::new(Db::open(&dir.path().join("ignis.db")).unwrap()),
            dir.path().join("memory"),
        );
        let id = store.list(false).unwrap()[0].id.to_string();
        let edited = dispatch_lines(
            &mut session,
            Slash::Memory,
            &args(&["edit", id.as_str(), "改用", "pip"]),
        );
        assert_eq!(edited, [format!("已改写（id={id}）：改用 pip")]);
        let listed = dispatch_lines(&mut session, Slash::Memory, &args(&["list"]));
        assert!(listed[1].contains("改用 pip"), "{listed:?}");
        assert!(
            !listed.iter().any(|line| line.contains("使用 uv")),
            "{listed:?}"
        );

        // delete：回执 + 列表回到只剩表头
        let deleted = dispatch_lines(&mut session, Slash::Memory, &args(&["delete", id.as_str()]));
        assert_eq!(deleted, [format!("已删除（id={id}）")]);
        assert_eq!(
            dispatch_lines(&mut session, Slash::Memory, &args(&["list"])),
            ["ID  SCOPE  KEY  TEXT  UPDATED"]
        );

        // 不存在的 id：既有助手的报错原样交回（REPL 打 `error: memory not found: {id}`）
        for words in [
            args(&["delete", id.as_str()]),
            args(&["edit", id.as_str(), "新文本"]),
        ] {
            let error = dispatch(&mut session, Slash::Memory, &words).unwrap_err();
            assert_eq!(error, format!("memory not found: {id}"), "{words:?}");
        }
    }

    /// `/memory add project` 绑定**当前会话工作区**：REPL 里没有 `--workspace` 可解析。
    #[test]
    fn memory_project_scope_binds_the_session_workspace() {
        let (dir, mut session) = session_fixture("slash-memory-project", "native");

        let added = dispatch_lines(
            &mut session,
            Slash::Memory,
            &args(&["add", "project", "env", "使用 uv"]),
        );
        assert_eq!(added, ["已记住（project/env）：使用 uv"]);

        // 事实落在会话工作区上（消息回执、列表的 SCOPE 列与库里同一行）
        let store = MemoryStore::new(
            Arc::new(Db::open(&dir.path().join("ignis.db")).unwrap()),
            dir.path().join("memory"),
        );
        let stored = store
            .recall_by_key(FactScope::Project, Some(session.workspace()), "env")
            .unwrap()
            .expect("project 事实应绑在会话工作区上");
        assert_eq!(stored.workspace.as_deref(), session.workspace().to_str());
        // 换一个工作区就查不到：不是「任何目录都命中」的宽绑定
        assert!(
            store
                .recall_by_key(FactScope::Project, Some(Path::new("/tmp/other")), "env")
                .unwrap()
                .is_none(),
            "project 事实不应绑在别的工作区上"
        );

        let listed = dispatch_lines(&mut session, Slash::Memory, &args(&["list"]));
        assert_eq!(cells(&listed[1])[1], "project", "{listed:?}");
    }

    /// `/memory` 的参数用法与复用来的报错逐字钉住（五条 usage 只写在这一处）。
    #[test]
    fn memory_usage_errors_are_pinned() {
        let (_dir, mut session) = session_fixture("slash-memory-usage", "native");

        // 未知子命令：一句总用法行（`list` 只认 `--all`，别的旗标同一条）
        for words in [args(&["nope"]), args(&["list", "--bogus"])] {
            let error = dispatch(&mut session, Slash::Memory, &words).unwrap_err();
            assert_eq!(
                error, "usage: /memory <list|search|add|edit|delete>",
                "{words:?}"
            );
        }
        // 三条缺参的用法行（scope/key/text、id/text、id 任一为空都算缺参）
        assert_eq!(
            dispatch(&mut session, Slash::Memory, &args(&["add"])).unwrap_err(),
            "usage: /memory add <user|project|global> <key> <text>"
        );
        assert_eq!(
            dispatch(&mut session, Slash::Memory, &args(&["add", "user", "key"])).unwrap_err(),
            "usage: /memory add <user|project|global> <key> <text>"
        );
        assert_eq!(
            dispatch(&mut session, Slash::Memory, &args(&["edit", "3"])).unwrap_err(),
            "usage: /memory edit <id> <text>"
        );
        assert_eq!(
            dispatch(&mut session, Slash::Memory, &args(&["delete"])).unwrap_err(),
            "usage: /memory delete <id>"
        );
        assert_eq!(
            dispatch(
                &mut session,
                Slash::Memory,
                &args(&["delete", "3", "extra"])
            )
            .unwrap_err(),
            "usage: /memory delete <id>"
        );

        // 用法错误不改库：`list` 仍是空的
        assert_eq!(
            dispatch_lines(&mut session, Slash::Memory, &args(&["list"])),
            ["ID  SCOPE  KEY  TEXT  UPDATED"]
        );

        // 非法 scope / id / 空检索词沿用既有助手的文案（不另写一份）
        assert_eq!(
            dispatch(
                &mut session,
                Slash::Memory,
                &args(&["add", "Project", "k", "t"])
            )
            .unwrap_err(),
            "--scope 只能是 user、project 或 global，收到「Project」"
        );
        assert_eq!(
            dispatch(&mut session, Slash::Memory, &args(&["edit", "abc", "t"])).unwrap_err(),
            "memory id 必须是数字：abc"
        );
        assert_eq!(
            dispatch(&mut session, Slash::Memory, &args(&["search"])).unwrap_err(),
            "检索词不能为空"
        );
    }

    /// `/memory list` 默认只看 active，`--all` 才把被取代的历史行带出来。
    #[test]
    fn memory_list_all_includes_superseded() {
        let (_dir, mut session) = session_fixture("slash-memory-all", "native");

        // 同一个 (scope, workspace, key) 写两次：旧值被取代
        dispatch_lines(
            &mut session,
            Slash::Memory,
            &args(&["add", "user", "pkg_manager", "使用 uv"]),
        );
        let added = dispatch_lines(
            &mut session,
            Slash::Memory,
            &args(&["add", "user", "pkg_manager", "改用 pip"]),
        );
        assert_eq!(
            added,
            ["已记住（user/pkg_manager）：改用 pip（旧值已失效）"]
        );

        // 默认（与无参同路）：只剩新值
        for command in [args(&["list"]), args(&[])] {
            let active = dispatch_lines(&mut session, Slash::Memory, &command);
            assert_eq!(active.len(), 2, "{command:?}: {active:?}");
            assert!(active[1].contains("改用 pip"), "{command:?}: {active:?}");
        }
        // `--all`：历史行也出来（表头 + 两行）
        let all = dispatch_lines(&mut session, Slash::Memory, &args(&["list", "--all"]));
        assert_eq!(all.len(), 3, "{all:?}");
        assert!(all.iter().any(|line| line.contains("使用 uv")), "{all:?}");
    }

    /// `/skills` 与 `ignis skills list` 逐行相等：同一个 `render`，读的也是同一个目录。
    #[test]
    fn skills_lines_match_the_cli_render() {
        let (dir, mut session) = session_fixture("slash-skills", "native");
        let skills_dir = dir.path().join("skills");

        // 一个用户技能 + 一个坏技能（后者按既有口径产生 `!` 警告行）
        fs::create_dir_all(skills_dir.join("zzz")).unwrap();
        fs::write(
            skills_dir.join("zzz").join("SKILL.md"),
            "---\nname: zzz-skill\ndescription: Sorts last.\n---\n# 正文\n",
        )
        .unwrap();
        fs::create_dir_all(skills_dir.join("broken")).unwrap();
        fs::write(
            skills_dir.join("broken").join("SKILL.md"),
            "# 没有 frontmatter\n只有正文\n",
        )
        .unwrap();

        // 会话里的是构造时的快照（reload 属 P4）：按 `skills::run` 的同一加载路径取一份目录，
        // 先渲染出 CLI 那边的逐行输出，再把它交给会话——断言的是「同一目录 → 同一逐行输出」。
        let catalog = SkillCatalog::load(&skills_dir);
        let expected: Vec<String> = skills::render(&catalog)
            .lines()
            .map(str::to_string)
            .collect();
        let table_rows = catalog.metas().len() + 1;
        let warnings = catalog.warnings().len();
        session.skills = catalog;

        let lines = dispatch_lines(&mut session, Slash::Skills, &[]);
        assert_eq!(lines, expected, "与 `ignis skills list` 不同");
        // 形状：表头 + 每个技能一行 + `! {warning}` 行（不是「空的成功」）
        assert_eq!(lines.len(), table_rows + warnings, "{lines:?}");
        assert_eq!(cells(&lines[0]), ["NAME", "SOURCE", "DESCRIPTION"]);
        assert!(
            lines.iter().any(|line| line.starts_with("zzz-skill")),
            "{lines:?}"
        );
        assert!(
            lines[table_rows..]
                .iter()
                .all(|line| line.starts_with("! ")),
            "警告只能跟在表后：{lines:?}"
        );
    }

    /// `/tasks` 的表与 `ignis task list` 逐行同形，后面跟一行 `ignis task show` 提示。
    #[test]
    fn tasks_lines_are_the_table_plus_hint() {
        let (dir, mut session) = session_fixture("slash-tasks", "native");

        // 空库：一句 `(no tasks)`（空表头与提示对没有任务的人没有信息量）
        assert_eq!(
            dispatch_lines(&mut session, Slash::Tasks, &[]),
            ["(no tasks)"]
        );

        insert_task(
            dir.path(),
            "019a1b2c-1111-7f00-8000-000000000001",
            STARTED_AT,
        );
        insert_task(
            dir.path(),
            "019a1b2c-2222-7f00-8000-000000000002",
            STARTED_AT + 1_000,
        );

        let lines = dispatch_lines(&mut session, Slash::Tasks, &[]);
        // 表头 + 两条 + 提示；最近的在前（与 `task list` 同一排序）
        assert_eq!(lines.len(), 4, "{lines:?}");
        assert_eq!(
            cells(&lines[0]),
            ["ID", "STATUS", "KIND", "STARTED", "PROMPT"]
        );
        assert!(lines[1].starts_with("019a1b2c-2222"), "{lines:?}");
        assert!(lines[2].starts_with("019a1b2c-1111"), "{lines:?}");
        assert!(lines[1].contains("修复配置路径"), "{lines:?}");
        assert!(
            lines[1].contains("success") || lines[1].contains("running"),
            "{lines:?}"
        );
        // 提示带最近一条的短 id（表的第一列，`task show` 接受这种唯一前缀）
        assert_eq!(lines[3], "hint: ignis task show 019a1b2c-2222");

        // 有参：条数照给出（表头 + 一行 + 提示）
        assert_eq!(
            dispatch_lines(&mut session, Slash::Tasks, &args(&["1"])).len(),
            3
        );

        // 非正整数：`abc`/`0`/负数/多余词都是同一条错误（也不读库）
        for words in [
            args(&["abc"]),
            args(&["0"]),
            args(&["-1"]),
            args(&["5", "6"]),
        ] {
            let error = dispatch(&mut session, Slash::Tasks, &words).unwrap_err();
            assert_eq!(error, "tasks: limit must be a positive number", "{words:?}");
        }
    }

    /// `/tasks` 无参给 10 条（与 `/help` 的 `(default 10)` 同一口径）。
    #[test]
    fn tasks_default_limit_is_ten() {
        let (dir, mut session) = session_fixture("slash-tasks-limit", "native");
        // 12 条任务：越后越新（`short_id` 的前 13 字符各不相同，方便认出哪 10 条进了表）
        let ids: Vec<String> = (0..12)
            .map(|index| format!("019a1b2c-{index:04x}-7f00-8000-000000000000"))
            .collect();
        for (index, id) in ids.iter().enumerate() {
            insert_task(dir.path(), id, STARTED_AT + index as i64 * 1_000);
        }

        let lines = dispatch_lines(&mut session, Slash::Tasks, &[]);
        // 表头 + 默认 10 行 + 提示
        assert_eq!(lines.len(), 12, "{lines:?}");
        assert!(lines[1].starts_with(&ids[11][..13]), "{lines:?}");
        assert!(lines[10].starts_with(&ids[2][..13]), "{lines:?}");
        for oldest in [&ids[0], &ids[1]] {
            assert!(
                !lines.iter().any(|line| line.starts_with(&oldest[..13])),
                "第 11、12 新的两条不该出现：{lines:?}"
            );
        }
        assert_eq!(
            lines[11],
            format!("hint: ignis task show {}", &ids[11][..13])
        );

        // 显式给 N 时不替用户砍：`/tasks 12` 出 12 行数据
        assert_eq!(
            dispatch_lines(&mut session, Slash::Tasks, &args(&["12"])).len(),
            14
        );
    }

    /// `/kernel` 的两行输出：当前内核名 + 非 native 的那行能力提示（从 `repl` 原样搬来）。
    #[test]
    fn kernel_lines_pair_the_current_kernel_with_its_capability_note() {
        assert_eq!(kernel_lines("native"), ["kernel: native"]);

        assert_eq!(kernel_lines("codex").len(), 2);
        assert_eq!(kernel_lines("codex")[0], "kernel: codex");
        // 提示行与 `commands::kernel_note` 是同一条（两处措辞分家 = 用户以为切换没生效）
        assert_eq!(kernel_lines("codex")[1], kernel_note("codex").unwrap());
        assert_eq!(kernel_lines("claude")[1], kernel_note("claude").unwrap());
    }
}
