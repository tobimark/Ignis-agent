//! `/learn` 的确认面：预览、提示词与 stdin 答案。
//!
//! 草稿只在用户按 `s` 之后才由命令层落盘：本模块只打预览并问一句，不写任何文件；生成的正文
//! 永不自动部署、也永不执行。写序与 `approval.rs::InteractiveApproval` 一致：预览与提示词
//! **先**打到 stdout，再从 stdin 读一行；`s`（trim + 大小写归一）之外的任何答案——`d`、空行、
//! EOF、读失败——一律按放弃收束。

use std::io::Write;

/// 确认提示词（逐字，不带换行之外的前后缀）。
pub(crate) const LEARN_PROMPT: &str = "save this skill draft? [s]ave [d]iscard> ";
/// 预览的行数上限与字节上限（8 KiB）、截断标记。
pub(crate) const LEARN_PREVIEW_LINES: usize = 120;
pub(crate) const LEARN_PREVIEW_BYTES: usize = 8 * 1024;
pub(crate) const LEARN_PREVIEW_MARKER: &str = "… [truncated]";

/// 草稿头行（逐字）：`draft skill {name}: untested, not deployed or executed automatically`。
///
/// 头行自己就说明「未经测试、不自动部署、不自动执行」——设计 §2.2 的非目标写进用户看得到的地方。
pub(crate) fn draft_header(name: &str) -> String {
    format!("draft skill {name}: untested, not deployed or executed automatically")
}

/// 预览行：先按行取前 [`LEARN_PREVIEW_LINES`] 行、再按 [`LEARN_PREVIEW_BYTES`] 字节取前缀（按字符
/// 边界切，不切坏 UTF-8）；被截断时末行是 [`LEARN_PREVIEW_MARKER`]，总字节数不超过上限。
///
/// 行数或字节任一处超限就带标记：标记行（连同它前面的换行）从字节预算里先扣掉，因此截断后的
/// 输出仍是「前若干个完整行 + 一行标记」，不会多出一个空行。
pub(crate) fn preview_lines(skill_md: &str) -> Vec<String> {
    // 1. 行数上限：先取前 LEARN_PREVIEW_LINES 行，还有剩的就是被截断了
    let mut iter = skill_md.lines();
    let selected: Vec<&str> = iter.by_ref().take(LEARN_PREVIEW_LINES).collect();
    let line_cut = iter.next().is_some();

    // 2. 没有截断且总字节（含行间换行）也没超：原文逐行交回
    let total =
        selected.iter().map(|line| line.len()).sum::<usize>() + selected.len().saturating_sub(1);
    if !line_cut && total <= LEARN_PREVIEW_BYTES {
        return selected.into_iter().map(str::to_string).collect();
    }

    // 3. 要走标记行：标记与它前面的那个换行先占掉预算，再逐行装
    let available = LEARN_PREVIEW_BYTES.saturating_sub(LEARN_PREVIEW_MARKER.len() + 1);
    let mut out: Vec<String> = Vec::new();
    let mut used = 0usize;
    for line in selected {
        let need = line.len() + usize::from(!out.is_empty()); // 行间换行
        if used + need <= available {
            used += need;
            out.push(line.to_string());
            continue;
        }
        // 放不下的这一行按字符边界切出剩余预算的前缀（预算为 0 时不再多出一行）
        let left = available.saturating_sub(used + usize::from(!out.is_empty()));
        if left > 0 {
            out.push(char_prefix(line, left).to_string());
        }
        break;
    }
    out.push(LEARN_PREVIEW_MARKER.to_string());
    out
}

