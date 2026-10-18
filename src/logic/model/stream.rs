#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StreamChunk {
    Delta(String),

    Think(String),

    /// 过程状态通知（如 LLM 调用指数退避重试进度），不改变消息流。
    Status(String),

    Done,

    Cancelled,

    Error(String),
}

/// 增量式 SSE（Server-Sent Events）解码器。
///
/// 供 openai / responses 两个 provider 共享：`feed()` 按行消费任意切割的
/// 字节流，空行处触发事件 dispatch，返回该批次完成事件的 data 载荷文本。
///
/// 兼容点（相对旧 find_double_newline 手工分帧）：
/// - LF 与 CRLF 行尾均可分帧（`\r` 于行尾剥离，含 `\r` 单独到达的跨包情形）；
/// - `data:` 后的前导空格全部剥离（规范允许无空格或单空格）；
/// - 同一事件多行 `data:` 以 `\n` 拼接后整体 dispatch；
/// - `:` 开头的注释行（如 `: keep-alive`）忽略；
/// - `finish()` 在 EOF 时把无空行结尾的残留行按事件收尾 dispatch。
pub(crate) struct SseDecoder {
    /// 尚未见行尾字符的残行字节。
    pending: Vec<u8>,
    /// 当前事件内已收集的 data 行（跨多行时按序暂存）。
    data_lines: Vec<String>,
}

impl SseDecoder {
    pub(crate) fn new() -> Self {
        Self {
            pending: Vec::new(),
            data_lines: Vec::new(),
        }
    }

    /// 喂入一批字节，返回本批次内完成事件（遇到空行边界）的 data 载荷，
    /// 按 dispatch 顺序排列。事件不含任何 `data:` 行时不产生元素。
    pub(crate) fn feed(&mut self, bytes: &[u8]) -> Vec<String> {
        let mut events = Vec::new();
        let mut buf = std::mem::take(&mut self.pending);
        buf.extend_from_slice(bytes);

        let mut start = 0;
        for (i, &b) in buf.iter().enumerate() {
            if b != b'\n' {
                continue;
            }
            // 行内容不含行尾符；CRLF 的 \r 一并剥离。
            let mut end = i;
            if end > start && buf[end - 1] == b'\r' {
                end -= 1;
            }
            if let Some(ev) = self.absorb_line(&buf[start..end]) {
                events.push(ev);
            }
            start = i + 1;
        }
        self.pending = buf.split_off(start);
        events
    }

    /// EOF 收尾：最后一个无空行结尾的残留事件仍按事件 dispatch。
    /// 之前已正常分帧的流调用此方法返回空向量（幂等）。
    pub(crate) fn finish(&mut self) -> Vec<String> {
        let mut events = Vec::new();
        if !self.pending.is_empty() {
            let mut line = std::mem::take(&mut self.pending);
            if line.last() == Some(&b'\r') {
                line.pop();
            }
            if let Some(ev) = self.absorb_line(&line) {
                events.push(ev);
            }
        }
        if !self.data_lines.is_empty() {
            events.push(std::mem::take(&mut self.data_lines).join("\n"));
        }
        events
    }

    /// 处理一条完整行：空行 = 事件边界 dispatch；注释行忽略；
    /// `data:` 行累积（前导空格全剥），其余字段行（event:/id:/retry:）忽略。
    fn absorb_line(&mut self, line: &[u8]) -> Option<String> {
        if line.is_empty() {
            if self.data_lines.is_empty() {
                return None;
            }
            return Some(std::mem::take(&mut self.data_lines).join("\n"));
        }
        if line[0] == b':' {
            return None;
        }
        let line = String::from_utf8_lossy(line);
        if let Some(value) = line.strip_prefix("data:") {
            // 规范允许 `data:` 后无空格或单空格；对 JSON 载荷而言
            // 剥离全部前导空格是安全的。
            self.data_lines
                .push(value.trim_start_matches(' ').to_string());
        }
        None
    }
}

impl Default for SseDecoder {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stream_chunk_delta_construct() {
        let c = StreamChunk::Delta("hello".to_string());
        match c {
            StreamChunk::Delta(s) => assert_eq!(s, "hello"),
            _ => panic!("expected Delta"),
        }
    }

    #[test]
    fn stream_chunk_done_and_error_construct() {
        let d = StreamChunk::Done;
        let e = StreamChunk::Error("oops".to_string());
        assert_eq!(d, StreamChunk::Done);
        assert!(matches!(e, StreamChunk::Error(s) if s == "oops"));
    }

    #[test]
    fn stream_chunk_partial_eq_and_clone() {
        let a = StreamChunk::Delta("x".to_string());
        let b = a.clone();
        assert_eq!(a, b);
        let c = StreamChunk::Done;
        assert_ne!(a, c);
        let d = StreamChunk::Error("e".to_string());
        assert_ne!(a, d);
    }

    // ---- SseDecoder ----

