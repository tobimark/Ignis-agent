//! TUI 的应用状态：滚动区、输入态、忙碌子循环与帧组装。
//!
//! 这一层**不持有会话**：任务/压缩/学习的 future 借用 `&mut Session` 期间，界面状态必须还能被
//! 驱动（排空捕获、重绘、缩放、接输入），所以会话只在帧组装时以 [`ViewFacts`] 快照的形式出现
//! ——忙碌循环里一个字节都不碰会话（借用冲突与「UI 等 UI」的死锁都从这里排除）。
//!
//! 空闲期与忙碌期是同一套状态机的两面：
//! - 空闲期由主循环（[`super::run`]）等输入块/信号/tick，把 [`TuiApp::on_bytes`] 的动作拿出去执行；
//! - 忙碌期由 [`TuiApp::drive`] 在同一个 `select!` 里推进 future、每 100ms 排空捕获并重绘、
//!   接 SIGINT/SIGWINCH/输入块。SIGINT 不结束忙碌循环：任务轮次的取消在 `run_task_with_out`
//!   自己的 watcher 里，`/compact` `/learn` 由 TUI 持有的令牌收。
//!
//! 审批（T4）是叠在忙碌期上的一态：`drive` 的同一个 `select!` 里收 [`ApprovalJob`]（请求与任务
//! future 一起推进，不「UI 等 UI」），4 行提示进滚动区、输入区首行换成经典的 `choice> `，按键由
//! [`TuiApp::approval_key`] 收——**绝不进编辑器**。
//!
//! `/learn` 的确认（T5）与审批同形：`ConfirmJob` 在同一个 `select!` 里收，头行/预览进滚动区、
//! 输入区首行是经典的确认提示词，按键由 [`TuiApp::confirm_key`] 收（同样绝不进编辑器）。

use std::future::Future;
use std::io::Read;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use ignis_runtime::{ApprovalDecision, ApprovalRequest};
use ignis_storage::SessionSummary;
use tokio::signal::unix::{Signal, SignalKind, signal};
use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;

use crate::approval::{Choice, MAX_UNKNOWN_INPUTS, details_json, parse_choice, prompt_lines};
use crate::commands::learn::{LEARN_PROMPT, asks_save, draft_header, preview_lines};
use crate::editor::history::History;
use crate::editor::keys::Key;
use crate::editor::reader::INVALID_WARNING;
use crate::render::width::display_width;
use crate::tui::Size;
use crate::tui::approval::ApprovalJob;
use crate::tui::capture::{self, CaptureSink};
use crate::tui::confirm::ConfirmJob;
use crate::tui::input::InputState;
use crate::tui::term;
use crate::tui::view::{
    FIXED_ROWS, InputBlock, Scrollback, TuiView, ViewFacts, compose, selector_lines, status_line,
    top_line,
};

/// 100ms 的心跳：忙碌期每 tick 排空一次捕获（流式输出落进滚动区），空闲期用它做 Esc 消歧。
const TICK: Duration = Duration::from_millis(100);

/// 空闲期的提示行（底部状态行；T6 的滚动口径与它同一句）。
pub(crate) const IDLE_HINT: &str = "PgUp/PgDn scroll";

/// 忙碌期的提示行（输入区第 3 行 + 底部状态行）。
pub(crate) const BUSY_HINT: &str = "Ctrl+C cancels";

/// 忙碌期输入区第 1 行的说明（chrome 里唯一一句「正在进行」）。
const WORKING: &str = "working…";

/// 审批态输入区第 1 行的提示词（逐字与经典审批提示词的最后一段一致）：4 行提示放不下 3 行的
/// 输入区，正文在滚动区，输入区只留经典的提示词。
const CHOICE_PROMPT: &str = "choice> ";

/// 审批态输入区第 3 行的提示（四个可选按键；`d` 先看明细再问一轮）。
const APPROVAL_HINT: &str = "a/s/r/d";

/// 确认态输入区第 3 行的提示（只有 `s` 保存；其余按键与 `d` 一样放弃）。
const CONFIRM_HINT: &str = "s/d";

/// 选择器输入区第 3 行与底部状态行的提示（↑↓ 移动、Enter 装载、Esc 关闭）。
pub(crate) const SELECTOR_HINT: &str = "↑↓ / Enter / Esc";

/// 进入主循环前的默认面板尺寸（守卫探到尺寸之前用；与 `term` 的回退 80×24 同值）。
const INITIAL_SIZE: Size = Size { cols: 80, rows: 24 };

/// 一次 `read` 最多收多少字节（够装一整段粘贴，又不至于让重绘攒太久）。
const READ_CHUNK: usize = 256;

/// 一次输入处理的结果（顺序即处理顺序；`Submit`/`Exit` 之后这一批的剩余按键忽略——忙碌期
/// 输入禁用是同一条规则）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    /// 这个按键没有动作（空操作、光标到头、没有候选的 Tab）。
    None,
    /// 缓冲或提示变了：按新状态重绘一帧。
    Redraw,
    /// 提交：整段文本（内部换行保留），缓冲已清空。
    Submit(String),
    /// 选择器选中一条会话：整条 id 交回外层执行（`Session::resume_session`），选择器已关闭。
    Resume(String),
    /// 退出：空缓冲 `Ctrl+D`、`Ctrl+C` 字节与空闲期 SIGINT 都走这条。
    Exit,
}

/// 忙碌的原因：任务轮次、压缩与学习各一档（提示行文案暂时共用同一句）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Busy {
    /// 一次任务轮次（`run_task_with_out`）。
    Task,
    /// `/compact`：摘要往返。
    Compact,
    /// `/learn`：草稿生成 + 内联确认。
    Learn,
}

/// 输入模式：T3 只有编辑器与忙碌两态；T4/T5/T6 各自**追加**一个变体（`Approval`/`Confirm`/
/// `Selector`）。审批/确认态带 [`oneshot::Sender`]，所以这里只派生 `Debug`（不再有 `Clone`/`Eq`）。
#[derive(Debug)]
pub enum InputMode {
    /// 编辑器：按键进 [`InputState`]。
    Editor,
    /// 忙碌：输入禁用，只等 future 结束。
    Busy(Busy),
    /// 审批：4 行提示已进滚动区，按键由 [`TuiApp::approval_key`] 收（**不进编辑器**）。
    Approval {
        /// 待审批的请求（`d` 打它的参数明细）。
        request: ApprovalRequest,
        /// 回信口（`None` = 已经回信）。
        reply: Option<oneshot::Sender<ApprovalDecision>>,
        /// 连续无效输入的计数（到 `MAX_UNKNOWN_INPUTS` 即 fail-safe 拒绝）。
        unknown: usize,
        /// 进入审批时记下的忙碌态：回信后恢复它——审批叠在忙碌期上（任务的 future 还在跑），
        /// 回信**不是**回编辑器的理由。
        resume: Busy,
    },
    /// 确认：`/learn` 的头行与预览已进滚动区，输入区首行是经典提示词，按键由
    /// [`TuiApp::confirm_key`] 收（**不进编辑器**）。
    Confirm {
        /// 回信口（`None` = 已经回信）。
        reply: Option<oneshot::Sender<bool>>,
        /// 进入确认时记下的忙碌态：回信后恢复它——确认叠在 `/learn` 的忙碌期上（草稿的 future
        /// 还在跑），回信**不是**回编辑器的理由。
        resume: Busy,
    },
    /// 选择器（`/sessions`）：列表进视口、按键由 [`TuiApp::selector_key`] 收（**不进编辑器**）。
    /// 只在空闲期开（忙碌期输入禁用，根本到不了这一步），所以没有「回哪个忙碌态」的问题。
    Selector {
        /// 本工作区的会话行（新近在前；与 `/sessions` 同一份）。
        rows: Vec<SessionSummary>,
        /// 选中行在 `rows` 里的 0 基下标（空表时是 0，没有可选项）。
        cursor: usize,
        /// 当前会话的整条 id（列表里带 `*`）。
        current: Option<String>,
    },
}

/// 进终端守卫之前就注册好的两个信号流（F4）。
///
/// 注册只该有这一次：`TerminalGuard::enter()` 与主循环之间还夹着建会话、`-c` 装载与起输入线程，
/// 这一段启动窗口里的 Ctrl+C 必须由主循环接住，而不是按默认处置把进程带走（备用屏与 raw mode
/// 会留在那里）；流本身由 [`TuiApp::attach_signals`] 接进应用。
pub struct Signals {
    /// SIGINT 流（注册失败 = `None`：按永不触发的信号源挂起，不 panic）。
    pub sigint: Option<Signal>,
    /// SIGWINCH 流（注册失败同上）。
    pub sigwinch: Option<Signal>,
}

impl Signals {
    /// 注册 SIGINT 与 SIGWINCH：调用方必须在 [`term::TerminalGuard::enter`] **之前**调它。
    pub fn install() -> Self {
        Self {
            sigint: signal(SignalKind::interrupt()).ok(),
            sigwinch: signal(SignalKind::window_change()).ok(),
        }
    }
}

/// TUI 的应用状态：不持有会话（future 借用 `&mut Session` 期间的借用冲突规避）。
pub struct TuiApp {
    /// 有界滚动区：捕获文本的落点。
    scrollback: Scrollback,
    /// 编辑器输入态（按键、缓冲、提示行）。
    input: InputState,
    /// 输入模式。
    mode: InputMode,
    /// 渲染器的输出缓冲（`CaptureSink` 写它，每 tick 排空）。
    capture: Arc<Mutex<Vec<u8>>>,
    /// 面板尺寸（主循环进入时与 SIGWINCH 之后刷新）。
    size: Size,
    /// 输入读取线程交回的一批批字节（空闲与忙碌两个循环共用）。
    chunks: Option<mpsc::UnboundedReceiver<InputChunk>>,
    /// 审批处理器交回的请求（`drive` 的审批分支从这里收；主循环进入时接上）。
    approvals: Option<mpsc::UnboundedReceiver<ApprovalJob>>,
    /// `/learn` 确认面交回的请求（`drive` 的确认分支从这里收；每轮 `/learn` 接上一次）。
    confirms: Option<mpsc::UnboundedReceiver<ConfirmJob>>,
    /// 本轮的取消令牌（`drive` 进入时挂上、退出时摘掉）：审批态的 `Ctrl+C` 字节与 SIGINT 同义，
    /// 靠它取消本轮任务（`/compact` `/learn` 的令牌；任务轮次给 `None`，取消在 `run_task` 内部）。
    cancel: Option<CancellationToken>,
    /// 信号流（SIGINT/SIGWINCH）：生产路径由 `tui::start_with` 在进守卫**之前**注册并接上
    /// （[`TuiApp::attach_signals`]）；没接上的应用（用例的 headless 夹具）在第一次用到时现场装
    /// 一份——`Some` 之后绝不再注册第二份。
    signals: Option<Signals>,
}

impl TuiApp {
    /// 装配应用状态：滚动区、历史、渲染器的捕获缓冲（`CaptureSink` 与这里共持一个 `Arc`）。
    pub fn new(scrollback: Scrollback, history: History, capture: Arc<Mutex<Vec<u8>>>) -> Self {
        Self {
            scrollback,
            input: InputState::new(history),
            mode: InputMode::Editor,
            capture,
            size: INITIAL_SIZE,
            chunks: None,
            approvals: None,
            confirms: None,
            cancel: None,
            signals: None,
        }
    }

    /// 接上启动时（进守卫之前）注册好的信号流：这是生产路径唯一的一次注册（F4）。
    pub(crate) fn attach_signals(&mut self, signals: Signals) {
        self.signals = Some(signals);
    }

    /// 接上输入读取线程的接收端（主循环进入时调一次）。
    pub(crate) fn attach_chunks(&mut self, chunks: mpsc::UnboundedReceiver<InputChunk>) {
        self.chunks = Some(chunks);
    }

    /// 接上审批处理器交回请求的接收端（主循环进入时调一次；没接上时 `drive` 的审批分支永远
    /// 挂起——经典路径与不提问的任务不受影响）。
    pub fn attach_approvals(&mut self, approvals: mpsc::UnboundedReceiver<ApprovalJob>) {
        self.approvals = Some(approvals);
    }

    /// 接上 `/learn` 确认面交回请求的接收端（每轮 `/learn` 接一次；没接上时 `drive` 的确认分支
    /// 永远挂起——不走 `/learn` 的轮次不受影响）。
    pub fn attach_confirms(&mut self, confirms: mpsc::UnboundedReceiver<ConfirmJob>) {
        self.confirms = Some(confirms);
    }

    /// 面板尺寸（主循环进入时与 SIGWINCH 之后刷新）。
    pub(crate) fn set_size(&mut self, size: Size) {
        self.size = size;
    }

    /// 渲染器的落点：与应用的滚动区共持一个缓冲（TUI 的单一捕获面）。
    pub fn capture_sink(&self) -> CaptureSink {
        CaptureSink::new(Arc::clone(&self.capture))
    }

