//! TUI（Beta）：终端层、捕获、帧合成、输入路由与主循环。
//!
//! 分层（自下而上）：
//! - [`term`]：备用屏/光标/尺寸/panic 兜底（raw mode 复用 [`crate::editor::raw::RawMode`]，
//!   `ISIG` 保持开着）；
//! - [`capture`]：渲染器的输出落点（[`capture::CaptureSink`]）与净化（[`capture::sanitize`]）
//!   ——「模型输出只能显示、不能驱动终端」这条防线只有一个入口；
//! - [`view`]：帧合成器（[`view::compose`]）与有界滚动区（[`view::Scrollback`]，上限 2000 行）；
//! - [`input`]：编辑器输入态（复用 P2 的状态机与解码器，只多一条瞬时提示行）；
//! - [`app`]：应用状态与两个循环的公共机件（[`app::TuiApp::on_bytes`] 出动作、
//!   [`app::TuiApp::drive`] 在忙碌期驱动借用会话的 future）；
//! - [`approval`]：通道式审批（[`approval::TuiApproval`] 把请求交给 UI 循环、按键由
//!   [`app::TuiApp::approval_key`] 收）——提示文案与经典 REPL 同源（`crate::approval`）；
//! - [`confirm`]：通道式确认面（[`confirm::tui_confirm`] 把 `/learn` 的确认请求交给 UI 循环、
//!   按键由 [`app::TuiApp::confirm_key`] 收）——头行/预览与经典 REPL 同源（`commands::learn`）；
//! - 本文件：可用性判定（[`tui_unavailable_reason`]）、降级 note（[`fallback_note`]）、帧用的
//!   会话事实快照（[`refresh_facts`]）、开机行（[`startup_lines`]）、提交分类（[`classify`]）与
//!   主循环（[`run`]）。开机行的品牌块与经典路径同一个函数，只有落点不同（进滚动区）。
//!
//! 可用性判定是**纯函数**：tty / `TERM` / 尺寸三件事都由调用方探好后传进来，用例不必造终端也不
//! 必改环境变量（`repl::run` 与 `tui::start` 各自把现场探好）。
//!
//! ## Beta 边界（已知限制，设计 §2.2 的非目标）
//!
//! 这一版**刻意不做**下面几件事；遇到时按降级或忽略处理，别把它们当缺陷：
//!
//! - **不支持鼠标**：没有鼠标事件解码与命中测试（终端自身的选区/滚动照旧可用）；
//! - **滚动区有界**：只留最近 [`view::SCROLLBACK_LINES`]（2000）行，超出的丢最旧——不做无界
//!   scrollback（内存与每帧重绘的代价都随行数线性长）；
//! - **任务运行中不接受输入**：忙碌期（任务轮次、`/compact`、`/learn`）的按键一律丢弃、缓冲一个
//!   字符都不进（输入区首行是 `working…`，提示行是 `Ctrl+C cancels`）；
//! - **不做打断重定向**：Ctrl+C 只取消**本轮**（任务 / 压缩 / 学习），不会转成别的动作（不转发给
//!   工具、不改审批决定）；
//! - **主题只有 dark/light 与无色**：`ui.theme` 两档 + `ui.color`/`NO_COLOR` 决定帧里有没有 SGR
//!   （`color=false` 时零 SGR；光标定位与清行不是颜色，照常写——见 [`view::compose`]）；
//! - **Esc 的消歧窗口是一个 100ms tick**：字节解码器无时钟（半条转义序列必须留在缓存里），裸 ESC
//!   由 [`app::TuiApp::on_tick`] 合成，代价是 Alt+Enter 被切在两个 tick 之间时可能被当成 Enter；
//! - **异常终止不做兜底（SIGTERM/SIGTSTP/SIGHUP）**：只有 SIGINT 与 SIGWINCH 有处理器（在进
//!   [`term::TerminalGuard`] 之前注册，见 [`start_with`]）；被 `kill`、`Ctrl+Z` 挂起或终端断开时
//!   进程直接走掉，备用屏与 raw mode 由终端自己收尾（回到 shell 可能看起来坏掉）——敲 `stty sane`
//!   或 `reset` 恢复。正常退出（`/exit`、空缓冲 `Ctrl+D`、空闲 `Ctrl+C`、stdin 关闭）与 panic 都由
//!   守卫的 `Drop` 还原；
//! - **极端批次会丢回车之后的按键**：粘贴后紧跟回车的同一批字节里，回车（提交）之后的按键会被丢掉
//!   ——与「忙碌期输入禁用」是同一条规则。

pub mod app;
pub mod approval;
pub mod capture;
pub mod confirm;
pub mod input;
pub mod term;
pub mod view;
pub mod welcome;

/// 终端尺寸（列 × 行）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Size {
    /// 列数。
    pub cols: usize,
    /// 行数。
    pub rows: usize,
}

/// TUI 可用的最小列数：小于它就降级经典 REPL（设计 §6.10「尺寸过小」）。
pub const MIN_COLS: usize = 20;

/// TUI 可用的最小行数：小于它就降级经典 REPL（设计 §6.10「尺寸过小」）。
pub const MIN_ROWS: usize = 8;

/// 进不去 TUI 的理由（`None` = 可用）。
///
/// 纯函数：tty / `TERM` / 尺寸三件事都由调用方探好后传进来，用例不必造终端也不必改环境变量。
/// 判定顺序即理由链——stdin → stdout → `TERM` → 尺寸，一条不过就报它那句，后面的不再看：
/// 用户看到的第一句永远是最靠前的那个硬条件。
pub fn tui_unavailable_reason(
    stdin_tty: bool,
    stdout_tty: bool,
    term: Option<&std::ffi::OsStr>,
    size: Option<Size>,
) -> Option<String> {
    if !stdin_tty {
        return Some("stdin is not a terminal".to_string());
    }
    if !stdout_tty {
        return Some("stdout is not a terminal".to_string());
    }
    if !crate::editor::raw::RawMode::supported(term) {
        return Some("TERM is dumb".to_string());
    }
    // 探不到尺寸按 0×0 处理：降级总是安全的那个方向，也让理由句保持同一个形状。
    let size = size.unwrap_or(Size { cols: 0, rows: 0 });
    if size.cols < MIN_COLS || size.rows < MIN_ROWS {
        return Some(format!("terminal too small ({}x{})", size.cols, size.rows));
    }
    None
}

/// 降级时的一行 note（逐字，英文 chrome）。
///
/// `reason` 来自 [`tui_unavailable_reason`] 或终端守卫的 io 错误；调用方把它整行打到经典路径
/// 上（`--tui` 不可用时只多这一行，其余一切照旧）。
///
/// 文案的唯一定义在 [`crate::repl::tui_unavailable_note`]（`repl` 不按平台门控，非 unix 的降级
/// 也要打同一句话）；这里只做转发，逐字不变。
pub fn fallback_note(reason: &str) -> String {
    crate::repl::tui_unavailable_note(reason)
}

// ---- 主循环接线（Task 3）：事实快照、提交分类、空闲/忙碌两个循环 ----

use std::ffi::OsStr;
use std::io::{self, IsTerminal};
use std::process::ExitCode;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use anyhow::Result;
use ignis_core::skills::SkillCatalog;
use ignis_core::task::TaskKind;
use ignis_runtime::{Role, RunStatus, RuntimeEvent};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::cli::Cli;
use crate::commands::slash::{self, Dispatch, Parsed, Slash};
use crate::commands::{Session, env_text_value, kernel_note, no_color_value};
use crate::editor::draw::CONTINUATION;
use crate::editor::history::History;
use crate::render::{RenderFlavor, Renderer, RendererOptions, color_depth, resolve_color};
use crate::repl::{alerts_banner_line, resume_continue};
use crate::tui::app::{Action, Busy, InputChunk, TuiApp, Wake};
use crate::tui::approval::{ApprovalJob, TuiApproval};
use crate::tui::capture::CaptureSink;
use crate::tui::term::{TerminalGuard, install_panic_hook};
use crate::tui::view::{
    AgentState, ContextUsage, Scrollback, ViewFacts, compose, estimate_tokens_from_chars,
};

