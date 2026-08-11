use crate::data::duckdb::loader::ModelRow;
use crossterm::event::KeyCode;
use ratatui::layout::Rect;
use ratatui::Frame;

pub const PRESET_TEMPLATES: &[(&str, &str, &str, &str)] = &[
    (
        "OpenAI 官方",
        "openai",
        "https://api.openai.com/v1",
        "OpenAI",
    ),
    (
        "Anthropic 官方",
        "anthropic",
        "https://api.anthropic.com",
        "Anthropic",
    ),
];

const MENU_ITEMS: &[(&str, bool)] = &[
    ("Model + Provider", true),
    ("工作区管理", false),
    ("Agent 改名", true),
    ("默认设置", false),
    ("记忆中台模式", true),
    ("上下文编辑", false),
];

#[derive(Debug, Clone)]
pub enum ActionResult {
    Navigate,

    Exit,
}

#[derive(Debug, Clone, Default)]
pub struct FormField {
    pub label: &'static str,
    pub value: String,
    pub is_secret: bool,
}

#[derive(Debug, Clone)]
pub struct AddModelForm {
    pub template_idx: Option<usize>,
    pub fields: Vec<FormField>,
    pub field_cursor: usize,
    pub submitted: bool,
}

#[derive(Debug, Clone)]
pub struct QuickAddForm {
    pub provider: String,
    pub fields: Vec<FormField>,
    pub field_cursor: usize,
    pub submitted: bool,
}

#[derive(Debug, Clone)]
pub struct ChangeKeyForm {
    pub fields: Vec<FormField>,
    pub field_cursor: usize,
    pub submitted: bool,
}

#[derive(Debug, Clone)]
pub struct SetDefaultSelect {
    pub candidates: Vec<ModelRow>,
    pub cursor: usize,
    pub submitted: bool,
}

#[derive(Debug, Clone, Default)]
pub struct RenameAgentForm {
    pub name: String,
    pub submitted: bool,
}

#[derive(Debug, Clone)]
pub enum ConfigView {
    Menu,

    ModelList,

    AddModel(AddModelForm),

    AddModelSelectTemplate {
        cursor: usize,
    },

    QuickAddSelectProvider {
        providers: Vec<String>,
        cursor: usize,
    },

    QuickAdd(QuickAddForm),

    ChangeKey(ChangeKeyForm),

    SetDefault(SetDefaultSelect),

    MemoryModeSelect {
        cursor: usize,
        submitted: bool,
    },

    RenameAgent(RenameAgentForm),
}

#[derive(Debug, Clone)]
pub struct ConfigPanel {
    pub view: ConfigView,

    pub menu_cursor: usize,

    pub list_cursor: usize,

    pub expanded: Option<usize>,

    pub models: Vec<ModelRow>,

    pub message: Option<(String, bool)>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum DbRequest {
    LoadModels,

    LoadProviders,

    LoadDefaultCandidates,

    SubmitAddModel {
        provider: String,
        api_url: String,
        api_type: String,
        api_key: String,
        name: String,
        model_id: String,
    },

    SubmitQuickAdd {
        provider: String,
        name: String,
        model_id: String,
    },

    SubmitChangeKey {
        provider: String,
        api_key: String,
    },

    SubmitSetDefault {
        model_id: String,
    },

    SaveMemoryMode {
        mode: String,
    },

    SubmitRenameAgent {
        display_name: String,
    },

    None,
}

impl ConfigPanel {
    pub fn new() -> Self {
        Self {
            view: ConfigView::Menu,
            menu_cursor: 0,
            list_cursor: 0,
            expanded: None,
            models: Vec::new(),
            message: None,
        }
    }

    pub fn reload_models(&mut self, models: Vec<ModelRow>) {
        self.models = models;
        if self.list_cursor >= self.models.len() && !self.models.is_empty() {
            self.list_cursor = self.models.len() - 1;
        }
    }

    pub fn pending_db_request(&self) -> DbRequest {
        match &self.view {
            ConfigView::ModelList if self.models.is_empty() => DbRequest::LoadModels,
            ConfigView::QuickAddSelectProvider { providers, .. } if providers.is_empty() => {
                DbRequest::LoadProviders
            }
            ConfigView::SetDefault(sel) if sel.candidates.is_empty() => {
                DbRequest::LoadDefaultCandidates
            }
            _ => self.check_form_submit(),
        }
    }

    fn check_form_submit(&self) -> DbRequest {
        match &self.view {
            ConfigView::AddModel(form) if form.submitted => DbRequest::SubmitAddModel {
                provider: form.fields[0].value.clone(),
                api_url: form.fields[1].value.clone(),
                api_type: form.fields[2].value.clone(),
                api_key: form.fields[3].value.clone(),
                name: form.fields[4].value.clone(),
                model_id: form.fields[5].value.clone(),
            },
            ConfigView::QuickAdd(form) if form.submitted => DbRequest::SubmitQuickAdd {
                provider: form.provider.clone(),
                name: form.fields[0].value.clone(),
                model_id: form.fields[1].value.clone(),
            },
            ConfigView::ChangeKey(form) if form.submitted => DbRequest::SubmitChangeKey {
                provider: form.fields[0].value.clone(),
                api_key: form.fields[1].value.clone(),
            },
            ConfigView::SetDefault(sel) if sel.submitted => {
                let model_id = sel
                    .candidates
                    .get(sel.cursor)
                    .map(|m| m.id.clone())
                    .unwrap_or_default();
                DbRequest::SubmitSetDefault { model_id }
            }
            ConfigView::MemoryModeSelect { cursor, submitted } if *submitted => {
                let mode = match cursor {
                    0 => "sync",
                    1 => "mixed",
                    _ => "async",
                };
                DbRequest::SaveMemoryMode {
                    mode: mode.to_string(),
                }
            }
            ConfigView::RenameAgent(form) if form.submitted => DbRequest::SubmitRenameAgent {
                display_name: form.name.clone(),
            },
            _ => DbRequest::None,
        }
    }

