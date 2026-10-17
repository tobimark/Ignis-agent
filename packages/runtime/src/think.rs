//! 推理分道分割器：把模型内联输出的 `<think>` / `<thinking>` 块从正文里剥出来。
//!
//! 部分模型（如 MiniMax-M2，见 `docs/spikes/2026-10-15-relay-streaming-toolcalls.md`）把思维链
//! 直接内联在回答里；渲染层要与正文分道展示，而正文通道（`Done.summary`、回填给模型的
//! assistant 消息、`TaskOutcome.final_text`）绝不能带上标记。分割器按流式分片工作：标记可能被
//! 切在任意位置（含跨多字节 UTF-8 边界），因此每 `push` 只吐出已能定道的部分，可能构成令牌
//! 真前缀的尾部留待下次；`finish` 时把残余按当前通道吐出——不吞字、不误判。

/// 一段已定道的文本：正文或推理，二者分通道下发。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Piece {
    /// 正文：进 `TextDelta`，并计入 canonical 累积（`Done.summary`、回填给模型的消息）。
    Text(String),
    /// 推理：只进 `ReasoningDelta`，绝不并入正文。
    Reasoning(String),
}

/// 开启推理块的令牌。
const OPEN_THINK: &str = "<think>";
const OPEN_THINKING: &str = "<thinking>";
/// 关闭推理块的令牌。
const CLOSE_THINK: &str = "</think>";
const CLOSE_THINKING: &str = "</thinking>";

/// 分割器当前所在的通道（跨 `push` 保留；初始在正文）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum Channel {
    /// 正文。
    #[default]
    Text,
    /// 推理块内部。
    Reasoning,
}

/// 令牌表：开启令牌切进推理通道，关闭令牌切回正文通道。四个令牌互不为前缀，
/// 故逐个 `starts_with` 判定即可；最长令牌是 `</thinking>`（11 字节）。
const TOKENS: [(&str, Channel); 4] = [
    (OPEN_THINKING, Channel::Reasoning),
    (CLOSE_THINKING, Channel::Text),
    (OPEN_THINK, Channel::Reasoning),
    (CLOSE_THINK, Channel::Text),
];

/// 流式推理分道分割器：`push` 喂增量、`finish` 收尾，输出按通道分开的 [`Piece`]。
///
/// 不变量：任何输入都**不丢字、不重复**；正文片段永远不含标记；未闭合的推理块整段留在推理
/// 通道。标记本身是纯记法，不产出任何片段。
#[derive(Debug, Default)]
pub struct ThinkSplitter {
    /// 当前通道。
    channel: Channel,
    /// 待定尾部：可能是某个令牌真前缀的后缀，留到下次拼接再判定（有界：最长 11 字节）。
    pending: String,
}

impl ThinkSplitter {
    pub fn new() -> Self {
        Self::default()
    }

    /// 喂入一段增量，返回已能定道的片段（可能为空：尾部还在等令牌补齐）。
    pub fn push(&mut self, text: &str) -> Vec<Piece> {
        self.pending.push_str(text);
        self.drain(false)
    }

    /// 流结束：把待定尾部按当前通道吐出（未闭合推理块的尾巴也留在推理通道）。
    pub fn finish(&mut self) -> Vec<Piece> {
        self.drain(true)
    }

    /// 扫描缓冲：完整令牌切通道且自身不产出；`flush=false` 时保留「可能是令牌真前缀」的尾部，
    /// `flush=true` 时把整段按当前通道按字面吐出。
    fn drain(&mut self, flush: bool) -> Vec<Piece> {
        let mut pieces = Vec::new();
        // 当前通道里攒下的文本，遇令牌（换通道）时结算成片段
        let mut text = String::new();
        // 已能定道的部分到此为止：`[0, index)` 是正文/推理内容，`index` 起是待定尾部
        let mut index = 0;
        while index < self.pending.len() {
            if let Some((token_len, channel)) = token_at(&self.pending[index..]) {
                if channel != self.channel {
                    push_piece(&mut pieces, self.channel, &mut text);
                    self.channel = channel;
                }
                index += token_len;
                continue;
            }
            // 尾部可能是某个令牌的真前缀：留到下次；收尾时按字面处理（不吞掉半个令牌）
            if !flush && is_token_prefix(&self.pending[index..]) {
                break;
            }
            // 按字符前进：多字节字符不会被切在中间
            let step = self.pending[index..]
                .chars()
                .next()
                .map_or(0, char::len_utf8);
            text.push_str(&self.pending[index..index + step]);
            index += step;
        }
        push_piece(&mut pieces, self.channel, &mut text);
        self.pending.drain(..index);
        pieces
    }
}

