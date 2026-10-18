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

use std::future::Future;
use std::io::Read;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use ignis_runtime::{ApprovalDecision, ApprovalRequest};
use tokio::signal::unix::{Signal, SignalKind, signal};
use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;

use crate::approval::{Choice, MAX_UNKNOWN_INPUTS, details_json, parse_choice, prompt_lines};
use crate::editor::history::History;
use crate::editor::keys::Key;
use crate::editor::reader::INVALID_WARNING;
use crate::render::width::display_width;
use crate::tui::Size;
use crate::tui::approval::ApprovalJob;
use crate::tui::capture::{self, CaptureSink};
use crate::tui::input::InputState;
use crate::tui::term;
use crate::tui::view::{
    InputBlock, Scrollback, TuiView, ViewFacts, compose, status_line, top_line,
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
    /// `/learn`：草稿生成（确认面在 T5）。
    Learn,
}

/// 输入模式：T3 只有编辑器与忙碌两态；T4/T5/T6 各自**追加**一个变体（`Approval`/`Confirm`/
/// `Selector`）。审批态带 [`oneshot::Sender`]，所以这里只派生 `Debug`（不再有 `Clone`/`Eq`）。
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
    },
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
    /// 本轮的取消令牌（`drive` 进入时挂上、退出时摘掉）：审批态的 `Ctrl+C` 字节与 SIGINT 同义，
    /// 靠它取消本轮任务（`/compact` `/learn` 的令牌；任务轮次给 `None`，取消在 `run_task` 内部）。
    cancel: Option<CancellationToken>,
    /// SIGINT 信号流（第一次用到时装上；装不上就永远挂起）。
    sigint: Option<Signal>,
    /// SIGWINCH 信号流（同上）。
    sigwinch: Option<Signal>,
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
            cancel: None,
            sigint: None,
            sigwinch: None,
        }
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
    /// 审批期的按键走 [`TuiApp::approval_key`]（**绝不进编辑器**）。
    pub fn on_bytes(&mut self, bytes: &[u8]) -> Vec<Action> {
        if matches!(self.mode, InputMode::Approval { .. }) {
            return self.approval_bytes(bytes);
        }
        if !matches!(self.mode, InputMode::Editor) {
            return Vec::new();
        }
        let mut actions = self.input.on_bytes(bytes);
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
                // 回信即结束审批：界面回到编辑器（要重绘一帧），剩下的按键随这一批丢掉
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

    /// 每 tick：排空捕获缓冲、消化「一个裸 ESC」；返回是否需要重绘。
    pub fn on_tick(&mut self) -> bool {
        // Esc 的消歧窗口 = 一个 tick：T3 没有 Esc 语义（选择器在 T6），吃掉即可
        let _ = self.input.take_lone_escape();
        self.ingest()
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
    /// 先把捕获里没换行的尾巴落地：这些行必须各占一行（不与半行粘在一起）。
    pub fn push_lines(&mut self, lines: impl IntoIterator<Item = String>) {
        self.scrollback.flush_pending();
        for line in lines {
            self.scrollback.push_text(&line);
            self.scrollback.push_text("\n");
        }
    }

    /// 进入忙碌态：输入禁用（输入区第 1 行换成 dim 的 `working…`）。
    pub fn begin_busy(&mut self, busy: Busy) {
        self.mode = InputMode::Busy(busy);
    }

    /// 收到一次审批请求：共用文案（[`prompt_lines`]，与经典 REPL 同一份）4 行逐字进滚动区
    /// ——3 行输入区放不下 4 行提示 + `choice> `，正文由滚动区承担；输入态切到审批。
    ///
    /// 返回落地的那些行（调用方据此重绘一帧；滚动区已经写好了）。工具调用是串行的，同一时刻
    /// 只会有一个待审批请求；真撞上第二个时前一个随替换丢掉——它的回信端掉落，那次审批因此
    /// fail-safe 拒绝（与 [`crate::tui::approval::TuiApproval`] 同一口径）。
    pub fn begin_approval(&mut self, job: ApprovalJob) -> Vec<String> {
        let lines = prompt_lines(&job.request);
        self.push_lines(lines.clone());
        self.mode = InputMode::Approval {
            request: job.request,
            reply: Some(job.reply),
            unknown: 0,
        };
        lines
    }

    /// 审批按键（`a`/`s`/`r`/`d`，大小写与空白归一——与经典 [`parse_choice`] 同一份）：`a`/`s`/`r`
    /// 回信并回到编辑器；`d` 把参数明细（与经典同一份美化 JSON）逐行打进滚动区后继续等；连续
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

    /// 收下审批态：回信（回信端已经没了就算了）并回到编辑器。
    fn end_approval(&mut self, decision: ApprovalDecision) {
        // 用编辑器态换出审批态：请求与无效输入计数随替换一起丢掉
        if let InputMode::Approval { reply, .. } =
            std::mem::replace(&mut self.mode, InputMode::Editor)
            && let Some(reply) = reply
        {
            let _ = reply.send(decision);
        }
    }

    /// 离开忙碌态：回到编辑器（缓冲是空的，光标在段首）。
    pub fn end_busy(&mut self) {
        self.mode = InputMode::Editor;
    }

    /// 当前是不是忙碌态（`None` = 空闲；审批自成一态，不算忙碌）。
    pub fn busy(&self) -> Option<Busy> {
        match self.mode {
            InputMode::Busy(busy) => Some(busy),
            InputMode::Editor | InputMode::Approval { .. } => None,
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
            InputMode::Editor => (self.input.block(), IDLE_HINT),
        };
        TuiView {
            size: self.size,
            color: facts.color,
            theme: facts.theme,
            top: top_line(facts),
            // 视口就是「最近 rows 行」：版式切到该显示的那一段是合成器的事
            viewport: self.scrollback.viewport(self.size.rows),
            input,
            status: status_line(facts, hint),
        }
    }

    /// 空闲期等到的一件事（主循环按它分派）。
    pub(crate) async fn wait_idle(&mut self) -> Wake {
        self.install_signals();
        let mut chunks = self.chunks.take();
        let mut sigint = self.sigint.take();
        let mut sigwinch = self.sigwinch.take();
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
        self.sigint = sigint;
        self.sigwinch = sigwinch;
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
        let mut sigint = self.sigint.take();
        let mut sigwinch = self.sigwinch.take();
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
                chunk = next_chunk(&mut chunks) => {
                    // 忙碌期输入禁用：字节照收（别留给空闲期），动作一律丢掉；审批态的按键由
                    // `on_bytes` 路由到审批面，真改了界面（明细/回信）就立刻重绘一帧，不等心跳
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
        self.sigint = sigint;
        self.sigwinch = sigwinch;
        // 本轮结束：摘掉令牌（审批态的按键不该动下一轮的取消）
        self.cancel = None;
        // 收尾：把捕获里剩下的字节排空、半行落地（本轮输出必须完整落进滚动区）
        self.flush_pending();
        output
    }

    /// 第一次用到时装上信号流（装不上——不支持的平台——就永远挂起，不 panic）。
    fn install_signals(&mut self) {
        if self.sigint.is_none() {
            self.sigint = signal(SignalKind::interrupt()).ok();
        }
        if self.sigwinch.is_none() {
            self.sigwinch = signal(SignalKind::window_change()).ok();
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

/// 审批态一次按键的语义（纯函数：归一与经典 [`parse_choice`] 同一份）。
enum ApprovalStep {
    /// 回信并回到编辑器。
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
    use serde_json::json;
    use tokio::sync::{mpsc, oneshot};
    use tokio_util::sync::CancellationToken;

    use crate::approval::{MAX_UNKNOWN_INPUTS, format_prompt, prompt_lines};
    use crate::editor::history::History;
    use crate::editor::keys::Key;
    use crate::editor::raw::RawMode;
    use crate::editor::reader::INVALID_WARNING;
    use crate::tui::Size;
    use crate::tui::approval::{ApprovalJob, TuiApproval};
    use crate::tui::capture::CaptureSink;
    use crate::tui::term::TerminalGuard;
    use crate::tui::view::{Scrollback, ViewFacts, compose, status_line, top_line};

    use super::{BUSY_HINT, Busy, IDLE_HINT, InputChunk, TuiApp};

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
    async fn drive_drains_the_capture_and_redraws_while_the_task_runs() {
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
    async fn drive_sigint_does_not_end_the_busy_loop() {
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

        // `a`：回信 AllowOnce、输入态回编辑器、编辑器缓冲一个字符都不进
        assert_eq!(app.on_bytes(b"a"), [super::Action::Redraw]);
        assert_eq!(rx.try_recv(), Ok(ApprovalDecision::AllowOnce));
        assert_eq!(app.busy(), None, "审批不是忙碌态");
        assert_eq!(app.view(&facts).input.lines[0], "❯ ", "回信即回编辑器");
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
            assert_eq!(app.view(&facts).input.lines[0], "❯ ", "{key}");
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
        assert_eq!(app.view(&facts).input.lines[0], "❯ ");

        // EOF（`Ctrl+D`）＝ Reject（fail-safe）
        let (job, mut rx) = approval_job();
        app.begin_approval(job);
        assert_eq!(app.approval_key(Key::CtrlD), None);
        assert_eq!(rx.try_recv(), Ok(ApprovalDecision::Reject));
        assert_eq!(app.view(&facts).input.lines[0], "❯ ");
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
        assert_eq!(app.view(&facts()).input.lines[0], "❯ ", "回到编辑器");
        assert!(rx.try_recv().is_err(), "只有一次回信");

        // 审批结束之后才轮到编辑器：`b`/`c` 这一次才进缓冲
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
        assert_eq!(app.view(&facts).input.lines[0], "❯ ", "回信后回编辑器");
    }
}