/// 提交回显的首行标记（与输入区、编辑器同一对标记）。
const ECHO_MARK: &str = "❯ ";

/// 会用到的会话事实快照（只用既有/本任务新增的 `pub` 访问器）。
///
/// 色阶的终端现场（`COLORTERM`/`TERM`）从进程环境读，转发给 [`refresh_facts_with`]。
pub fn refresh_facts(session: &Session) -> ViewFacts {
    // 非 UTF-8 的取值照样算「已设置」：转换与经典路径共用 `no_color_value` / `env_text_value`
    refresh_facts_with(
        session,
        no_color_value(std::env::var_os("NO_COLOR")).as_deref(),
        env_text_value(std::env::var_os("COLORTERM")).as_deref(),
        env_text_value(std::env::var_os("TERM")).as_deref(),
    )
}

/// 带 `NO_COLOR` 与终端现场注入的变体（用例不必改进程环境）。
pub fn refresh_facts_with(
    session: &Session,
    no_color: Option<&str>,
    colorterm: Option<&str>,
    term: Option<&str>,
) -> ViewFacts {
    let ui = session.ui_config();
    // TUI 只在终端上跑：stdout 恒是 tty，颜色只看 `ui.color` 与 `NO_COLOR`；色阶在它的结论之上
    let color = resolve_color(ui.color, true, no_color);
    let capabilities = session.model_capabilities();
    let context = context_usage(session, &capabilities);
    ViewFacts {
        version: env!("CARGO_PKG_VERSION"),
        workspace: session.workspace().display().to_string(),
        kernel: session.kernel().to_string(),
        session: slash::session_label(session.current_session()),
        mode: session.output_mode(),
        interaction_mode: session.interaction_mode(),
        theme: ui.theme,
        color,
        depth: color_depth(color, colorterm, term),
        budget: session.context_budget(),
        history_messages: session.history().len(),
        stats: session.last_prompt_stats().copied(),
        usage: session.last_usage().copied(),
        native: session.is_native(),
        model: session.model().to_string(),
        provider: capabilities.provider.clone(),
        capabilities,
        context,
        tools: session.tool_names(),
        skills: session.skill_metas(),
        memory: session.memory_count(),
    }
}

/// 上下文占用：Provider Usage 优先（exact/estimated 都照它的标记），否则用 PromptStats 的
/// 字符数做**明确估算**；窗口来自能力解析的 `effective_limit`，未知时显示 `?`。
fn context_usage(
    session: &Session,
    capabilities: &ignis_runtime::caps::ModelCapabilities,
) -> Option<ContextUsage> {
    let window = capabilities.effective_limit;
    if let Some(usage) = session.last_context_usage() {
        // 上下文占用是这次请求的输入 token；输出 token 属于最近一轮用量，不混进窗口占用。
        return Some(ContextUsage::new(
            usage.input_tokens,
            usage.estimated,
            window,
        ));
    }
    if let Some(stats) = session.last_prompt_stats() {
        let chars = stats.system_chars.saturating_add(stats.history_chars);
        return Some(ContextUsage::new(
            estimate_tokens_from_chars(chars),
            true,
            window,
        ));
    }
    // 还没有任何用量/组装统计：窗口已知时仍然展示窗口（用量显示 `-`），不假装 0。
    Some(ContextUsage::unknown(window))
}

/// 一条已提交输入的本地分类（复用 [`slash::parse`]，只把四个需要会话/模型的形状分出来）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Submitted {
    /// 空行/纯空白：跳过。
    Empty,
    /// 未知命令的错误行（本地报错，**绝不**送模型）。
    Unknown(String),
    /// `/exit`。
    Exit,
    /// `/sessions`：开全屏选择器（空闲期的一态；Enter 交回 `Action::Resume`）。
    Sessions,
    /// `/compact`：需要模型往返，在忙碌子循环里执行。
    Compact,
    /// `/learn [task-id]`：需要模型往返与内联确认（忙碌子循环里的 `learn_turn`）。
    Learn(Option<String>),
    /// 其余同步命令：外层借用会话走一遍 `slash::dispatch`。
    Command(slash::Slash, Vec<String>),
    /// 已发现的技能：`/<name> <task>`，转成带 `load_skill` 指令的任务。
    Skill(String, String),
    /// 任务文本。
    Task(String),
}

/// 分类一行已提交的输入：只按 TUI 的执行方式把几个形状挑出来，其余照 `slash` 那一份口径走。
pub fn classify(line: &str, skills: &SkillCatalog) -> Submitted {
    match slash::parse_with_skills(line, skills) {
        Parsed::Empty => Submitted::Empty,
        Parsed::Unknown(name) => Submitted::Unknown(name),
        Parsed::Command(Slash::Exit, _) => Submitted::Exit,
        Parsed::Command(Slash::Sessions, _) => Submitted::Sessions,
        Parsed::Command(Slash::Compact, _) => Submitted::Compact,
        Parsed::Command(Slash::Learn, args) => Submitted::Learn(args.first().cloned()),
        Parsed::Command(command, args) => Submitted::Command(command, args),
        Parsed::Skill(name, prompt) => Submitted::Skill(name, prompt),
        Parsed::Task(text) => Submitted::Task(text),
    }
}

/// 提交回显：首行 `❯ `、续行 `… `（与编辑器同一对标记）；空行（含纯空白）不回显。
///
/// 输入区在提交后立刻进入忙碌态（第 1 行换成 `working…`），回显是用户确认「我提交了什么」的唯一
/// 落点；多行输入按行落地，内部换行原样保留。
fn echo_lines(line: &str) -> Vec<String> {
    if line.trim().is_empty() {
        return Vec::new();
    }
    line.split('\n')
        .enumerate()
        .map(|(index, text)| {
            let marker = if index == 0 { ECHO_MARK } else { CONTINUATION };
            format!("{marker}{text}")
        })
        .collect()
}

/// 画一帧（空闲循环的重绘点；写失败忽略，画不出帧不该带走一场会话）。
fn draw_frame(term: &mut TerminalGuard, app: &TuiApp, facts: &ViewFacts) {
    let _ = term.write(compose(&app.view(facts)).as_bytes());
}

/// 启动 TUI：可用性判定 → 守卫 → 会话（含 `-c`）→ 主循环。
///
/// `Ok(None)` = 不可用（已往 stdout 打了一行 [`fallback_note`]），调用方落回经典 REPL。
pub async fn start(cli: &Cli) -> Result<Option<ExitCode>> {
    start_with(
        cli,
        io::stdin().is_terminal(),
        io::stdout().is_terminal(),
        std::env::var_os("TERM").as_deref(),
        term::probe_size(libc::STDOUT_FILENO),
        || {
            // 审批走通道式（T4）：handler 交给会话，接收端交给主循环（`drive` 的审批分支）
            let (handler, approvals) = TuiApproval::channel();
            let session = Session::new_with_handler(cli, Arc::new(handler))?;
            Ok((session, approvals))
        },
    )
    .await
}

