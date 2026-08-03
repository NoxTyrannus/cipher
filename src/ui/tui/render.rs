use crate::mode_runtime::ModeKind;
use crate::ui::tui::state::{TuiMessage, TuiMode, TuiState};
use crate::ui::tui::status_line::{fit_to_width, segments_from_snapshot};
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Paragraph, Wrap};
use ratatui::Frame;

pub fn render(state: &TuiState, frame: &mut Frame) {
    if state.mode == TuiMode::Config {
        super::config_panel::render(&state.config_panel, frame, frame.area());
        return;
    }
    let area = frame.area();

    let error_h = if state.current_error.is_some() { 1 } else { 0 };
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(1),
            Constraint::Length(1),
            Constraint::Length(error_h),
            Constraint::Length(3),
        ])
        .split(area);

    render_messages(state, frame, chunks[0]);
    render_status_line(state, frame, chunks[1]);
    if error_h > 0 {
        render_error(state, frame, chunks[2]);
    }
    render_input(state, frame, chunks[3]);
}

fn render_messages(state: &TuiState, frame: &mut Frame, area: Rect) {
    if state.messages.is_empty() {
        return;
    }
    let user_w = area.width as usize;
    let inner_w = area.width.saturating_sub(2) as usize;
    let heights: Vec<u16> = state
        .messages
        .iter()
        .map(|m| msg_height(m, user_w, inner_w))
        .collect();
    let total_h: u16 = heights.iter().sum();
    let visible_h = area.height;

    if total_h <= visible_h {
        let constraints: Vec<Constraint> = heights.iter().map(|&h| Constraint::Length(h)).collect();
        let rects = Layout::vertical(constraints).split(area);
        for (msg, rect) in state.messages.iter().zip(rects.iter()) {
            render_one_message(msg, *rect, frame);
        }
    } else {
        let mut skip = 0u16;
        let mut render_from = 0usize;
        for (i, &h) in heights.iter().enumerate() {
            if total_h - skip <= visible_h {
                render_from = i;
                break;
            }
            skip += h;
        }

        let max_scroll = total_h.saturating_sub(visible_h) as usize;
        let offset = state.scroll_offset.min(max_scroll);
        let mut remaining = offset;
        for i in (0..render_from).rev() {
            if remaining == 0 {
                break;
            }
            let h = heights[i] as usize;
            if h <= remaining {
                remaining -= h;
                render_from = i;
            } else {
                break;
            }
        }
        let visible_heights: Vec<Constraint> = heights[render_from..]
            .iter()
            .map(|&h| Constraint::Length(h))
            .collect();
        let rects = Layout::vertical(visible_heights).split(area);
        for (i, rect) in rects.iter().enumerate() {
            render_one_message(&state.messages[render_from + i], *rect, frame);
        }
    }
}

fn msg_height(msg: &TuiMessage, user_w: usize, inner_w: usize) -> u16 {
    match msg {
        TuiMessage::User(t) => wrapped_lines(t, user_w) + 1,
        TuiMessage::Assistant(t) => wrapped_lines(t, inner_w) + 2,
        TuiMessage::Streaming { content, error, .. } => {
            let body = content_to_display(content, error.as_deref());
            wrapped_lines(&body, inner_w) + 2
        }
        TuiMessage::Think { text, .. } => wrapped_lines(text, inner_w) + 2,
        TuiMessage::Request(t) => wrapped_lines(t, inner_w) + 2,
    }
}

fn wrapped_lines(text: &str, width: usize) -> u16 {
    if width == 0 {
        return 1;
    }
    let mut total = 0usize;
    for line in text.split('\n') {
        let w = ratatui::text::Line::from(line).width();
        total += if w == 0 { 1 } else { w.div_ceil(width) };
    }
    total.max(1) as u16
}

