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

/// v0.4.6 think 显示开关纯函数：某模式是否渲染思考面板。
///
/// - `mode == "unni"` → `unni_override.unwrap_or(ui_show_think)`（UNNI per-mode 覆盖全局）；
/// - 其他模式 → 跟随全局 `ui_show_think`（KEEP/LOOP 不受 `[mode_styles.unni]` 覆盖影响）。
pub fn show_think_for_mode(ui_show_think: bool, unni_override: Option<bool>, mode: &str) -> bool {
    if mode.eq_ignore_ascii_case("unni") {
        unni_override.unwrap_or(ui_show_think)
    } else {
        ui_show_think
    }
}

/// 思考面板相关消息：`Think` 块（「思考」标题+边框），以及未完成的 `Streaming`
/// 气泡（「思考中」标题+边框）。show_think=false 时两者整体不渲染（含标题与边框）。
fn is_think_display(msg: &TuiMessage) -> bool {
    matches!(msg, TuiMessage::Think { .. })
        || matches!(
            msg,
            TuiMessage::Streaming {
                finished: false,
                ..
            }
        )
}

/// v0.4.8 空气泡清理：以下消息不渲染（高度 0、跳过绘制）：
/// - `Think { text }` 且 `text` 为空；
/// - `Streaming { finished: true, content: 空, error: None }`。
///
/// **保留**：`finished=false` 的空 Streaming（「思考中」进度占位）、`error=Some` 的错误气泡、
/// `content` 非空的正常气泡。
fn is_empty_bubble(msg: &TuiMessage) -> bool {
    match msg {
        TuiMessage::Think { text, .. } => text.is_empty(),
        TuiMessage::Streaming {
            content,
            finished,
            error,
            ..
        } => *finished && content.is_empty() && error.is_none(),
        _ => false,
    }
}

