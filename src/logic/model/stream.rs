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

pub(crate) fn find_double_newline(buf: &[u8]) -> Option<usize> {
    buf.windows(2).position(|w| w == b"\n\n")
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

    #[test]
    fn find_double_newline_returns_some_at_boundary() {
        let buf = b"data: hi\n\ndata: bye\n\n";
        let pos = find_double_newline(buf);
        assert_eq!(pos, Some(8));
    }

    #[test]
    fn find_double_newline_returns_none_when_absent() {
        let buf = b"data: hi\ndata: bye";
        assert_eq!(find_double_newline(buf), None);
    }

    #[test]
    fn find_double_newline_handles_short_buf() {
        assert_eq!(find_double_newline(b""), None);
        assert_eq!(find_double_newline(b"x"), None);
        assert_eq!(find_double_newline(b"x\n"), None);
    }
}