/// [`start`] 的注入变体：可用性探针与「会话工厂」都由调用方给（用例不造终端、不改进程环境、
/// 也不碰真实 home）。
///
/// 顺序钉死（R4）：可用性判定（不通过 → 一行 note + `Ok(None)`，**不建会话**，工厂根本不会被
/// 调用）→ `install_panic_hook()` → [`TerminalGuard::enter`]（io 错误 → 一行 note + `Ok(None)`；
/// 成功之后任何早退都由守卫 `Drop` 还原备用屏与 termios）→ 会话 + `-c` + 开机行 → [`run`]。
/// 生产入口 [`start`] 传的是进程现场与真实工厂，与用例走同一条路径。
///
/// **信号流在进守卫之前注册**（F4）：可用性判定之后、[`TerminalGuard::enter`] 之前调一次
/// [`app::Signals::install`]，把两个流交给 [`run`] → [`app::TuiApp::attach_signals`]。启动窗口
/// （建会话、`-c` 装载、起输入线程）里的 SIGINT 从此由主循环接住，而不是按默认处置把进程带走。
async fn start_with<B>(
    cli: &Cli,
    stdin_tty: bool,
    stdout_tty: bool,
    term: Option<&OsStr>,
    size: Option<Size>,
    build: B,
) -> Result<Option<ExitCode>>
where
    B: FnOnce() -> Result<(Session, mpsc::UnboundedReceiver<ApprovalJob>)>,
{
    // 1. 可用性：tty / TERM / 尺寸三件事都探好后交给纯函数；一条不过就只打一行 note
    if let Some(reason) = tui_unavailable_reason(stdin_tty, stdout_tty, term, size) {
        println!("{}", fallback_note(&reason));
        return Ok(None);
    }

    // 2. 信号流与 panic 兜底：两者都在进守卫**之前**做——守卫成功之后每一条退出路径都靠它们
    //    收尾；这一份注册是唯一的一次（`TuiApp` 里绝不重复注册）
    let signals = app::Signals::install();
    install_panic_hook();
    let (mut term, _) = match TerminalGuard::enter() {
        Ok(entered) => entered,
        Err(error) => {
            println!("{}", fallback_note(&error.to_string()));
            return Ok(None);
        }
    };

    // 3. 会话（含 `-c`）与开机行：TUI 用新的渲染风味；模型能力在首帧前解析一次（Registry +
    //    用户配置是同步的，只有 OpenRouter/Ollama/vLLM 会做一次短超时探测）。
    let (mut session, approvals) = build()?;
    session.set_render_flavor(RenderFlavor::Tui);
    session.refresh_model_capabilities().await;
    let resume_note =
        resume_continue(&mut session, cli.continue_session).map_err(anyhow::Error::msg)?;
    let mut startup = startup_lines(&session, resume_note, term.size().cols, cli.no_banner);
    // `-c` 命中时把 canonical 历史重放进滚动区：只恢复模型上下文、不显示旧对话会让人以为
    // 开了一个新会话。欢迎面板因此自然让位给 transcript。
    if cli.continue_session && session.current_session().is_some() {
        let facts = refresh_facts(&session);
        startup.extend(resumed_transcript_lines(&session, &facts, term.size().cols));
    }

    // 4. 主循环：输入读取线程 + 块通道；退出码原样交回
    let (tx, chunks) = mpsc::unbounded_channel();
    app::spawn_input_reader(tx);
    let code = run(
        &mut session,
        &mut term,
        chunks,
        approvals,
        startup,
        signals,
        !cli.no_banner,
    )
    .await?;
    Ok(Some(code))
}

/// 开机行进滚动区的行（顺序即落地顺序）：`-c` 的 note（未命中才有）→ 内核能力提示（非 native
/// 才有）→ 未读告警行（有才有）——每一句都来自经典路径同一份函数/常量，TUI 不另写文案。
/// 品牌横幅不进这里：它由实时欢迎面板按当前尺寸合成。
///
/// 终端现场（`COLORTERM`/`TERM`/`NO_COLOR`）从进程环境读，转发给 [`startup_lines_with`]
/// ——与 [`refresh_facts`] 同一手法。宽度由调用方给（[`start_with`] 传 `term.size().cols`）。
fn startup_lines(
    session: &Session,
    resume_note: Option<&'static str>,
    cols: usize,
    no_banner: bool,
) -> Vec<String> {
    startup_lines_with(
        session,
        resume_note,
        cols,
        no_banner,
        env_text_value(std::env::var_os("COLORTERM")).as_deref(),
        env_text_value(std::env::var_os("TERM")).as_deref(),
        no_color_value(std::env::var_os("NO_COLOR")).as_deref(),
    )
}

/// 带终端现场注入的变体（用例不必改进程环境）。
///
/// `term_supported` 恒 `true`：能走到这里的现场已经过 [`tui_unavailable_reason`] 的 `TERM` 关卡
/// （dumb / 缺失 / 空串在那就降级了），品牌块的哑终端保守规则在这里没有第二种可能。
fn startup_lines_with(
    session: &Session,
    resume_note: Option<&'static str>,
    _cols: usize,
    _no_banner: bool,
    _colorterm: Option<&str>,
    _term: Option<&str>,
    _no_color: Option<&str>,
) -> Vec<String> {
    // 品牌横幅进的是 TUI 的实时欢迎面板（可随 Resize 重排），不再写进 scrollback；
    // 开机行只保留 `-c` note、内核能力提示与未读告警。
    let mut lines: Vec<String> = Vec::new();
    lines.extend(resume_note.map(str::to_string));
    if let Some(note) = kernel_note(session.kernel()) {
        lines.push(note);
    }
    if let Some(alert) = alerts_banner_line(session.unread_alerts()) {
        lines.push(alert);
    }
    lines
}

/// `-c` 命中后的可见 transcript：`resumed session …` 提示 + 用户回显 + assistant Markdown。
///
/// 只恢复模型历史而不把旧对话画出来，用户会以为开了一个新会话；这里把 canonical 历史
/// 按与实时轮次一致的 `❯`/Markdown 排版重放进滚动区，欢迎面板因此自然让位。
fn resumed_transcript_lines(session: &Session, facts: &ViewFacts, cols: usize) -> Vec<String> {
    let mut lines = Vec::new();
    if let Some(id) = session.current_session() {
        lines.extend(slash::resume_lines(id, session.history().len()));
        lines.push(String::new());
    }
    let options = RendererOptions {
        mode: session.output_mode(),
        color: facts.color,
        theme: facts.theme,
        depth: facts.depth,
        width: cols.max(1),
    };
    for message in session.history() {
        match message.role {
            Role::User => {
                lines.extend(echo_lines(&message.content));
                lines.push(String::new());
            }
            Role::Assistant => {
                lines.extend(render_history_text(&message.content, options));
                lines.push(String::new());
            }
            Role::System | Role::Tool => {}
        }
    }
    while lines.last().is_some_and(|line| line.is_empty()) {
        lines.pop();
    }
    lines
}

/// 用实时轮次同一个 Markdown 渲染器重放一条 assistant 历史。
fn render_history_text(text: &str, options: RendererOptions) -> Vec<String> {
    let (sink, buffer) = CaptureSink::pair();
    let sink: Box<dyn std::io::Write + Send> = Box::new(sink);
    let mut renderer = Renderer::new(sink, options, Arc::new(AtomicBool::new(false)))
        .with_flavor(RenderFlavor::Tui);
    renderer.on_event(&RuntimeEvent::TextDelta {
        text: text.to_string(),
    });
    renderer.finish();
    let bytes = capture::drain(&buffer);
    let text = capture::sanitize(&bytes);
    text.lines().map(str::to_string).collect()
}