    /// 喂一段原始字节；返回要外层执行的动作。
    ///
    /// 忙碌期一律没有动作（输入禁用）：字节照吃（别留给空闲期），缓冲一个字符都不进。
    /// 审批/确认/选择器期的按键分别走 [`TuiApp::approval_key`] / [`TuiApp::confirm_key`] /
    /// [`TuiApp::selector_key`]（**绝不进**编辑器）。
    ///
    /// 编辑器态里 `PageUp`/`PageDown` 额外翻一屏滚动区（步长 = 可视视口行数，见
    /// [`content_rows`]）：编辑器状态机对它们是空操作（经典口径），滚动因此要在这里落地并补一次
    /// 重绘。
    pub fn on_bytes(&mut self, bytes: &[u8]) -> Vec<Action> {
        if matches!(self.mode, InputMode::Approval { .. }) {
            return self.approval_bytes(bytes);
        }
        if matches!(self.mode, InputMode::Confirm { .. }) {
            return self.confirm_bytes(bytes);
        }
        if matches!(self.mode, InputMode::Selector { .. }) {
            return self.selector_bytes(bytes);
        }
        if !matches!(self.mode, InputMode::Editor) {
            return Vec::new();
        }
        let step = content_rows(self.size);
        let mut actions = Vec::new();
        for (key, action) in self.input.on_bytes_with_keys(bytes) {
            let scrolled = match key {
                Key::PageUp => {
                    self.scrollback.scroll_up(step);
                    true
                }
                Key::PageDown => {
                    self.scrollback.scroll_down(step);
                    true
                }
                _ => false,
            };
            // 编辑器对翻页键是空操作（`Idle`）：滚动本身要一帧才看得见
            actions.push(match (scrolled, action) {
                (true, Action::None) => Action::Redraw,
                (_, action) => action,
            });
        }
        // 非法 UTF-8：与经典共用同一份文案的一行告警进滚动区，并请求一次重绘
        if self.input.take_invalid() {
            self.push_lines([INVALID_WARNING.to_string()]);
            if actions.is_empty() {
                actions.push(Action::Redraw);
            }
        }
        actions
    }

    /// 审批态的一批字节：解码成按键逐个交给 [`TuiApp::approval_key`]。回信之后这一批剩下的按键
    /// 丢掉（与 `Submit`/`Exit` 之后忽略剩余按键同一条规则）——审批按键因此绝不落进编辑器。
    fn approval_bytes(&mut self, bytes: &[u8]) -> Vec<Action> {
        let keys = self.input.keys(bytes);
        let mut redraw = false;
        for key in keys {
            redraw |= self.approval_key(key).is_some();
            if !matches!(self.mode, InputMode::Approval { .. }) {
                // 回信即结束审批：界面回到审批记下的忙碌态（要重绘一帧），剩下的按键随这一批丢掉
                redraw = true;
                break;
            }
        }
        if redraw {
            vec![Action::Redraw]
        } else {
            Vec::new()
        }
    }

    /// 确认态的一批字节：解码成按键逐个交给 [`TuiApp::confirm_key`]。回信之后这一批剩下的按键
    /// 丢掉（与 `Submit`/`Exit` 之后忽略剩余按键同一条规则）——确认按键因此绝不落进编辑器。
    fn confirm_bytes(&mut self, bytes: &[u8]) -> Vec<Action> {
        let keys = self.input.keys(bytes);
        let mut redraw = false;
        for key in keys {
            redraw |= self.confirm_key(key).is_some();
            if !matches!(self.mode, InputMode::Confirm { .. }) {
                // 回信即结束确认：界面回到确认记下的忙碌态（要重绘一帧），剩下的按键随这一批丢掉
                redraw = true;
                break;
            }
        }
        if redraw {
            vec![Action::Redraw]
        } else {
            Vec::new()
        }
    }

    /// 选择器态的一批字节：解码成按键逐个交给 [`TuiApp::selector_key`]。选择器关闭（Enter/Esc/
    /// Ctrl+D）之后这一批剩下的按键丢掉（与「提交之后忽略剩余按键」同一条规则）——选择器的按键
    /// 因此绝不落进编辑器。
    fn selector_bytes(&mut self, bytes: &[u8]) -> Vec<Action> {
        let keys = self.input.keys(bytes);
        let mut actions = Vec::new();
        for key in keys {
            actions.extend(self.selector_key(key));
            if !matches!(self.mode, InputMode::Selector { .. }) {
                break;
            }
        }
        actions
    }

    /// 每 tick：排空捕获缓冲、消化「一个裸 ESC」；返回是否需要重绘。
    ///
    /// Esc 消歧窗口 = 一个 tick（[`crate::editor::keys::Decoder::take_lone_escape`]）：吃到裸 ESC
    /// 就**合成一次 Esc 按键**交给选择器——只有它认这个键（关闭），其余输入态忽略（T3 起就是
    /// 「吃掉即可」）。Esc 之外的按键一律由字节那条路进选择器。
    pub fn on_tick(&mut self) -> bool {
        let escaped = self.input.take_lone_escape();
        let routed = escaped && !self.selector_key(Key::Esc).is_empty();
        let ingested = self.ingest();
        routed || ingested
    }

    /// 排空捕获缓冲 → 滚动区（净化解出的是 `\n` 分隔文本，半行留在滚动区里等下一次）。
    pub fn ingest(&mut self) -> bool {
        let bytes = capture::drain(&self.capture);
        if bytes.is_empty() {
            return false;
        }
        // 整段都是控制字节（清屏、改标题）时屏幕没有变化：不白写一帧
        let text = capture::sanitize(&bytes);
        if text.is_empty() {
            return false;
        }
        self.scrollback.push_text(&text);
        true
    }

    /// 收尾：排空捕获并把最后一半行落地（一轮结束时调一次）；返回是否有变化。
    pub fn flush_pending(&mut self) -> bool {
        let changed = self.ingest();
        self.scrollback.flush_pending();
        changed
    }

    /// 一批完整的行进滚动区（命令输出、错误行、提交回显都走它）。
    ///
    /// 每一行先过 [`capture::sanitize`] 再入区（F1）：这条路口子上的行也可能带着命令输出或会话
    /// 标题里的控制字节，滚动区里因此**不再存控制字节**（`compose` 那一层是同一函数的第二道
    /// 保险）。先把捕获里没换行的尾巴落地：这些行必须各占一行（不与半行粘在一起）。
    pub fn push_lines(&mut self, lines: impl IntoIterator<Item = String>) {
        self.scrollback.flush_pending();
        for line in lines {
            self.scrollback
                .push_text(&capture::sanitize(line.as_bytes()));
            self.scrollback.push_text("\n");
        }
    }

    /// 进入忙碌态：输入禁用（输入区第 1 行换成 dim 的 `working…`）。
    pub fn begin_busy(&mut self, busy: Busy) {
        self.mode = InputMode::Busy(busy);
    }

    /// 收到一次审批请求：共用文案（[`prompt_lines`]，与经典 REPL 同一份）4 行逐字进滚动区
    /// ——3 行输入区放不下 4 行提示 + `choice> `，正文由滚动区承担；输入态切到审批，并记下
    /// **当时的忙碌态**（[`TuiApp::busy`]）：回信之后恢复到它，而不是回到编辑器——审批只在任务
    /// 的 future 推进期间到达，任务还在跑，输入必须继续禁用。
    ///
    /// 返回落地的那些行（调用方据此重绘一帧；滚动区已经写好了）。工具调用是串行的，同一时刻
    /// 只会有一个待审批请求；真撞上第二个时前一个随替换丢掉——它的回信端掉落，那次审批因此
    /// fail-safe 拒绝（与 [`crate::tui::approval::TuiApproval`] 同一口径）。
    pub fn begin_approval(&mut self, job: ApprovalJob) -> Vec<String> {
        // 审批叠在忙碌期上：记下当时的忙碌态。真从空闲态被驱动（只在用例里可能）时按任务轮次
        // 恢复——唯一会在运行中输入的那一档，也保证这里不 panic。
        let resume = self.busy().unwrap_or(Busy::Task);
        let lines = prompt_lines(&job.request);
        self.push_lines(lines.clone());
        self.mode = InputMode::Approval {
            request: job.request,
            reply: Some(job.reply),
            unknown: 0,
            resume,
        };
        lines
    }

    /// 审批按键（`a`/`s`/`r`/`d`，大小写与空白归一——与经典 [`parse_choice`] 同一份）：`a`/`s`/`r`
    /// 回信并恢复审批开始时记下的忙碌态（**不是**编辑器：任务的 future 还在跑）；`d` 把参数明细
    /// （与经典同一份美化 JSON）逐行打进滚动区后继续等；连续
    /// `MAX_UNKNOWN_INPUTS` 次无效输入或 EOF（`Ctrl+D`）＝ [`ApprovalDecision::Reject`]（fail-safe）；
    /// `Ctrl+C` 的字节与 SIGINT 同义：拒绝并取消本轮任务令牌。
    ///
    /// 返回要落进滚动区的行（只有 `d` 的明细有；滚动区已经写好了）；不在审批态时什么都不做。
    pub fn approval_key(&mut self, key: Key) -> Option<Vec<String>> {
        if !matches!(self.mode, InputMode::Approval { .. }) {
            return None;
        }
        match approval_step(key) {
            ApprovalStep::Reply(decision) => {
                // `Ctrl+C` 字节（终端没开 ISIG 时它就是那个信号）：与 SIGINT 同义，顺手取消本轮令牌
                if key == Key::CtrlC
                    && let Some(token) = self.cancel.as_ref()
                {
                    token.cancel();
                }
                self.end_approval(decision);
                None
            }
            ApprovalStep::Details => {
                let InputMode::Approval { request, .. } = &self.mode else {
                    return None;
                };
                let lines: Vec<String> = details_json(&request.args)
                    .lines()
                    .map(str::to_string)
                    .collect();
                self.push_lines(lines.clone());
                Some(lines)
            }
            ApprovalStep::Unknown => {
                let InputMode::Approval { unknown, .. } = &mut self.mode else {
                    return None;
                };
                *unknown += 1;
                let give_up = *unknown >= MAX_UNKNOWN_INPUTS;
                if give_up {
                    // 连续无效输入到上限：fail-safe 拒绝（与经典 `decide_from_lines` 同一口径）
                    self.end_approval(ApprovalDecision::Reject);
                }
                None
            }
        }
    }

    /// 收下审批态：回信（回信端已经没了就算了）并恢复审批开始时记下的忙碌态——审批叠在忙碌期
    /// 上（任务的 future 还在跑），**绝不**直接回编辑器；回编辑器只有 [`TuiApp::end_busy`] 一条路。
    fn end_approval(&mut self, decision: ApprovalDecision) {
        let InputMode::Approval { reply, resume, .. } = &mut self.mode else {
            return; // 不在审批态：什么都不做（只有审批按键会走到这里）
        };
        // 先把回信口与恢复态取出来：请求与无效输入计数随换态一起丢掉
        let reply = reply.take();
        let resume = *resume;
        self.mode = InputMode::Busy(resume);
        if let Some(reply) = reply {
            let _ = reply.send(decision);
        }
    }

    /// 收到一次 `/learn` 确认请求：头行（[`draft_header`]）与预览（[`preview_lines`]）逐行进
    /// 滚动区——与经典 REPL 同一份纯函数与常量（预览的截断口径只有那一处）；输入态切到确认，
    /// 并记下**当时的忙碌态**（[`TuiApp::busy`]）：回信之后恢复到它，而不是回到编辑器——草稿的
    /// future 还在跑，输入必须继续禁用。
    ///
    /// 返回落地的那些行（调用方据此重绘一帧；滚动区已经写好了）。`/learn` 一次只会有一个待答的
    /// 确认请求；真撞上第二个时前一个随替换丢掉——它的回信端掉落，那次确认因此按放弃收束
    /// （与 [`crate::tui::confirm::tui_confirm`] 同一口径）。
    pub fn begin_confirm(&mut self, job: ConfirmJob) -> Vec<String> {
        // 确认叠在 `/learn` 的忙碌期上：记下当时的忙碌态。真从空闲态被驱动（只在用例里可能）时
        // 按学习档恢复——确认面只在 `/learn` 里出现，也保证这里不 panic。
        let resume = self.busy().unwrap_or(Busy::Learn);
        let mut lines = vec![draft_header(&job.name)];
        lines.extend(preview_lines(&job.skill_md));
        self.push_lines(lines.clone());
        self.mode = InputMode::Confirm {
            reply: Some(job.reply),
            resume,
        };
        lines
    }

    /// 确认按键：只有 `s`（trim + 大小写归一，与经典 [`crate::commands::learn::asks_save`] 同一
    /// 口径）回 `true`；其它任何按键、EOF（`Ctrl+D`）都回 `false`；`Ctrl+C` 的字节与 SIGINT 同义
    /// ——回 `false` 并取消本轮令牌（与 [`TuiApp::approval_key`] 同一条口径）。
    ///
    /// 回信只送一次（回信口随即取走）；不在确认态时什么都不做。返回 `Some(结论)` = 这一次按键
    /// 回答了本次确认。
    pub fn confirm_key(&mut self, key: Key) -> Option<bool> {
        if !matches!(self.mode, InputMode::Confirm { .. }) {
            return None;
        }
        // 与经典同一份答案归一：单字符也过 `asks_save`（trim + 大小写）
        let save = match key {
            Key::Char(ch) => asks_save(Some(&ch.to_string())),
            _ => false,
        };
        // `Ctrl+C` 字节（终端没开 ISIG 时它就是那个信号）：与 SIGINT 同义，顺手取消本轮令牌
        if key == Key::CtrlC
            && let Some(token) = self.cancel.as_ref()
        {
            token.cancel();
        }
        self.end_confirm(save);
        Some(save)
    }

