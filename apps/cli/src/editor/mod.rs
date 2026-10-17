//! 手写行编辑器（raw mode）。
//!
//! 分层：[`raw`] 是终端 I/O 的进出层（termios 开关、bracketed paste 开关、可用性判定），
//! 不碰按键语义、不存状态、不画屏幕；解码（`keys`）、状态机（`state`）、重绘（`draw`）、
//! 历史（`history`）与读取层（[`reader`]）各自成模块。除了读取层与 [`raw`]，其余模块都应当能在
//! 没有终端的单测里跑。

pub mod draw;
pub mod history;
pub mod keys;
pub mod raw;
pub mod reader;
pub mod state;