/// TUI 主循环：空闲期读输入并执行动作，忙碌期驱动 future。返回进程退出码。
///
/// 空闲循环 = 「等输入块/SIGINT/SIGWINCH/100ms tick → 处理动作 → 刷新事实 → 重绘」；忙碌循环在
/// [`TuiApp::drive`] 里（future 借用会话期间这里只读事实快照）。每一条退出路径（`/exit`、空缓冲
/// `Ctrl+D`、空闲 SIGINT、stdin 关闭）都靠调用方持有守卫的 `Drop` 还原终端。
///
/// `approvals` 是 [`TuiApproval::channel`] 的回信端（内联审批画在忙碌循环的滚动区里）；
/// `startup` 是 [`startup_lines`] 的开机行（品牌块、`-c` note、内核能力提示、未读告警行），先落
/// 滚动区再画第一帧；`signals` 是进守卫之前注册好的 SIGINT/SIGWINCH 流（F4），接进 [`TuiApp`]
/// 之后由它持有——应用自己绝不重复注册。
pub async fn run(
    session: &mut Session,
    term: &mut TerminalGuard,
    chunks: mpsc::UnboundedReceiver<InputChunk>,
    approvals: mpsc::UnboundedReceiver<ApprovalJob>,
    startup: Vec<String>,
    signals: app::Signals,
    welcome: bool,
) -> Result<ExitCode> {
    // 捕获缓冲：渲染器与滚动区共持（每轮任务的 `CaptureSink` 由 `TuiApp::capture_sink` 现取）
    let (_, buffer) = CaptureSink::pair();
    // 输入历史与经典编辑器同一份（`~/.ignis/history`）：TUI 里按过的 ↑ 在经典路径里也认
    let mut app = TuiApp::new(
        Scrollback::new(),
        History::load(session.home().join("history")),
        buffer,
    );
    app.attach_chunks(chunks);
    app.attach_approvals(approvals);
    // 信号流接上（进守卫之前注册的那一份）：启动窗口里的 SIGINT 由主循环接住
    app.attach_signals(signals);
    app.set_size(term.size());
    // `--no-banner` 关闭实时欢迎面板（开机行仍照常落地）
    app.set_welcome(welcome);
    // 开机行先落滚动区：第一帧就带着它们（与经典路径同一份文案）
    if !startup.is_empty() {
        app.push_lines(startup);
    }
    let mut facts = refresh_facts(session);
    app.sync_skills(&facts);
    draw_frame(term, &app, &facts);

    loop {
        match app.wait_idle().await {
            Wake::Chunk(InputChunk::Bytes(bytes)) => {
                let actions = app.on_bytes(&bytes);
                let dirty = actions.iter().any(|action| *action != Action::None);
                if let Some(code) =
                    handle_actions(session, &mut app, &mut facts, Some(&mut *term), actions).await?
                {
                    return Ok(code);
                }
                if dirty {
                    draw_frame(term, &app, &facts);
                }
            }
            // stdin 关闭（对端走了）：与 EOF 同一条路，干净退出
            Wake::Chunk(InputChunk::Eof) => return Ok(ExitCode::SUCCESS),
            // 空闲期 Ctrl-C：还原终端并以 0 退出
            Wake::Sigint => return Ok(ExitCode::SUCCESS),
            Wake::Sigwinch => {
                app.set_size(term.refresh_size());
                draw_frame(term, &app, &facts);
            }
            Wake::Tick => {
                if app.on_tick() {
                    draw_frame(term, &app, &facts);
                }
            }
        }
    }
}

/// 执行一批输入动作（`term` 为空 = 用例的无终端路径）。返回 `Some(code)` = 主循环该退出。
async fn handle_actions(
    session: &mut Session,
    app: &mut TuiApp,
    facts: &mut ViewFacts,
    mut term: Option<&mut TerminalGuard>,
    actions: Vec<Action>,
) -> Result<Option<ExitCode>> {
    for action in actions {
        match action {
            // 空操作与重绘都在批末统一处理（一批至多一帧）
            Action::None | Action::Redraw => {}
            Action::Exit => return Ok(Some(ExitCode::SUCCESS)),
            Action::Palette => app.open_palette(facts),
            Action::Submit(line) => {
                if let Some(code) =
                    submit_line(session, app, facts, term.as_deref_mut(), &line).await?
                {
                    return Ok(Some(code));
                }
            }
            // 选择器选中一条会话：装载它的历史（与经典 `/resume` 同一条路：整条 id）。成功回
            // `slash::resume_lines` 一行、失败回 `error: {e}` 一行（选择器此前已关闭），随后刷新
            // 事实快照（会话标签与会话历史都换了）
            Action::Resume(id) => {
                match session.resume_session(&id) {
                    Ok(messages) => app.push_lines(slash::resume_lines(&id, messages)),
                    Err(error) => app.push_lines([format!("error: {error}")]),
                }
                *facts = refresh_facts(session);
                app.sync_skills(facts);
            }
        }
    }
    Ok(None)
}

/// 执行一条已提交的输入：回显 → 分类 → 各自落地；返回 `Some(code)` = 主循环该退出。
///
/// 每条分支结束时都刷新一次事实快照（`/details` 切了模式、跑完一轮任务都会改它）。
async fn submit_line(
    session: &mut Session,
    app: &mut TuiApp,
    facts: &mut ViewFacts,
    term: Option<&mut TerminalGuard>,
    line: &str,
) -> Result<Option<ExitCode>> {
    // 回显：用户提交的内容留在滚动区（空行不留）；任务轮次后留一个空行，正文不贴在 ❯ 行下面
    let echo = echo_lines(line);
    let has_echo = !echo.is_empty();
    if has_echo {
        app.push_lines(echo);
    }
    let submitted = {
        let skills = session.skills();
        classify(line, &skills)
    };
    if matches!(&submitted, Submitted::Task(_) | Submitted::Skill(_, _)) && has_echo {
        app.push_lines([String::new()]);
    }

    let mut exit = false;
    match submitted {
        // 空行：什么都不做（编辑器可能提交空串）
        Submitted::Empty => {}
        // 未知命令：本地报错行，绝不送模型
        Submitted::Unknown(name) => app.push_lines([slash::unknown_command_line(&name)]),
        Submitted::Exit => exit = true,
        // `/sessions`：开全屏选择器（游标在第一条、当前会话带 `*`；Enter 由空闲循环那条
        // `Action::Resume` 执行）。空表也开（内容是 `(no sessions)`）；读库失败与经典同形一行
        Submitted::Sessions => match session.sessions() {
            Ok(rows) => app.open_selector(rows, session.current_session().map(str::to_string)),
            Err(error) => app.push_lines([format!("error: {error}")]),
        },
        // `/compact` 需要模型往返：忙碌子循环里驱动（成功逐行进滚动区、错误进 `error: {…}` 行）
        Submitted::Compact => compact_turn(session, app, facts, term).await,
        // `/learn` 需要模型往返 + 内联确认：同一个忙碌子循环里驱动（确认面接进本轮）
        Submitted::Learn(arg) => learn_turn(session, app, facts, term, arg).await,
        Submitted::Command(command, args) => {
            // `/model <name>` 改的是会话覆盖；`/kernel` 可能从外部内核切回 native。两种情况下
            // 能力解析都要跟着刷新；`/model info` 读的是当前缓存，不重复探测。
            let refresh_caps =
                (matches!(command, Slash::Model) && !args.is_empty() && args[0] != "info")
                    || matches!(command, Slash::Kernel);
            exit = sync_command(session, app, command, &args);
            if refresh_caps {
                session.refresh_model_capabilities().await;
            }
        }
        Submitted::Skill(name, prompt) => {
            task_turn(
                session,
                app,
                facts,
                term,
                slash::skill_task_prompt(&name, &prompt),
            )
            .await
        }
        Submitted::Task(prompt) => task_turn(session, app, facts, term, prompt).await,
    }
    *facts = refresh_facts(session);
    app.sync_skills(facts);
    Ok(exit.then_some(ExitCode::SUCCESS))
}

/// 走一遍 `slash::dispatch`：输出行进滚动区、命令自己的错误进 `error: {…}` 行。
/// 返回真表示 `/exit`（外层据此收摊）。
fn sync_command(session: &mut Session, app: &mut TuiApp, command: Slash, args: &[String]) -> bool {
    match slash::dispatch(session, command, args) {
        Ok(Dispatch::Lines(lines)) => {
            app.push_lines(lines);
            false
        }
        Ok(Dispatch::Exit) => true,
        // 两个模型往返的形状在 [`classify`] 就被分走了；真到了这里也只是不动作（绝不 panic）
        Ok(Dispatch::Compact) | Ok(Dispatch::Learn(_)) => false,
        Err(error) => {
            app.push_lines([format!("error: {error}")]);
            false
        }
    }
}

/// `/compact`：忙碌子循环里驱动一次压缩（`Session::compact` 本来就收取消令牌，不需要新接缝）。
async fn compact_turn(
    session: &mut Session,
    app: &mut TuiApp,
    facts: &ViewFacts,
    term: Option<&mut TerminalGuard>,
) {
    app.begin_busy(Busy::Compact);
    let cancel = CancellationToken::new();
    let result = match term {
        Some(term) => {
            app.drive(term, facts, session.compact(cancel.clone()), Some(cancel))
                .await
        }
        None => {
            app.drive_headless(facts, session.compact(cancel.clone()), Some(cancel))
                .await
        }
    };
    let state = if result.is_ok() {
        AgentState::Completed
    } else {
        AgentState::Failed
    };
    app.end_busy();
    app.finish_turn(state);
    match result {
        Ok(lines) => app.push_lines(lines),
        Err(error) => app.push_lines([format!("error: {error}")]),
    }
}

