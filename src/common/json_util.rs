pub fn extract_json_block(text: &str) -> Option<String> {
    let start = text.find("```json")?;
    let after_start = &text[start + 7..];
    let end = after_start.find("```")?;
    Some(after_start[..end].trim().to_string())
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

#[cfg(test)]
mod tests {
    use super::truncate_utf8_boundary;

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
