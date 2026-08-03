use crate::agent::agent_pool::registry::{AgentIdentity, AgentStatus};
use crate::agent::agent_pool::AgentPoolSnapshot;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};

const PENDING_ALERT_THRESHOLD: usize = 10;

const COGNITIVE_SHOW_BELOW: u32 = 5;

#[derive(Debug, Clone, Default)]
pub struct StatusLineState {
    snapshot: Option<AgentPoolSnapshot>,
}

impl StatusLineState {
    pub fn new() -> Self {
        Self { snapshot: None }
    }

    pub fn update(&mut self, snapshot: AgentPoolSnapshot) {
        self.snapshot = Some(snapshot);
    }

    pub fn snapshot(&self) -> Option<&AgentPoolSnapshot> {
        self.snapshot.as_ref()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Segment {
    name: &'static str,
    status: String,
    status_style: Style,
}

impl Segment {
    fn working(name: &'static str) -> Self {
        Self {
            name,
            status: "工作中".to_string(),
            status_style: Style::default().fg(Color::Green),
        }
    }

    fn idle(name: &'static str) -> Self {
        Self {
            name,
            status: "空闲".to_string(),
            status_style: Style::default().fg(Color::DarkGray),
        }
    }

    fn pending(name: &'static str, n: usize) -> Self {
        let style = if n >= PENDING_ALERT_THRESHOLD {
            Style::default().fg(Color::Red).add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Color::Yellow)
        };
        Self {
            name,
            status: format!("pending:{n}"),
            status_style: style,
        }
    }

    fn width(&self) -> usize {
        Line::from(format!("[{}]{}", self.name, self.status)).width()
    }

    fn to_spans(&self) -> Vec<Span<'static>> {
        vec![
            Span::styled(format!("[{}]", self.name), Style::default().fg(Color::Gray)),
            Span::styled(self.status.clone(), self.status_style),
        ]
    }
}

fn platform_segment(
    name: &'static str,
    pending_depth: usize,
    active_batch: &Option<String>,
) -> Segment {
    if active_batch.is_some() {
        Segment::working(name)
    } else if pending_depth > 0 {
        Segment::pending(name, pending_depth)
    } else {
        Segment::idle(name)
    }
}

pub(crate) fn segments_from_snapshot(s: &AgentPoolSnapshot) -> Vec<Segment> {
    let mut segments = Vec::with_capacity(6);

    let thinking_running = s.entries.iter().any(|e| {
        matches!(e.identity, AgentIdentity::ThinkingEngine { .. })
            && e.status == AgentStatus::Running
    });
    segments.push(if thinking_running {
        Segment::working("思考引擎")
    } else {
        Segment::idle("思考引擎")
    });

    segments.push(platform_segment(
        "执行中台",
        s.execution_pending_depth,
        &s.execution_active_batch,
    ));
    segments.push(platform_segment(
        "洞察中台",
        s.insight_pending_depth,
        &s.insight_active_batch,
    ));
    segments.push(platform_segment(
        "记忆中台",
        s.memory_pending_depth,
        &s.memory_active_batch,
    ));

    if (1..COGNITIVE_SHOW_BELOW).contains(&s.cognitive_remaining) {
        segments.push(Segment {
            name: "认知维护",
            status: format!("剩余{}", s.cognitive_remaining),
            status_style: Style::default().fg(Color::Blue),
        });
    }

    if s.repair_in_flight > 0 {
        segments.push(Segment {
            name: "修复中",
            status: s.repair_in_flight.to_string(),
            status_style: Style::default()
                .fg(Color::Magenta)
                .add_modifier(Modifier::BOLD),
        });
    }

    segments
}

const SEP: &str = "  ";

const ELLIPSIS: &str = "…";