fn render_one_message(msg: &TuiMessage, rect: Rect, frame: &mut Frame) {
    match msg {
        TuiMessage::User(text) => {
            let para = Paragraph::new(vec![
                Line::from(vec![
                    Span::styled("● · ", Style::default().fg(Color::DarkGray)),
                    Span::styled(text.as_str(), Style::default().fg(Color::Gray)),
                ]),
                Line::from(""),
            ]);
            frame.render_widget(para, rect);
        }
        TuiMessage::Assistant(text) => render_bordered(
            frame,
            rect,
            text,
            BorderType::LightDoubleDashed,
            "cipher · 消息",
            Color::Gray,
        ),
        TuiMessage::Streaming {
            content,
            finished,
            error,
            ..
        } => {
            let (title, color) = if error.is_some() {
                ("cipher · 错误", Color::Red)
            } else if *finished {
                ("cipher · 消息", Color::Gray)
            } else {
                ("cipher · 流式", Color::Gray)
            };
            let display_text = content_to_display(content, error.as_deref());
            render_bordered(
                frame,
                rect,
                &display_text,
                BorderType::LightDoubleDashed,
                title,
                color,
            );
        }
        TuiMessage::Think { text, .. } => render_bordered(
            frame,
            rect,
            text,
            BorderType::LightDoubleDashed,
            "cipher · 思考",
            Color::DarkGray,
        ),
        TuiMessage::Request(text) => render_bordered(
            frame,
            rect,
            text,
            BorderType::Plain,
            "cipher · 请求",
            Color::Cyan,
        ),
    }
}

fn render_bordered(
    frame: &mut Frame,
    rect: Rect,
    text: &str,
    border: BorderType,
    title: &str,
    border_color: Color,
) {
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(border)
        .border_style(Style::default().fg(border_color))
        .title(Line::from(vec![Span::styled(
            title,
            Style::default()
                .fg(border_color)
                .add_modifier(Modifier::BOLD),
        )]));
    let para = Paragraph::new(text).block(block).wrap(Wrap { trim: false });
    frame.render_widget(para, rect);
}

fn render_status_line(state: &TuiState, frame: &mut Frame, area: Rect) {
    let line = if let Some(snap) = state.status_line.snapshot() {
        let segments = segments_from_snapshot(snap);
        fit_to_width(&segments, area.width as usize)
    } else {
        Line::from("")
    };
    frame.render_widget(Paragraph::new(line), area);
}

fn render_error(state: &TuiState, frame: &mut Frame, area: Rect) {
    if let Some(err) = &state.current_error {
        let friendly = friendly_error(err);
        let para = Paragraph::new(Line::from(vec![
            Span::styled("error: ", Style::default().fg(Color::Yellow)),
            Span::styled(friendly, Style::default().fg(Color::White)),
        ]));
        frame.render_widget(para, area);
    }
}

