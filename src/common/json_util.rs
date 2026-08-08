pub fn extract_json_block(text: &str) -> Option<String> {
    let start = text.find("```json")?;
    let after_start = &text[start + 7..];
    let end = after_start.find("```")?;
    Some(after_start[..end].trim().to_string())
}

/// 剥离推理前言: 移除所有 `<think>...</think>` 块（含未闭合的 `<think>` 到末尾），
/// 再移除首个 `{` 之前的散文前缀；若全文无 `{` 则返回（think 剥离后的）原样。
pub fn strip_reasoning_preamble(text: &str) -> String {
    let without_think = strip_think_blocks(text);
    match without_think.find('{') {
        Some(start) => without_think[start..].to_string(),
        None => without_think,
    }
}

fn strip_think_blocks(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    loop {
        match rest.find("<think>") {
            Some(start) => {
                out.push_str(&rest[..start]);
                let after_open = &rest[start + "<think>".len()..];
                match after_open.find("</think>") {
                    Some(end) => rest = &after_open[end + "</think>".len()..],
                    None => return out,
                }
            }
            None => {
                out.push_str(rest);
                return out;
            }
        }
    }
}

/// 从首个 `{` 起做字符串感知的平衡扫描（跟踪引号内/转义状态），
/// 返回第一个完整 `{...}` 子串；找不到返回 None。
pub fn extract_first_json_object(text: &str) -> Option<String> {
    let start = text.find('{')?;
    let mut depth = 0i32;
    let mut in_string = false;
    let mut escaped = false;
    for (i, c) in text[start..].char_indices() {
        if in_string {
            if escaped {
                escaped = false;
            } else if c == '\\' {
                escaped = true;
            } else if c == '"' {
                in_string = false;
            }
            continue;
        }
        match c {
            '"' => in_string = true,
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    let end = start + i + 1;
                    return Some(text[start..end].to_string());
                }
            }
            _ => {}
        }
    }
    None
}