    pub fn clear_db_request(&mut self) {
        match &mut self.view {
            ConfigView::AddModel(f) => f.submitted = false,
            ConfigView::QuickAdd(f) => f.submitted = false,
            ConfigView::ChangeKey(f) => f.submitted = false,
            ConfigView::SetDefault(s) => s.submitted = false,
            ConfigView::MemoryModeSelect { submitted, .. } => *submitted = false,
            ConfigView::RenameAgent(f) => f.submitted = false,
            _ => {}
        }
    }

    pub fn handle_key(&mut self, key: KeyCode) -> ActionResult {
        match std::mem::replace(&mut self.view, ConfigView::Menu) {
            ConfigView::Menu => self.handle_menu_key(key),
            ConfigView::ModelList => self.handle_model_list_key(key),
            ConfigView::AddModelSelectTemplate { mut cursor } => {
                let r = self.handle_template_select_key(key, &mut cursor);
                if matches!(self.view, ConfigView::Menu) {
                    self.view = ConfigView::AddModelSelectTemplate { cursor };
                }
                r
            }
            ConfigView::AddModel(mut form) => {
                let r = self.handle_form_key(
                    key,
                    &mut form.fields,
                    &mut form.field_cursor,
                    &mut form.submitted,
                );
                if matches!(self.view, ConfigView::Menu) {
                    self.view = ConfigView::AddModel(form);
                }
                r
            }
            ConfigView::QuickAddSelectProvider {
                mut providers,
                mut cursor,
            } => {
                let r = self.handle_provider_select_key(key, &mut providers, &mut cursor);
                if matches!(self.view, ConfigView::Menu) {
                    self.view = ConfigView::QuickAddSelectProvider { providers, cursor };
                }
                r
            }
            ConfigView::QuickAdd(mut form) => {
                let r = self.handle_form_key(
                    key,
                    &mut form.fields,
                    &mut form.field_cursor,
                    &mut form.submitted,
                );
                if matches!(self.view, ConfigView::Menu) {
                    self.view = ConfigView::QuickAdd(form);
                }
                r
            }
            ConfigView::ChangeKey(mut form) => {
                let r = self.handle_form_key(
                    key,
                    &mut form.fields,
                    &mut form.field_cursor,
                    &mut form.submitted,
                );
                if matches!(self.view, ConfigView::Menu) {
                    self.view = ConfigView::ChangeKey(form);
                }
                r
            }
            ConfigView::SetDefault(mut sel) => {
                let r = self.handle_set_default_key(key, &mut sel);
                if matches!(self.view, ConfigView::Menu) {
                    self.view = ConfigView::SetDefault(sel);
                }
                r
            }
            ConfigView::MemoryModeSelect {
                mut cursor,
                mut submitted,
            } => {
                let r = self.handle_memory_mode_key(key, &mut cursor, &mut submitted);

                if matches!(self.view, ConfigView::Menu) && self.expanded.is_some() {
                    self.view = ConfigView::MemoryModeSelect { cursor, submitted };
                }
                r
            }
            ConfigView::RenameAgent(mut form) => {
                let r = self.handle_rename_agent_key(key, &mut form);
                if matches!(self.view, ConfigView::Menu) {
                    self.view = ConfigView::RenameAgent(form);
                }
                r
            }
        }
    }

    fn handle_menu_key(&mut self, key: KeyCode) -> ActionResult {
        match key {
            KeyCode::Up => {
                if self.menu_cursor > 0 {
                    self.menu_cursor -= 1;
                }
                ActionResult::Navigate
            }
            KeyCode::Down => {
                if self.menu_cursor < MENU_ITEMS.len() - 1 {
                    self.menu_cursor += 1;
                }
                ActionResult::Navigate
            }
            KeyCode::Right | KeyCode::Enter => {
                let idx = self.menu_cursor;
                let enabled = MENU_ITEMS[idx].1;
                if enabled {
                    self.expanded = Some(idx);
                    if idx == 4 {
                        self.view = ConfigView::MemoryModeSelect {
                            cursor: 1,
                            submitted: false,
                        };
                    } else if idx == 2 {
                        self.view = ConfigView::RenameAgent(RenameAgentForm::default());
                    } else {
                        self.view = ConfigView::ModelList;
                    }
                    self.list_cursor = 0;
                }
                ActionResult::Navigate
            }
            KeyCode::Left | KeyCode::Esc => ActionResult::Exit,
            _ => ActionResult::Navigate,
        }
    }

    fn handle_model_list_key(&mut self, key: KeyCode) -> ActionResult {
        match key {
            KeyCode::Up => {
                if self.list_cursor > 0 {
                    self.list_cursor -= 1;
                }
                ActionResult::Navigate
            }
            KeyCode::Down => {
                if !self.models.is_empty() && self.list_cursor < self.models.len() - 1 {
                    self.list_cursor += 1;
                }
                ActionResult::Navigate
            }
            KeyCode::Left => {
                self.view = ConfigView::Menu;
                self.expanded = None;
                ActionResult::Navigate
            }
            KeyCode::Esc => ActionResult::Exit,

            KeyCode::Char('a') => {
                self.view = ConfigView::AddModelSelectTemplate { cursor: 0 };
                ActionResult::Navigate
            }
            KeyCode::Char('q') => {
                self.view = ConfigView::QuickAddSelectProvider {
                    providers: Vec::new(),
                    cursor: 0,
                };
                ActionResult::Navigate
            }
            KeyCode::Char('k') => {
                let default_provider = self
                    .models
                    .get(self.list_cursor)
                    .map(|m| m.provider.clone())
                    .unwrap_or_default();
                self.view = ConfigView::ChangeKey(ChangeKeyForm {
                    fields: vec![
                        FormField {
                            label: "provider",
                            value: default_provider,
                            is_secret: false,
                        },
                        FormField {
                            label: "新 API key",
                            value: String::new(),
                            is_secret: true,
                        },
                    ],
                    field_cursor: 0,
                    submitted: false,
                });
                ActionResult::Navigate
            }
            KeyCode::Char('d') => {
                self.view = ConfigView::SetDefault(SetDefaultSelect {
                    candidates: Vec::new(),
                    cursor: 0,
                    submitted: false,
                });
                ActionResult::Navigate
            }
            _ => ActionResult::Navigate,
        }
    }