/// 缓冲开头是哪个完整令牌：返回（令牌长度，该令牌切到的通道）。
fn token_at(rest: &str) -> Option<(usize, Channel)> {
    TOKENS
        .iter()
        .find(|(token, _)| rest.starts_with(token))
        .map(|(token, channel)| (token.len(), *channel))
}

/// 是否是某个令牌的**真**前缀（只有真前缀才值得留到下次拼接）。
fn is_token_prefix(rest: &str) -> bool {
    TOKENS
        .iter()
        .any(|(token, _)| rest.len() < token.len() && token.starts_with(rest))
}

/// 把当前通道攒下的文本结算成一个片段（空串不产出片段）。
fn push_piece(pieces: &mut Vec<Piece>, channel: Channel, text: &mut String) {
    if text.is_empty() {
        return;
    }
    let text = std::mem::take(text);
    pieces.push(match channel {
        Channel::Text => Piece::Text(text),
        Channel::Reasoning => Piece::Reasoning(text),
    });
}

#[cfg(test)]
mod tests {
    use super::{Piece, ThinkSplitter};

    /// 整个输入一次投喂 + 收尾的片段序列（分片不变量的参照物）。
    fn pieces_of_whole(input: &str) -> Vec<Piece> {
        let mut splitter = ThinkSplitter::new();
        let mut pieces = splitter.push(input);
        pieces.extend(splitter.finish());
        pieces
    }

    /// 合并相邻同通道片段：分片方式只改变片段边界，不该改变通道语义。
    fn merge(pieces: &[Piece]) -> Vec<Piece> {
        let mut merged: Vec<Piece> = Vec::new();
        for piece in pieces {
            match (merged.last_mut(), piece) {
                (Some(Piece::Text(acc)), Piece::Text(text)) => acc.push_str(text),
                (Some(Piece::Reasoning(acc)), Piece::Reasoning(text)) => acc.push_str(text),
                _ => merged.push(piece.clone()),
            }
        }
        merged
    }

    /// 正文通道的全文。
    fn text_of(pieces: &[Piece]) -> String {
        pieces
            .iter()
            .filter_map(|piece| match piece {
                Piece::Text(text) => Some(text.as_str()),
                Piece::Reasoning(_) => None,
            })
            .collect()
    }

    /// 推理通道的全文。
    fn reasoning_of(pieces: &[Piece]) -> String {
        pieces
            .iter()
            .filter_map(|piece| match piece {
                Piece::Reasoning(text) => Some(text.as_str()),
                Piece::Text(_) => None,
            })
            .collect()
    }

    /// brief 钉死：无标记的文本原样透传（分片只影响边界，不影响内容）。
    #[test]
    fn plain_text_passes_through_unchanged() {
        let mut splitter = ThinkSplitter::new();
        assert_eq!(splitter.push("hello"), vec![Piece::Text("hello".into())]);
        assert!(splitter.finish().is_empty());

        // 像标记但不是标记的文本照常走正文
        let mut splitter = ThinkSplitter::new();
        assert_eq!(
            splitter.push("<thinkx>还是正文"),
            vec![Piece::Text("<thinkx>还是正文".into())]
        );

        // 流恰好在半个标记处结束：残余按当前（正文）通道吐出，不吞字
        let mut splitter = ThinkSplitter::new();
        assert!(splitter.push("<thi").is_empty());
        assert_eq!(splitter.finish(), vec![Piece::Text("<thi".into())]);
    }

    /// brief 钉死：`先说明<think>内心戏</think>结论` → 正文 / 推理 / 正文 三段。
    #[test]
    fn think_block_splits_into_reasoning_and_text_pieces() {
        let mut splitter = ThinkSplitter::new();
        assert_eq!(
            splitter.push("先说明<think>内心戏</think>结论"),
            vec![
                Piece::Text("先说明".into()),
                Piece::Reasoning("内心戏".into()),
                Piece::Text("结论".into()),
            ]
        );
        assert!(splitter.finish().is_empty());
    }

    /// brief 钉死：`<thinking>` 变体同样识别（开闭两枚都要）。
    #[test]
    fn thinking_variant_is_recognized_too() {
        assert_eq!(
            pieces_of_whole("前<thinking>想</thinking>后"),
            vec![
                Piece::Text("前".into()),
                Piece::Reasoning("想".into()),
                Piece::Text("后".into()),
            ]
        );
    }

    /// brief 钉死：标记被逐字符分片（每个字符一次 `push`）仍能正确分道。
    #[test]
    fn per_character_pushes_still_split() {
        let mut splitter = ThinkSplitter::new();
        let mut pieces = Vec::new();
        for ch in "先说明<think>内心戏</think>结论".chars() {
            pieces.extend(splitter.push(&ch.to_string()));
        }
        pieces.extend(splitter.finish());

        assert_eq!(
            merge(&pieces),
            vec![
                Piece::Text("先说明".into()),
                Piece::Reasoning("内心戏".into()),
                Piece::Text("结论".into()),
            ]
        );
        assert_eq!(text_of(&pieces), "先说明结论");
        assert_eq!(reasoning_of(&pieces), "内心戏");
    }