    /// 收下确认态：回信（回信端已经没了就算了）并恢复确认开始时记下的忙碌态——确认叠在
    /// `/learn` 的忙碌期上（草稿的 future 还在跑），**绝不**直接回编辑器；回编辑器只有
    /// [`TuiApp::end_busy`] 一条路。
    fn end_confirm(&mut self, save: bool) {
        let InputMode::Confirm { reply, resume } = &mut self.mode else {
            return; // 不在确认态：什么都不做（只有确认按键会走到这里）
        };
        // 先把回信口与恢复态取出来：换态之后请求不再可见
        let reply = reply.take();
        let resume = *resume;
        self.mode = InputMode::Busy(resume);
        if let Some(reply) = reply {
            let _ = reply.send(save);
        }
    }

    /// 离开忙碌态：回到编辑器（缓冲是空的，光标在段首）——这是回编辑器的**唯一**一条路
    /// （审批/确认回信只回到它记下的忙碌态，见 [`TuiApp::end_approval`] / [`TuiApp::end_confirm`]）。
    pub fn end_busy(&mut self) {
        self.mode = InputMode::Editor;
    }

    /// 打开 `/sessions` 选择器（只在空闲期；空表也开，内容是 `(no sessions)`）。
    ///
    /// 列表就是 `/sessions` 那张表（[`selector_lines`] 只加选择前缀），游标从第一行起；
    /// `current` 是当前会话的整条 id（列表里带 `*`），还没有当前会话时给 `None`。
    pub fn open_selector(&mut self, rows: Vec<SessionSummary>, current: Option<String>) {
        self.mode = InputMode::Selector {
            rows,
            cursor: 0,
            current,
        };
    }

    /// 选择器按键：↑↓ 移动（到边界不动）、Enter 交回 [`Action::Resume`]（整条会话 id）并关闭、
    /// Esc 关闭、Ctrl+D 关闭、其余忽略。
    ///
    /// 关闭都是回编辑器（选择器只在空闲期开）；空表没有可选项，Enter 不动作（用 Esc/Ctrl+D 关）。
    /// 不在选择器态时什么都不做（Esc 的合成路由因此只有选择器认它）。
    pub fn selector_key(&mut self, key: Key) -> Vec<Action> {
        if !matches!(self.mode, InputMode::Selector { .. }) {
            return Vec::new();
        }
        match key {
            Key::Up | Key::Down => {
                let InputMode::Selector { rows, cursor, .. } = &mut self.mode else {
                    return Vec::new();
                };
                // 空表：0 就是唯一的位置（上下都是它）
                let last = rows.len().saturating_sub(1);
                *cursor = match key {
                    Key::Up => cursor.saturating_sub(1),
                    _ => (*cursor + 1).min(last),
                };
                vec![Action::Redraw]
            }
            Key::Enter => {
                let id = match &self.mode {
                    InputMode::Selector { rows, cursor, .. } => {
                        rows.get(*cursor).map(|row| row.session.id.clone())
                    }
                    _ => None,
                };
                let Some(id) = id else {
                    return Vec::new(); // 空表：没有可选项
                };
                self.mode = InputMode::Editor;
                vec![Action::Resume(id)]
            }
            Key::Esc | Key::CtrlD => {
                self.mode = InputMode::Editor;
                vec![Action::Redraw]
            }
            _ => Vec::new(),
        }
    }

    /// 当前是不是忙碌态（`None` = 空闲）：审批/确认叠在忙碌期上，返回它们记下的那一档（`resume`）
    /// ——回信前后对调用方都是「还在忙」；选择器是空闲期的一态，也给 `None`。
    pub fn busy(&self) -> Option<Busy> {
        match self.mode {
            InputMode::Busy(busy) => Some(busy),
            InputMode::Approval { resume, .. } | InputMode::Confirm { resume, .. } => Some(resume),
            InputMode::Editor | InputMode::Selector { .. } => None,
        }
    }

    /// 组装一帧（合成器是纯函数；这里只把状态拼进 [`TuiView`]）。
    pub fn view(&self, facts: &ViewFacts) -> TuiView {
        let (input, hint) = match &self.mode {
            // 忙碌期没有可编辑的输入：光标藏起来，两行都是说明性文案
            InputMode::Busy(_) => (
                InputBlock {
                    lines: [WORKING.to_string(), String::new(), BUSY_HINT.to_string()],
                    cursor: None,
                },
                BUSY_HINT,
            ),
            // 审批：正文（4 行提示）在滚动区，输入区首行只留经典的 `choice> `（光标停在它后面）
            InputMode::Approval { .. } => (
                InputBlock {
                    lines: [
                        CHOICE_PROMPT.to_string(),
                        String::new(),
                        APPROVAL_HINT.to_string(),
                    ],
                    cursor: Some((0, display_width(CHOICE_PROMPT))),
                },
                BUSY_HINT,
            ),
            // 确认：头行/预览在滚动区，输入区首行就是经典的完整提示词（光标停在它后面）
            InputMode::Confirm { .. } => (
                InputBlock {
                    lines: [
                        LEARN_PROMPT.to_string(),
                        String::new(),
                        CONFIRM_HINT.to_string(),
                    ],
                    cursor: Some((0, display_width(LEARN_PROMPT))),
                },
                BUSY_HINT,
            ),
            // 选择器：列表整块进视口（下面按同一份行拼），输入区只留提示行、没有光标
            InputMode::Selector { .. } => (
                InputBlock {
                    lines: [String::new(), String::new(), SELECTOR_HINT.to_string()],
                    cursor: None,
                },
                SELECTOR_HINT,
            ),
            InputMode::Editor => (self.input.block(), IDLE_HINT),
        };
        TuiView {
            size: self.size,
            color: facts.color,
            theme: facts.theme,
            top: top_line(facts),
            // 视口就是「最近 rows 行」：版式切到该显示的那一段是合成器的事。选择器开着时视口整块
            // 让给它（那一份列表就是用户此刻要看的东西）
            viewport: match &self.mode {
                InputMode::Selector {
                    rows,
                    cursor,
                    current,
                } => {
                    // 列表围绕光标开窗（F3）：切给可视高度那么多行，选中行（列表里第 `1 + cursor`
                    // 行，第 0 行是表头）始终在窗口里，窗口随光标移动
                    let lines = selector_lines(rows, current.as_deref(), *cursor);
                    selector_window(lines, 1 + *cursor, content_rows(self.size))
                }
                _ => self.scrollback.viewport(self.size.rows),
            },
            input,
            status: status_line(facts, hint),
        }
    }

    /// 空闲期等到的一件事（主循环按它分派）。
    pub(crate) async fn wait_idle(&mut self) -> Wake {
        self.install_signals();
        let mut chunks = self.chunks.take();
        let (mut sigint, mut sigwinch) = self.take_signals();
        // 用 `sleep` 而不是 `interval`：每次等待重新计时，等到的永远是「安静 100ms 之后」的
        // 那一次心跳（首个 tick 不会立刻返回，也就不会空转）
        let wake = tokio::select! {
            chunk = next_chunk(&mut chunks) => match chunk {
                Some(chunk) => Wake::Chunk(chunk),
                // 读取线程走了（stdin 关闭、进程退出途中）：与 EOF 同一条路
                None => Wake::Chunk(InputChunk::Eof),
            },
            _ = next_signal(&mut sigint) => Wake::Sigint,
            _ = next_signal(&mut sigwinch) => Wake::Sigwinch,
            _ = tokio::time::sleep(TICK) => Wake::Tick,
        };
        self.chunks = chunks;
        self.put_signals(sigint, sigwinch);
        wake
    }

    /// 忙碌期子循环：驱动一个借用会话的 future，同时处理重绘/缩放/SIGINT/输入字节。
    /// `cancel` 给 `/compact` `/learn`（任务轮次给 `None`：取消在 `run_task` 内部，经典语义不变）。
    pub async fn drive<F>(
        &mut self,
        term: &mut term::TerminalGuard,
        facts: &ViewFacts,
        fut: F,
        cancel: Option<CancellationToken>,
    ) -> F::Output
    where
        F: Future,
    {
        self.drive_with(Some(term), facts, fut, cancel).await
    }

    /// 无终端的 [`TuiApp::drive`]：不写帧，其余逐字相同（T3 的循环级用例走它）。
    pub(crate) async fn drive_headless<F>(
        &mut self,
        facts: &ViewFacts,
        fut: F,
        cancel: Option<CancellationToken>,
    ) -> F::Output
    where
        F: Future,
    {
        self.drive_with(None, facts, fut, cancel).await
    }

    /// 两个入口的公共实现：`term` 为空 = 不画帧（用例）。**循环体内不碰会话**（状态读 `facts`）。
    ///
    /// 审批（T4）与确认（T5）的请求在同一个 `select!` 里收：请求到达即 [`TuiApp::begin_approval`]
    /// （提示进滚动区、输入态切到审批），future 照常推进——「UI 等 UI」的死锁因此不可能发生。
    async fn drive_with<F>(
        &mut self,
        mut term: Option<&mut term::TerminalGuard>,
        facts: &ViewFacts,
        fut: F,
        cancel: Option<CancellationToken>,
    ) -> F::Output
    where
        F: Future,
    {
        self.install_signals();
        tokio::pin!(fut);
        // 100ms 心跳：第一次 `tick()` 立刻返回（进场先排空一次捕获），之后每 100ms 一次；
        // 漏掉的 tick 延后一格而不补发（终端卡顿时不补一串空转）
        let mut tick = tokio::time::interval(TICK);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let mut chunks = self.chunks.take();
        let mut approvals = self.approvals.take();
        let mut confirms = self.confirms.take();
        let (mut sigint, mut sigwinch) = self.take_signals();
        // 本轮令牌挂进应用：审批态的 `Ctrl+C` 字节靠它取消本轮任务（见 `TuiApp::approval_key`）
        self.cancel = cancel.clone();

        // 进场先画一帧：忙碌态的输入区（`working…`）立刻可见
        self.draw(term.as_deref_mut(), facts);
        let output = loop {
            tokio::select! {
                output = &mut fut => break output,
                _ = tick.tick() => {
                    if self.on_tick() {
                        self.draw(term.as_deref_mut(), facts);
                    }
                }
                _ = next_signal(&mut sigint) => {
                    // 忙碌期留在界内：任务轮次的取消在 `run_task_with_out` 自己的 watcher 里；
                    // `/compact` `/learn` 由 TUI 持有的令牌收
                    if let Some(token) = cancel.as_ref() {
                        token.cancel();
                    }
                }
                _ = next_signal(&mut sigwinch) => {
                    if let Some(guard) = term.as_deref_mut() {
                        self.size = guard.refresh_size();
                    }
                    self.draw(term.as_deref_mut(), facts);
                }
                job = next_approval(&mut approvals) => {
                    // 收到审批请求：提示进滚动区、输入态切到审批，然后重绘一帧；future 继续推进
                    if let Some(job) = job {
                        let _ = self.begin_approval(job);
                        self.draw(term.as_deref_mut(), facts);
                    }
                }
                job = next_confirm(&mut confirms) => {
                    // 收到 `/learn` 确认请求：头行/预览进滚动区、输入态切到确认，然后重绘一帧；
                    // 草稿的 future 继续推进（等回信）
                    if let Some(job) = job {
                        let _ = self.begin_confirm(job);
                        self.draw(term.as_deref_mut(), facts);
                    }
                }
                chunk = next_chunk(&mut chunks) => {
                    // 忙碌期输入禁用：字节照收（别留给空闲期），动作一律丢掉；审批/确认态的按键由
                    // `on_bytes` 路由到各自的按键面，真改了界面（明细/回信）就立刻重绘一帧，不等心跳
                    if let Some(InputChunk::Bytes(bytes)) = chunk
                        && !self.on_bytes(&bytes).is_empty()
                    {
                        self.draw(term.as_deref_mut(), facts);
                    }
                }
            }
        };
        self.chunks = chunks;
        self.approvals = approvals;
        self.confirms = confirms;
        self.put_signals(sigint, sigwinch);
        // 本轮结束：摘掉令牌（审批态的按键不该动下一轮的取消）
        self.cancel = None;
        // 收尾：把捕获里剩下的字节排空、半行落地（本轮输出必须完整落进滚动区）
        self.flush_pending();
        output
    }

    /// 第一次用到时装上信号流（装不上——不支持的平台——就永远挂起，不 panic）。
    ///
    /// 已经接上（生产路径在进守卫之前装的，见 [`TuiApp::attach_signals`]）或已经现场装过一次的
    /// （`Some` 就是「装过」）什么都不做：**一份就够，绝不重复注册**——重复注册会让同一路信号多
    /// 出一条没人读的流。
    fn install_signals(&mut self) {
        if self.signals.is_none() {
            self.signals = Some(Signals::install());
        }
    }