    #[test]
    fn decoder_lf_stream_dispatches_events_in_order() {
        let mut d = SseDecoder::new();
        let events = d.feed(b"data: {\"a\":1}\n\ndata: {\"b\":2}\n\n");
        assert_eq!(
            events,
            vec!["{\"a\":1}".to_string(), "{\"b\":2}".to_string()]
        );
        assert!(d.finish().is_empty(), "已完整分帧，EOF 无残留");
    }

    #[test]
    fn decoder_crlf_stream_dispatches_events() {
        let mut d = SseDecoder::new();
        let events = d.feed(b"data: one\r\n\r\ndata: two\r\n\r\n");
        assert_eq!(events, vec!["one".to_string(), "two".to_string()]);
        assert!(d.finish().is_empty());
    }

    #[test]
    fn decoder_mixed_line_endings_dispatch() {
        // LF / CRLF 混用（部分代理会改写行尾）。
        let mut d = SseDecoder::new();
        let events = d.feed(b"data: a\n\r\ndata: b\r\n\ndata: c\n\n");
        assert_eq!(
            events,
            vec!["a".to_string(), "b".to_string(), "c".to_string()]
        );
    }

    #[test]
    fn decoder_data_prefix_without_space() {
        let mut d = SseDecoder::new();
        let events = d.feed(b"data:{\"a\":1}\n\n");
        assert_eq!(events, vec!["{\"a\":1}".to_string()]);
    }

    #[test]
    fn decoder_data_prefix_extra_spaces_stripped() {
        let mut d = SseDecoder::new();
        let events = d.feed(b"data:   {\"a\":1}\n\n");
        assert_eq!(events, vec!["{\"a\":1}".to_string()]);
    }

    #[test]
    fn decoder_multiline_data_joined_with_newline() {
        let mut d = SseDecoder::new();
        let events = d.feed(b"data: {\"a\":\ndata: 1}\n\n");
        assert_eq!(events, vec!["{\"a\":\n1}".to_string()]);
    }

    #[test]
    fn decoder_event_split_across_feed_boundaries() {
        // 半个 JSON 分两次 feed：第一包不得 dispatch，拼齐并遇空行后整体 dispatch。
        let mut d = SseDecoder::new();
        assert!(d.feed(b"data: {\"delta\":").is_empty());
        assert!(d.feed(b"\"hi\"}").is_empty());
        let events = d.feed(b"\n\n");
        assert_eq!(events, vec!["{\"delta\":\"hi\"}".to_string()]);
    }

    #[test]
    fn decoder_crlf_split_across_feed_boundaries() {
        // \r 在上一包结尾、\n 在下一包开头。
        let mut d = SseDecoder::new();
        assert!(d.feed(b"data: x\r").is_empty());
        let events = d.feed(b"\n\n");
        assert_eq!(events, vec!["x".to_string()]);
    }

    #[test]
    fn decoder_empty_data_line_dispatches_empty_payload() {
        // 裸 `data:` 行产生空串载荷（由消费方跳过），而非破坏分帧。
        let mut d = SseDecoder::new();
        let events = d.feed(b"data:\n\ndata: x\n\n");
        assert_eq!(events, vec![String::new(), "x".to_string()]);
    }

    #[test]
    fn decoder_comment_lines_ignored() {
        let mut d = SseDecoder::new();
        // 纯注释事件不产生元素。
        let events = d.feed(b": keep-alive\n\n: ping\r\n\r\ndata: x\n\n");
        assert_eq!(events, vec!["x".to_string()]);
    }

    #[test]
    fn decoder_event_without_data_lines_dispatches_nothing() {
        let mut d = SseDecoder::new();
        let events = d.feed(b"event: foo\nid: 1\nretry: 1000\n\n");
        assert!(events.is_empty());
    }

    #[test]
    fn decoder_done_sentinel_dispatched_as_data() {
        // 解码器不解释 [DONE]，只保证按 data 载荷原样送达。
        let mut d = SseDecoder::new();
        let events = d.feed(b"data: [DONE]\n\n");
        assert_eq!(events, vec!["[DONE]".to_string()]);
    }

    #[test]
    fn decoder_finish_dispatches_trailing_event_without_blank_line() {
        // EOF 无空行结尾：带换行的残留行。
        let mut d = SseDecoder::new();
        assert!(d.feed(b"data: tail\n").is_empty());
        assert_eq!(d.finish(), vec!["tail".to_string()]);
        assert!(d.finish().is_empty(), "finish 幂等");
    }

    #[test]
    fn decoder_finish_dispatches_trailing_partial_line() {
        // EOF 无空行结尾：连换行都没有的残行。
        let mut d = SseDecoder::new();
        assert!(d.feed(b"data: {\"z\":").is_empty());
        assert!(d.feed(b"9}").is_empty());
        assert_eq!(d.finish(), vec!["{\"z\":9}".to_string()]);
    }

    #[test]
    fn decoder_finish_after_complete_stream_is_empty() {
        let mut d = SseDecoder::new();
        let events = d.feed(b"data: a\n\ndata: b\n\n");
        assert_eq!(events.len(), 2);
        assert!(d.finish().is_empty());
    }
}