pub fn truncate_utf8_boundary(text: &str, max_bytes: usize) -> &str {
    if text.len() <= max_bytes {
        return text;
    }
    let mut end = max_bytes;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

/// 救助性修复损坏的 JSON 文本（参考 pi repairJson）。
/// - 字符串内的裸控制字符（换行/制表/回车及 0x00-0x1F）转为合法转义；
/// - 非法反斜杠（后随字符不是 `"` `\` `/` `b` `f` `n` `r` `t` `u`）加倍为 `\\`；
/// - 字符串外内容原样保留。
pub fn repair_json(text: &str) -> String {
    let chars: Vec<char> = text.chars().collect();
    let mut out = String::with_capacity(text.len() + 8);
    let mut in_string = false;
    let mut i = 0;

    while i < chars.len() {
        let c = chars[i];
        if !in_string {
            if c == '"' {
                in_string = true;
            }
            out.push(c);
            i += 1;
            continue;
        }

        match c {
            '"' => {
                in_string = false;
                out.push('"');
                i += 1;
            }
            '\\' => match chars.get(i + 1).copied() {
                Some(next)
                    if matches!(next, '"' | '\\' | '/' | 'b' | 'f' | 'n' | 'r' | 't' | 'u') =>
                {
                    out.push('\\');
                    out.push(next);
                    i += 2;
                }
                _ => {
                    out.push('\\');
                    out.push('\\');
                    i += 1;
                }
            },
            c if (c as u32) < 0x20 => {
                match c {
                    '\n' => out.push_str("\\n"),
                    '\t' => out.push_str("\\t"),
                    '\r' => out.push_str("\\r"),
                    other => out.push_str(&format!("\\u{:04x}", other as u32)),
                }
                i += 1;
            }
            _ => {
                out.push(c);
                i += 1;
            }
        }
    }

    out
}

/// 头尾截断: 超长文本保留头 60% 尾 40%, 中间以 `...[truncated N chars]...` 标记。
/// 基于字符数截断, UTF-8 边界天然安全; 短文本原样返回。
pub fn truncate_head_tail(text: &str, max_chars: usize) -> String {
    if max_chars == 0 {
        return String::new();
    }
    let chars: Vec<char> = text.chars().collect();
    if chars.len() <= max_chars {
        return text.to_string();
    }
    let head_len = max_chars * 60 / 100;
    let tail_len = max_chars - head_len;
    let head: String = chars[..head_len].iter().collect();
    let tail: String = chars[chars.len() - tail_len..].iter().collect();
    let truncated = chars.len() - max_chars;
    format!("{head}...[truncated {truncated} chars]...{tail}")
}

#[cfg(test)]
mod tests {
    use super::extract_first_json_object;
    use super::repair_json;
    use super::strip_reasoning_preamble;
    use super::truncate_head_tail;
    use super::truncate_utf8_boundary;

    #[test]
    fn strip_reasoning_preamble_removes_closed_think_block() {
        let input = "<think>let me think</think>\n{\"think\": \"a\", \"say\": \"b\"}";
        let stripped = strip_reasoning_preamble(input);
        assert!(!stripped.contains("<think>"));
        assert!(stripped.starts_with("{\"think\""), "got: {stripped}");
    }

    #[test]
    fn strip_reasoning_preamble_removes_unclosed_think_block() {
        let input = "<think>this reasoning never closes";
        assert_eq!(strip_reasoning_preamble(input), "");
    }

    #[test]
    fn strip_reasoning_preamble_removes_multiple_think_blocks() {
        let input = "<think>first</think> preamble <think>second</think>{\"a\": 1}";
        let stripped = strip_reasoning_preamble(input);
        assert_eq!(stripped, "{\"a\": 1}", "got: {stripped}");
    }

    #[test]
    fn strip_reasoning_preamble_strips_prose_prefix() {
        let input =
            "好的, 我来分析。目标文件是 sales.csv。\n{\"arguments\": {\"path\": \"sales.csv\"}}";
        let stripped = strip_reasoning_preamble(input);
        assert!(stripped.starts_with('{'), "got: {stripped}");
        assert!(!stripped.contains("好的"));
    }

    #[test]
    fn strip_reasoning_preamble_no_brace_returns_unchanged() {
        let input = "纯散文没有 JSON 对象";
        assert_eq!(strip_reasoning_preamble(input), input);
    }

    #[test]
    fn extract_first_json_object_nested() {
        let input = "prefix {\"a\": {\"b\": [1, 2]}, \"c\": {}} tail";
        let obj = extract_first_json_object(input).expect("应找到");
        assert_eq!(obj, "{\"a\": {\"b\": [1, 2]}, \"c\": {}}");
    }

    #[test]
    fn extract_first_json_object_ignores_braces_in_strings() {
        let input = "{\"content\": \"a {not json} b\", \"done\": true}";
        let obj = extract_first_json_object(input).expect("应找到");
        assert_eq!(obj, input);
    }

    #[test]
    fn extract_first_json_object_handles_escaped_quotes() {
        let input = "{\"path\": \"C:\\\\\", \"note\": \"say \\\"hi\\\"\"}";
        let obj = extract_first_json_object(input).expect("应找到");
        assert_eq!(obj, input);
    }

    #[test]
    fn extract_first_json_object_unclosed_returns_none() {
        assert_eq!(extract_first_json_object("{\"a\": 1"), None);
        assert_eq!(extract_first_json_object("no brace here"), None);
    }

    #[test]
    fn truncate_head_tail_short_text_unchanged() {
        let text = "短文本";
        assert_eq!(truncate_head_tail(text, 4000), text);
    }

    #[test]
    fn truncate_head_tail_long_text_keeps_head_and_tail() {
        let text = format!("{}MIDDLE{}", "HEAD-".repeat(500), "-TAIL".repeat(500));
        let result = truncate_head_tail(&text, 4000);
        assert!(result.starts_with("HEAD-"), "开头保留");
        assert!(result.ends_with("-TAIL"), "结尾保留");
        assert!(result.contains("[truncated"), "含截断标记");
        assert!(result.len() < text.len(), "应被截短");
    }

    #[test]
    fn truncate_head_tail_cjk_safe() {
        let text = "中文内容".repeat(2000);
        let result = truncate_head_tail(&text, 100);
        assert!(result.starts_with("中文内容"));
        assert!(result.ends_with("中文内容"));
        assert!(!result.is_empty());
    }

    #[test]
    fn truncate_head_tail_marker_reports_exact_chars() {
        let text = "a".repeat(100);
        let result = truncate_head_tail(&text, 40);
        assert!(result.contains("[truncated 60 chars]"), "got: {result}");
    }

    #[test]
    fn repair_bare_newline_inside_string() {
        let input = "{\"content\": \"line1\nline2\"}";
        let repaired = repair_json(input);
        assert!(repaired.contains("line1\\nline2"), "got: {repaired}");
        assert!(
            serde_json::from_str::<serde_json::Value>(&repaired).is_ok(),
            "repaired text must be valid JSON: {repaired}"
        );
    }

    #[test]
    fn repair_unescaped_content_outside_string_untouched() {
        let input = "prefix {not json\"} tail";
        let repaired = repair_json(input);
        assert_eq!(repaired, input);
    }

    #[test]
    fn repair_illegal_backslash_path() {
        let input = r#"{"path": "C:\path\to"}"#;
        let repaired = repair_json(input);
        assert!(
            repaired.contains("C:\\\\path\\to"),
            "非法 \\p 与 \\o 加倍, 合法 \\t 转义保留: got: {repaired}"
        );
        assert!(
            serde_json::from_str::<serde_json::Value>(&repaired).is_ok(),
            "repaired text must be valid JSON: {repaired}"
        );
    }

    #[test]
    fn repair_mixed_bare_controls_and_backslashes() {
        let input = "{\"a\": \"x\ny\", \"b\": \"C:\\d\"}";
        let repaired = repair_json(input);
        assert!(repaired.contains("x\\ny"), "got: {repaired}");
        assert!(repaired.contains("C:\\\\d"), "got: {repaired}");
        assert!(
            serde_json::from_str::<serde_json::Value>(&repaired).is_ok(),
            "repaired text must be valid JSON: {repaired}"
        );
    }

    #[test]
    fn repair_valid_json_unchanged() {
        let input = r#"{"a": "ok", "b": [1, 2], "c": "\n\t\"quoted\"\\"}"#;
        assert_eq!(repair_json(input), input);
    }

    #[test]
    fn repair_nested_large_content_stays_valid() {
        let input = r#"{"template_kind": "normal", "nodes": [{"id": "n1", "capability": "file.write", "task_description": "大段文本: line1
line2
line3", "prefilled_arguments": {"path": "a.txt", "content": "first line
second line"}}]}"#;
        let repaired = repair_json(input);
        assert!(
            serde_json::from_str::<serde_json::Value>(&repaired).is_ok(),
            "repaired text must be valid JSON: {repaired}"
        );
        let value: serde_json::Value = serde_json::from_str(&repaired).unwrap();
        let content = value["nodes"][0]["prefilled_arguments"]["content"]
            .as_str()
            .unwrap();
        assert_eq!(content, "first line\nsecond line");
    }

    #[test]
    fn repair_tab_and_control_chars() {
        let input = "{\"x\": \"a\tb\u{0001}c\"}";
        let repaired = repair_json(input);
        assert!(repaired.contains("a\\tb"), "got: {repaired}");
        assert!(repaired.contains("\\u0001"), "got: {repaired}");
        assert!(
            serde_json::from_str::<serde_json::Value>(&repaired).is_ok(),
            "repaired text must be valid JSON: {repaired}"
        );
    }

    #[test]
    fn truncate_ascii_keeps_max_bytes() {
        let s = "a".repeat(3000);
        assert_eq!(truncate_utf8_boundary(&s, 2000).len(), 2000);
    }

    #[test]
    fn truncate_cjk_never_panics_on_boundary() {
        let s = "指".repeat(1000);
        let t = truncate_utf8_boundary(&s, 2000);
        assert_eq!(t.len() % 3, 0, "必须停在字符边界: len={}", t.len());
        assert!(t.is_char_boundary(t.len()));
        assert_eq!(t.len(), 1998, "回退到最近的 3 字节边界");
    }

    #[test]
    fn truncate_emoji_falls_back_to_safe_boundary() {
        let s = "🧱".repeat(600);
        let t = truncate_utf8_boundary(&s, 2001);
        assert_eq!(t.len() % 4, 0);
        assert!(t.is_char_boundary(t.len()));
        assert_eq!(t.len(), 2000);
    }

    #[test]
    fn truncate_short_input_returns_unchanged() {
        let s = "短文本";
        assert_eq!(truncate_utf8_boundary(s, 2000), s);
    }
}