    /// 把两个信号流取出来（`select!` 的借用比 `&mut self` 活得久）：取出与放回必须成对
    /// （[`TuiApp::put_signals`]）；槽位本身留着——它就是「已经装过」的判据。
    fn take_signals(&mut self) -> (Option<Signal>, Option<Signal>) {
        match self.signals.as_mut() {
            Some(signals) => (signals.sigint.take(), signals.sigwinch.take()),
            None => (None, None),
        }
    }

    /// 把 [`TuiApp::take_signals`] 取出来的两个流放回去（下一轮等待接着用同一份）。
    fn put_signals(&mut self, sigint: Option<Signal>, sigwinch: Option<Signal>) {
        if let Some(signals) = self.signals.as_mut() {
            signals.sigint = sigint;
            signals.sigwinch = sigwinch;
        }
    }

    /// 画一帧（`term` 为空 = headless：什么都不写）。
    fn draw(&self, term: Option<&mut term::TerminalGuard>, facts: &ViewFacts) {
        let Some(guard) = term else {
            return;
        };
        let view = self.view(facts);
        // 写失败（EPIPE 等）忽略：画不出帧不该带走一场会话
        let _ = guard.write(compose(&view).as_bytes());
    }
}

/// 内容区（消息视口 / 选择器列表）的可视行数：`rows` 减去版式的固定行（顶部状态行 1 + 输入区 3 +
/// 底部状态行 1），至少 1 行——极小窗口里也是「看得见一行」而不是零行。
///
/// 这一个口径就是 PgUp/PgDn 的步长（F2）与选择器开窗的高度（F3），与帧的版式同源
/// （[`FIXED_ROWS`]）。
fn content_rows(size: Size) -> usize {
    size.rows.saturating_sub(FIXED_ROWS).max(1)
}

/// 选择器列表的开窗：取包含 `selected` 行的连续 `visible` 行（窗口贴住列表两端，绝不越界）。
///
/// `selector_lines` 的产物只是被切了一段——表头与列宽都不重排；列表不长于 `visible` 时原样返回
/// （既有断言因此逐字不变）。`visible` 至少取 1 行。
fn selector_window(lines: Vec<String>, selected: usize, visible: usize) -> Vec<String> {
    let visible = visible.max(1);
    if lines.len() <= visible {
        return lines;
    }
    let last_start = lines.len() - visible;
    let start = selected.saturating_sub(visible - 1).min(last_start);
    lines[start..start + visible].to_vec()
}

/// 空闲期等到的一件事（主循环按它分派）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Wake {
    /// 输入读取线程交回的一批字节（或 EOF）。
    Chunk(InputChunk),
    /// SIGINT：空闲期退出、忙碌期留在界内（见 [`TuiApp::drive`]）。
    Sigint,
    /// 窗口变化：重新探测尺寸并重绘。
    Sigwinch,
    /// 100ms 心跳：排空捕获、消化裸 ESC。
    Tick,
}

/// 输入读取线程送出的一批字节（raw mode 下读到几个字节就返回）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InputChunk {
    /// 一段刚读到的字节。
    Bytes(Vec<u8>),
    /// stdin 关闭（读到 0 字节或读失败）：主循环据此收摊。
    Eof,
}

/// 起一条脱离线程按块读 stdin（与 `repl::next_line` 同一手法：进程退出时它随进程消失）。
///
/// raw mode 下 `read` 读到几个字节就返回；`ErrorKind::Interrupted` 是「被信号打断」，继续读。
/// 读到 0 字节（stdin 关闭）或真读取错误都按 EOF 交回，接收端据此收摊。
pub fn spawn_input_reader(tx: mpsc::UnboundedSender<InputChunk>) {
    std::thread::spawn(move || {
        let mut input = std::io::stdin().lock();
        let mut buf = [0u8; READ_CHUNK];
        loop {
            match input.read(&mut buf) {
                Ok(0) => break,
                Ok(read) => {
                    if tx.send(InputChunk::Bytes(buf[..read].to_vec())).is_err() {
                        return; // 接收端已经走了（进程退出途中）
                    }
                }
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(_) => break,
            }
        }
        let _ = tx.send(InputChunk::Eof);
    });
}

/// 等下一批输入字节；通道关闭（读取线程退出）时清掉槽位并返回 `None`（此后永远挂起——
/// 发送端全消失时 `recv()` 会立刻给 `None`，不清掉就是空转）。
async fn next_chunk(slot: &mut Option<mpsc::UnboundedReceiver<InputChunk>>) -> Option<InputChunk> {
    let closed = match slot.as_mut() {
        Some(rx) => match rx.recv().await {
            Some(chunk) => return Some(chunk),
            None => true,
        },
        // 还没接上（headless 用例）：永远挂起
        None => std::future::pending().await,
    };
    if closed {
        *slot = None;
    }
    None
}

/// 等下一个信号；信号流装不上（不支持的平台）或已经结束时永远挂起。
async fn next_signal(slot: &mut Option<Signal>) {
    let ended = match slot.as_mut() {
        Some(signal) => signal.recv().await.is_none(),
        None => std::future::pending().await,
    };
    if ended {
        // 结束（不会发生在信号流上）：清掉槽位，别让它每轮立刻完成造成空转
        *slot = None;
    }
}

/// 等下一次审批请求；没接上（经典路径、没注入审批处理器的会话）时永远挂起。
///
/// 发送端全消失（`Session` 连同它的审批处理器一起掉了）时清掉槽位并返回 `None`：此后 `recv()`
/// 会立刻返回 `None`，不清掉就是空转。
async fn next_approval(
    slot: &mut Option<mpsc::UnboundedReceiver<ApprovalJob>>,
) -> Option<ApprovalJob> {
    let closed = match slot.as_mut() {
        Some(rx) => match rx.recv().await {
            Some(job) => return Some(job),
            None => true,
        },
        None => std::future::pending().await,
    };
    if closed {
        *slot = None;
    }
    None
}

/// 等下一次 `/learn` 确认请求；没接上（不走 `/learn` 的轮次）时永远挂起。
///
/// 发送端全消失（那一轮 `/learn` 的确认闭包连同 future 一起掉了）时清掉槽位并返回 `None`：此后
/// `recv()` 会立刻返回 `None`，不清掉就是空转。
async fn next_confirm(
    slot: &mut Option<mpsc::UnboundedReceiver<ConfirmJob>>,
) -> Option<ConfirmJob> {
    let closed = match slot.as_mut() {
        Some(rx) => match rx.recv().await {
            Some(job) => return Some(job),
            None => true,
        },
        None => std::future::pending().await,
    };
    if closed {
        *slot = None;
    }
    None
}

/// 审批态一次按键的语义（纯函数：归一与经典 [`parse_choice`] 同一份）。
enum ApprovalStep {
    /// 回信并恢复审批开始时记下的忙碌态（不是编辑器）。
    Reply(ApprovalDecision),
    /// `d`：参数明细进滚动区，继续等。
    Details,
    /// 无效输入：计数（到 `MAX_UNKNOWN_INPUTS` 即 fail-safe 拒绝）。
    Unknown,
}

/// 按键 → 审批语义：`a`/`s`/`r`/`d` 走经典的 [`parse_choice`]（大小写与空白归一），
/// `Ctrl+C` 与 SIGINT 同义、`Ctrl+D`（EOF）也是 Reject，其余按键都算无效输入。
fn approval_step(key: Key) -> ApprovalStep {
    match key {
        Key::CtrlC | Key::CtrlD => ApprovalStep::Reply(ApprovalDecision::Reject),
        Key::Char(ch) => match parse_choice(&ch.to_string()) {
            Some(Choice::ApproveOnce) => ApprovalStep::Reply(ApprovalDecision::AllowOnce),
            Some(Choice::Always) => ApprovalStep::Reply(ApprovalDecision::AllowSession),
            Some(Choice::Reject) => ApprovalStep::Reply(ApprovalDecision::Reject),
            Some(Choice::Details) => ApprovalStep::Details,
            None => ApprovalStep::Unknown,
        },
        _ => ApprovalStep::Unknown,
    }
}

#[cfg(test)]
mod tests {
    use std::io::Write;
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use ignis_core::config::{OutputMode, Theme};
    use ignis_runtime::{ApprovalDecision, ApprovalHandler, ApprovalRequest, RiskLevel};
    use ignis_storage::{SessionRecord, SessionSummary};
    use serde_json::json;
    use tokio::sync::{mpsc, oneshot};
    use tokio_util::sync::CancellationToken;

    use crate::approval::{MAX_UNKNOWN_INPUTS, format_prompt, prompt_lines};
    use crate::commands::learn::{
        LEARN_PREVIEW_LINES, LEARN_PREVIEW_MARKER, LEARN_PROMPT, draft_header, preview_lines,
    };
    use crate::editor::history::History;
    use crate::editor::keys::Key;
    use crate::editor::raw::{RawMode, serial_slot};
    use crate::editor::reader::INVALID_WARNING;
    use crate::tui::Size;
    use crate::tui::approval::{ApprovalJob, TuiApproval};
    use crate::tui::capture::CaptureSink;
    use crate::tui::confirm::{self, ConfirmJob};
    use crate::tui::term::TerminalGuard;
    use crate::tui::view::{Scrollback, ViewFacts, compose, selector_lines, status_line, top_line};

    use super::{
        Action, BUSY_HINT, Busy, IDLE_HINT, InputChunk, InputMode, SELECTOR_HINT, Signals, TuiApp,
        Wake,
    };

    /// 事实快照：顶/底状态行只关心拼法（与 T2 的用例同形）。
    fn facts() -> ViewFacts {
        ViewFacts {
            version: env!("CARGO_PKG_VERSION"),
            workspace: "/tmp/ws".to_string(),
            kernel: "native".to_string(),
            session: "new".to_string(),
            mode: OutputMode::Normal,
            theme: Theme::Dark,
            color: false,
            budget: 200_000,
            history_messages: 0,
            stats: None,
            usage: None,
            native: true,
        }
    }

    /// 无终端的应用夹具：空滚动区 + 内存历史 + 共享捕获缓冲。
    fn app() -> (TuiApp, Arc<Mutex<Vec<u8>>>) {
        let (_, buffer) = CaptureSink::pair();
        let state = TuiApp::new(Scrollback::new(), History::in_memory(), Arc::clone(&buffer));
        (state, buffer)
    }

    /// 往捕获缓冲写一段字节（模拟渲染器的流式输出）。
    fn write(buffer: &Arc<Mutex<Vec<u8>>>, bytes: &[u8]) {
        buffer
            .lock()
            .expect("捕获锁不该中毒")
            .extend_from_slice(bytes);
    }

    /// 记录写出字节的写入器：帧落在它里面（pty 用例的终端后备）。
    #[derive(Clone, Default)]
    struct Recorder(Arc<Mutex<Vec<u8>>>);

    impl Write for Recorder {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0
                .lock()
                .expect("记录锁不该中毒")
                .extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl Recorder {
        fn text(&self) -> String {
            String::from_utf8(self.0.lock().expect("记录锁不该中毒").clone())
                .expect("帧是合法 UTF-8")
        }
    }

    /// 开一对 pty（主端 = 终端模拟器一侧，从端 = 程序看的那个 tty）。
    fn open_pty() -> (OwnedFd, OwnedFd) {
        let mut master: RawFd = -1;
        let mut slave: RawFd = -1;
        // SAFETY: `openpty` 只往两个已初始化的出参里写；返回值非 0 表示失败，此时不读它们。
        let rc = unsafe {
            libc::openpty(
                &mut master,
                &mut slave,
                std::ptr::null_mut(),
                std::ptr::null(),
                std::ptr::null(),
            )
        };
        assert_eq!(rc, 0, "openpty 失败：{}", std::io::Error::last_os_error());
        // SAFETY: 上面 rc == 0，两个 fd 都是本进程新拿到的所有权，交给 `OwnedFd` 收尾关闭。
        unsafe { (OwnedFd::from_raw_fd(master), OwnedFd::from_raw_fd(slave)) }
    }