/// `/learn [task-id]`：忙碌子循环里驱动草稿生成 + 内联确认（接缝是 [`Session::learn_with`]；
/// 头行/预览/提示词与经典 `/learn` 同一份文案，确认键由 [`TuiApp::confirm_key`] 收）。
///
/// 确认面每轮现接：`confirm::channel()` 的接收端交给应用，发送端经 [`confirm::tui_confirm`] 交给
/// `learn_with`——请求与草稿的 future 在 `drive` 的同一个 `select!` 里推进（不「UI 等 UI」）。
/// 成功逐行进滚动区、错误进 `error: {…}` 行；`end_busy()` 无条件收尾（确认回信只回到忙碌态）。
async fn learn_turn(
    session: &mut Session,
    app: &mut TuiApp,
    facts: &ViewFacts,
    term: Option<&mut TerminalGuard>,
    arg: Option<String>,
) {
    app.begin_busy(Busy::Learn);
    let cancel = CancellationToken::new();
    let (tx, confirms) = confirm::channel();
    app.attach_confirms(confirms);
    let learn = session.learn_with(arg.as_deref(), cancel.clone(), confirm::tui_confirm(tx));
    let result = match term {
        Some(term) => app.drive(term, facts, learn, Some(cancel)).await,
        None => app.drive_headless(facts, learn, Some(cancel)).await,
    };
    let state = if result.is_ok() {
        AgentState::Completed
    } else {
        AgentState::Failed
    };
    app.end_busy();
    app.finish_turn(state);
    match result {
        Ok(lines) => app.push_lines(lines),
        Err(error) => app.push_lines([format!("error: {error}")]),
    }
}

/// 一轮任务：忙碌子循环里驱动 `run_task_with_out`（渲染输出、结束行与错误行都落进同一个捕获面）。
///
/// 取消令牌给 `None`：任务轮次的 Ctrl-C 由 `run_task_with_out` 自己的 watcher 收，经典语义不变。
async fn task_turn(
    session: &mut Session,
    app: &mut TuiApp,
    facts: &ViewFacts,
    term: Option<&mut TerminalGuard>,
    prompt: String,
) {
    app.begin_busy(Busy::Task);
    // 段标：滚动区里明确区分「模型在想」与「结果正文」；正文开始时会再补 `◉ Response`。
    app.push_lines(["\u{1b}[2m◈ Thinking…\u{1b}[0m".to_string()]);
    let out: Box<dyn std::io::Write + Send> = Box::new(app.capture_sink());
    let result = match term {
        Some(term) => {
            app.drive(
                term,
                facts,
                session.run_task_with_out(prompt, TaskKind::Repl, out),
                None,
            )
            .await
        }
        None => {
            app.drive_headless(
                facts,
                session.run_task_with_out(prompt, TaskKind::Repl, out),
                None,
            )
            .await
        }
    };
    app.end_busy();
    let state = match &result {
        Ok(outcome) if outcome.status == RunStatus::Success => AgentState::Completed,
        Ok(_) | Err(_) => AgentState::Failed,
    };
    app.finish_turn(state);
    if let Err(error) = result {
        app.push_lines([format!("error: {error}")]);
    }
}

#[cfg(test)]
mod tests {
    use std::ffi::OsStr;
    use std::path::Path;
    use std::process::ExitCode;
    use std::time::Duration;

    use clap::Parser;
    use ignis_core::config::{ColorChoice, Config, UiConfig};
    use ignis_core::skills::SkillCatalog;
    use ignis_core::task::TaskKind;
    use ignis_runtime::caps::CapabilitySource;
    use ignis_runtime::test_support::{ScriptStep, ScriptedModel};
    use ignis_storage::NewAlert;
    use tokio::sync::mpsc;

    use crate::cli::Cli;
    use crate::commands::Session;
    use crate::commands::fixtures::{
        session_at_home, session_fixture, session_fixture_with_config,
    };
    use crate::commands::kernel::fixtures::TempDir;
    use crate::commands::kernel_note;
    use crate::commands::slash::{self, Slash};
    use crate::editor::history::History;
    use crate::render::ColorDepth;
    use crate::repl::{alerts_banner_line, no_session_note};
    use crate::tui::app::{InputChunk, SELECTOR_HINT, TuiApp};
    use crate::tui::approval::ApprovalJob;
    use crate::tui::capture::CaptureSink;
    use crate::tui::view::{InputBlock, Scrollback, ViewFacts, compose};

    use super::{
        MIN_COLS, MIN_ROWS, Size, Submitted, classify, fallback_note, refresh_facts,
        refresh_facts_with, start_with, startup_lines, startup_lines_with, tui_unavailable_reason,
    };

    /// `/learn` 用例的固定草稿（与 pty / `commands` 的用例同一形状）：合法 frontmatter + 一段正文。
    const DRAFT_MD: &str =
        "---\nname: demo-skill\ndescription: Demo draft.\n---\n# 步骤\n1. 做事\n";

    /// 「够大」的常规终端：表驱动用例的可用基线。
    fn ok_size() -> Size {
        Size { cols: 80, rows: 24 }
    }