fn render_messages(state: &TuiState, frame: &mut Frame, area: Rect) {
    if state.messages.is_empty() {
        return;
    }
    let show_think = show_think_for_mode(
        state.ui_show_think,
        state.unni_show_think,
        state.current_mode.as_str(),
    );
    let user_w = area.width as usize;
    let inner_w = area.width.saturating_sub(2) as usize;
    let agent_name = &state.agent_name;
    let heights: Vec<u16> = state
        .messages
        .iter()
        .map(|m| {
            if !show_think && is_think_display(m) {
                0
            } else {
                msg_height(m, user_w, inner_w)
            }
        })
        .collect();
    let total_h: u16 = heights.iter().sum();
    let visible_h = area.height;

    if total_h <= visible_h {
        let constraints: Vec<Constraint> = heights.iter().map(|&h| Constraint::Length(h)).collect();
        let rects = Layout::vertical(constraints).split(area);
        for (msg, rect) in state.messages.iter().zip(rects.iter()) {
            if !show_think && is_think_display(msg) {
                continue;
            }
            render_one_message(msg, *rect, frame, agent_name);
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
        // v0.4.9 滚动剪辑修复（疑点②）：从 render_from 起，只取能放进视口的消息，
        // 最后一条按剩余高度裁剪。此前直接把 `heights[render_from..]`（总高可远超
        // visible_h）整体喂给 `Layout::vertical`，ratatui 会对超出的 Length 约束过度
        // 分配，导致滚动后渲染出碎片化/串行的非连续消息（用户滚动失效的另一表现）。
        let mut to_render: Vec<Constraint> = Vec::new();
        let mut consumed = 0u16;
        let mut n = render_from;
        while n < heights.len() && consumed < visible_h {
            let h = heights[n];
            let take = h.min(visible_h - consumed);
            to_render.push(Constraint::Length(take));
            consumed += take;
            n += 1;
        }
        let rects = Layout::vertical(to_render).split(area);
        for (i, rect) in rects.iter().enumerate() {
            let msg = &state.messages[render_from + i];
            if !show_think && is_think_display(msg) {
                continue;
            }
            render_one_message(msg, *rect, frame, agent_name);
        }
    }
}

fn msg_height(msg: &TuiMessage, user_w: usize, inner_w: usize) -> u16 {
    if is_empty_bubble(msg) {
        return 0;
    }
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

fn render_one_message(msg: &TuiMessage, rect: Rect, frame: &mut Frame, agent_name: &str) {
    if is_empty_bubble(msg) {
        return;
    }
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
            &format!("{agent_name} · 消息"),
            Color::Gray,
        ),
        TuiMessage::Streaming {
            content,
            finished,
            error,
            ..
        } => {
            let (title, color) = if error.is_some() {
                (format!("{agent_name} · 错误"), Color::Red)
            } else if *finished {
                (format!("{agent_name} · 消息"), Color::Gray)
            } else {
                (format!("{agent_name} · 思考中"), Color::Gray)
            };
            let display_text = content_to_display(content, error.as_deref());
            render_bordered(
                frame,
                rect,
                &display_text,
                BorderType::LightDoubleDashed,
                &title,
                color,
            );
        }
        TuiMessage::Think { text, .. } => render_bordered(
            frame,
            rect,
            text,
            BorderType::LightDoubleDashed,
            &format!("{agent_name} · 思考"),
            Color::DarkGray,
        ),
        TuiMessage::Request(text) => render_bordered(
            frame,
            rect,
            text,
            BorderType::Plain,
            &format!("{agent_name} · 请求"),
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
    fn show_think_for_mode_unni_follows_global_or_override() {
        // 全局 true / UNNI None → 显示。
        assert!(show_think_for_mode(true, None, "unni"));
        // 全局 true / UNNI Some(false) → 隐藏。
        assert!(!show_think_for_mode(true, Some(false), "unni"));
        // 全局 false / UNNI None → 隐藏。
        assert!(!show_think_for_mode(false, None, "unni"));
        // 全局 false / UNNI Some(true) → UNNI 下反而显示（覆盖优先级最高）。
        assert!(show_think_for_mode(false, Some(true), "unni"));
    }

    #[test]
    fn show_think_for_mode_keep_loop_follow_global_only() {
        // KEEP/LOOP 跟随全局，不受 [mode_styles.unni] 覆盖影响。
        assert!(show_think_for_mode(true, Some(false), "keep"));
        assert!(show_think_for_mode(true, Some(false), "loop"));
        assert!(!show_think_for_mode(false, Some(true), "keep"));
        assert!(!show_think_for_mode(false, Some(true), "loop"));
        // 大小写不敏感。
        assert!(!show_think_for_mode(true, Some(false), "UNNI"));
        assert!(show_think_for_mode(true, None, "UNNI"));
    }

    #[test]
    fn render_hides_think_panel_when_show_think_false() {
        // 全局关闭思考显示：Think 块与未完成 Streaming（「思考中」）整体不渲染。
        let mut s = TuiState::new();
        s.ui_show_think = false;
        s.push_streaming("inst-1".into());
        s.push_think("inst-1", "internal plan text");
        s.append_delta("inst-1", "visible answer text");
        let text = render_to_text(&s);
        assert!(
            !text.contains("internal plan text"),
            "think 文本不得上屏, got: {text}"
        );
        assert!(
            !text.contains('思') && !text.contains('考'),
            "思考面板标题不得出现, got: {text}"
        );
        assert!(
            !text.contains("visible answer text"),
            "未完成 Streaming 气泡（思考中）也不得渲染, got: {text}"
        );
    }

    #[test]
    fn render_unni_override_false_hides_think_but_keep_shows() {
        // UNNI per-mode 覆盖 false：UNNI 下思考面板隐藏。
        let mut unni = TuiState::new();
        unni.current_mode = ModeKind::Unni;
        unni.ui_show_think = true;
        unni.unni_show_think = Some(false);
        unni.push_streaming("inst-1".into());
        unni.push_think("inst-1", "hidden in unni");
        let text = render_to_text(&unni);
        assert!(!text.contains("hidden in unni"), "got: {text}");

        // 同一覆盖不作用于 KEEP：KEEP 仍显示思考面板。
        let mut keep = TuiState::new();
        keep.current_mode = ModeKind::Keep;
        keep.ui_show_think = true;
        keep.unni_show_think = Some(false);
        keep.push_streaming("inst-1".into());
        keep.push_think("inst-1", "visible in keep");
        let text = render_to_text(&keep);
        assert!(text.contains("visible in keep"), "got: {text}");
    }

    #[test]
    fn render_think_blocks_show_by_default() {
        // 缺省（ui_show_think=true）：既有行为不变——think 面板照常显示。
        let mut s = TuiState::new();
        s.push_streaming("inst-1".into());
        s.push_think("inst-1", "default visible");
        let text = render_to_text(&s);
        assert!(text.contains("default visible"), "got: {text}");
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
    fn render_skips_empty_think_bubble() {
        // v0.4.8 空气泡清理：`Think { text: 空 }` 不渲染（高度 0、跳过绘制）。
        let mut s = TuiState::new();
        s.messages.push(TuiMessage::Think {
            id: "inst-1".into(),
            text: String::new(),
        });

        assert_eq!(msg_height(&s.messages[0], 40, 38), 0, "空 Think 高度应为 0");

        let text = render_to_text(&s);
        assert!(
            !text.contains('思') && !text.contains('考'),
            "空 Think 气泡不应渲染标题/内容, got: {text}"
        );
    }

    #[test]
    fn render_skips_empty_finished_streaming_bubble() {
        // v0.4.8 空气泡清理：`Streaming { finished: true, content: 空, error: None }` 不渲染。
        let mut s = TuiState::new();
        s.messages.push(TuiMessage::Streaming {
            id: "inst-1".into(),
            content: String::new(),
            finished: true,
            error: None,
        });

        assert_eq!(
            msg_height(&s.messages[0], 40, 38),
            0,
            "空 finished Streaming 高度应为 0"
        );

        let text = render_to_text(&s);
        assert!(
            !text.contains("消息"),
            "空 finished Streaming 气泡不应渲染标题, got: {text}"
        );
    }

    #[test]
    fn scroll_with_empty_placeholders_reaches_oldest_and_tail() {
        // 长历史 + 空占位混合：空气泡不占滚动空间，滚动语义不回归。
        let mut s = TuiState::new();
        for i in 0..30 {
            s.push_user(format!("msg {i}"));
            s.messages.push(TuiMessage::Think {
                id: format!("t{i}"),
                text: String::new(),
            });
            s.messages.push(TuiMessage::Streaming {
                id: format!("s{i}"),
                content: String::new(),
                finished: true,
                error: None,
            });
        }

        // 滚到顶：空占位不占滚动空间，最老历史应可见。
        s.scroll_up(100_000);
        let text = render_to_text(&s);
        assert!(
            text.contains("msg 0"),
            "scroll 到顶应显示最老历史 msg 0, got: {text}"
        );

        // 滚回尾：
        s.scroll_to_tail();
        let text = render_to_text(&s);
        assert!(
            text.contains("msg 29"),
            "滚回尾部应显示最新历史 msg 29, got: {text}"
        );
    }

    #[test]
    fn render_new_message_while_scrolled_keeps_oldest_visible() {
        // v0.4.9 滚动失效回归（端到端）：用户滚到历史顶部后，思考流/收尾新消息到达，
        // 视口不应跳回尾部——最老消息仍应可见。
        let mut s = TuiState::new();
        for i in 0..20 {
            s.push_user(format!("msg {i}"));
        }
        s.scroll_up(100_000); // 滚到顶
        let before = render_to_text(&s);
        assert!(
            before.contains("msg 0"),
            "前置：滚到顶应显示最老历史 msg 0, got: {before}"
        );

        // 新消息到达：每 chunk 的 push_think（思考流）+ 收尾 push_assistant。
        s.push_think("inst-1", "deep thoughts");
        s.push_assistant("reply here".into());
        let text = render_to_text(&s);
        assert!(
            text.contains("msg 0"),
            "滚动中到达新消息应保持视口位置（msg 0 仍可见）, got: {text}"
        );
        assert!(
            !text.contains("reply here"),
            "滚动中到达新消息不应跳回尾部显示最新消息, got: {text}"
        );
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