    fn handle_form_key(
        &mut self,
        key: KeyCode,
        fields: &mut [FormField],
        cursor: &mut usize,
        submitted: &mut bool,
    ) -> ActionResult {
        match key {
            KeyCode::Left => {
                self.view = ConfigView::ModelList;
                ActionResult::Navigate
            }
            KeyCode::Esc => ActionResult::Exit,
            KeyCode::Tab | KeyCode::Right => {
                if *cursor < fields.len() - 1 {
                    *cursor += 1;
                }
                ActionResult::Navigate
            }
            KeyCode::Enter => {
                if *cursor < fields.len() - 1 {
                    *cursor += 1;
                } else {
                    *submitted = true;
                }
                ActionResult::Navigate
            }
            KeyCode::Backspace => {
                fields[*cursor].value.pop();
                ActionResult::Navigate
            }
            KeyCode::Char(c) => {
                fields[*cursor].value.push(c);
                ActionResult::Navigate
            }
            _ => ActionResult::Navigate,
        }
    }

    fn handle_template_select_key(&mut self, key: KeyCode, cursor: &mut usize) -> ActionResult {
        match key {
            KeyCode::Up => {
                if *cursor > 0 {
                    *cursor -= 1;
                }
                ActionResult::Navigate
            }
            KeyCode::Down => {
                if *cursor < PRESET_TEMPLATES.len() {
                    *cursor += 1;
                }
                ActionResult::Navigate
            }
            KeyCode::Left => {
                self.view = ConfigView::ModelList;
                ActionResult::Navigate
            }
            KeyCode::Esc => ActionResult::Exit,
            KeyCode::Right | KeyCode::Enter => {
                let (provider, api_url, api_type) = if *cursor < PRESET_TEMPLATES.len() {
                    let t = PRESET_TEMPLATES[*cursor];
                    (t.1.to_string(), t.2.to_string(), t.3.to_string())
                } else {
                    (String::new(), String::new(), String::new())
                };
                self.view = ConfigView::AddModel(AddModelForm {
                    template_idx: if *cursor < PRESET_TEMPLATES.len() {
                        Some(*cursor)
                    } else {
                        None
                    },
                    fields: vec![
                        FormField {
                            label: "provider",
                            value: provider,
                            is_secret: false,
                        },
                        FormField {
                            label: "api_url",
                            value: api_url,
                            is_secret: false,
                        },
                        FormField {
                            label: "api_type",
                            value: api_type,
                            is_secret: false,
                        },
                        FormField {
                            label: "API key",
                            value: String::new(),
                            is_secret: true,
                        },
                        FormField {
                            label: "模型显示名",
                            value: String::new(),
                            is_secret: false,
                        },
                        FormField {
                            label: "model_id",
                            value: String::new(),
                            is_secret: false,
                        },
                    ],
                    field_cursor: 3,
                    submitted: false,
                });
                ActionResult::Navigate
            }
            _ => ActionResult::Navigate,
        }
    }

    fn handle_provider_select_key(
        &mut self,
        key: KeyCode,
        providers: &mut [String],
        cursor: &mut usize,
    ) -> ActionResult {
        match key {
            KeyCode::Up => {
                if *cursor > 0 {
                    *cursor -= 1;
                }
                ActionResult::Navigate
            }
            KeyCode::Down => {
                if *cursor < providers.len().saturating_sub(1) {
                    *cursor += 1;
                }
                ActionResult::Navigate
            }
            KeyCode::Left => {
                self.view = ConfigView::ModelList;
                ActionResult::Navigate
            }
            KeyCode::Esc => ActionResult::Exit,
            KeyCode::Right | KeyCode::Enter => {
                if providers.is_empty() {
                    self.message = Some((
                        "无已配置 key 的 provider, 先 '新增 model' 配一个".into(),
                        true,
                    ));
                    self.view = ConfigView::ModelList;
                } else {
                    let provider = providers[*cursor].clone();
                    self.view = ConfigView::QuickAdd(QuickAddForm {
                        provider,
                        fields: vec![
                            FormField {
                                label: "模型显示名",
                                value: String::new(),
                                is_secret: false,
                            },
                            FormField {
                                label: "model_id",
                                value: String::new(),
                                is_secret: false,
                            },
                        ],
                        field_cursor: 0,
                        submitted: false,
                    });
                }
                ActionResult::Navigate
            }
            _ => ActionResult::Navigate,
        }
    }

    fn handle_set_default_key(&mut self, key: KeyCode, sel: &mut SetDefaultSelect) -> ActionResult {
        match key {
            KeyCode::Up => {
                if sel.cursor > 0 {
                    sel.cursor -= 1;
                }
                ActionResult::Navigate
            }
            KeyCode::Down => {
                if sel.cursor < sel.candidates.len().saturating_sub(1) {
                    sel.cursor += 1;
                }
                ActionResult::Navigate
            }
            KeyCode::Left => {
                self.view = ConfigView::ModelList;
                ActionResult::Navigate
            }
            KeyCode::Esc => ActionResult::Exit,
            KeyCode::Right | KeyCode::Enter => {
                sel.submitted = true;
                ActionResult::Navigate
            }
            _ => ActionResult::Navigate,
        }
    }