fn render_input(state: &TuiState, frame: &mut Frame, area: Rect) {
    let mode_str = mode_display(state.current_mode);

    let prefix = format!("[{}] > ", mode_str);
    let para = Paragraph::new(Line::from(vec![
        Span::styled(
            prefix.clone(),
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(state.input.clone(), Style::default().fg(Color::White)),
    ]));
    frame.render_widget(para, area);

    let prefix_w = ratatui::text::Line::from(prefix.as_str()).width() as u16;
    let input_w = ratatui::text::Line::from(state.input.as_str()).width() as u16;
    let cursor_x = area.x + prefix_w + input_w;
    let cursor_y = area.y;
    frame.set_cursor_position((cursor_x, cursor_y));
}

fn content_to_display(content: &str, error: Option<&str>) -> String {
    error
        .map(friendly_error)
        .unwrap_or_else(|| content.to_string())
}

fn friendly_error(err: &str) -> String {
    if err.contains("invalid_json_output") || (err.contains("parse") && err.contains("JSON")) {
        "模型输出格式异常，正在重试...".to_string()
    } else if err.contains("timeout") || err.contains("timed out") {
        "模型响应超时，请检查网络或模型状态".to_string()
    } else if err.contains("rate limit") || err.contains("429") {
        "API 请求频率过高，请稍后重试".to_string()
    } else if err.contains("auth") || err.contains("api key") || err.contains("unauthorized") {
        "API 认证失败，请检查 API Key 配置".to_string()
    } else if err.contains("retry") && err.contains("exceeded") {
        "模型输出持续异常，已记录".to_string()
    } else {
        err.to_string()
    }
}

fn mode_display(mode: ModeKind) -> &'static str {
    match mode {
        ModeKind::Unni => "UNNI",
        ModeKind::Keep => "KEEP",
        ModeKind::Loop => "LOOP",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    fn render_to_text(state: &TuiState) -> String {
        let backend = TestBackend::new(40, 10);
        let mut terminal = Terminal::new(backend).unwrap();

        let frame = terminal.draw(|f| render(state, f)).unwrap();
        frame.buffer.content().iter().map(|c| c.symbol()).collect()
    }

    #[test]
    fn render_shows_user_message() {
        let mut s = TuiState::new();
        s.push_user("hello".into());
        let text = render_to_text(&s);
        assert!(text.contains("hello"), "应显示用户消息内容");
        assert!(text.contains("●"), "应显示用户消息标签");
    }

    #[test]
    fn render_assistant_uses_dashed_border() {
        let mut s = TuiState::new();
        s.push_assistant("reply".into());
        let text = render_to_text(&s);
        assert!(text.contains("reply"), "应显示 AI 消息内容");
        assert!(text.contains('╎'), "Dashed 框应有 ╎ 竖线, got: {text}");
        assert!(
            !text.contains('│'),
            "LightDoubleDashed 框不应有 Plain │ 竖线"
        );
    }

    #[test]
    fn wrapped_lines_counts_cjk_display_width() {
        assert_eq!(
            wrapped_lines("你好你好", 5),
            2,
            "4 CJK = 8 cells, width 5 → 2 行"
        );

        assert_eq!(wrapped_lines("hello", 3), 2, "5 ASCII, width 3 → 2 行");
        assert_eq!(wrapped_lines("", 10), 1, "空文本 → 1 行");
    }

    #[test]
    fn render_request_uses_plain_border() {
        let mut s = TuiState::new();
        s.push_request("ask?".into());
        let text = render_to_text(&s);
        assert!(text.contains("ask?"), "应显示请求内容");
        assert!(text.contains('│'), "Plain 实线框应有 │ 竖线, got: {text}");
    }

    #[test]
    fn render_error_shows_when_set() {
        let mut s = TuiState::new();
        s.set_error("neterr".into());
        let text = render_to_text(&s);
        assert!(text.contains("error:"), "出错时应显示 error: 行");
        assert!(text.contains("neterr"), "原始错误信息应显示");
    }

    #[test]
    fn render_no_error_line_when_none() {
        let s = TuiState::new();
        let text = render_to_text(&s);
        assert!(!text.contains("error:"), "无错误时不应显示 error 行");
    }

    #[test]
    fn render_shows_current_mode_in_input() {
        let mut s = TuiState::new();
        s.current_mode = ModeKind::Keep;
        let text = render_to_text(&s);
        assert!(text.contains("KEEP"), "输入栏应显示当前 mode");
    }

    #[test]
    fn render_think_block_shows_title_text_and_dashed_border() {
        let mut s = TuiState::new();
        s.push_streaming("inst-1".into());
        s.push_think("inst-1", "thinking visibly");
        let text = render_to_text(&s);
        assert!(
            text.contains("thinking visibly"),
            "think 文本应上屏, got: {text}"
        );
        assert!(
            text.contains('思') && text.contains('考'),
            "Think 块标题应含 \"思考\", got: {text}"
        );
        assert!(text.contains('╎'), "Think 块应为虚线框 (╎), got: {text}");
    }

    #[test]
    fn render_think_block_uses_darkgray_border() {
        let mut s = TuiState::new();
        s.push_think("inst-1", "internal");
        let backend = TestBackend::new(40, 10);
        let mut terminal = Terminal::new(backend).unwrap();
        let frame = terminal.draw(|f| render(&s, f)).unwrap();
        let title_cell = frame
            .buffer
            .content()
            .iter()
            .find(|c| c.symbol() == "思")
            .expect("标题应含 思");
        assert_eq!(
            title_cell.fg,
            Color::DarkGray,
            "Think 块应比 say 的 Gray 更暗 (DarkGray)"
        );
    }

    #[test]
    fn render_think_before_streaming_bubble() {
        let mut s = TuiState::new();
        s.push_streaming("inst-1".into());
        s.push_think("inst-1", "plan first");
        s.append_delta("inst-1", "say second");
        let text = render_to_text(&s);
        let think_pos = text.find("plan first").expect("think 上屏");
        let say_pos = text.find("say second").expect("say 上屏");
        assert!(think_pos < say_pos, "Think 块应排在 say bubble 之前");
    }

    #[test]
    fn render_streaming_shows_text() {
        let mut s = TuiState::new();
        s.push_streaming("inst-1".into());
        s.append_delta("inst-1", "streaming text");
        let text = render_to_text(&s);
        assert!(
            text.contains("streaming text"),
            "streaming content should render, got: {text}"
        );

        assert!(
            text.contains('╎'),
            "Streaming message should have Dashed border"
        );
    }

    #[test]
    fn render_cancelled_streaming_shows_interrupted() {
        let mut s = TuiState::new();
        s.push_streaming("inst-1".into());
        s.append_delta("inst-1", "partial");
        s.mark_cancelled("inst-1");
        let text = render_to_text(&s);
        assert!(
            !text.contains("partial"),
            "unvalidated partial text must not render after cancellation, got: {text}"
        );

        assert!(
            text.contains('╎'),
            "Streaming message should have Dashed border, got: {text}"
        );
    }

    #[test]
    fn render_errored_streaming_shows_reason_without_partial_output() {
        let mut s = TuiState::new();
        s.push_streaming("inst-1".into());
        s.append_delta("inst-1", "unvalidated partial output");
        s.mark_error("inst-1", "network timeout");

        let text = render_to_text(&s);

        assert!(
            text.contains('模'),
            "friendly error (CJK) should render, got: {text}"
        );
        assert!(
            !text.contains("unvalidated partial output"),
            "unvalidated partial output must not render, got: {text}"
        );
    }

    #[test]
    fn scroll_when_messages_overflow_viewport() {
        let mut s = TuiState::new();

        for i in 0..20 {
            s.push_user(format!("msg {i}"));
        }
        let text = render_to_text(&s);

        assert!(text.contains("msg 19"), "latest message should be visible");
    }

    #[test]
    fn exit_criteria_1a_real_terminal_conversation_flow() {
        let mut s = TuiState::new();
        s.push_user("hello".into());
        s.push_assistant("world".into());
        let text = render_to_text(&s);
        assert!(
            text.contains("hello"),
            "conversation flow: user message visible"
        );
        assert!(
            text.contains("world"),
            "conversation flow: assistant message visible"
        );

        assert!(
            !text.contains("Memory"),
            "no Memory panel (6-Tab design removed)"
        );
        assert!(
            !text.contains("Insight"),
            "no Insight panel (6-Tab design removed)"
        );
    }

    #[test]
    fn exit_criteria_1b_input_bar_mode_format() {
        let mut s = TuiState::new();
        s.current_mode = ModeKind::Unni;
        let text = render_to_text(&s);
        assert!(
            text.contains("[UNNI]"),
            "mode name in brackets, got: {text}"
        );
        assert!(text.contains('>'), "arrow prompt after mode, got: {text}");
    }

    #[test]
    fn exit_criteria_1c_border_type_for_messages() {
        let mut s = TuiState::new();
        s.push_assistant("reply".into());
        let text = render_to_text(&s);
        assert!(text.contains('╎'), "assistant message has Dashed border");
    }

    #[test]
    fn exit_criteria_1d_request_plain_border() {
        let mut s = TuiState::new();
        s.push_request("question?".into());
        let text = render_to_text(&s);
        assert!(text.contains("question?"), "request content visible");
        assert!(text.contains('│'), "Plain border for request");
    }

    #[test]
    fn exit_criteria_1e_no_tab_panels() {
        let mut s = TuiState::new();
        s.push_user("test".into());
        s.push_assistant("reply".into());
        s.push_request("q?".into());

        let backend = TestBackend::new(40, 20);
        let mut terminal = Terminal::new(backend).unwrap();
        let frame = terminal.draw(|f| render(&s, f)).unwrap();
        let text: String = frame.buffer.content().iter().map(|c| c.symbol()).collect();

        assert!(text.contains("test"), "user message in conversation flow");
        assert!(
            text.contains("reply"),
            "assistant message in conversation flow"
        );
        assert!(text.contains("q?"), "request in conversation flow");
    }
}