    /// 常规终端名：`TERM` 这一格在多数用例里都不是判据。
    fn ok_term() -> Option<&'static OsStr> {
        Some(OsStr::new("xterm-256color"))
    }

    /// 六格判定链的逐字口径：stdin → stdout → `TERM` → 尺寸，一条不过就报它那句。
    #[test]
    fn tui_unavailable_reason_covers_every_guard() {
        // 全绿：可用。
        assert_eq!(
            tui_unavailable_reason(true, true, ok_term(), Some(ok_size())),
            None
        );

        // 第一句：stdin 不是 tty（比 stdout 那条更靠前）。
        assert_eq!(
            tui_unavailable_reason(false, true, ok_term(), Some(ok_size())).as_deref(),
            Some("stdin is not a terminal")
        );
        assert_eq!(
            tui_unavailable_reason(false, false, ok_term(), Some(ok_size())).as_deref(),
            Some("stdin is not a terminal"),
            "两条都不过时先报 stdin 那句（判定顺序即理由链）"
        );

        // 第二句：stdout 不是 tty。
        assert_eq!(
            tui_unavailable_reason(true, false, ok_term(), Some(ok_size())).as_deref(),
            Some("stdout is not a terminal")
        );

        // 第三句：`TERM` 不可用（dumb / 缺失 / 空串都算）。
        for dumb in [Some(OsStr::new("dumb")), None, Some(OsStr::new(""))] {
            assert_eq!(
                tui_unavailable_reason(true, true, dumb, Some(ok_size())).as_deref(),
                Some("TERM is dumb"),
                "{dumb:?} 必须判为不可用"
            );
        }

        // 第四句：尺寸过小（逐字带上实际尺寸）或探不到尺寸。
        for (size, reason) in [
            (Size { cols: 19, rows: 24 }, "terminal too small (19x24)"),
            (Size { cols: 20, rows: 7 }, "terminal too small (20x7)"),
            (Size { cols: 80, rows: 7 }, "terminal too small (80x7)"),
            (Size { cols: 0, rows: 0 }, "terminal too small (0x0)"),
        ] {
            assert_eq!(
                tui_unavailable_reason(true, true, ok_term(), Some(size)).as_deref(),
                Some(reason),
                "{size:?}"
            );
        }
        assert_eq!(
            tui_unavailable_reason(true, true, ok_term(), None).as_deref(),
            Some("terminal too small (0x0)"),
            "探不到尺寸按「太小」处理：降级总是安全的那个方向"
        );

        // 边界属于可用一侧：恰好 20×8 放行。
        assert_eq!(
            tui_unavailable_reason(true, true, ok_term(), Some(Size { cols: 20, rows: 8 })),
            None
        );
        assert_eq!((MIN_COLS, MIN_ROWS), (20, 8), "最小尺寸常量逐字钉住");
    }

    /// 降级 note 逐字（英文 chrome；Task 7 的 pty 用例按它 expect）。
    #[test]
    fn fallback_note_is_pinned() {
        assert_eq!(
            fallback_note("TERM is dumb"),
            "note: TUI unavailable (TERM is dumb); falling back to the classic REPL"
        );
        assert_eq!(
            fallback_note("terminal too small (10x4)"),
            "note: TUI unavailable (terminal too small (10x4)); falling back to the classic REPL"
        );
    }

    /// `Size` 是 `Copy + Eq + Debug`：帧合成按值传它，用例直接比较。
    #[test]
    fn size_is_copy_eq_and_debug() {
        fn round_trip<T: Copy + Eq + std::fmt::Debug>(value: T) -> T {
            value
        }
        let size = Size { cols: 80, rows: 24 };
        assert_eq!(round_trip(size), size, "Copy/Eq/Debug 是接口契约：{size:?}");
        assert_ne!(
            Size { cols: 80, rows: 24 },
            Size { cols: 80, rows: 25 },
            "行数参与相等判定"
        );
    }

    // ---- 循环级用例（headless） ----

    /// 无终端的应用夹具：空滚动区 + 内存历史 + 独立捕获缓冲。
    fn headless_app() -> TuiApp {
        let (_, buffer) = CaptureSink::pair();
        TuiApp::new(Scrollback::new(), History::in_memory(), buffer)
    }

    /// 滚动区里看得见的行（视口就是「最近 `rows` 行」）。
    fn viewport(app: &TuiApp, facts: &ViewFacts) -> Vec<String> {
        app.view(facts).viewport
    }

    /// 用例助手：把「提交一行 + 处理动作」跑成一次调用（不碰终端；动作处理与主循环同一份）。
    async fn submit(
        session: &mut Session,
        app: &mut TuiApp,
        facts: &mut ViewFacts,
        line: &str,
    ) -> anyhow::Result<Option<ExitCode>> {
        let mut bytes = line.as_bytes().to_vec();
        bytes.push(b'\r');
        let actions = app.on_bytes(&bytes);
        super::handle_actions(session, app, facts, None, actions).await
    }

    /// 一条已提交输入的本地分类：命令、技能、任务与未知命令各走各的路。
    #[test]
    fn classify_routes_commands_skills_tasks_and_unknown() {
        let skills = SkillCatalog::load(Path::new("/nonexistent/ignis-tui-classify-skills"));
        let cases = [
            ("", Submitted::Empty),
            ("  ", Submitted::Empty),
            ("/nope", Submitted::Unknown("nope".to_string())),
            (
                "/server-health check disk",
                Submitted::Skill("server-health".to_string(), "check disk".to_string()),
            ),
            ("/exit", Submitted::Exit),
            ("/sessions", Submitted::Sessions),
            ("/compact", Submitted::Compact),
            ("/learn", Submitted::Learn(None)),
            (
                "/learn 019a1b2c-3d4e-7f00-8000-000000000001",
                Submitted::Learn(Some("019a1b2c-3d4e-7f00-8000-000000000001".to_string())),
            ),
            (
                "/kernel codex",
                Submitted::Command(Slash::Kernel, vec!["codex".to_string()]),
            ),
            ("你好", Submitted::Task("你好".to_string())),
            (
                "第一行\n第二行",
                Submitted::Task("第一行\n第二行".to_string()),
                // 多行文本原样进任务（内部换行保留、只裁首尾空白）
            ),
        ];
        for (line, want) in cases {
            assert_eq!(classify(line, &skills), want, "{line:?}");
        }
    }

    /// 事实快照：版本号跟包版本联动、会话标签与 `/status` 同源，颜色只看 `NO_COLOR` 那一处约定。
    #[test]
    fn refresh_facts_reads_the_session_and_the_no_color_convention() {
        let (_dir, session) = session_fixture("tui-facts", "native");
        let facts = refresh_facts_with(&session, Some("1"), None, Some("xterm-256color"));

        assert_eq!(facts.version, env!("CARGO_PKG_VERSION"));
        assert_eq!(facts.workspace, session.workspace().display().to_string());
        assert_eq!(facts.kernel, "native");
        assert_eq!(
            facts.session,
            slash::session_label(session.current_session())
        );
        assert_eq!(facts.mode, session.output_mode());
        assert_eq!(facts.budget, session.context_budget());
        assert_eq!(facts.history_messages, session.history().len());
        assert_eq!(facts.stats, session.last_prompt_stats().copied());
        assert_eq!(facts.usage, session.last_usage().copied());
        assert_eq!(facts.model, session.model());
        assert_eq!(facts.provider, "OpenAI");
        assert_eq!(facts.capabilities.context_window, Some(128_000));
        assert_eq!(
            facts.context.map(|context| context.segment(10)).as_deref(),
            Some("ctx -/128K [----------] --"),
            "首帧已知窗口，没有用量时显示 `-` 而不是假 0"
        );
        assert_eq!(facts.tools.len(), 10, "6 内置 + 4 元工具");
        assert_eq!(facts.skills.len(), 3, "内置技能目录");
        assert_eq!(facts.memory, Some(0), "空记忆库");
        assert!(facts.native, "native 内核");
        assert!(!facts.color, "`NO_COLOR=1`（非空）恒无色");
        assert_eq!(facts.depth, ColorDepth::None, "不上色时色阶恒 None");
        let colored_facts = refresh_facts_with(&session, None, None, Some("xterm-256color"));
        assert!(colored_facts.color, "tty + auto → 上色");
        assert_eq!(colored_facts.depth, ColorDepth::Ansi256, "256 色按 TERM 判");

        // `ui.color = Always`：没有 `NO_COLOR` 时上色；`NO_COLOR` 非空时照样无色
        let (_dir, always) = session_fixture_with_config(
            "tui-facts-always",
            "native",
            Config {
                ui: UiConfig {
                    color: ColorChoice::Always,
                    ..UiConfig::default()
                },
                ..Config::default()
            },
        );
        let truecolor = refresh_facts_with(&always, None, Some("truecolor"), Some("xterm"));
        assert!(truecolor.color);
        assert_eq!(truecolor.depth, ColorDepth::True, "COLORTERM 的真彩优先");
        let no_color = refresh_facts_with(&always, Some("1"), Some("truecolor"), Some("xterm"));
        assert!(!no_color.color);
        assert_eq!(no_color.depth, ColorDepth::None, "`NO_COLOR` 压在色阶之上");

        // `ui.color = never`：tty + 没有 NO_COLOR 也恒无色 → 帧里零 SGR
        let (_dir, never) = session_fixture_with_config(
            "tui-facts-never",
            "native",
            Config {
                ui: UiConfig {
                    color: ColorChoice::Never,
                    ..UiConfig::default()
                },
                ..Config::default()
            },
        );
        let plain = refresh_facts_with(&never, None, Some("truecolor"), Some("xterm"));
        assert!(!plain.color, "`never` 恒无色");
        assert_eq!(plain.depth, ColorDepth::None, "`never` 的色阶同样是 None");
        let mut app = headless_app();
        app.push_lines(["答复一".to_string()]);
        let frame = compose(&app.view(&plain));
        for sgr in ["\u{1b}[0m", "\u{1b}[1m", "\u{1b}[2m", "\u{1b}[90m"] {
            assert!(!frame.contains(sgr), "无色帧里不许有 {sgr:?}：{frame:?}");
        }
        // 同一屏上色时确实带 SGR：证明上面那条判据不是空转
        let colored = compose(&app.view(&ViewFacts {
            color: true,
            ..plain
        }));
        assert!(colored.contains("\u{1b}[2m"), "{colored:?}");

        // 不带注入的入口读进程环境（这里只钉它不 panic、且与注入变体同形）
        assert_eq!(refresh_facts(&session).version, env!("CARGO_PKG_VERSION"));
    }

    /// `-c` 的可见历史：用户回显与 assistant Markdown 都要重放，且无色现场零 SGR。
    #[test]
    fn resumed_transcript_replays_user_and_assistant_messages() {
        let (_dir, mut session) = session_fixture("tui-transcript", "native");
        let id = session
            .store()
            .create_session(session.workspace(), "native", None, "问一句")
            .expect("创建会话")
            .id;
        session
            .store()
            .append_turn(&id, "问一句", Some("答一句"))
            .expect("追加轮次");
        session.resume_session(&id).expect("恢复会话");

        let facts = refresh_facts_with(&session, Some("1"), None, Some("xterm-256color"));
        let lines = super::resumed_transcript_lines(&session, &facts, 80);
        let joined = lines.join("\n");
        assert!(joined.contains("resumed session"), "{joined}");
        assert!(joined.contains("❯ 问一句"), "{joined}");
        assert!(joined.contains("答一句"), "{joined}");
        assert!(
            !lines.iter().any(|line| line.contains('\u{1b}')),
            "无色现场零 SGR：{joined}"
        );
    }

    /// `/model <name>`：切换后能力解析与状态栏窗口必须跟着换，不能沿用旧模型的缓存。
    #[tokio::test]
    async fn model_switch_refreshes_capabilities_and_facts() {
        let (_dir, mut session) = session_fixture("tui-model-switch", "native");
        let mut app = headless_app();
        let mut facts = refresh_facts(&session);
        assert_eq!(facts.model, "gpt-4o-mini");
        assert_eq!(facts.capabilities.context_window, Some(128_000));
        assert_eq!(
            facts.context.map(|context| context.segment(10)).as_deref(),
            Some("ctx -/128K [----------] --")
        );

        submit(&mut session, &mut app, &mut facts, "/model gpt-4.1")
            .await
            .unwrap();

        assert_eq!(session.model(), "gpt-4.1");
        assert_eq!(facts.model, "gpt-4.1");
        assert_eq!(facts.capabilities.context_window, Some(1_047_576));
        assert_eq!(facts.capabilities.source, CapabilitySource::Registry);
        assert_eq!(
            facts.context.map(|context| context.segment(10)).as_deref(),
            Some("ctx -/1M [----------] --"),
            "切换模型后窗口跟着换，不能沿用旧模型的 128K"
        );
    }

    /// 同步命令：回显与命令的输出行都进滚动区，动作之后会话事实跟着刷新；`/exit` 交回退出码 0。
    #[tokio::test]
    async fn sync_commands_write_their_lines_into_the_scrollback() {
        let (_dir, mut session) = session_fixture("tui-sync", "native");
        let mut app = headless_app();
        let mut facts = refresh_facts(&session);

        let code = submit(&mut session, &mut app, &mut facts, "/kernel")
            .await
            .unwrap();
        assert_eq!(code, None);
        let lines = viewport(&app, &facts);
        assert!(lines.contains(&"❯ /kernel".to_string()), "回显：{lines:?}");
        assert!(lines.contains(&"kernel: native".to_string()), "{lines:?}");

        submit(&mut session, &mut app, &mut facts, "/details minimal")
            .await
            .unwrap();
        let lines = viewport(&app, &facts);
        assert!(
            lines.contains(&"output mode: minimal".to_string()),
            "{lines:?}"
        );
        assert_eq!(facts.mode, session.output_mode(), "动作之后刷新会话事实");

        // `/status` 的每一行都落地（与经典同一份文案）
        let expected = slash::status_lines(
            session.kernel(),
            session.model(),
            session.workspace(),
            session.current_session(),
            session.unread_alerts(),
            session.last_usage(),
        );
        submit(&mut session, &mut app, &mut facts, "/status")
            .await
            .unwrap();
        let lines = viewport(&app, &facts);
        for line in &expected {
            assert!(lines.contains(line), "{line:?} 没落地：{lines:?}");
        }

        // 未知命令：本地报错行，绝不送模型（这里只钉它落在滚动区）
        submit(&mut session, &mut app, &mut facts, "/nope")
            .await
            .unwrap();
        let lines = viewport(&app, &facts);
        assert!(
            lines.contains(&"Unknown command: /nope — /help lists commands".to_string()),
            "{lines:?}"
        );

        let code = submit(&mut session, &mut app, &mut facts, "/exit")
            .await
            .unwrap();
        assert_eq!(code, Some(ExitCode::SUCCESS));
    }

    /// `/sessions` 现在开全屏选择器（T3 的经典表格分支已删）：输入态是选择器、视口逐行是
    /// `slash::sessions_lines` 的内容加 `> `/`  ` 前缀，滚动区里**不再**追一行表格。
    #[tokio::test]
    async fn sessions_command_opens_the_selector_instead_of_printing_the_table() {
        let (_dir, mut session) = session_fixture("tui-sessions", "native");
        // 先造两条会话行：选择器里才有表头之外的内容
        session
            .store()
            .create_session(session.workspace(), "native", None, "第一句")
            .unwrap();
        session
            .store()
            .create_session(session.workspace(), "native", None, "第二句")
            .unwrap();
        let mut app = headless_app();
        let mut facts = refresh_facts(&session);

        submit(&mut session, &mut app, &mut facts, "/sessions")
            .await
            .unwrap();

        // 输入态是选择器：没有光标、提示行换那一句
        let view = app.view(&facts);
        assert_eq!(
            view.input,
            InputBlock {
                lines: Vec::new(),
                hint: SELECTOR_HINT.to_string(),
                cursor: None,
            },
            "选择器不落光标、提示行是选择器那一句"
        );

        // 视口逐行是 `sessions_lines` 的内容加 `> `/`  ` 前缀（当前会话还没绑：没有 `*`）
        let rows = session.sessions().unwrap();
        let table = slash::sessions_lines(&rows, session.current_session());
        assert_eq!(view.viewport.len(), table.len(), "选择器列表就是那张表");
        assert!(view.viewport[0].starts_with("  ID"), "{:?}", view.viewport);
        for (line, base) in view.viewport.iter().zip(&table) {
            assert!(
                line == &format!("  {base}") || line == &format!("> {base}"),
                "{line:?} 不是前缀化的 {base:?}"
            );
        }

        // 关掉选择器：滚动区里只有提交回显，**没有**追进来的表格
        assert!(
            app.on_bytes(b"\x1b").is_empty(),
            "裸 ESC 先留在解码器缓存里"
        );
        assert!(app.on_tick(), "tick 上的裸 ESC 关掉选择器");
        let lines = viewport(&app, &facts);
        assert!(lines.contains(&"❯ /sessions".to_string()), "{lines:?}");
        assert!(
            !lines.iter().any(|line| line.contains("MESSAGES")),
            "不再往滚动区追一行表格：{lines:?}"
        );
    }

    /// `/compact` 在忙碌子循环里被驱动（T3 就完整支持）：空会话给一行 `error: …`，回到空闲态。
    #[tokio::test]
    async fn compact_is_driven_in_the_busy_loop() {
        let (_dir, mut session) = session_fixture("tui-compact", "native");
        let mut app = headless_app();
        let mut facts = refresh_facts(&session);

        let code = submit(&mut session, &mut app, &mut facts, "/compact")
            .await
            .unwrap();
        assert_eq!(code, None);
        assert_eq!(app.busy(), None, "轮次结束必须回到空闲态");

        let lines = viewport(&app, &facts);
        assert!(
            lines.contains(&"error: compact: no session yet".to_string()),
            "{lines:?}"
        );
    }

    /// `/learn` 现在走确认接缝（T3 的临时错误行已删）：空会话给经典的人话错误，轮次结束回到空闲态。
    #[tokio::test]
    async fn learn_is_driven_in_the_busy_loop_and_reports_the_classic_error() {
        let (_dir, mut session) = session_fixture("tui-learn-error", "native");
        let mut app = headless_app();
        let mut facts = refresh_facts(&session);

        let code = submit(&mut session, &mut app, &mut facts, "/learn")
            .await
            .unwrap();
        assert_eq!(code, None);
        assert_eq!(app.busy(), None, "轮次结束必须回到空闲态");

        let lines = viewport(&app, &facts);
        assert!(
            lines.contains(&"error: learn: no completed task to learn from".to_string()),
            "{lines:?}"
        );
        assert!(
            !lines
                .iter()
                .any(|line| line.contains("not wired in the TUI yet")),
            "T3 的临时错误行必须已被接缝替换：{lines:?}"
        );
    }

    /// `/learn` 的完整往返（脚本化模型 + 内联确认）：草稿生成 → 确认态（头行/预览进滚动区）→ `s`
    /// 落盘 → 滚动区里两行回显、文件在盘上；轮次结束回到空闲态。
    #[tokio::test]
    async fn learn_turn_confirms_inline_and_saves_the_draft() {
        let model = ScriptedModel::start(vec![
            ScriptStep::Text("答复一".into()),
            ScriptStep::Text(DRAFT_MD.into()),
        ]);
        let config = Config {
            api_key: Some("test-key".into()),
            model: ignis_runtime::ModelConfig {
                base_url: model.base_url(),
                model: "test-model".into(),
                ..ignis_runtime::ModelConfig::default()
            },
            ..Config::default()
        };
        let (dir, mut session) = session_fixture_with_config("tui-learn-confirm", "native", config);
        session
            .run_task("你好".into(), TaskKind::Repl)
            .await
            .expect("先跑一轮成功任务（`/learn` 的目标）");

        let mut app = headless_app();
        let (chunks_tx, chunks) = mpsc::unbounded_channel();
        app.attach_chunks(chunks);
        let mut facts = refresh_facts(&session);

        // 确认键重试发送：草稿生成完成之前到达的 `s` 会被忙碌期丢掉（输入禁用），落在确认态上的
        // 那一次才会被收下——不依赖具体时序，也不会把按键误当提交（确认态自成一个按键盘面）。
        let bytes = chunks_tx.clone();
        let sender = tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_millis(10)).await;
                if bytes.send(InputChunk::Bytes(b"s".to_vec())).is_err() {
                    return;
                }
            }
        });

        let code = tokio::time::timeout(
            Duration::from_secs(10),
            submit(&mut session, &mut app, &mut facts, "/learn"),
        )
        .await
        .expect("确认面必须在一个合理的时限内落定")
        .unwrap();
        sender.abort();
        assert_eq!(code, None);
        assert_eq!(app.busy(), None, "确认 + 落盘 + reload 之后回到空闲态");

        let lines = viewport(&app, &facts);
        assert!(
            lines.contains(&"saved skill demo-skill".to_string()),
            "{lines:?}"
        );
        assert!(
            lines.contains(&"reloaded skills (4)".to_string()),
            "{lines:?}"
        );
        let path = dir
            .path()
            .join("skills")
            .join("demo-skill")
            .join("SKILL.md");
        assert!(path.is_file(), "{}", path.display());
    }

    // ---- 入口与降级（T6） ----

    /// `--tui` 不可用：一行 note + `Ok(None)`，且**不建会话**（临时 home 上不会出现 `ignis.db`）。
    ///
    /// 可用性探针与会话工厂都是注入的（生产入口 [`super::start`] 传的是进程现场与真实工厂，与
    /// 这条用例走同一条路径）：工厂一旦被调用，会话就在临时 home 上建出来——判据文件会现身。
    #[tokio::test]
    async fn start_falls_back_without_touching_the_session() {
        let dir = TempDir::new("tui-start-fallback");
        let cli = Cli::try_parse_from(["ignis", "--tui"]).expect("--tui 应可解析");

        for (stdin_tty, stdout_tty, term, size) in [
            // 四条硬条件各来一遍：一条不过就 `Ok(None)`，后面的不再看
            (true, true, Some(OsStr::new("dumb")), Some(ok_size())),
            (false, true, ok_term(), Some(ok_size())),
            (true, false, ok_term(), Some(ok_size())),
            (true, true, ok_term(), Some(Size { cols: 10, rows: 4 })),
        ] {
            let build = || -> anyhow::Result<(Session, mpsc::UnboundedReceiver<ApprovalJob>)> {
                let approvals = mpsc::unbounded_channel().1;
                Ok((
                    session_at_home(
                        dir.path(),
                        "native",
                        Config {
                            api_key: Some("test-key".into()),
                            ..Config::default()
                        },
                    ),
                    approvals,
                ))
            };
            let outcome = start_with(&cli, stdin_tty, stdout_tty, term, size, build).await;
            assert_eq!(
                outcome.expect("不可用不是错误"),
                None,
                "{stdin_tty}/{stdout_tty}/{term:?}/{size:?}：不可用 = Ok(None)，调用方落回经典路径"
            );
        }

        assert!(
            !dir.path().join("ignis.db").exists(),
            "不可用时不建会话：home 下没有新建 ignis.db"
        );
    }

    /// 开机行（进滚动区）：`-c` 的 note → 内核能力提示（非 native 才有）→ 未读告警行（有才有）。
    ///
    /// 品牌横幅由 TUI 的实时欢迎面板合成（可随 Resize 重排），**不再**写进 scrollback；
    /// `--no-banner` 只关欢迎面板，不改变开机行。
    #[test]
    fn tui_startup_lines_skip_the_brand_block() {
        let (_dir, codex) = session_fixture("tui-startup", "codex");
        let lines = startup_lines_with(
            &codex,
            Some(no_session_note()),
            200,
            false,
            None,
            Some("xterm-256color"),
            Some("1"),
        );
        assert_eq!(
            lines,
            [
                no_session_note().to_string(),
                kernel_note("codex").expect("非 native 有能力提示"),
            ],
            "只剩 note 与内核提示"
        );
        assert!(
            !lines.iter().any(|line| line.contains("IGNIS v")),
            "品牌横幅不写 scrollback：{lines:?}"
        );
        assert!(
            !lines.iter().any(|line| line == "● Ready"),
            "经典状态块不进 TUI：{lines:?}"
        );
        assert!(
            !lines.iter().any(|line| line.contains("unread")),
            "0 条未读：没有告警行：{lines:?}"
        );

        // `--no-banner` 只影响欢迎面板，不改变开机行。
        assert_eq!(
            startup_lines_with(
                &codex,
                Some(no_session_note()),
                200,
                true,
                None,
                Some("xterm-256color"),
                Some("1"),
            ),
            lines
        );

        // 上色现场也不会凭空多出品牌块（欢迎面板自己上色）。
        let colored = startup_lines_with(
            &codex,
            None,
            200,
            false,
            Some("truecolor"),
            Some("xterm-256color"),
            None,
        );
        assert_eq!(
            colored,
            [kernel_note("codex").expect("非 native 有能力提示")]
        );
        assert!(!colored.iter().any(|line| line.contains('\u{1b}')));

        // 未读告警：告警行就是 `alerts_banner_line` 那一份（排在最后）
        let (_dir, alerting) = session_fixture("tui-startup-alerts", "native");
        for index in 0..2 {
            alerting
                .db()
                .insert_alert(&NewAlert {
                    severity: "info".into(),
                    title: format!("告警 {index}"),
                    body: None,
                    source_job_id: None,
                    source_task_id: None,
                    dedup_key: format!("tui-startup-{index}"),
                })
                .expect("造两条未读告警");
        }
        assert_eq!(
            startup_lines_with(
                &alerting,
                None,
                80,
                true,
                None,
                Some("xterm-256color"),
                None
            ),
            [alerts_banner_line(Some(2)).expect("有未读就有告警行")]
        );

        // native 且 `-c` 命中：所有 note 都不出现
        let (_dir, native) = session_fixture("tui-startup-native", "native");
        assert_eq!(
            startup_lines_with(&native, None, 80, true, None, Some("xterm-256color"), None),
            Vec::<String>::new()
        );
        assert_eq!(
            startup_lines(&native, None, 80, false),
            Vec::<String>::new()
        );
    }
}