    /// 给 pty 设窗口尺寸（终端模拟器那一侧的 `TIOCSWINSZ`）。
    fn set_winsize(fd: RawFd, cols: u16, rows: u16) {
        let size = libc::winsize {
            ws_row: rows,
            ws_col: cols,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        // SAFETY: `size` 是本函数独占的一份 `winsize`；`TIOCSWINSZ` 只读它。
        let rc = unsafe { libc::ioctl(fd, libc::TIOCSWINSZ, &size) };
        assert_eq!(
            rc,
            0,
            "TIOCSWINSZ 失败：{}",
            std::io::Error::last_os_error()
        );
    }

    /// pty 后备的守卫（用例走与生产同一条 `enter_with` 路径）。
    fn enter_guard(recorder: &Recorder, slave_fd: RawFd) -> TerminalGuard {
        TerminalGuard::enter_with(Box::new(recorder.clone()), slave_fd, RawMode::enable_on)
            .expect("pty 从端是真 tty，进入守卫必须成功")
            .0
    }

    /// 捕获排空：净化解出的是 `\n` 分隔文本，半行留在滚动区里等下一次；模型输出里的清屏序列
    /// 一个字节都进不了滚动区。
    #[test]
    fn capture_is_sanitized_and_split_into_lines_on_drain() {
        let (mut app, buffer) = app();
        let facts = facts();

        write(&buffer, b"a\nb");
        assert!(app.ingest(), "有内容就要重绘");
        assert_eq!(
            app.view(&facts).viewport,
            ["a"],
            "半行留在滚动区里等下一次（`b` 还没换行）"
        );
        assert!(buffer.lock().expect("捕获锁不该中毒").is_empty(), "排空");
        assert!(!app.ingest(), "再排空：没有变化");

        write(&buffer, b"\n");
        assert!(app.ingest());
        assert_eq!(app.view(&facts).viewport, ["a", "b"]);

        // 模型输出里的清屏/OSC 序列整条丢掉，文本留下
        write(&buffer, "\u{1b}[2J打了花\u{1b}]0;标题\u{7}\n".as_bytes());
        assert!(app.ingest());
        let lines = app.view(&facts).viewport;
        assert!(lines.contains(&"打了花".to_string()), "{lines:?}");
        assert!(
            lines.iter().all(|line| !line.contains('\u{1b}')),
            "滚动区里不许有 ESC：{lines:?}"
        );
    }

    /// 帧协议自身的序列（藏/显光标、归位、清行）。
    const FRAME_SEQUENCES: [&str; 4] = ["\x1b[?25l", "\x1b[?25h", "\x1b[H", "\x1b[K"];

    /// 调色板的六个 SGR（`capture` 的净化白名单）：chrome 与渲染器输出的正当上色。
    const PALETTE_SGR: [&str; 6] = [
        "\x1b[0m", "\x1b[1m", "\x1b[2m", "\x1b[31m", "\x1b[36m", "\x1b[90m",
    ];

    /// 帧里第一条「既不属于帧协议、也不属于调色板白名单」的转义段（`None` = 干净）。
    ///
    /// F1 的判据：内容行漏出来的转义（清屏 `\x1b[2J`、OSC 改标题、裸 ESC）两者都不是，一出现就
    /// 会被这里抓住。
    fn stray_escape(frame: &str) -> Option<String> {
        let mut rest = frame;
        while let Some(start) = rest.find('\u{1b}') {
            let tail = &rest[start..];
            if let Some(known) = FRAME_SEQUENCES
                .iter()
                .chain(PALETTE_SGR.iter())
                .find(|sequence| tail.starts_with(**sequence))
            {
                rest = &tail[known.len()..];
                continue;
            }
            // 光标绝对定位（`\x1b[{row};{col}H`）：参数只有数字与 `;`
            if let Some(body) = tail.strip_prefix("\x1b[")
                && let Some((params, _)) = body.split_once('H')
                && !params.is_empty()
                && params.chars().all(|ch| ch.is_ascii_digit() || ch == ';')
            {
                rest = &body[params.len() + 1..];
                continue;
            }
            return Some(tail.chars().take(8).collect());
        }
        None
    }

    /// F1（终审修复波）：帧的内容来源全部净化——`push_lines` 的每一行先过 `capture::sanitize`
    /// 再进滚动区，`compose` 在截断之前对每一条内容行（viewport/top/input/status）再过一遍同一
    /// 函数。含清屏（`\x1b[2J`）与 OSC 改标题（`\x1b]0;x\x07`）的行既进不了滚动区、也进不了整帧：
    /// 帧里除协议自身的序列与调色板白名单之外没有任何 ESC。
    #[test]
    fn escapes_never_reach_the_scrollback_or_the_frame() {
        let (mut app, _buffer) = app();
        let facts = facts();
        app.set_size(Size { cols: 60, rows: 12 });
        app.push_lines([
            "打了花\u{1b}[2J还活着\u{1b}]0;标题\u{7}".to_string(),
            "\u{1b}[2J整行都是控制字节".to_string(),
        ]);

        // 滚动区（命令输出/模型流式输出的落点）里一个 ESC 都没有，文本照留
        let viewport = app.view(&facts).viewport;
        assert_eq!(viewport, ["打了花还活着", "整行都是控制字节"]);
        assert!(
            viewport.iter().all(|line| !line.contains('\u{1b}')),
            "{viewport:?}"
        );

        // top/status/输入区也塞进同类字节：合成器在截断之前净化（无色与上色两条路都过）
        for color in [false, true] {
            let mut view = app.view(&facts);
            view.color = color;
            view.top = "IGNIS\u{1b}[2J v0 · \u{1b}]0;top\u{7}".to_string();
            view.input.lines[0] = "❯ \u{1b}[2J输入".to_string();
            view.input.lines[1] = "\u{1b}]0;续行\u{7}x".to_string();
            view.input.lines[2] = "\u{1b}[2m提示\u{1b}[2J".to_string();
            view.status = "ctx 0\u{1b}[2J/1 chars · \u{1b}]0;s\u{7}".to_string();

            let frame = compose(&view);
            assert!(!frame.contains("\u{1b}[2J"), "清屏序列进了帧：{frame:?}");
            assert!(!frame.contains("\u{1b}]"), "OSC 进了帧：{frame:?}");
            assert!(!frame.contains('\u{7}'), "BEL 进了帧：{frame:?}");
            assert_eq!(
                stray_escape(&frame),
                None,
                "帧里除协议自身的序列与调色板之外不许有 ESC：{frame:?}"
            );
        }
    }

    /// F2（终审修复波）：编辑器态里的 PgUp/PgDn 真的翻滚动区——步长 = 可视视口行数
    /// （`rows - 5`，至少 1），翻上去看得到旧行、PgDn 一路回到贴底；字节不进编辑器缓冲。
    #[test]
    fn page_keys_scroll_the_scrollback_and_return_to_the_bottom() {
        let (mut app, _buffer) = app();
        let facts = facts();
        app.set_size(Size { cols: 80, rows: 12 });
        let lines: Vec<String> = (1..=40).map(|index| format!("line {index}")).collect();
        app.push_lines(lines.clone());
        let visible = 12 - 5; // 步长与帧里的消息区行数是同一个口径

        // 帧里真的画出来的那些行（每行以清行序列收尾；80 列装得下，不会截断）
        let framed = |app: &TuiApp| -> Vec<String> {
            let frame = compose(&app.view(&facts));
            lines
                .iter()
                .filter(|line| frame.contains(&format!("{line}\u{1b}[K")))
                .cloned()
                .collect()
        };

        assert_eq!(framed(&app), lines[lines.len() - visible..], "一开始贴着底");

        // PgUp：翻上去一屏，看到的是更旧的那一段（最新一行离开视口）
        assert_eq!(app.on_bytes(b"\x1b[5~"), [Action::Redraw], "翻页要重绘一帧");
        assert_eq!(
            framed(&app),
            lines[lines.len() - 2 * visible..lines.len() - visible],
            "翻上去看得到旧行"
        );

        // 再翻一屏：更旧
        assert_eq!(app.on_bytes(b"\x1b[5~"), [Action::Redraw]);
        assert_eq!(
            framed(&app),
            lines[lines.len() - 3 * visible..lines.len() - 2 * visible]
        );

        // PgDn 一路回到底：逐字回到贴底那一段
        assert_eq!(app.on_bytes(b"\x1b[6~"), [Action::Redraw]);
        assert_eq!(
            framed(&app),
            lines[lines.len() - 2 * visible..lines.len() - visible]
        );
        assert_eq!(app.on_bytes(b"\x1b[6~"), [Action::Redraw]);
        assert_eq!(
            framed(&app),
            lines[lines.len() - visible..],
            "PgDn 回到贴底"
        );
        assert_eq!(app.input.text(), "", "翻页键一个字符都不进编辑器缓冲");

        // 贴底之后再按 PgDn：offset 不越界（不 panic、内容照旧）
        assert_eq!(app.on_bytes(b"\x1b[6~"), [Action::Redraw]);
        assert_eq!(framed(&app), lines[lines.len() - visible..]);
    }

    /// F4（终审修复波）：启动时（进守卫之前）注册好的信号流被真正接进应用——`wait_idle` 从它上面
    /// 收得到 SIGWINCH，且取出来等信号的两个流在等待结束后**还回去**（下一轮接着用同一份，
    /// 应用绝不重复注册）。
    #[tokio::test]
    async fn attached_signals_drive_the_idle_loop() {
        let (mut app, _buffer) = app();
        app.attach_signals(Signals::install());
        assert!(app.signals.is_some(), "接上之后应用手里就有一份信号流");

        // 每 10ms 给自己发一次 SIGWINCH：注册生效的话它必然先于 100ms 的心跳到达
        let raiser = tokio::spawn(async {
            for _ in 0..20 {
                // SAFETY: `raise` 只影响本进程，SIGWINCH 的默认处置本来就是「忽略」
                unsafe { libc::raise(libc::SIGWINCH) };
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        });
        let wake = app.wait_idle().await;
        raiser.abort();

        assert_eq!(wake, Wake::Sigwinch, "接上的 SIGWINCH 流必须被主循环用上");
        assert!(
            app.signals
                .as_ref()
                .is_some_and(|signals| signals.sigint.is_some() && signals.sigwinch.is_some()),
            "两个流都回到槽位里：下一轮接着用它们，不重复注册"
        );
    }

    /// 忙碌期输入禁用：`on_bytes` 一律没有动作，缓冲一个字符都不进；忙碌态只由前后两态
    /// 的显式调用决定。
    #[test]
    fn input_is_ignored_while_busy() {
        let (mut app, _buffer) = app();
        assert_eq!(app.busy(), None);

        app.begin_busy(Busy::Task);
        assert_eq!(app.busy(), Some(Busy::Task));
        assert!(app.on_bytes("你好".as_bytes()).is_empty(), "忙碌期不产动作");
        assert_eq!(app.input.text(), "", "忙碌期输入的字符一个都不进缓冲");

        // 忙碌期的输入区：dim 的 `working…` + 提示行 `Ctrl+C cancels`（没有光标）
        let block = app.view(&facts()).input;
        assert_eq!(
            block.lines,
            ["working…".to_string(), String::new(), BUSY_HINT.to_string()]
        );
        assert_eq!(block.cursor, None);
        // 上色时这两行是 dim（chrome 由合成器按槽位上色；`working…` 不是输入标记那一槽）
        let colored = app.view(&ViewFacts {
            color: true,
            ..facts()
        });
        let frame = compose(&colored);
        assert!(
            frame.contains("\u{1b}[2mworking…\u{1b}[0m\u{1b}[K"),
            "{frame:?}"
        );
        assert!(
            frame.contains(&format!("\u{1b}[2m{BUSY_HINT}\u{1b}[0m\u{1b}[K")),
            "{frame:?}"
        );

        app.end_busy();
        assert_eq!(app.busy(), None);
        assert_eq!(app.on_bytes(b"a"), [super::Action::Redraw], "回到编辑器");
        assert_eq!(app.input.text(), "a");
    }

    /// 非法 UTF-8：与经典共用同一份文案的一行告警进滚动区，并请求重绘。
    #[test]
    fn invalid_utf8_warns_once_into_the_scrollback() {
        let (mut app, _buffer) = app();
        let actions = app.on_bytes(b"\xff");
        assert!(
            actions.contains(&super::Action::Redraw),
            "告警必须触发一次重绘：{actions:?}"
        );
        let lines = app.view(&facts()).viewport;
        assert!(
            lines.iter().any(|line| line == INVALID_WARNING),
            "{lines:?}"
        );
    }

    /// 帧组装：顶部状态行、消息视口、输入区（含光标）与底部状态行各就各位，且与 `compose`
    /// 的产物对得上。
    #[test]
    fn view_puts_the_viewport_input_and_status_lines_in_place() {
        let (mut app, _buffer) = app();
        let facts = facts();
        app.set_size(Size { cols: 40, rows: 12 });
        app.push_lines(["答复一".to_string(), "第二行".to_string()]);
        app.on_bytes("你好".as_bytes());

        // 面板尺寸来自应用状态（帧组装不自己探终端）
        assert_eq!(app.view(&facts).size, Size { cols: 40, rows: 12 });
        // 80×24 下面板装得下这三行文案：与合成器的产物逐字对得上
        app.set_size(Size { cols: 80, rows: 24 });
        let view = app.view(&facts);
        assert_eq!(view.size, Size { cols: 80, rows: 24 });
        assert_eq!(view.color, facts.color);
        assert_eq!(view.theme, facts.theme);
        assert_eq!(view.top, top_line(&facts));
        assert_eq!(view.status, status_line(&facts, IDLE_HINT));
        assert_eq!(view.viewport, ["答复一", "第二行"]);
        assert_eq!(view.input, app.input.block(), "输入区就是输入态的那个块");
        assert_eq!(view.input.lines[0], "❯ 你好");
        assert_eq!(
            view.input.cursor,
            Some((0, 6)),
            "`❯ ` 2 列 + 文本 4 列（中文按显示宽度）"
        );

        // 与合成器的产物对得上：三块内容都逐字出现在帧里
        let frame = compose(&view);
        for line in [
            view.top.as_str(),
            view.input.lines[0].as_str(),
            view.status.as_str(),
        ] {
            assert!(frame.contains(line), "{line:?} 必须在帧里：{frame:?}");
        }
    }

    /// 忙碌子循环：每 tick 排空捕获并重绘；future 结束时把最后一半行落地。
    #[tokio::test]
    #[allow(clippy::await_holding_lock)] // 刻意跨 await 持槽：测试整段都要与 editor::raw / tui::term 的用例串行
    async fn drive_drains_the_capture_and_redraws_while_the_task_runs() {
        // 真进一次 raw mode（守卫走 `enter_guard`）就会写进程级的 `ACTIVE`：与 `editor::raw` /
        // `tui::term` 的用例共用同一条串行槽
        let _serial = serial_slot();
        let (_master, slave) = open_pty();
        let slave_fd = slave.as_raw_fd();
        set_winsize(slave_fd, 100, 30);
        let recorder = Recorder::default();
        let mut guard = enter_guard(&recorder, slave_fd);
        let (mut app, buffer) = app();
        let facts = facts();

        let outcome = app
            .drive(
                &mut guard,
                &facts,
                async {
                    // 驱动期间把捕获写进共享缓冲（渲染器的流式输出就是这么写的）
                    write(&buffer, "答复一\n".as_bytes());
                    // 让出到下一次心跳之后：tick 分支必须真的跑一轮（排空 + 重绘）
                    tokio::time::sleep(std::time::Duration::from_millis(150)).await;
                    write(&buffer, "答复二\n".as_bytes());
                    "done"
                },
                None,
            )
            .await;

        assert_eq!(outcome, "done");
        assert!(
            recorder.text().contains("答复一"),
            "驱动期间排空的捕获必须进了帧：{}",
            recorder.text()
        );
        assert!(
            buffer.lock().expect("捕获锁不该中毒").is_empty(),
            "收尾排空捕获"
        );
        assert_eq!(
            app.view(&facts).viewport,
            ["答复一", "答复二"],
            "future 末尾那一行也要落地（收尾 flush）"
        );
        assert_eq!(app.busy(), None, "drive 不自己改输入态（由调用方成对调用）");
    }

    /// SIGINT 不结束忙碌子循环：取消令牌交回 TUI 持有，future 收令返回、`drive` 正常收尾。
    #[tokio::test]
    #[allow(clippy::await_holding_lock)] // 同上：串行槽刻意持到测试结束
    async fn drive_sigint_does_not_end_the_busy_loop() {
        let _serial = serial_slot();
        let (_master, slave) = open_pty();
        let slave_fd = slave.as_raw_fd();
        let recorder = Recorder::default();
        let mut guard = enter_guard(&recorder, slave_fd);
        let (mut app, _buffer) = app();
        let facts = facts();
        app.begin_busy(Busy::Task);

        let cancel = CancellationToken::new();
        let trigger = cancel.clone();
        // 等价于 `drive` 的 SIGINT 分支：取消 TUI 持有的令牌（future 自己收令返回）
        tokio::spawn(async move {
            tokio::task::yield_now().await;
            trigger.cancel();
        });
        let token = cancel.clone();
        let outcome = app
            .drive(
                &mut guard,
                &facts,
                async move {
                    token.cancelled().await;
                    "cancelled"
                },
                Some(cancel),
            )
            .await;

        assert_eq!(outcome, "cancelled", "drive 交回 future 的输出，不自己收摊");
        assert!(
            recorder.text().contains("working…"),
            "忙碌态的输入区必须画出来：{}",
            recorder.text()
        );
    }

    // ---- 内联审批（T4） ----

    /// 一次审批请求（与 `approval.rs` 的用例同形：工具名/摘要/参数/风险各一格）。
    fn approval_request() -> ApprovalRequest {
        ApprovalRequest {
            tool: "fs_write".to_string(),
            summary: "Writing a.txt".to_string(),
            args: json!({ "path": "a.txt", "recursive": true }),
            risk: RiskLevel::Elevated,
        }
    }

    /// 一条审批请求 + 回信口的收端（用例在这一侧观察回信）。
    fn approval_job() -> (ApprovalJob, oneshot::Receiver<ApprovalDecision>) {
        let (reply, rx) = oneshot::channel();
        (
            ApprovalJob {
                request: approval_request(),
                reply,
            },
            rx,
        )
    }

    /// 内联审批的版式与键位（裁定 3）：4 行共用文案逐字进滚动区（3 行输入区放不下）、输入区首行
    /// 恰好是经典的 `choice> `、提示行 `a/s/r/d`；`a`/`s`/`r` 各回各的信并回到编辑器，`d` 打参数
    /// 明细（与经典同一份美化 JSON）后继续等，到上限的无效输入与 `Ctrl+D` 都 fail-safe 拒绝。
    #[tokio::test]
    async fn approval_keys_reply_to_the_pending_request() {
        let (mut app, _buffer) = app();
        let facts = facts();

        // 版式：四行提示落在滚动区，输入区是 `choice> ` + `a/s/r/d`
        let (job, mut rx) = approval_job();
        let landed = app.begin_approval(job);
        assert_eq!(
            landed,
            prompt_lines(&approval_request()),
            "落地的就是共用文案"
        );
        assert_eq!(
            app.view(&facts).viewport,
            prompt_lines(&approval_request()),
            "4 行提示逐字进滚动区"
        );
        let block = app.view(&facts).input;
        assert_eq!(block.lines[0], "choice> ", "输入区首行是经典的提示词");
        assert_eq!(block.lines[2], "a/s/r/d", "提示行");
        assert!(
            format_prompt(&approval_request()).ends_with(&block.lines[0]),
            "输入区首行与经典提示词的最后一段逐字一致"
        );
        assert!(rx.try_recv().is_err(), "还没按键：没有回信");
        assert_eq!(app.view(&facts).status, status_line(&facts, BUSY_HINT));

        // `a`：回信 AllowOnce、输入态回到审批开始时记下的忙碌态（**不是**编辑器——任务的 future
        // 还在跑）、编辑器缓冲一个字符都不进
        assert_eq!(app.on_bytes(b"a"), [super::Action::Redraw]);
        assert_eq!(rx.try_recv(), Ok(ApprovalDecision::AllowOnce));
        assert_eq!(
            app.busy(),
            Some(Busy::Task),
            "回信后回到记下的忙碌态（这条用例没走 `begin_busy`，按任务轮次兜底）"
        );
        assert_eq!(
            app.view(&facts).input.lines[0],
            "working…",
            "回信即回忙碌态，不是回编辑器"
        );
        assert_eq!(app.input.text(), "");

        // `s`/`r`（大写也归一）：每次各起一次审批
        for (key, want) in [
            ("s", ApprovalDecision::AllowSession),
            ("R", ApprovalDecision::Reject),
        ] {
            let (job, mut rx) = approval_job();
            app.begin_approval(job);
            app.on_bytes(key.as_bytes());
            assert_eq!(rx.try_recv(), Ok(want), "{key}");
            assert_eq!(app.view(&facts).input.lines[0], "working…", "{key}");
        }

        // `d`：参数明细（美化 JSON 逐行）落进滚动区，提示保持、继续等回信
        let (job, mut rx) = approval_job();
        app.begin_approval(job);
        let lines = app
            .approval_key(Key::Char('d'))
            .expect("`d` 的明细就是要落地的行");
        assert!(
            lines.contains(&"{".to_string())
                && lines.iter().any(|line| line.starts_with("  \"path\"")),
            "明细是美化 JSON：{lines:?}"
        );
        let viewport = app.view(&facts).viewport;
        assert!(viewport.contains(&"{".to_string()), "{viewport:?}");
        assert!(
            viewport.iter().any(|line| line.starts_with("  \"path\"")),
            "{viewport:?}"
        );
        assert_eq!(app.view(&facts).input.lines[0], "choice> ", "还在等回信");
        for line in prompt_lines(&approval_request()) {
            assert!(viewport.contains(&line), "提示仍在：{line:?}");
        }
        assert!(rx.try_recv().is_err(), "`d` 不回信");

        // 连续无效输入：到 `MAX_UNKNOWN_INPUTS` 才 fail-safe 拒绝（与经典同一常量）
        let (job, mut rx) = approval_job();
        app.begin_approval(job);
        for _ in 0..MAX_UNKNOWN_INPUTS - 1 {
            app.on_bytes(b"?");
            assert!(rx.try_recv().is_err(), "没到上限不拒绝");
            assert_eq!(app.view(&facts).input.lines[0], "choice> ", "还在审批态");
        }
        app.on_bytes(b"?");
        assert_eq!(
            rx.try_recv(),
            Ok(ApprovalDecision::Reject),
            "{MAX_UNKNOWN_INPUTS} 次无效输入"
        );
        assert_eq!(app.view(&facts).input.lines[0], "working…");

        // EOF（`Ctrl+D`）＝ Reject（fail-safe）
        let (job, mut rx) = approval_job();
        app.begin_approval(job);
        assert_eq!(app.approval_key(Key::CtrlD), None);
        assert_eq!(rx.try_recv(), Ok(ApprovalDecision::Reject));
        assert_eq!(app.view(&facts).input.lines[0], "working…");
    }

    /// 审批按键绝不落进编辑器（裁定 3）：审批态喂 `"abc"`，`a` 照契约回信，同一批剩下的按键随
    /// 审批结束一起丢掉（与「提交之后忽略剩余按键」同一条规则）；编辑器缓冲一个字符都不进，
    /// 也没有任何提交/退出动作——审批按键与编辑器缓冲是两条路。
    #[tokio::test]
    async fn approval_key_does_not_reach_the_editor() {
        let (mut app, _buffer) = app();
        let (job, mut rx) = approval_job();
        app.begin_approval(job);

        let actions = app.on_bytes(b"abc");

        assert_eq!(app.input.text(), "", "审批按键一个字符都不进编辑器缓冲");
        assert_eq!(
            rx.try_recv(),
            Ok(ApprovalDecision::AllowOnce),
            "`a` 是审批键：照契约回信"
        );
        assert!(
            !actions
                .iter()
                .any(|action| matches!(action, super::Action::Submit(_) | super::Action::Exit)),
            "审批态不产编辑器的动作：{actions:?}"
        );
        assert_eq!(
            app.view(&facts()).input.lines[0],
            "working…",
            "回信后回到忙碌态：编辑器还没轮到"
        );
        assert!(rx.try_recv().is_err(), "只有一次回信");

        // 编辑器要等 `end_busy()`（任务的 future 结束）才轮到：`b`/`c` 这一次才进缓冲
        app.end_busy();
        assert_eq!(app.view(&facts()).input.lines[0], "❯ ");
        assert_eq!(
            app.on_bytes(b"bc"),
            [super::Action::Redraw, super::Action::Redraw]
        );
        assert_eq!(app.input.text(), "bc");
    }

    /// 审批与任务 future 在同一个 `select!` 里推进（裁定 4：无「UI 等 UI」死锁）：请求到达 → 提示
    /// 落进滚动区 → 输入态切到审批；`Ctrl+C` 的字节与 SIGINT 同义（裁定 3）：Reject + 取消本轮
    /// 任务令牌。
    #[tokio::test]
    async fn drive_receives_approvals_and_ctrl_c_rejects_and_cancels_the_token() {
        let (mut app, _buffer) = app();
        let facts = facts();
        let (handler, approvals) = TuiApproval::channel();
        let (chunks_tx, chunks) = mpsc::unbounded_channel();
        app.attach_approvals(approvals);
        app.attach_chunks(chunks);

        // 审批处理器在另一个任务上等回信（UI 一收下请求就会画进滚动区）
        let deciding = tokio::spawn(async move { handler.decide(approval_request()).await });
        let cancel = CancellationToken::new();
        // 等审批态真的建立（第一轮 select 就会收下通道里那条请求）再送 `Ctrl+C` 的字节
        let bytes = chunks_tx.clone();
        let sender = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(50)).await;
            let _ = bytes.send(InputChunk::Bytes(b"\x03".to_vec()));
        });

