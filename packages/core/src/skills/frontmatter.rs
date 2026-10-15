//! SKILL.md 的 frontmatter 解析：手写实现（零依赖，离线环境没有 serde_yaml 之类的现成库）。

/// 解析 SKILL.md 的 frontmatter，返回 `(name, description, body)`。
///
/// 约定（SKILL.md 的最小子集）：首行必须是 `---` 围栏，其后是 `key: value` 行，直到闭合的
/// `---` 围栏；`body` 是闭合围栏之后的全部内容（可以为空）。值的首尾成对引号会剥掉；描述里的
/// 换行折叠成单个空格、控制字符剔除（保证注入系统提示时仍是单行）。
///
/// 返回 `None` 的形态：没有起始围栏、没有闭合围栏、`name` 或 `description` 缺失或为空。
pub fn parse(text: &str) -> Option<(String, String, String)> {
    let text = text.trim_start();
    let mut fields: Vec<(String, String)> = Vec::new();
    let mut body_start = None;
    let mut offset = 0usize;

    for (index, raw_line) in text.lines().enumerate() {
        let line = raw_line.trim_end();
        // 行尾换行也被算进偏移，闭合围栏之后才切得出正文
        let next = offset + raw_line.len() + newline_len(&text[offset + raw_line.len()..]);

        if index == 0 {
            if line != "---" {
                return None;
            }
        } else if line == "---" {
            body_start = Some(next);
            break;
        } else {
            push_field(&mut fields, line);
        }
        offset = next;
    }

    // 没有闭合围栏，或 name/description 缺失、为空：都按畸形处理，由调用方跳过并记警告
    let body_start = body_start?;
    let name = field(&fields, "name", false)?;
    let description = field(&fields, "description", true)?;
    Some((name, description, text[body_start..].to_owned()))
}

/// 行尾换行的字节数：`\r\n` 为 2、`\n` 为 1、文件末尾为 0。
fn newline_len(tail: &str) -> usize {
    if tail.starts_with("\r\n") {
        2
    } else if tail.starts_with('\n') {
        1
    } else {
        0
    }
}

/// 归入一行 frontmatter：`key: value` 开新字段；其余行（缩进续行等）并入上一个字段的值，中间保留
/// 换行——多行描述靠这一步拼齐，再在清洗时折叠成单行。
fn push_field(fields: &mut Vec<(String, String)>, line: &str) {
    if let Some((key, value)) = line.split_once(':') {
        let key = key.trim();
        if !key.is_empty() && !key.contains(char::is_whitespace) {
            fields.push((key.to_owned(), value.to_owned()));
            return;
        }
    }
    if let Some((_, value)) = fields.last_mut() {
        value.push('\n');
        value.push_str(line);
    }
}

/// 取字段值：键名大小写不敏感，重复出现时以最后一条为准；清洗后为空按缺失处理。
fn field(fields: &[(String, String)], key: &str, fold_spaces: bool) -> Option<String> {
    let value = fields
        .iter()
        .rev()
        .find(|(name, _)| name.eq_ignore_ascii_case(key))?
        .1
        .as_str();
    let value = sanitize(value, fold_spaces);
    (!value.is_empty()).then_some(value)
}

/// 值清洗：剥掉首尾成对引号并去掉首尾空白；`fold_spaces` 为真时把换行等空白折叠成单个空格
/// （描述要单行），控制字符一律剔除。
fn sanitize(value: &str, fold_spaces: bool) -> String {
    let value = unquote(value);
    let mut cleaned = String::with_capacity(value.len());
    let mut pending_space = false;
    for ch in value.chars() {
        if fold_spaces && ch.is_whitespace() {
            // 行首空白不占位，行中间的连续空白合并成一个空格
            pending_space = !cleaned.is_empty();
            continue;
        }
        if ch.is_control() {
            continue;
        }
        if pending_space {
            cleaned.push(' ');
            pending_space = false;
        }
        cleaned.push(ch);
    }
    cleaned
}