    fn handle_memory_mode_key(
        &mut self,
        key: KeyCode,
        cursor: &mut usize,
        submitted: &mut bool,
    ) -> ActionResult {
        match key {
            KeyCode::Up => {
                if *cursor > 0 {
                    *cursor -= 1;
                }
                ActionResult::Navigate
            }
            KeyCode::Down => {
                if *cursor < 2 {
                    *cursor += 1;
                }
                ActionResult::Navigate
            }
            KeyCode::Left => {
                self.view = ConfigView::Menu;
                self.expanded = None;
                ActionResult::Navigate
            }
            KeyCode::Esc => ActionResult::Exit,
            KeyCode::Right | KeyCode::Enter => {
                *submitted = true;
                ActionResult::Navigate
            }
            _ => ActionResult::Navigate,
        }
    }

    fn handle_rename_agent_key(
        &mut self,
        key: KeyCode,
        form: &mut RenameAgentForm,
    ) -> ActionResult {
        match key {
            KeyCode::Left | KeyCode::Esc => {
                self.view = ConfigView::Menu;
                self.expanded = None;
                ActionResult::Navigate
            }
            KeyCode::Enter => {
                if !form.name.is_empty() {
                    form.submitted = true;
                }
                ActionResult::Navigate
            }
            KeyCode::Backspace => {
                form.name.pop();
                ActionResult::Navigate
            }
            KeyCode::Char(c) => {
                form.name.push(c);
                ActionResult::Navigate
            }
            _ => ActionResult::Navigate,
        }
    }

    pub fn nav_help(&self) -> &'static str {
        match &self.view {
            ConfigView::Menu => "← 退出设置    ↑↓ 选择    → 进入    Esc 退出",
            ConfigView::ModelList => {
                "← 返回上级    ↑↓ 选择    a 新增  q 快速  k 改Key  d 切默认    Esc 退出"
            }
            ConfigView::AddModelSelectTemplate { .. }
            | ConfigView::QuickAddSelectProvider { .. }
            | ConfigView::SetDefault(_)
            | ConfigView::MemoryModeSelect { .. } => {
                "← 返回上级    ↑↓ 选择    → 确认    Esc 退出设置"
            }
            ConfigView::AddModel(_) | ConfigView::QuickAdd(_) | ConfigView::ChangeKey(_) => {
                "← 取消        Tab 下一字段  Enter 确认    Esc 退出设置"
            }
            ConfigView::RenameAgent(_) => "← 取消    Enter 确认    Esc 退出设置",
        }
    }
}

impl Default for ConfigPanel {
    fn default() -> Self {
        Self::new()
    }
}

pub fn render(panel: &ConfigPanel, frame: &mut Frame, area: Rect) {
    use ratatui::layout::{Constraint, Direction, Layout};
    use ratatui::style::{Color, Modifier, Style};
    use ratatui::text::{Line, Span};
    use ratatui::widgets::Paragraph;

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Min(3),
            Constraint::Length(1),
        ])
        .split(area);

    let title = Paragraph::new(Line::from(vec![Span::styled(
        "⚙ 设置",
        Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD),
    )]));
    frame.render_widget(title, chunks[0]);

    let content_area = chunks[1];
    match &panel.view {
        ConfigView::Menu => render_menu(panel, frame, content_area),
        ConfigView::ModelList => render_model_list(panel, frame, content_area),
        ConfigView::AddModelSelectTemplate { cursor } => {
            render_template_list(panel, frame, content_area, *cursor);
        }
        ConfigView::AddModel(form) => {
            render_form(panel, frame, content_area, &form.fields, form.field_cursor);
        }
        ConfigView::QuickAddSelectProvider { providers, cursor } => {
            render_provider_list(panel, frame, content_area, providers, *cursor);
        }
        ConfigView::QuickAdd(form) => {
            render_form(panel, frame, content_area, &form.fields, form.field_cursor);
        }
        ConfigView::ChangeKey(form) => {
            render_form(panel, frame, content_area, &form.fields, form.field_cursor);
        }
        ConfigView::SetDefault(sel) => render_set_default(panel, frame, content_area, sel),
        ConfigView::MemoryModeSelect { cursor, .. } => {
            render_memory_mode_select(panel, frame, content_area, *cursor);
        }
        ConfigView::RenameAgent(form) => {
            render_rename_agent(panel, frame, content_area, form);
        }
    }

    if let Some((msg, is_error)) = &panel.message {
        let color = if *is_error { Color::Red } else { Color::Green };
        let msg_para = Paragraph::new(Line::from(vec![Span::styled(
            msg.as_str(),
            Style::default().fg(color),
        )]));
        let msg_area = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Min(1), Constraint::Length(1)])
            .split(content_area);
        frame.render_widget(msg_para, msg_area[1]);
    }

    let nav = Paragraph::new(Line::from(vec![Span::styled(
        panel.nav_help(),
        Style::default().fg(Color::DarkGray),
    )]));
    frame.render_widget(nav, chunks[2]);
}

fn render_menu(panel: &ConfigPanel, frame: &mut Frame, area: Rect) {
    use ratatui::style::{Color, Modifier, Style};
    use ratatui::text::{Line, Span};
    use ratatui::widgets::Paragraph;

    let mut lines: Vec<Line> = Vec::new();
    for (i, (title, enabled)) in MENU_ITEMS.iter().enumerate() {
        let is_expanded = panel.expanded == Some(i);
        let is_selected = panel.menu_cursor == i;
        let marker = if is_expanded { "▼" } else { "▶" };

        let style = if is_selected {
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Color::Gray)
        };

        let suffix = if *enabled { "→" } else { "(待后续)" };
        lines.push(Line::from(vec![
            Span::styled(format!("  {} ", marker), style),
            Span::styled(*title, style),
            Span::raw("  "),
            Span::styled(suffix, Style::default().fg(Color::DarkGray)),
        ]));

        if is_expanded && *enabled {
            lines.push(Line::from(vec![
                Span::raw("    "),
                Span::styled(
                    "↑↓ 选择 → 进入  ← 折叠",
                    Style::default().fg(Color::DarkGray),
                ),
            ]));
        }
    }
    frame.render_widget(Paragraph::new(lines), area);
}