        let token = cancel.clone();
        let outcome = app
            .drive_headless(
                &facts,
                async move {
                    cancel.cancelled().await;
                    "cancelled"
                },
                Some(token),
            )
            .await;

        sender.await.expect("送字节的任务应结束");
        assert_eq!(outcome, "cancelled", "`Ctrl+C` 的字节取消了本轮令牌");
        assert_eq!(
            deciding.await.expect("审批任务应结束"),
            ApprovalDecision::Reject
        );
        assert_eq!(
            app.view(&facts).viewport,
            prompt_lines(&approval_request()),
            "请求被 UI 收下并画进滚动区（与 future 同一个 select，没有死锁）"
        );
        assert_eq!(
            app.view(&facts).input.lines[0],
            "working…",
            "回信后回忙碌态（任务的 future 还在跑），不是编辑器"
        );
    }

    /// 审批回信之后任务 future 还在跑（T3 的单任务策略）：`end_approval` 恢复的是审批开始时记下的
    /// 忙碌态，**不是**编辑器——输入区首行仍是 `working…`（既没有 `❯ ` 提示词、也没有
    /// `PgUp/PgDn scroll`）、喂键产不出动作、编辑器缓冲一个字符都不进、`busy()` 也是 `Some(resume)`；
    /// 只有 `end_busy()`（future 结束）才回编辑器、按键才重新进缓冲。
    #[tokio::test]
    async fn approval_reply_restores_the_busy_state_instead_of_the_editor() {
        let (mut app, _buffer) = app();
        let facts = facts();

        // 三档忙碌态各起一次审批：回信恢复的就是各档自己（不是写死某一档）
        for busy in [Busy::Task, Busy::Compact, Busy::Learn] {
            app.begin_busy(busy);
            let (job, mut rx) = approval_job();
            app.begin_approval(job);
            assert_eq!(app.busy(), Some(busy), "审批叠在忙碌期上：忙碌态照旧");
            assert_eq!(
                app.view(&facts).input.lines[0],
                "choice> ",
                "审批态还是经典提示词"
            );

            assert_eq!(app.on_bytes(b"a"), [super::Action::Redraw]);
            assert_eq!(rx.try_recv(), Ok(ApprovalDecision::AllowOnce));
            assert_eq!(app.busy(), Some(busy), "回信后恢复记下的忙碌态");
            let block = app.view(&facts).input;
            assert_eq!(block.lines[0], "working…", "输入区首行是忙碌行，不是 `❯ `");
            assert_eq!(block.lines[2], BUSY_HINT, "提示行也回到忙碌那一句");
            assert_eq!(block.cursor, None, "忙碌期藏起光标");
            assert_eq!(
                app.view(&facts).status,
                status_line(&facts, BUSY_HINT),
                "底部状态行也是忙碌那一句（不是 `PgUp/PgDn scroll`）"
            );
        }

        // 任务还在跑：`x\r` 一个动作都产不出来（更没有 `Submit`），编辑器缓冲一个字符都不进
        let actions = app.on_bytes(b"x\r");
        assert!(actions.is_empty(), "忙碌期输入禁用：{actions:?}");
        assert!(
            !actions
                .iter()
                .any(|action| matches!(action, super::Action::Submit(_))),
            "忙碌期产不出 `Submit`：{actions:?}"
        );
        assert_eq!(app.input.text(), "", "缓冲一个字符都不进");

        // 只有 `end_busy()`（任务的 future 结束）才回编辑器：这之后按键才进缓冲、Enter 才提交
        app.end_busy();
        assert_eq!(app.busy(), None);
        assert_eq!(app.view(&facts).input.lines[0], "❯ ");
        assert_eq!(
            app.on_bytes(b"x\r"),
            [
                super::Action::Redraw,
                super::Action::Submit("x".to_string())
            ]
        );
        assert_eq!(app.input.text(), "", "提交后缓冲清空");
    }