    /// brief 钉死：标记可跨 `push` 分片（`"<thi"` + `"nk>内心"`）。
    #[test]
    fn marker_spanning_pushes_is_recognized() {
        let mut splitter = ThinkSplitter::new();
        assert!(
            splitter.push("<thi").is_empty(),
            "半截标记是待定尾部，不吐片段"
        );
        assert_eq!(
            splitter.push("nk>内心"),
            vec![Piece::Reasoning("内心".into())]
        );
    }

    /// 流式纪律：标记已完整时不留残余——标记之前的正文立刻吐出，不必等流结束。
    #[test]
    fn complete_marker_does_not_hold_back_preceding_text() {
        let mut splitter = ThinkSplitter::new();
        assert_eq!(
            splitter.push("先说明<think>"),
            vec![Piece::Text("先说明".into())]
        );
        assert_eq!(splitter.push("内心"), vec![Piece::Reasoning("内心".into())]);
        assert!(splitter.finish().is_empty());
    }

    /// brief 钉死：未闭合的 `<think>` 整段归推理（`finish` 按当前通道吐残余，不吞字）。
    #[test]
    fn unclosed_think_block_stays_reasoning_through_finish() {
        // 无待定尾部：`push` 当场定道，`finish` 没有残余
        let mut splitter = ThinkSplitter::new();
        assert_eq!(
            splitter.push("<think>没有结束"),
            vec![Piece::Reasoning("没有结束".into())]
        );
        assert!(splitter.finish().is_empty());

        // 待定尾部攒着半个标记：`finish` 把它按当前（推理）通道吐出，不丢也不误判成正文
        let mut splitter = ThinkSplitter::new();
        let mut pieces = splitter.push("<think>没有结束</thin");
        assert_eq!(pieces, vec![Piece::Reasoning("没有结束".into())]);
        pieces.extend(splitter.finish());
        assert_eq!(
            pieces,
            vec![
                Piece::Reasoning("没有结束".into()),
                Piece::Reasoning("</thin".into()),
            ]
        );
        assert_eq!(reasoning_of(&pieces), "没有结束</thin");
        assert!(text_of(&pieces).is_empty());
    }

    /// brief 钉死：孤立的 `</think>` 不把文本推去推理通道——它前后的文本都归 `Text`。
    #[test]
    fn stray_closing_tag_keeps_text_in_text_channel() {
        let pieces = pieces_of_whole("前面</think>后面");
        assert_eq!(merge(&pieces), vec![Piece::Text("前面后面".into())]);
        assert_eq!(text_of(&pieces), "前面后面");
        assert!(reasoning_of(&pieces).is_empty());
    }

    /// brief 钉死：随机切分不变量——同一输入按任意分片喂入，通道语义与整体投喂一致（不丢不重）。
    #[test]
    fn random_fragmentation_keeps_channel_semantics() {
        let input = "先说明<think>内心戏</thinking>结论<thinking>再想</think>收尾";
        let expected = vec![
            Piece::Text("先说明".into()),
            Piece::Reasoning("内心戏".into()),
            Piece::Text("结论".into()),
            Piece::Reasoning("再想".into()),
            Piece::Text("收尾".into()),
        ];
        assert_eq!(pieces_of_whole(input), expected);

        let chars: Vec<char> = input.chars().collect();
        // 确定性伪随机（不引依赖、可复现）：1–5 个字符一片，切 100 轮
        let mut seed = 0x5eed_1234_u64;
        for round in 0..100 {
            let mut splitter = ThinkSplitter::new();
            let mut pieces = Vec::new();
            let mut index = 0;
            while index < chars.len() {
                seed = seed
                    .wrapping_mul(6_364_136_223_846_793_005)
                    .wrapping_add(1_442_695_040_888_963_407);
                let width = (seed >> 33) as usize % 5 + 1;
                let end = (index + width).min(chars.len());
                let part: String = chars[index..end].iter().collect();
                pieces.extend(splitter.push(&part));
                index = end;
            }
            pieces.extend(splitter.finish());

            assert_eq!(
                merge(&pieces),
                expected,
                "第 {round} 轮的分片丢了/错了片段：{pieces:?}"
            );
            assert_eq!(
                text_of(&pieces),
                "先说明结论收尾",
                "第 {round} 轮正文不完整"
            );
            assert_eq!(
                reasoning_of(&pieces),
                "内心戏再想",
                "第 {round} 轮推理不完整"
            );
        }
    }
}