fn render_model_list(panel: &ConfigPanel, frame: &mut Frame, area: Rect) {
    use ratatui::style::{Color, Modifier, Style};
    use ratatui::text::{Line, Span};
    use ratatui::widgets::Paragraph;

    let mut lines: Vec<Line> = Vec::new();

    lines.push(Line::from(vec![Span::styled(
        "▼ Model + Provider",
        Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD),
    )]));
    lines.push(Line::from(""));

    if panel.models.is_empty() {
        lines.push(Line::from(vec![Span::styled(
            "  (无模型, 按 a 新增)",
            Style::default().fg(Color::DarkGray),
        )]));
    } else {
        for (i, m) in panel.models.iter().enumerate() {
            let has_key = m.api_key.as_ref().map(|k| !k.is_empty()).unwrap_or(false);
            let mark = if has_key { "✓" } else { "✗" };
            let is_selected = panel.list_cursor == i;
            let style = if is_selected {
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(Color::Gray)
            };
            lines.push(Line::from(vec![
                Span::styled(format!("  [{}] ", mark), style),
                Span::styled(format!("{:<30}", m.id), style),
                Span::raw(" | "),
                Span::styled(
                    format!("{:>10}", &m.provider),
                    Style::default().fg(Color::DarkGray),
                ),
                Span::raw(" | "),
                Span::styled(
                    if has_key { "已设" } else { "空" },
                    Style::default().fg(if has_key { Color::Green } else { Color::Yellow }),
                ),
            ]));
        }
    }

    lines.push(Line::from(""));

    lines.push(Line::from(vec![
        Span::styled("  [a] 新增  ", Style::default().fg(Color::Green)),
        Span::styled("[q] 快速新增  ", Style::default().fg(Color::Green)),
        Span::styled("[k] 改Key  ", Style::default().fg(Color::Green)),
        Span::styled("[d] 切默认", Style::default().fg(Color::Green)),
    ]));

    frame.render_widget(Paragraph::new(lines), area);
}

fn render_template_list(_panel: &ConfigPanel, frame: &mut Frame, area: Rect, cursor: usize) {
    use ratatui::style::{Color, Modifier, Style};
    use ratatui::text::{Line, Span};
    use ratatui::widgets::Paragraph;

    let mut lines: Vec<Line> = vec![
        Line::from(""),
        Line::from(vec![Span::styled(
            "选择 provider 模板:",
            Style::default().fg(Color::Gray),
        )]),
        Line::from(""),
    ];

    for (i, (name, _, _, _)) in PRESET_TEMPLATES.iter().enumerate() {
        let is_sel = cursor == i;
        let style = if is_sel {
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Color::Gray)
        };
        let marker = if is_sel { "▶" } else { " " };
        lines.push(Line::from(vec![Span::styled(
            format!(" {} {}", marker, name),
            style,
        )]));
    }

    let is_sel = cursor == PRESET_TEMPLATES.len();
    let style = if is_sel {
        Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(Color::Gray)
    };
    let marker = if is_sel { "▶" } else { " " };
    lines.push(Line::from(vec![Span::styled(
        format!(" {} 自定义", marker),
        style,
    )]));

    frame.render_widget(Paragraph::new(lines), area);
}

fn render_form(
    _panel: &ConfigPanel,
    frame: &mut Frame,
    area: Rect,
    fields: &[FormField],
    cursor: usize,
) {
    use ratatui::style::{Color, Modifier, Style};
    use ratatui::text::{Line, Span};
    use ratatui::widgets::Paragraph;

    let mut lines: Vec<Line> = Vec::new();
    lines.push(Line::from(""));

    for (i, f) in fields.iter().enumerate() {
        let is_current = i == cursor;
        let style = if is_current {
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Color::Gray)
        };
        let display: String = if f.is_secret && !f.value.is_empty() {
            "•".repeat(f.value.len())
        } else {
            f.value.clone()
        };
        let marker = if is_current { "▶" } else { " " };
        lines.push(Line::from(vec![
            Span::styled(format!(" {} ", marker), style),
            Span::styled(format!("{}: ", f.label), style),
            Span::styled(display.to_string(), Style::default().fg(Color::White)),
            if is_current {
                Span::styled("_", Style::default().fg(Color::Cyan))
            } else {
                Span::raw("")
            },
        ]));
    }

    lines.push(Line::from(""));
    lines.push(Line::from(vec![Span::styled(
        "  Tab/→ 下一字段  Enter 确认  ← 取消",
        Style::default().fg(Color::DarkGray),
    )]));

    frame.render_widget(Paragraph::new(lines), area);
}

fn render_provider_list(
    _panel: &ConfigPanel,
    frame: &mut Frame,
    area: Rect,
    providers: &[String],
    cursor: usize,
) {
    use ratatui::style::{Color, Modifier, Style};
    use ratatui::text::{Line, Span};
    use ratatui::widgets::Paragraph;

    let mut lines: Vec<Line> = vec![
        Line::from(""),
        Line::from(vec![Span::styled(
            "选 provider (带出 api_url/api_key/api_type):",
            Style::default().fg(Color::Gray),
        )]),
        Line::from(""),
    ];

    if providers.is_empty() {
        lines.push(Line::from(vec![Span::styled(
            "  (无已配置 key 的 provider)",
            Style::default().fg(Color::DarkGray),
        )]));
    } else {
        for (i, p) in providers.iter().enumerate() {
            let is_sel = cursor == i;
            let style = if is_sel {
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(Color::Gray)
            };
            let marker = if is_sel { "▶" } else { " " };
            lines.push(Line::from(vec![Span::styled(
                format!(" {} {}", marker, p),
                style,
            )]));
        }
    }
    frame.render_widget(Paragraph::new(lines), area);
}