    /// 循环级：审批在任务 future 推进期间回信之后，界面必须留在忙碌态——回信键那一批之后紧跟的
    /// 编辑器按键（`x`）也不进缓冲，要等 `end_busy()`（future 结束，调用方在 `drive` 之后调）才回
    /// 编辑器。审批态在进循环前就建立（与生产同序：`begin_busy` → 收下请求 → `drive`），两条输入
    /// 块的先后由通道 FIFO 决定——没有时序竞态。
    #[tokio::test]
    async fn approval_reply_inside_the_loop_stays_busy_until_the_future_ends() {
        let (mut app, _buffer) = app();
        let facts = facts();
        let (chunks_tx, chunks) = mpsc::unbounded_channel();
        app.attach_chunks(chunks);
        app.begin_busy(Busy::Task);
        let (job, mut rx) = approval_job();
        app.begin_approval(job);

        let bytes = chunks_tx.clone();
        let outcome = app
            .drive_headless(
                &facts,
                async move {
                    // 回信键先进审批面：`a` → AllowOnce、界面回忙碌态
                    let _ = bytes.send(InputChunk::Bytes(b"a".to_vec()));
                    // 紧跟着的编辑器按键（同一轮任务里用户敲的）：忙碌期必须一个字符都不收
                    let _ = bytes.send(InputChunk::Bytes(b"x".to_vec()));
                    // 让出到下一次心跳之后：两条输入块都排到（通道 FIFO），future 还在跑
                    tokio::time::sleep(Duration::from_millis(150)).await;
                    "done"
                },
                None,
            )
            .await;

        assert_eq!(outcome, "done");
        assert_eq!(
            rx.try_recv(),
            Ok(ApprovalDecision::AllowOnce),
            "`a` 在循环里回信"
        );
        assert_eq!(app.busy(), Some(Busy::Task), "future 结束前一直在忙碌态");
        assert_eq!(
            app.view(&facts).input.lines[0],
            "working…",
            "输入区首行还是忙碌行（不是 `❯ `）"
        );
        assert_eq!(app.input.text(), "", "回信之后的按键一个字符都不进缓冲");

        // 只有 `end_busy()`（future 结束）回编辑器
        app.end_busy();
        assert_eq!(app.busy(), None);
        assert_eq!(app.view(&facts).input.lines[0], "❯ ");
    }

    // ---- 内联确认（T5） ----

    /// 一次确认请求 + 回信口的收端（用例在这一侧观察回信）。
    fn confirm_job(name: &str, skill_md: &str) -> (ConfirmJob, oneshot::Receiver<bool>) {
        let (reply, rx) = oneshot::channel();
        (
            ConfirmJob {
                name: name.to_string(),
                skill_md: skill_md.to_string(),
                reply,
            },
            rx,
        )
    }

    /// 内联确认的版式与键位：头行 + 预览逐行进滚动区（共用文案，不另写截断）、输入区第 1 行恰好是
    /// 经典的 `LEARN_PROMPT`；只有 `s`（trim + 大小写归一）回 `true`，`d`/`q`/`Ctrl+D` 都回
    /// `false`；回信只送一次；`Ctrl+C` 回 `false` 且取消本轮令牌。
    #[tokio::test]
    async fn tui_confirm_pushes_the_header_and_preview_and_awaits_s() {
        let (mut app, _buffer) = app();
        let facts = facts();
        let skill_md = "---\nname: demo-skill\ndescription: Demo draft.\n---\n# 步骤\n1. 做事\n";

        // 版式：头行 + 预览行逐字进滚动区，输入区首行是经典的确认提示词
        app.begin_busy(Busy::Learn);
        let (job, mut rx) = confirm_job("demo-skill", skill_md);
        let landed = app.begin_confirm(job);
        let mut want = vec![
            "draft skill demo-skill: untested, not deployed or executed automatically".to_string(),
        ];
        want.extend(preview_lines(skill_md));
        assert_eq!(landed, want, "落地的就是共用文案（头行 + 预览）");
        assert_eq!(
            landed.first().unwrap(),
            &draft_header("demo-skill"),
            "头行来自共用函数"
        );
        assert_eq!(app.view(&facts).viewport, want, "滚动区逐字收到");
        let block = app.view(&facts).input;
        assert_eq!(block.lines[0], LEARN_PROMPT, "输入区第 1 行就是经典提示词");
        assert_eq!(block.lines[0], "save this skill draft? [s]ave [d]iscard> ");
        assert_eq!(block.lines[2], "s/d", "提示行");
        assert!(rx.try_recv().is_err(), "还没按键：没有回信");
        assert_eq!(app.busy(), Some(Busy::Learn), "确认叠在忙碌期上");
        assert_eq!(app.view(&facts).status, status_line(&facts, BUSY_HINT));

        // `s`：回信 `true`，输入态回到记下的忙碌态（不是编辑器）；回信只有一次
        assert_eq!(app.on_bytes(b"s"), [super::Action::Redraw]);
        assert_eq!(rx.try_recv(), Ok(true));
        assert_eq!(app.busy(), Some(Busy::Learn), "回信后仍是忙碌态");
        assert_eq!(app.view(&facts).input.lines[0], "working…", "不是回编辑器");
        assert!(rx.try_recv().is_err(), "只有一次回信");
        assert_eq!(
            app.confirm_key(Key::Char('s')),
            None,
            "回信之后不再受理按键"
        );

        // `S`（大小写归一）也保存；`d`/`q` 与 EOF（`Ctrl+D`）都放弃，且都留在忙碌态
        for (key, want) in [("S", true), ("d", false), ("q", false), ("\u{4}", false)] {
            let (job, mut rx) = confirm_job("demo-skill", skill_md);
            app.begin_confirm(job);
            app.on_bytes(key.as_bytes());
            assert_eq!(rx.try_recv(), Ok(want), "{key:?}");
            assert_eq!(app.busy(), Some(Busy::Learn), "{key:?}");
        }

        // 裸按键入口：`Ctrl+D`（EOF）也是放弃
        let (job, mut rx) = confirm_job("demo-skill", skill_md);
        app.begin_confirm(job);
        assert_eq!(app.confirm_key(Key::CtrlD), Some(false));
        assert_eq!(rx.try_recv(), Ok(false));

        // `Ctrl+C` 的字节与 SIGINT 同义：回 `false` 并取消 TUI 持有的本轮令牌
        let cancel = CancellationToken::new();
        app.cancel = Some(cancel.clone());
        let (job, mut rx) = confirm_job("demo-skill", skill_md);
        app.begin_confirm(job);
        assert_eq!(app.confirm_key(Key::CtrlC), Some(false));
        assert!(cancel.is_cancelled(), "`Ctrl+C` 必须取消本轮令牌");
        assert_eq!(rx.try_recv(), Ok(false));
        app.cancel = None;

        // 确认按键绝不落进编辑器：喂 `"abc"`（`a` 是「其它按键」→ 放弃），缓冲一个字符都不进
        let (job, mut rx) = confirm_job("demo-skill", skill_md);
        app.begin_confirm(job);
        assert_eq!(app.on_bytes(b"abc"), [super::Action::Redraw]);
        assert_eq!(rx.try_recv(), Ok(false));
        assert_eq!(app.input.text(), "", "确认按键一个字符都不进缓冲");
        assert!(rx.try_recv().is_err(), "只有一次回信");
    }

    /// 预览的截断只来自 `commands::learn` 的共享常量（不另写一份）：130 行的 skill_md 给出
    /// `LEARN_PREVIEW_LINES + 1` 行预览、末行是截断标记。
    #[tokio::test]
    async fn confirm_preview_caps_come_from_the_shared_constants() {
        let (mut app, _buffer) = app();
        let facts = facts();
        let long = (0..130)
            .map(|index| format!("line {index}"))
            .collect::<Vec<_>>()
            .join("\n");

        app.begin_busy(Busy::Learn);
        let (job, _rx) = confirm_job("demo-skill", &long);
        let landed = app.begin_confirm(job);

        // 头行 + 预览；预览完全来自共享函数（行数上限 + 标记）
        assert_eq!(landed.len(), LEARN_PREVIEW_LINES + 2, "头行 + 预览 + 标记");
        assert_eq!(landed[0], draft_header("demo-skill"));
        assert_eq!(landed[1..], preview_lines(&long), "预览就是共享函数的产物");
        assert_eq!(landed[1..].len(), LEARN_PREVIEW_LINES + 1);
        assert_eq!(landed.last().unwrap(), LEARN_PREVIEW_MARKER);
        // 滚动区的视口里出现截断标记（视口就是最近 `rows` 行，即落地那批行的尾巴）
        let viewport = app.view(&facts).viewport;
        assert_eq!(
            viewport,
            landed[landed.len() - viewport.len()..],
            "视口就是最近落地的那些行"
        );
        assert_eq!(viewport.last().unwrap(), LEARN_PREVIEW_MARKER);
        assert_eq!(
            viewport
                .iter()
                .filter(|line| *line == LEARN_PREVIEW_MARKER)
                .count(),
            1,
            "截断标记只出现一次：{viewport:?}"
        );
    }

