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
use ignis_runtime::Usage;

use super::Session;
use super::kernel::{KERNELS, NATIVE_KERNEL, kernel_note};

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
    /// `/tasks [N]`：最近任务表（Task 6 接上）。
    Tasks,
    /// `/skills`：技能目录（Task 6 接上）。
    Skills,
    /// `/memory …`：长期记忆（Task 6 接上）。
    Memory,
    /// `/details [minimal|normal|debug]`：显示或切换输出模式。
    Details,
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

/// 帮助分组（顺序即 [`help_lines`] 的输出顺序）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Group {
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
/// 顺序即组内顺序；`SESSION` 组的三条会话命令属 P3，本阶段不进表（帮助里因此也不出现）。
const COMMANDS: [Spec; 9] = [
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
    for group in [Group::Agent, Group::Intelligence, Group::Interface] {
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

/// `/help` 的输出：三个分组与九条命令（顺序与 [`COMMANDS`] 一致）。
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

/// `/status` 的输出：内核、模型、工作区、未读告警、最近一轮用量（顺序固定）。
///
/// 非 native 的内核在 `kernel:` 行后追加能力提示行（与 `/kernel` 同一份文案），模型行给
/// `(kernel default)`——它们用各自 CLI 的默认模型，Ignis 这边的模型名对它们没有意义。
/// P2 还没有会话行（P3 才加）。
pub fn status_lines(
    kernel: &str,
    model: &str,
    workspace: &Path,
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
        // 已知命令但参数面还没有候选（`/memory` 的子命令补全随 Task 6），未知命令更没有
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
            session.unread_alerts(),
            session.last_usage(),
        ))),
        // Task 6 接上三个复用面命令：命令名已在表里（帮助完整、未知命令口径正确），
        // 这里先给一句占位错误——静默无输出会让用户以为命令生效了。
        Slash::Memory => Err("/memory: not implemented yet".to_string()),
        Slash::Skills => Err("/skills: not implemented yet".to_string()),
        Slash::Tasks => Err("/tasks: not implemented yet".to_string()),
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use ignis_core::config::OutputMode;
    use ignis_runtime::Usage;

    use super::*;
    use crate::commands::fixtures::session_fixture;

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

        // 组名独占一行、按固定顺序（`SESSION` 组属 P3，本阶段不出现）
        let groups: Vec<&str> = lines
            .iter()
            .filter(|line| !line.starts_with("  "))
            .map(String::as_str)
            .collect();
        assert_eq!(groups, ["AGENT", "INTELLIGENCE", "INTERFACE"], "{lines:?}");

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

    /// 帮助里绝不出现后续阶段的命令（P3/P4 才实现，提前列出来等于说谎）。
    #[test]
    fn help_lines_never_mentions_later_phases() {
        let help = help_lines().join("\n");
        for later in [
            "/new",
            "/sessions",
            "/resume",
            "/context",
            "/compact",
            "/learn",
            "/reload",
        ] {
            assert!(
                !help.contains(later),
                "{later} 不该出现在 P2 的帮助里：{help}"
            );
        }
        assert!(!help.contains("SESSION"), "{help}");
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

    /// `/status` 的真值表：五行固定顺序，非 native 追加能力提示行。
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

        assert_eq!(
            status_lines("native", "test-model", path, Some(2), Some(&usage)),
            [
                "kernel: native",
                "model: test-model",
                "workspace: /tmp/ws",
                "unread alerts: 2",
                "last turn usage: in=1234 out=567 tokens (exact)",
            ]
        );

        // 告警计数读失败 / 本会话还没跑过任务
        assert_eq!(
            status_lines("native", "test-model", path, None, None),
            [
                "kernel: native",
                "model: test-model",
                "workspace: /tmp/ws",
                "unread alerts: unavailable",
                "last turn usage: -",
            ]
        );

        // 估算用量按 `(estimated)` 标出；0 条未读照实显示
        let lines = status_lines("native", "test-model", path, Some(0), Some(&estimated));
        assert_eq!(lines.len(), 5, "{lines:?}");
        assert_eq!(lines[3], "unread alerts: 0");
        assert_eq!(lines[4], "last turn usage: in=12 out=3 tokens (estimated)");

        // 非 native：kernel 行后是能力提示行，模型行给 (kernel default)
        let codex = status_lines("codex", "test-model", path, Some(1), None);
        assert_eq!(codex.len(), 6, "{codex:?}");
        assert_eq!(codex[0], "kernel: codex");
        assert_eq!(codex[1], kernel_note("codex").unwrap());
        assert_eq!(codex[2], "model: (kernel default)");
        assert_eq!(codex[3], "workspace: /tmp/ws");
        assert_eq!(codex[4], "unread alerts: 1");
        assert_eq!(codex[5], "last turn usage: -");
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

    /// Task 6 才接上的三个复用面命令：命令名已在表里（帮助完整），分发先给一句占位错误。
    /// Task 6 落地时把这条用例换成真断言。
    #[test]
    fn dispatch_later_phase_commands_report_not_implemented() {
        let (_dir, mut session) = session_fixture("slash-placeholder", "native");

        for (command, name) in [
            (Slash::Memory, "/memory"),
            (Slash::Skills, "/skills"),
            (Slash::Tasks, "/tasks"),
        ] {
            let error = dispatch(&mut session, command, &[]).unwrap_err();
            assert_eq!(error, format!("{name}: not implemented yet"), "{command:?}");
        }
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