pub(crate) fn fit_to_width(segments: &[Segment], width: usize) -> Line<'static> {
    if width == 0 || segments.is_empty() {
        return Line::from("");
    }

    let mut kept = segments.len();
    loop {
        let dropped = kept < segments.len();
        let body_w: usize = segments[..kept].iter().map(Segment::width).sum::<usize>()
            + SEP.len() * kept.saturating_sub(1);
        let total_w = if dropped {
            body_w + SEP.len() + ELLIPSIS.len()
        } else {
            body_w
        };
        if total_w <= width {
            break;
        }
        if kept > 1 {
            kept -= 1;
        } else {
            return narrow_fallback(&segments[0], width);
        }
    }

    let dropped = kept < segments.len();
    let mut spans: Vec<Span<'static>> = Vec::new();
    for (i, seg) in segments[..kept].iter().enumerate() {
        if i > 0 {
            spans.push(Span::raw(SEP));
        }
        spans.extend(seg.to_spans());
    }
    if dropped {
        spans.push(Span::raw(SEP));
        spans.push(Span::styled(ELLIPSIS, Style::default().fg(Color::DarkGray)));
    }
    Line::from(spans)
}

fn narrow_fallback(seg: &Segment, width: usize) -> Line<'static> {
    debug_assert!(width > 0);
    if width == 1 {
        return Line::from(Span::styled(ELLIPSIS, Style::default().fg(Color::DarkGray)));
    }

    let full = format!("[{}]{}", seg.name, seg.status);
    let budget = width - 1;
    let mut truncated = String::new();
    for ch in full.chars() {
        let candidate = format!("{truncated}{ch}");
        if Line::from(candidate.as_str()).width() > budget {
            break;
        }
        truncated.push(ch);
    }
    Line::from(vec![
        Span::styled(truncated, Style::default().fg(Color::Gray)),
        Span::styled(ELLIPSIS, Style::default().fg(Color::DarkGray)),
    ])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::agent_pool::registry::AgentEntry;

    fn stub_snapshot() -> AgentPoolSnapshot {
        AgentPoolSnapshot {
            entries: Vec::new(),
            execution_pending_depth: 0,
            insight_pending_depth: 0,
            memory_pending_depth: 0,
            execution_active_batch: None,
            insight_active_batch: None,
            memory_active_batch: None,
            cognitive_remaining: 0,
            repair_in_flight: 0,
            captured_at: std::time::Instant::now(),
        }
    }

    fn thinking_entry(running: bool) -> AgentEntry {
        AgentEntry {
            id: "inst-1".into(),
            identity: AgentIdentity::ThinkingEngine {
                instance_id: "inst-1".into(),
            },
            status: if running {
                AgentStatus::Running
            } else {
                AgentStatus::Idle
            },
            created_at: std::time::Instant::now(),
        }
    }

    fn line_text(line: &Line) -> String {
        line.spans.iter().map(|s| s.content.to_string()).collect()
    }

    #[test]
    fn thinking_engine_running_shows_working() {
        let mut s = stub_snapshot();
        s.entries.push(thinking_entry(true));
        let segs = segments_from_snapshot(&s);
        assert_eq!(segs[0].status, "工作中");
        assert_eq!(segs[0].status_style.fg, Some(Color::Green));
    }

    #[test]
    fn thinking_engine_idle_when_no_entries() {
        let s = stub_snapshot();
        let segs = segments_from_snapshot(&s);
        assert_eq!(segs[0].status, "空闲");
        assert_eq!(segs[0].status_style.fg, Some(Color::DarkGray));
    }

    #[test]
    fn platform_active_batch_shows_working() {
        let mut s = stub_snapshot();
        s.execution_active_batch = Some("batch-1".into());
        let segs = segments_from_snapshot(&s);
        assert_eq!(segs[1].name, "执行中台");
        assert_eq!(segs[1].status, "工作中");
        assert_eq!(segs[1].status_style.fg, Some(Color::Green));
    }

    #[test]
    fn platform_pending_shows_count_yellow_below_threshold() {
        let mut s = stub_snapshot();
        s.insight_pending_depth = 3;
        let segs = segments_from_snapshot(&s);
        assert_eq!(segs[2].status, "pending:3");
        assert_eq!(segs[2].status_style.fg, Some(Color::Yellow));
        assert!(!segs[2].status_style.add_modifier.contains(Modifier::BOLD));
    }

    #[test]
    fn platform_pending_at_threshold_is_red_bold() {
        let mut s = stub_snapshot();
        s.memory_pending_depth = 10;
        let segs = segments_from_snapshot(&s);
        assert_eq!(segs[3].status, "pending:10");
        assert_eq!(segs[3].status_style.fg, Some(Color::Red));
        assert!(segs[3].status_style.add_modifier.contains(Modifier::BOLD));
    }

    #[test]
    fn cognitive_shown_only_when_one_to_four() {
        let mut s = stub_snapshot();
        s.cognitive_remaining = 2;
        let segs = segments_from_snapshot(&s);
        assert!(segs
            .iter()
            .any(|g| g.name == "认知维护" && g.status == "剩余2"));

        s.cognitive_remaining = 5;
        assert!(!segments_from_snapshot(&s)
            .iter()
            .any(|g| g.name == "认知维护"));

        s.cognitive_remaining = 0;
        assert!(!segments_from_snapshot(&s)
            .iter()
            .any(|g| g.name == "认知维护"));
    }

    #[test]
    fn repair_shown_only_when_in_flight() {
        let mut s = stub_snapshot();
        s.repair_in_flight = 1;
        let segs = segments_from_snapshot(&s);
        let repair = segs
            .iter()
            .find(|g| g.name == "修复中")
            .expect("应显示修复中");
        assert_eq!(repair.status, "1");
        assert_eq!(repair.status_style.fg, Some(Color::Magenta));

        s.repair_in_flight = 0;
        assert!(!segments_from_snapshot(&s)
            .iter()
            .any(|g| g.name == "修复中"));
    }

    fn full_segments() -> Vec<Segment> {
        let mut s = stub_snapshot();
        s.entries.push(thinking_entry(true));
        s.execution_pending_depth = 3;
        s.memory_active_batch = Some("b".into());
        s.cognitive_remaining = 2;
        segments_from_snapshot(&s)
    }

    #[test]
    fn fits_all_when_width_ample() {
        let segs = full_segments();
        let line = fit_to_width(&segs, 200);
        let text = line_text(&line);
        assert!(text.contains("[思考引擎]工作中"));
        assert!(text.contains("[执行中台]pending:3"));
        assert!(text.contains("[认知维护]剩余2"));
        assert!(!text.contains(ELLIPSIS), "宽度充足不应截断: {text}");
    }

    #[test]
    fn drops_lowest_priority_first_with_ellipsis() {
        let segs = full_segments();
        let width = segs[0].width() + SEP.len() + segs[1].width() + SEP.len() + ELLIPSIS.len();
        let line = fit_to_width(&segs, width);
        let text = line_text(&line);
        assert!(text.contains("[思考引擎]工作中"), "思考引擎必保留: {text}");
        assert!(
            text.contains("[执行中台]pending:3"),
            "执行优先于洞察: {text}"
        );
        assert!(!text.contains("洞察中台"), "洞察应被丢弃: {text}");
        assert!(!text.contains("认知维护"), "认知应最先被丢弃: {text}");
        assert!(text.contains(ELLIPSIS), "截断应有 …: {text}");
        assert!(
            line.width() <= width,
            "行宽不得超 {width}: {}",
            line.width()
        );
    }

    #[test]
    fn extreme_narrow_keeps_thinking_engine_truncated() {
        let segs = full_segments();
        let line = fit_to_width(&segs, 8);
        let text = line_text(&line);
        assert!(text.contains(ELLIPSIS), "极端窄应有 …: {text}");
        assert!(line.width() <= 8, "行宽不得超 8: {}", line.width());
        assert!(!text.contains("执行中台"), "极端窄只留思考引擎残段: {text}");
    }

    #[test]
    fn cjk_width_counts_cells_not_chars() {
        let seg = Segment::idle("思考引擎");
        assert_eq!(seg.width(), 10 + 4, "[思考引擎]空闲 = 14 cells");
    }

    #[test]
    fn width_one_renders_ellipsis_only() {
        let segs = full_segments();
        let line = fit_to_width(&segs, 1);
        assert_eq!(line_text(&line), ELLIPSIS);
    }

    #[test]
    fn width_zero_renders_empty() {
        let segs = full_segments();
        let line = fit_to_width(&segs, 0);
        assert_eq!(line_text(&line), "");
    }

    #[test]
    fn all_idle_renders_all_four_segments() {
        let s = stub_snapshot();
        let segs = segments_from_snapshot(&s);
        assert_eq!(segs.len(), 4);
        assert!(segs.iter().all(|g| g.status == "空闲"));
        let line = fit_to_width(&segs, 120);
        assert!(!line_text(&line).contains(ELLIPSIS));
    }
}