    /// 确认回信之后草稿的 future 还在跑（T4 裁定的同一形状）：`end_confirm` 恢复确认开始时记下的
    /// 忙碌态、**不是**编辑器；输入区首行仍是 `working…`、提示行与底部状态行也回到忙碌那一句，
    /// 喂键产不出动作、编辑器缓冲一个字符都不进；只有 `end_busy()`（future 结束）才回编辑器。
    #[tokio::test]
    async fn confirm_reply_restores_the_busy_state_instead_of_the_editor() {
        let (mut app, _buffer) = app();
        let facts = facts();

        app.begin_busy(Busy::Learn);
        let (job, mut rx) = confirm_job("demo-skill", "---\nname: demo-skill\n---\n");
        app.begin_confirm(job);
        assert_eq!(
            app.busy(),
            Some(Busy::Learn),
            "确认叠在忙碌期上：忙碌态照旧"
        );
        assert_eq!(
            app.view(&facts).input.lines[0],
            LEARN_PROMPT,
            "确认态是经典提示词"
        );

        assert_eq!(app.on_bytes(b"s"), [super::Action::Redraw]);
        assert_eq!(rx.try_recv(), Ok(true));
        assert_eq!(app.busy(), Some(Busy::Learn), "回信后恢复记下的忙碌态");
        let block = app.view(&facts).input;
        assert_eq!(block.lines[0], "working…", "输入区首行是忙碌行，不是 `❯ `");
        assert_eq!(block.lines[2], BUSY_HINT, "提示行也回到忙碌那一句");
        assert_eq!(block.cursor, None, "忙碌期藏起光标");
        assert_eq!(
            app.view(&facts).status,
            status_line(&facts, BUSY_HINT),
            "底部状态行也是忙碌那一句（不是 `PgUp/PgDn scroll`）"
        );

        // future 还在跑：`x\r` 一个动作都产不出来（更没有 `Submit`），缓冲一个字符都不进
        let actions = app.on_bytes(b"x\r");
        assert!(actions.is_empty(), "忙碌期输入禁用：{actions:?}");
        assert_eq!(app.input.text(), "", "缓冲一个字符都不进");

        // 只有 `end_busy()`（future 结束）才回编辑器：这之后按键才进缓冲、Enter 才提交
        app.end_busy();
        assert_eq!(app.busy(), None);
        assert_eq!(app.view(&facts).input.lines[0], "❯ ");
        assert_eq!(
            app.on_bytes(b"x\r"),
            [
                super::Action::Redraw,
                super::Action::Submit("x".to_string())
            ]
        );
    }

    /// 循环级：确认请求由 `drive` 的接收分支收下（与草稿的 future 同一个 `select!`），`s` 的字节
    /// 回信 `true`；界面在 future 结束前留在忙碌态，`end_busy()` 之后才回编辑器。请求在进循环前
    /// 就已入队（与生产同序：`begin_busy` → 接上确认面 → 收下请求 → `drive`），按键的先后由 50ms
    /// 的等待保证——没有时序竞态。
    #[tokio::test]
    async fn drive_receives_confirms_and_stays_busy_until_the_future_ends() {
        let (mut app, _buffer) = app();
        let facts = facts();
        let (chunks_tx, chunks) = mpsc::unbounded_channel();
        app.attach_chunks(chunks);
        let (tx, confirms) = confirm::channel();
        app.attach_confirms(confirms);
        app.begin_busy(Busy::Learn);

        // 请求先入队：第一轮 select 收下它、输入态切到确认；回信由 `s` 的字节给出
        let (reply, answered) = oneshot::channel();
        tx.send(ConfirmJob {
            name: "demo-skill".to_string(),
            skill_md: "---\nname: demo-skill\n---\n".to_string(),
            reply,
        })
        .expect("确认通道还在");
        let bytes = chunks_tx.clone();
        let sender = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(50)).await;
            let _ = bytes.send(InputChunk::Bytes(b"s".to_vec()));
        });

        let outcome = app
            .drive_headless(&facts, async move { answered.await.unwrap_or(false) }, None)
            .await;

        sender.await.expect("送字节的任务应结束");
        assert!(outcome, "`s` 的字节回了 `true`");
        assert!(
            app.view(&facts)
                .viewport
                .contains(&draft_header("demo-skill")),
            "确认请求被 UI 收下并画进滚动区（与 future 同一个 select，没有死锁）：{:?}",
            app.view(&facts).viewport
        );
        assert_eq!(app.busy(), Some(Busy::Learn), "future 结束前一直在忙碌态");
        assert_eq!(
            app.view(&facts).input.lines[0],
            "working…",
            "输入区首行还是忙碌行（不是 `❯ `）"
        );

        // 只有 `end_busy()`（future 结束）回编辑器
        app.end_busy();
        assert_eq!(app.busy(), None);
        assert_eq!(app.view(&facts).input.lines[0], "❯ ");
    }

    // ---- 选择器（T6） ----

    /// 一条会话行（与 `/sessions` 读到的同一形状）。
    fn session_summary(
        id: &str,
        title: &str,
        updated_at: i64,
        message_count: usize,
    ) -> SessionSummary {
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

    /// 选择器：三行 + 当前会话 → `open_selector` 后视口就是带前缀的表格；↑↓ 移动、Enter 交回整条
    /// id 并关闭、Esc/Ctrl+D 关闭且不交回动作；空表也能开（内容是 `(no sessions)`，Enter 不动作）；
    /// `on_tick` 上的裸 ESC 合成一次 Esc 路由——选择器认它（关闭），编辑器忽略。
    #[test]
    fn selector_moves_and_resumes() {
        let newest = "019a1b2c-3333-7f00-8000-000000000003";
        let middle = "019a1b2c-2222-7f00-8000-000000000002";
        let oldest = "019a1b2c-1111-7f00-8000-000000000001";
        let rows = vec![
            session_summary(newest, "最新一条", 300, 4),
            session_summary(middle, "中间一条", 200, 3),
            session_summary(oldest, "最旧一条", 100, 2),
        ];
        let (mut app, _buffer) = app();
        let facts = facts();

        // 打开：输入态是选择器（没有光标、提示行是选择器那一句），视口是 `selector_lines` 的产物
        app.open_selector(rows.clone(), Some(middle.to_string()));
        assert!(matches!(app.mode, InputMode::Selector { .. }));
        assert_eq!(app.busy(), None, "选择器是空闲期的一态，不是忙碌");
        let view = app.view(&facts);
        assert_eq!(view.input.cursor, None, "选择器不落光标");
        assert_eq!(
            view.input.lines[2], SELECTOR_HINT,
            "输入区提示行换选择器那一句"
        );
        assert_eq!(view.viewport, selector_lines(&rows, Some(middle), 0));
        assert!(
            view.viewport[1].starts_with("> "),
            "第一条是选中行：{:?}",
            view.viewport
        );
        assert!(
            view.viewport[2].contains(&format!("*{}", &middle[..13])),
            "当前会话带 `*`：{:?}",
            view.viewport
        );

        // ↑ 在第一条上不动（不越界）；↓ 到第二条
        assert_eq!(app.selector_key(Key::Up), [Action::Redraw]);
        assert_eq!(
            app.view(&facts).viewport,
            selector_lines(&rows, Some(middle), 0)
        );
        assert_eq!(app.selector_key(Key::Down), [Action::Redraw]);
        assert_eq!(
            app.view(&facts).viewport,
            selector_lines(&rows, Some(middle), 1)
        );
        assert!(
            app.view(&facts).viewport[2].starts_with("> "),
            "第二条是选中行：{:?}",
            app.view(&facts).viewport
        );

        // Enter：交回第二条的整条 id 并关闭选择器（回到编辑器）
        assert_eq!(
            app.selector_key(Key::Enter),
            [Action::Resume(middle.to_string())]
        );
        assert!(matches!(app.mode, InputMode::Editor), "交回即关闭");
        assert_eq!(app.view(&facts).input.lines[0], "❯ ", "回到编辑器");

        // Esc 与 Ctrl+D 都关闭、都不交回动作；关闭之后按键回到编辑器
        for key in [Key::Esc, Key::CtrlD] {
            app.open_selector(rows.clone(), None);
            let actions = app.selector_key(key);
            assert!(
                !actions
                    .iter()
                    .any(|action| matches!(action, Action::Resume(_))),
                "{key:?}: {actions:?}"
            );
            assert!(matches!(app.mode, InputMode::Editor), "{key:?} 关闭选择器");
            assert_eq!(
                app.on_bytes(b"a"),
                [Action::Redraw],
                "{key:?} 之后按键回编辑器"
            );
            assert_eq!(app.input.text(), "a");
            app.on_bytes(b"\x15"); // 清空：下一次循环从空缓冲开始
        }

        // `on_tick` 的 Esc 消歧：裸 ESC 合成一次 Esc 路由——选择器认它（关闭），其余输入态忽略
        app.open_selector(rows, None);
        assert!(
            app.on_bytes(b"\x1b").is_empty(),
            "裸 ESC 先留在解码器缓存里"
        );
        assert!(app.on_tick(), "tick 上吃到裸 ESC：选择器关闭，要重绘");
        assert!(matches!(app.mode, InputMode::Editor), "Esc 关闭选择器");
        assert!(app.on_bytes(b"\x1b").is_empty());
        assert!(!app.on_tick(), "编辑器不认 Esc：没有变化");

        // 空表也开：内容就是 `(no sessions)`；Enter 没有可选项（不动作、也不关）
        app.open_selector(Vec::new(), None);
        assert_eq!(app.view(&facts).viewport, ["(no sessions)"]);
        assert_eq!(
            app.selector_key(Key::Down),
            [Action::Redraw],
            "空表上移动不越界"
        );
        assert!(
            app.selector_key(Key::Enter).is_empty(),
            "空表上 Enter 不动作"
        );
        assert!(
            matches!(app.mode, InputMode::Selector { .. }),
            "空表上 Enter 不关选择器（Esc/Ctrl+D 才关）"
        );
    }

    /// 其余按键一律忽略：普通字符/未绑定键/Tab/退格都不产动作、不关选择器、绝不落进编辑器缓冲；
    /// 忽略之后选择器照常工作。
    #[test]
    fn selector_key_ignores_other_keys() {
        let id = "019a1b2c-2222-7f00-8000-000000000002";
        let rows = vec![session_summary(id, "一条会话", 100, 2)];
        let (mut app, _buffer) = app();
        let facts = facts();

        app.open_selector(rows.clone(), Some(id.to_string()));
        for key in [
            Key::Char('a'),
            Key::Other,
            Key::Tab,
            Key::Backspace,
            // F2 新增的两个变体在选择器里同样「其余忽略」（翻页只归编辑器态的滚动区）
            Key::PageUp,
            Key::PageDown,
        ] {
            assert!(app.selector_key(key).is_empty(), "{key:?} 应被忽略");
            assert!(
                app.on_bytes(b"a").is_empty(),
                "{key:?} 之后也没有编辑器的动作"
            );
        }
        assert!(
            matches!(app.mode, InputMode::Selector { .. }),
            "忽略的按键不该关掉选择器"
        );
        assert_eq!(app.input.text(), "", "选择器的按键一个字符都不进编辑器缓冲");
        assert_eq!(
            app.view(&facts).viewport,
            selector_lines(&rows, Some(id), 0)
        );

        // 忽略这些按键之后选择器照常工作
        assert_eq!(
            app.selector_key(Key::Enter),
            [Action::Resume(id.to_string())]
        );
    }

    /// F3（终审修复波）：选择器列表围绕光标开窗——`rows = 8` 的终端给列表 `rows - 5` = 3 行，
    /// 选中行（`> `）始终在窗口里，窗口随光标移动（贴头、随行、贴尾）。
    #[test]
    fn selector_window_keeps_the_cursor_row_visible_at_eight_rows() {
        let rows: Vec<SessionSummary> = (0..6)
            .map(|index| {
                session_summary(
                    &format!("019a1b2c-2222-7f00-8000-00000000000{index}"),
                    &format!("会话 {index}"),
                    100 + index,
                    1,
                )
            })
            .collect();
        let (mut app, _buffer) = app();
        let facts = facts();
        app.set_size(Size { cols: 60, rows: 8 });
        app.open_selector(rows.clone(), None);
        let visible = 8 - 5;

        // 游标在第 0 行：窗口贴着列表头（表头 + 第 1、2 行），选中行在窗口里
        let viewport = app.view(&facts).viewport;
        assert_eq!(viewport.len(), visible, "8 行终端给列表 3 行：{viewport:?}");
        assert_eq!(
            viewport,
            selector_lines(&rows, None, 0)[..visible].to_vec(),
            "贴头"
        );
        assert!(viewport[1].starts_with("> "), "{viewport:?}");

        // ↓ 到第 4 行（下标 3）：窗口跟着移到第 2..5 行，`> ` 行仍在窗口里
        for _ in 0..3 {
            assert_eq!(app.selector_key(Key::Down), [Action::Redraw]);
        }
        let viewport = app.view(&facts).viewport;
        assert_eq!(viewport.len(), visible);
        assert_eq!(
            viewport,
            selector_lines(&rows, None, 3)[2..5].to_vec(),
            "窗口随光标移动"
        );
        assert!(
            viewport[2].starts_with("> "),
            "> 行必须在窗口里：{viewport:?}"
        );

        // 再到最后一行（下标 5）：窗口贴着列表尾，`> ` 行还是看得见（表头滚出去了）
        for _ in 0..2 {
            assert_eq!(app.selector_key(Key::Down), [Action::Redraw]);
        }
        let viewport = app.view(&facts).viewport;
        assert_eq!(
            viewport,
            selector_lines(&rows, None, 5)[4..7].to_vec(),
            "到底时窗口贴着列表尾"
        );
        assert!(
            viewport[2].starts_with("> "),
            "最后一行也看得见：{viewport:?}"
        );
        assert!(
            !viewport.iter().any(|line| line.starts_with("  ID")),
            "表头随窗口滚出：{viewport:?}"
        );

        // 列表装得下时不开窗（既有断言的口径不变）：换一个短表
        app.open_selector(rows[..1].to_vec(), None);
        assert_eq!(
            app.view(&facts).viewport,
            selector_lines(&rows[..1], None, 0),
            "短表原样全给"
        );
    }
}