/// 按字节上限取前缀，切点落在字符边界上（不切坏 UTF-8；中文等宽字符因此可能少那么一两个）。
fn char_prefix(text: &str, max_bytes: usize) -> &str {
    if text.len() <= max_bytes {
        return text;
    }
    let mut end = max_bytes;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

/// 答案判定：`trim` + 小写后等于 `s` 才保存；`d`、空串、EOF（`None`）与其它输入一律放弃。
pub(crate) fn asks_save(line: Option<&str>) -> bool {
    line.is_some_and(|line| line.trim().eq_ignore_ascii_case("s"))
}

/// 打头行 + 预览 + 提示词，再从 `lines` 读**一行**并按 [`asks_save`] 收束（纯函数，测试直用）。
///
/// 写序钉死：预览与提示词在**读之前**打（`flush` 保证它们先到终端），答案随后才读一行；输出写
/// 失败不算致命（终端的读仍按原样进行），读取端提前结束（EOF）时 `asks_save(None)` 按放弃收束。
pub(crate) fn answer_from_lines(
    name: &str,
    skill_md: &str,
    mut lines: impl Iterator<Item = String>,
    out: &mut impl Write,
) -> bool {
    let _ = writeln!(out, "{}", draft_header(name));
    for line in preview_lines(skill_md) {
        let _ = writeln!(out, "{line}");
    }
    let _ = write!(out, "{LEARN_PROMPT}");
    let _ = out.flush();
    asks_save(lines.next().as_deref())
}

/// 与 [`confirm_save`] 同一份行为，但名字先取成 `String`：确认面要的是 `'static` 的未来
/// （`learn_with` 的闭包先把名字从草稿里取出来，正文本来就是 `to_skill_md()` 的新字符串）。
///
/// 真 stdin/stdout 的包装：`spawn_blocking` + [`crate::input::lossy_lines`]（`approval.rs` 的同一
/// 手法；非法 UTF-8 按替换字符读出，读取失败与 EOF 都结束迭代）；阻塞任务 panic 或被取消时按
/// **放弃**收束——生产路径永远不会「没读到答案就落盘」。
pub(crate) async fn confirm_save_owned(name: String, skill_md: String) -> bool {
    tokio::task::spawn_blocking(move || {
        let stdin = std::io::stdin();
        let mut out = std::io::stdout();
        let lines = crate::input::lossy_lines(stdin.lock());
        answer_from_lines(&name, &skill_md, lines, &mut out)
    })
    .await
    .unwrap_or(false)
}

/// 经典 REPL 的确认面（引用版）：把参数变成拥有者后交给 [`confirm_save_owned`]，行为逐字不变。
///
/// `pub`（而不是 `pub(crate)`）：经典路径现在经 `Session::learn` → [`confirm_save_owned`] 走，
/// 这个引用版眼下没有生产调用点，`pub(crate)` 会在 `-D warnings` 下报 `dead_code`（与 T4 的
/// `TuiApp::attach_approvals` 同一处理）；接缝的拥有者版是 [`confirm_save_owned`]，本项只是它
/// 的引用形状。
pub async fn confirm_save(name: &str, skill_md: String) -> bool {
    confirm_save_owned(name.to_string(), skill_md).await
}

#[cfg(test)]
mod tests {
    use crate::commands::ConfirmLearn;

    use super::{
        LEARN_PREVIEW_BYTES, LEARN_PREVIEW_LINES, LEARN_PREVIEW_MARKER, LEARN_PROMPT,
        answer_from_lines, asks_save, confirm_save_owned, draft_header, preview_lines,
    };

    /// 预览的双上限：行数先切（前 120 行 + 标记 = 121 行）、字节上限再切（按字符边界，不切坏
    /// UTF-8）；没超上限的文本逐行原样交回。
    #[test]
    fn preview_lines_caps_lines_bytes_and_marks_the_cut() {
        // 130 行：取前 LEARN_PREVIEW_LINES 行，末行是截断标记
        let text = (0..130)
            .map(|index| format!("line {index}"))
            .collect::<Vec<_>>()
            .join("\n");
        let lines = preview_lines(&text);
        assert_eq!(lines.len(), LEARN_PREVIEW_LINES + 1, "{lines:?}");
        assert_eq!(lines[0], "line 0");
        assert_eq!(lines[LEARN_PREVIEW_LINES - 1], "line 119");
        assert_eq!(lines[LEARN_PREVIEW_LINES], LEARN_PREVIEW_MARKER);

        // 没被截断（含恰好等于行数上限）：逐行原样、不带标记
        assert_eq!(preview_lines("a\nb\nc\n"), ["a", "b", "c"]);
        let at_limit = (0..LEARN_PREVIEW_LINES)
            .map(|index| format!("line {index}"))
            .collect::<Vec<_>>()
            .join("\n");
        let lines = preview_lines(&at_limit);
        assert_eq!(lines.len(), LEARN_PREVIEW_LINES);
        assert_ne!(lines.last().unwrap(), LEARN_PREVIEW_MARKER);

        // 单行 9 KiB 的 ASCII：总字节不超过上限，末行是标记
        let huge = "x".repeat(9 * 1024);
        let lines = preview_lines(&huge);
        assert_eq!(lines.last().unwrap(), LEARN_PREVIEW_MARKER);
        let bytes: usize = lines.iter().map(String::len).sum::<usize>() + lines.len() - 1;
        assert!(bytes <= LEARN_PREVIEW_BYTES, "{bytes}");

        // 长中文按字符边界切：不 panic、仍是合法 UTF-8、切出来的前缀全是整字符
        let chinese = "长".repeat(9 * 1024);
        let lines = preview_lines(&chinese);
        assert_eq!(lines.last().unwrap(), LEARN_PREVIEW_MARKER);
        let prefix = lines[..lines.len() - 1].join("\n");
        assert!(prefix.len() < LEARN_PREVIEW_BYTES - LEARN_PREVIEW_MARKER.len());
        assert!(prefix.chars().all(|ch| ch == '长'), "{prefix:?}");
    }

    /// 草稿头行逐字钉住（英文 chrome）：它自己就说明「未经测试、不自动部署、不自动执行」。
    #[test]
    fn draft_header_is_pinned() {
        assert_eq!(
            draft_header("demo-skill"),
            "draft skill demo-skill: untested, not deployed or executed automatically"
        );
    }

    /// 答案判定：只有 `s`（trim + 大小写归一）保存；`d`、空串、别的词与 EOF（`None`）都放弃。
    #[test]
    fn answer_from_lines_saves_only_on_s() {
        for answer in ["s\n", " S ", "S"] {
            assert!(asks_save(Some(answer)), "{answer:?}");
        }
        for answer in [
            None,
            Some("d"),
            Some(""),
            Some("save"),
            Some("y"),
            Some("ss"),
        ] {
            assert!(!asks_save(answer), "{answer:?}");
        }

        // 预览与提示词在读之前打出来：EOF（空迭代器）按放弃收束，输出里头行、预览、提示词俱全
        let skill_md = "---\nname: demo-skill\ndescription: Demo.\n---\n# 步骤\n";
        let mut out = Vec::new();
        let saved = answer_from_lines(
            "demo-skill",
            skill_md,
            Vec::<String>::new().into_iter(),
            &mut out,
        );
        assert!(!saved, "EOF 必须放弃");
        let text = String::from_utf8(out).unwrap();
        assert!(
            text.contains(
                "draft skill demo-skill: untested, not deployed or executed automatically"
            ),
            "{text}"
        );
        assert!(text.contains("# 步骤"), "{text}");
        assert!(text.ends_with(LEARN_PROMPT), "{text}");

        // `s` 那一支保存；`d` / 空行放弃
        for (answer, want) in [("s\n", true), ("d\n", false), ("\n", false)] {
            let mut out = Vec::new();
            let saved = answer_from_lines(
                "demo-skill",
                skill_md,
                [answer.to_string()].into_iter(),
                &mut out,
            );
            assert_eq!(saved, want, "{answer:?}");
        }

        // 被截断的预览：`out` 里出现标记行（末行之后紧跟提示词）
        let long = (0..LEARN_PREVIEW_LINES + 5)
            .map(|index| format!("line {index}"))
            .collect::<Vec<_>>()
            .join("\n");
        let mut out = Vec::new();
        answer_from_lines("demo-skill", &long, ["d".to_string()].into_iter(), &mut out);
        let text = String::from_utf8(out).unwrap();
        assert!(
            text.lines().any(|line| line == LEARN_PREVIEW_MARKER),
            "{text}"
        );
    }

    /// `confirm_save_owned` 与经典 `confirm_save` 是同一份行为：拥有者版接 `(String, String)`、交
    /// [`ConfirmLearn`]（`learn_with` 的确认面要的 `'static` 形状），引用版是给它包的一层薄壳。
    /// 提示词、头行、预览与答案归一都在 [`answer_from_lines`] 这一份实现里——上一条用例（本任务
    /// 未改）继续逐字钉住它们。
    #[test]
    fn confirm_save_owned_matches_the_classic_prompt() {
        // 经典委托体的取参形状（`learn` 里那一句的编译期检查；不执行——真执行会去读 stdin）：
        // `Box::pin(confirm_save_owned(..))` 就是 `ConfirmLearn`。
        let _classic_delegation: fn(String, String) -> ConfirmLearn =
            |name, skill_md| Box::pin(confirm_save_owned(name, skill_md));

        // 两个入口共用同一份提示词与答案归一（经典路径的逐字断言在上一条用例里）
        assert_eq!(LEARN_PROMPT, "save this skill draft? [s]ave [d]iscard> ");
        for answer in [Some("s"), Some(" S "), Some("S")] {
            assert!(asks_save(answer), "只有 `s` 保存：{answer:?}");
        }
        for answer in [None, Some("d"), Some(""), Some("save")] {
            assert!(!asks_save(answer), "其它输入一律放弃：{answer:?}");
        }
    }
}