fn render_set_default(_panel: &ConfigPanel, frame: &mut Frame, area: Rect, sel: &SetDefaultSelect) {
    use ratatui::style::{Color, Modifier, Style};
    use ratatui::text::{Line, Span};
    use ratatui::widgets::Paragraph;

    let mut lines: Vec<Line> = vec![
        Line::from(""),
        Line::from(vec![Span::styled(
            "选默认模型:",
            Style::default().fg(Color::Gray),
        )]),
        Line::from(""),
    ];

    for (i, m) in sel.candidates.iter().enumerate() {
        let is_sel = sel.cursor == i;
        let style = if is_sel {
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Color::Gray)
        };
        let marker = if is_sel { "▶" } else { " " };
        lines.push(Line::from(vec![Span::styled(
            format!(" {} {} ({})", marker, m.id, m.provider),
            style,
        )]));
    }
    frame.render_widget(Paragraph::new(lines), area);
}

fn render_memory_mode_select(_panel: &ConfigPanel, frame: &mut Frame, area: Rect, cursor: usize) {
    use ratatui::style::{Color, Modifier, Style};
    use ratatui::text::{Line, Span};
    use ratatui::widgets::Paragraph;

    let options: [(&str, &str); 3] = [
        ("sync", "同步 — 记忆回音触发续跑 (settle 完成后新实例)"),
        ("mixed", "混合 — 洞察回音触发 + 有界等待 settle (默认)"),
        ("async", "异步 — 洞察回音触发, 记忆异步落库按时间戳替换"),
    ];

    let mut lines: Vec<Line> = vec![
        Line::from(""),
        Line::from(vec![Span::styled(
            "记忆中台模式 (切换需无飞行消息: UNNI/KEEP 下无实例运行):",
            Style::default().fg(Color::Gray),
        )]),
        Line::from(""),
    ];
    for (i, (name, desc)) in options.iter().enumerate() {
        let is_sel = cursor == i;
        let style = if is_sel {
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Color::Gray)
        };
        let marker = if is_sel { "▶" } else { " " };
        lines.push(Line::from(vec![
            Span::styled(format!(" {} ", marker), style),
            Span::styled(*name, style),
            Span::styled("  ", Style::default()),
            Span::styled(*desc, Style::default().fg(Color::DarkGray)),
        ]));
    }
    frame.render_widget(Paragraph::new(lines), area);
}