/// 剥掉首尾成对的单/双引号；不成对（或只有一个字符）就原样返回。
fn unquote(value: &str) -> &str {
    let value = value.trim();
    for quote in ['"', '\''] {
        if value.len() >= 2 && value.starts_with(quote) && value.ends_with(quote) {
            return value[quote.len_utf8()..value.len() - quote.len_utf8()].trim();
        }
    }
    value
}

#[cfg(test)]
mod tests {
    use super::parse;

    #[test]
    fn parse_reads_name_description_and_body() {
        let text = "---\nname: demo-skill\ndescription: A demo skill.\n---\n# 正文\n1. 做事\n";

        let (name, description, body) = parse(text).expect("正常的 SKILL.md 应当解析成功");

        assert_eq!(name, "demo-skill");
        assert_eq!(description, "A demo skill.");
        assert_eq!(body, "# 正文\n1. 做事\n");
    }

    #[test]
    fn parse_requires_a_leading_fence() {
        // 没有起始围栏：整段都被当成正文
        assert!(parse("name: demo\ndescription: 描述\n---\n正文\n").is_none());
        // 围栏不在首行（前面有标题）同样不算
        assert!(parse("# 标题\n---\nname: demo\n---\n正文\n").is_none());
    }

    #[test]
    fn parse_requires_the_closing_fence() {
        assert!(parse("---\nname: demo\ndescription: 描述\n正文\n").is_none());
    }

    #[test]
    fn parse_requires_a_non_empty_name_and_description() {
        assert!(parse("---\ndescription: 只有描述\n---\n正文\n").is_none());
        assert!(parse("---\nname: only-name\n---\n正文\n").is_none());
        // 空串与纯空白都算缺失
        assert!(parse("---\nname: \"\"\ndescription: 描述\n---\n正文\n").is_none());
        assert!(parse("---\nname: demo\ndescription:   \n---\n正文\n").is_none());
    }

    #[test]
    fn parse_strips_surrounding_quotes() {
        let (name, description, _) =
            parse("---\nname: \"demo\"\ndescription: 'A quoted one.'\n---\n").unwrap();

        assert_eq!(name, "demo");
        assert_eq!(description, "A quoted one.");

        // 不是成对的引号原样保留
        let (_, description, _) =
            parse("---\nname: demo\ndescription: 这是「引号」\n---\n").unwrap();
        assert_eq!(description, "这是「引号」");
    }

    #[test]
    fn parse_ignores_other_frontmatter_keys() {
        let (name, description, _) =
            parse("---\nname: demo\nallowed-tools: shell_run\n description: 正常描述\n---\n正文\n")
                .expect("未知键不影响解析");

        assert_eq!(name, "demo");
        assert_eq!(description, "正常描述");
    }

    #[test]
    fn parse_folds_multiline_descriptions_and_drops_control_characters() {
        // 描述跨两行（第二行是续行）且夹着 BEL 与制表符：折叠成单行，结构不被撑坏
        let text =
            "---\nname: multi\ndescription: \"第一行\n第二行\u{7}\t结束\"\n---\n\n# 正文\n步骤\n";

        let (name, description, body) = parse(text).unwrap();

        assert_eq!(name, "multi");
        assert_eq!(description, "第一行 第二行 结束");
        assert_eq!(body, "\n# 正文\n步骤\n");
    }

    #[test]
    fn parse_allows_an_empty_body() {
        let (name, _, body) = parse("---\nname: demo\ndescription: 描述\n---\n").unwrap();

        assert_eq!(name, "demo");
        assert!(body.is_empty());

        // 闭合围栏之后只剩换行时，正文就是那个换行
        let (_, _, body) = parse("---\nname: demo\ndescription: 描述\n---\n\n").unwrap();
        assert_eq!(body, "\n");
    }

    #[test]
    fn parse_handles_crlf_files() {
        let text = "---\r\nname: demo\r\ndescription: A CRLF skill.\r\n---\r\n正文\r\n";

        let (name, description, body) = parse(text).unwrap();

        assert_eq!(name, "demo");
        assert_eq!(description, "A CRLF skill.");
        assert_eq!(body, "正文\r\n");
    }
}