fn render_rename_agent(
    _panel: &ConfigPanel,
    frame: &mut Frame,
    area: Rect,
    form: &RenameAgentForm,
) {
    use ratatui::style::{Color, Modifier, Style};
    use ratatui::text::{Line, Span};
    use ratatui::widgets::Paragraph;

    let lines: Vec<Line> = vec![
        Line::from(""),
        Line::from(vec![Span::styled(
            "输入 agent 的新显示名称:",
            Style::default().fg(Color::Gray),
        )]),
        Line::from(""),
        Line::from(vec![
            Span::styled(
                "  ▶ ",
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                "新名称: ",
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(form.name.clone(), Style::default().fg(Color::White)),
            Span::styled("_", Style::default().fg(Color::Cyan)),
        ]),
        Line::from(""),
        Line::from(vec![Span::styled(
            "  Enter 确认    ← 取消",
            Style::default().fg(Color::DarkGray),
        )]),
    ];

    frame.render_widget(Paragraph::new(lines), area);
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    fn render_to_text(panel: &ConfigPanel) -> String {
        let backend = TestBackend::new(50, 15);
        let mut terminal = Terminal::new(backend).unwrap();
        let frame = terminal.draw(|f| render(panel, f, f.area())).unwrap();
        frame.buffer.content().iter().map(|c| c.symbol()).collect()
    }

    #[test]
    fn menu_shows_5_items() {
        let panel = ConfigPanel::new();
        let text = render_to_text(&panel);

        assert!(text.contains("Model + Provider"), "menu item 1");

        assert!(text.contains('工'), "menu item 2");
        assert!(text.contains("Agent"), "menu item 3 (Agent prefix)");
        assert!(text.contains('默'), "menu item 4");
        assert!(text.contains('上'), "menu item 5");

        let pending_count = text.matches('(').count();
        assert_eq!(
            pending_count, 3,
            "3 disabled items show (待后续), got: {text}"
        );
    }

    #[test]
    fn menu_shows_folded_marker() {
        let panel = ConfigPanel::new();
        let text = render_to_text(&panel);
        assert!(text.contains('▶'), "folded items show ▶");
    }

    #[test]
    fn menu_shows_expanded_marker() {
        let mut panel = ConfigPanel::new();
        panel.expanded = Some(0);
        let text = render_to_text(&panel);
        assert!(text.contains('▼'), "expanded item shows ▼");
    }

    #[test]
    fn nav_bar_shows_exit_hint() {
        let panel = ConfigPanel::new();
        let text = render_to_text(&panel);

        assert!(text.contains("Esc"), "nav bar shows Esc hint");
    }

    #[test]
    fn menu_down_moves_cursor() {
        let mut p = ConfigPanel::new();
        assert_eq!(p.menu_cursor, 0);
        p.handle_key(KeyCode::Down);
        assert_eq!(p.menu_cursor, 1);
    }

    #[test]
    fn menu_up_wraps_or_stops() {
        let mut p = ConfigPanel::new();
        p.handle_key(KeyCode::Up);
        assert_eq!(p.menu_cursor, 0);
    }

    #[test]
    fn menu_right_enters_model_list() {
        let mut p = ConfigPanel::new();
        p.handle_key(KeyCode::Right);
        assert!(matches!(p.view, ConfigView::ModelList));
        assert_eq!(p.expanded, Some(0));
    }

    #[test]
    fn menu_left_exits() {
        let mut p = ConfigPanel::new();
        let r = p.handle_key(KeyCode::Left);
        assert!(matches!(r, ActionResult::Exit));
    }

    #[test]
    fn menu_esc_exits() {
        let mut p = ConfigPanel::new();
        let r = p.handle_key(KeyCode::Esc);
        assert!(matches!(r, ActionResult::Exit));
    }

    #[test]
    fn menu_right_on_memory_mode_enters_select() {
        let mut p = ConfigPanel::new();

        for _ in 0..4 {
            p.handle_key(KeyCode::Down);
        }
        assert_eq!(p.menu_cursor, 4);
        p.handle_key(KeyCode::Right);
        assert!(matches!(p.view, ConfigView::MemoryModeSelect { .. }));
        assert_eq!(p.expanded, Some(4));
    }

    #[test]
    fn memory_mode_select_navigates_and_submits() {
        let mut p = ConfigPanel::new();
        for _ in 0..4 {
            p.handle_key(KeyCode::Down);
        }
        p.handle_key(KeyCode::Right);

        assert!(matches!(
            p.pending_db_request(),
            DbRequest::None | DbRequest::LoadModels
        ));

        p.handle_key(KeyCode::Up);
        p.handle_key(KeyCode::Enter);
        assert_eq!(
            p.pending_db_request(),
            DbRequest::SaveMemoryMode {
                mode: "sync".to_string()
            }
        );

        p.clear_db_request();
        assert_eq!(p.pending_db_request(), DbRequest::None);

        p.handle_key(KeyCode::Left);
        assert!(matches!(p.view, ConfigView::Menu));
    }

    #[test]
    fn model_list_shows_models() {
        use crate::data::duckdb::loader::ModelRow;
        let mut panel = ConfigPanel::new();
        panel.view = ConfigView::ModelList;
        panel.models = vec![ModelRow {
            id: "agent_plan-test".into(),
            name: "test".into(),
            provider: "agent_plan".into(),
            api_url: "http://x".into(),
            api_type: "OpenAI".into(),
            api_protocol: "openai-v1".into(),
            model_id: "test".into(),
            api_key: Some("sk-xxx".into()),
            config: None,
        }];
        let text = render_to_text(&panel);
        assert!(text.contains("agent_plan-test"), "model id visible");
        assert!(text.contains('✓'), "key status visible (has_key marker)");
    }

    #[test]
    fn model_list_shows_hotkey_hints() {
        let mut panel = ConfigPanel::new();
        panel.view = ConfigView::ModelList;
        let text = render_to_text(&panel);
        assert!(text.contains("[a]"), "add hotkey hint");
        assert!(text.contains("[q]"), "quick add hotkey hint");
        assert!(text.contains("[k]"), "change key hotkey hint");
        assert!(text.contains("[d]"), "set default hotkey hint");
    }

    #[test]
    fn model_list_empty_shows_hint() {
        let mut panel = ConfigPanel::new();
        panel.view = ConfigView::ModelList;
        let text = render_to_text(&panel);
        assert!(
            text.contains('a'),
            "empty hint mentions 'a' key, got: {text}"
        );
    }

    #[test]
    fn model_list_down_moves_cursor() {
        let mut p = ConfigPanel::new();
        p.view = ConfigView::ModelList;
        p.models = vec![fake_model("a"), fake_model("b")];
        p.handle_key(KeyCode::Down);
        assert_eq!(p.list_cursor, 1);
    }

    #[test]
    fn model_list_left_returns_to_menu() {
        let mut p = ConfigPanel::new();
        p.view = ConfigView::ModelList;
        p.handle_key(KeyCode::Left);
        assert!(matches!(p.view, ConfigView::Menu));
        assert!(p.expanded.is_none());
    }

    #[test]
    fn model_list_a_enters_add_template() {
        let mut p = ConfigPanel::new();
        p.view = ConfigView::ModelList;
        p.handle_key(KeyCode::Char('a'));
        assert!(matches!(p.view, ConfigView::AddModelSelectTemplate { .. }));
    }

    #[test]
    fn model_list_k_enters_change_key() {
        let mut p = ConfigPanel::new();
        p.view = ConfigView::ModelList;
        p.models = vec![fake_model("test")];
        p.handle_key(KeyCode::Char('k'));
        assert!(matches!(p.view, ConfigView::ChangeKey(_)));
    }

    #[test]
    fn model_list_d_enters_set_default() {
        let mut p = ConfigPanel::new();
        p.view = ConfigView::ModelList;
        p.handle_key(KeyCode::Char('d'));
        assert!(matches!(p.view, ConfigView::SetDefault(_)));
    }

    #[test]
    fn form_typing_fills_field() {
        let mut p = ConfigPanel::new();
        p.view = ConfigView::ChangeKey(ChangeKeyForm {
            fields: vec![
                FormField {
                    label: "provider",
                    value: String::new(),
                    is_secret: false,
                },
                FormField {
                    label: "key",
                    value: String::new(),
                    is_secret: true,
                },
            ],
            field_cursor: 0,
            submitted: false,
        });
        p.handle_key(KeyCode::Char('a'));
        p.handle_key(KeyCode::Char('b'));
        if let ConfigView::ChangeKey(f) = &p.view {
            assert_eq!(f.fields[0].value, "ab");
        } else {
            panic!("still ChangeKey");
        }
    }

    #[test]
    fn form_tab_moves_to_next_field() {
        let mut p = ConfigPanel::new();
        p.view = ConfigView::ChangeKey(ChangeKeyForm {
            fields: vec![
                FormField {
                    label: "a",
                    value: String::new(),
                    is_secret: false,
                },
                FormField {
                    label: "b",
                    value: String::new(),
                    is_secret: true,
                },
            ],
            field_cursor: 0,
            submitted: false,
        });
        p.handle_key(KeyCode::Tab);
        if let ConfigView::ChangeKey(f) = &p.view {
            assert_eq!(f.field_cursor, 1);
        } else {
            panic!();
        }
    }

    #[test]
    fn form_backspace_deletes() {
        let mut p = ConfigPanel::new();
        p.view = ConfigView::ChangeKey(ChangeKeyForm {
            fields: vec![FormField {
                label: "x",
                value: "ab".into(),
                is_secret: false,
            }],
            field_cursor: 0,
            submitted: false,
        });
        p.handle_key(KeyCode::Backspace);
        if let ConfigView::ChangeKey(f) = &p.view {
            assert_eq!(f.fields[0].value, "a");
        } else {
            panic!();
        }
    }

    #[test]
    fn form_left_cancels_to_model_list() {
        let mut p = ConfigPanel::new();
        p.view = ConfigView::ChangeKey(ChangeKeyForm {
            fields: vec![FormField {
                label: "x",
                value: String::new(),
                is_secret: false,
            }],
            field_cursor: 0,
            submitted: false,
        });
        p.handle_key(KeyCode::Left);
        assert!(matches!(p.view, ConfigView::ModelList));
    }

    #[test]
    fn template_select_down_moves_cursor() {
        let mut p = ConfigPanel::new();
        p.view = ConfigView::AddModelSelectTemplate { cursor: 0 };
        p.handle_key(KeyCode::Down);
        if let ConfigView::AddModelSelectTemplate { cursor } = &p.view {
            assert_eq!(*cursor, 1);
        } else {
            panic!();
        }
    }

    #[test]
    fn template_select_enter_enters_form() {
        let mut p = ConfigPanel::new();
        p.view = ConfigView::AddModelSelectTemplate { cursor: 0 };
        p.handle_key(KeyCode::Enter);
        assert!(matches!(p.view, ConfigView::AddModel(_)));
    }

    #[test]
    fn provider_select_enter_enters_quickadd() {
        let mut p = ConfigPanel::new();
        p.view = ConfigView::QuickAddSelectProvider {
            providers: vec!["agent_plan".into()],
            cursor: 0,
        };
        p.handle_key(KeyCode::Enter);
        assert!(matches!(p.view, ConfigView::QuickAdd(_)));
    }

    #[test]
    fn provider_select_empty_shows_error() {
        let mut p = ConfigPanel::new();
        p.view = ConfigView::QuickAddSelectProvider {
            providers: vec![],
            cursor: 0,
        };
        p.handle_key(KeyCode::Enter);
        assert!(matches!(p.view, ConfigView::ModelList));
        assert!(p.message.is_some());
    }

    #[test]
    fn set_default_down_moves_cursor() {
        let mut p = ConfigPanel::new();
        p.view = ConfigView::SetDefault(SetDefaultSelect {
            candidates: vec![fake_model("a"), fake_model("b")],
            cursor: 0,
            submitted: false,
        });
        p.handle_key(KeyCode::Down);
        if let ConfigView::SetDefault(s) = &p.view {
            assert_eq!(s.cursor, 1);
        } else {
            panic!();
        }
    }

    #[test]
    fn set_default_enter_sets_submitted() {
        let mut p = ConfigPanel::new();
        p.view = ConfigView::SetDefault(SetDefaultSelect {
            candidates: vec![fake_model("a")],
            cursor: 0,
            submitted: false,
        });
        p.handle_key(KeyCode::Enter);
        if let ConfigView::SetDefault(s) = &p.view {
            assert!(s.submitted);
        } else {
            panic!();
        }
    }

    #[test]
    fn form_enter_on_last_field_sets_submitted() {
        let mut p = ConfigPanel::new();
        p.view = ConfigView::ChangeKey(ChangeKeyForm {
            fields: vec![
                FormField {
                    label: "p",
                    value: "x".into(),
                    is_secret: false,
                },
                FormField {
                    label: "k",
                    value: "y".into(),
                    is_secret: true,
                },
            ],
            field_cursor: 1,
            submitted: false,
        });
        p.handle_key(KeyCode::Enter);
        if let ConfigView::ChangeKey(f) = &p.view {
            assert!(f.submitted);
            assert_eq!(f.field_cursor, 1);
        } else {
            panic!();
        }
    }

    #[test]
    fn pending_db_request_none_when_not_submitted() {
        let mut p = ConfigPanel::new();
        p.view = ConfigView::ChangeKey(ChangeKeyForm {
            fields: vec![
                FormField {
                    label: "p",
                    value: "x".into(),
                    is_secret: false,
                },
                FormField {
                    label: "k",
                    value: "y".into(),
                    is_secret: true,
                },
            ],
            field_cursor: 0,
            submitted: false,
        });
        let req = p.pending_db_request();
        assert!(matches!(req, DbRequest::None));
    }

    #[test]
    fn pending_db_request_submit_change_key_when_submitted() {
        let mut p = ConfigPanel::new();
        p.view = ConfigView::ChangeKey(ChangeKeyForm {
            fields: vec![
                FormField {
                    label: "provider",
                    value: "test".into(),
                    is_secret: false,
                },
                FormField {
                    label: "key",
                    value: "newkey".into(),
                    is_secret: true,
                },
            ],
            field_cursor: 1,
            submitted: true,
        });
        let req = p.pending_db_request();
        match req {
            DbRequest::SubmitChangeKey { provider, api_key } => {
                assert_eq!(provider, "test");
                assert_eq!(api_key, "newkey");
            }
            other => panic!("unexpected {:?}", other),
        }
    }

    fn fake_model(id: &str) -> ModelRow {
        ModelRow {
            id: id.into(),
            name: id.into(),
            provider: "test".into(),
            api_url: "http://x".into(),
            api_type: "OpenAI".into(),
            api_protocol: "openai-v1".into(),
            model_id: id.into(),
            api_key: Some("key".into()),
            config: None,
        }
    }
}
