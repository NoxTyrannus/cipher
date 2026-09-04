use crate::data::duckdb::loader::ModelRow;
use crate::data::workspace_store::WorkspaceRow;
use crossterm::event::KeyCode;
use ratatui::layout::Rect;
use ratatui::Frame;

pub const PRESET_TEMPLATES: &[(&str, &str, &str, &str)] = &[(
    "OpenAI 官方",
    "openai",
    "https://api.openai.com/v1",
    "OpenAI",
)];

const MENU_ITEMS: &[(&str, bool)] = &[
    ("Model + Provider", true),
    ("工作区管理", true),
    ("Agent 改名", true),
    ("模式设置", true),
];

/// 模式设置子菜单项（与 /config CLI 的 manage_mode_styles 保持一致）：
/// 按模式分组 —— UNNI 模式设置（思考输出）/ KEEP 模式设置（Token/时间预算）。
/// 放弃项 1/2/10：协同节点固定洞察 + mix 机制删除；LOOP 暂无模式项，后续有需求再加。
const MODE_STYLE_SUBMENU_LEN: usize = 2;
/// UNNI 模式设置分组内项数（当前仅「思考输出」）。
const UNNI_MODE_MENU_LEN: usize = 1;
/// KEEP 模式设置分组内项数（Token 预算 / 时间预算）。
const KEEP_MODE_MENU_LEN: usize = 2;
/// 思考输出二选：开 / 关。
const SHOW_THINK_OPTIONS_LEN: usize = 2;

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
pub struct DeleteModelConfirm {
    pub model_id: String,
    pub model_name: String,
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
#[derive(Debug, Clone, Default)]
pub struct AddWorkspaceForm {
    pub path: String,
    pub submitted: bool,
}

/// v0.5.0：新增工作区路径不存在时的确认步（任务书 §3.2「该目录当前不存在，是否继续？」）。
#[derive(Debug, Clone)]
pub struct AddWorkspaceConfirm {
    pub path: String,
    pub submitted: bool,
}

#[derive(Debug, Clone)]
pub struct DeleteWorkspaceConfirm {
    pub workspace_id: String,
    pub workspace_name: String,
    pub submitted: bool,
}

#[derive(Debug, Clone)]
pub struct SetDefaultWorkspaceSelect {
    pub candidates: Vec<WorkspaceRow>,
    pub cursor: usize,
    pub submitted: bool,
}

#[derive(Debug, Clone)]
pub struct KeepBudgetInput {
    pub target: usize,
    pub input: String,
    pub submitted: bool,
}

/// 思考输出二选（UNNI 模式设置分组内）：cursor=0 开 / 1 关。
#[derive(Debug, Clone)]
pub struct ShowThinkSelect {
    pub cursor: usize,
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

    SetDefault(SetDefaultSelect),

    DeleteModelConfirm(DeleteModelConfirm),

    /// 模式设置：按模式分组子菜单（UNNI 模式设置 / KEEP 模式设置）。
    /// 放弃项 1/2/10：协同节点固定洞察与 Mix 项已删除（协同节点固定洞察）。
    ModeStyleSubMenu {
        cursor: usize,
    },

    /// UNNI 模式设置分组（当前仅「思考输出」）。
    UnniModeMenu {
        cursor: usize,
    },

    /// 思考输出 开/关 二选（UNNI 分组内）。
    ShowThinkSelect(ShowThinkSelect),

    /// KEEP 模式设置分组（Token 预算 / 时间预算）。
    KeepModeMenu {
        cursor: usize,
    },

    /// KEEP 预算数字输入表单（KEEP 分组内 target=0 token / target=1 时间）。
    KeepBudgetInput(KeepBudgetInput),

    RenameAgent(RenameAgentForm),

    WorkspaceList,

    AddWorkspace(AddWorkspaceForm),

    AddWorkspaceConfirm(AddWorkspaceConfirm),

    DeleteWorkspaceConfirm(DeleteWorkspaceConfirm),

    SetDefaultWorkspace(SetDefaultWorkspaceSelect),
}

#[derive(Debug, Clone)]
pub struct ConfigPanel {
    pub view: ConfigView,

    pub menu_cursor: usize,

    pub list_cursor: usize,

    pub expanded: Option<usize>,

    pub models: Vec<ModelRow>,
    pub workspaces: Vec<WorkspaceRow>,

    pub message: Option<(String, bool)>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum DbRequest {
    LoadModels,

    LoadDefaultCandidates,

    SubmitAddModel {
        provider: String,
        api_url: String,
        api_type: String,
        api_key: String,
        name: String,
        model_id: String,
    },

    DeleteModel {
        model_id: String,
    },

    SubmitSetDefault {
        model_id: String,
    },

    SaveModeStyle {
        target: usize,
        value: String,
    },

    /// UNNI 思考输出（[mode_styles.unni] show_think）：show=true 开 / false 关。
    /// 只控制 TUI 渲染是否显示 think 实例输出，不改变 thinking 执行链。
    SaveShowThink {
        show: bool,
    },

    SubmitRenameAgent {
        display_name: String,
    },

    LoadWorkspaces,

    SubmitAddWorkspace {
        path: String,
    },

    SubmitDeleteWorkspace {
        id: String,
    },

    SubmitSetDefaultWorkspace {
        id: String,
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
            workspaces: Vec::new(),
            message: None,
        }
    }

    pub fn reload_workspaces(&mut self, workspaces: Vec<WorkspaceRow>) {
        self.workspaces = workspaces;
        if self.list_cursor >= self.workspaces.len() && !self.workspaces.is_empty() {
            self.list_cursor = self.workspaces.len() - 1;
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
            ConfigView::SetDefault(sel) if sel.candidates.is_empty() => {
                DbRequest::LoadDefaultCandidates
            }
            ConfigView::WorkspaceList if self.workspaces.is_empty() => DbRequest::LoadWorkspaces,
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
            ConfigView::DeleteModelConfirm(form) if form.submitted => DbRequest::DeleteModel {
                model_id: form.model_id.clone(),
            },
            ConfigView::SetDefault(sel) if sel.submitted => {
                let model_id = sel
                    .candidates
                    .get(sel.cursor)
                    .map(|m| m.id.clone())
                    .unwrap_or_default();
                DbRequest::SubmitSetDefault { model_id }
            }
            ConfigView::KeepBudgetInput(form) if form.submitted => DbRequest::SaveModeStyle {
                target: form.target,
                value: form.input.trim().to_string(),
            },
            ConfigView::ShowThinkSelect(sel) if sel.submitted => DbRequest::SaveShowThink {
                show: sel.cursor == 0,
            },
            ConfigView::RenameAgent(form) if form.submitted => DbRequest::SubmitRenameAgent {
                display_name: form.name.clone(),
            },
            ConfigView::AddWorkspace(form) if form.submitted => DbRequest::SubmitAddWorkspace {
                path: form.path.trim().to_string(),
            },
            ConfigView::AddWorkspaceConfirm(form) if form.submitted => {
                DbRequest::SubmitAddWorkspace {
                    path: form.path.trim().to_string(),
                }
            }
            ConfigView::DeleteWorkspaceConfirm(form) if form.submitted => {
                DbRequest::SubmitDeleteWorkspace {
                    id: form.workspace_id.clone(),
                }
            }
            ConfigView::SetDefaultWorkspace(sel) if sel.submitted => {
                let id = sel
                    .candidates
                    .get(sel.cursor)
                    .map(|w| w.id.clone())
                    .unwrap_or_default();
                DbRequest::SubmitSetDefaultWorkspace { id }
            }
            _ => DbRequest::None,
        }
    }

    pub fn clear_db_request(&mut self) {
        match &mut self.view {
            ConfigView::AddModel(f) => f.submitted = false,
            ConfigView::SetDefault(s) => s.submitted = false,
            ConfigView::DeleteModelConfirm(f) => f.submitted = false,
            ConfigView::KeepBudgetInput(form) => form.submitted = false,
            ConfigView::ShowThinkSelect(sel) => sel.submitted = false,
            ConfigView::RenameAgent(f) => f.submitted = false,
            ConfigView::AddWorkspace(f) => f.submitted = false,
            ConfigView::AddWorkspaceConfirm(f) => f.submitted = false,
            ConfigView::DeleteWorkspaceConfirm(f) => f.submitted = false,
            ConfigView::SetDefaultWorkspace(s) => s.submitted = false,
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
            ConfigView::SetDefault(mut sel) => {
                let r = self.handle_set_default_key(key, &mut sel);
                if matches!(self.view, ConfigView::Menu) {
                    self.view = ConfigView::SetDefault(sel);
                }
                r
            }
            ConfigView::DeleteModelConfirm(mut form) => {
                let r = self.handle_delete_model_confirm_key(key, &mut form);
                if matches!(self.view, ConfigView::Menu) {
                    self.view = ConfigView::DeleteModelConfirm(form);
                }
                r
            }
            ConfigView::ModeStyleSubMenu { mut cursor } => {
                let r = self.handle_mode_style_submenu_key(key, &mut cursor);

                if matches!(self.view, ConfigView::Menu) && self.expanded.is_some() {
                    self.view = ConfigView::ModeStyleSubMenu { cursor };
                }
                r
            }
            ConfigView::UnniModeMenu { mut cursor } => {
                let r = self.handle_unni_mode_menu_key(key, &mut cursor);

                if matches!(self.view, ConfigView::Menu) && self.expanded.is_some() {
                    self.view = ConfigView::UnniModeMenu { cursor };
                }
                r
            }
            ConfigView::ShowThinkSelect(mut sel) => {
                let r = self.handle_show_think_select_key(key, &mut sel);

                if matches!(self.view, ConfigView::Menu) && self.expanded.is_some() {
                    self.view = ConfigView::ShowThinkSelect(sel);
                }
                r
            }
            ConfigView::KeepModeMenu { mut cursor } => {
                let r = self.handle_keep_mode_menu_key(key, &mut cursor);

                if matches!(self.view, ConfigView::Menu) && self.expanded.is_some() {
                    self.view = ConfigView::KeepModeMenu { cursor };
                }
                r
            }
            ConfigView::KeepBudgetInput(mut form) => {
                let r = self.handle_keep_budget_input_key(key, &mut form);

                if matches!(self.view, ConfigView::Menu) {
                    self.view = ConfigView::KeepBudgetInput(form);
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
            ConfigView::WorkspaceList => self.handle_workspace_list_key(key),
            ConfigView::AddWorkspace(mut form) => {
                let r = self.handle_workspace_form_key(key, &mut form);
                if matches!(self.view, ConfigView::Menu) {
                    self.view = ConfigView::AddWorkspace(form);
                }
                r
            }
            ConfigView::AddWorkspaceConfirm(mut form) => {
                let r = self.handle_add_workspace_confirm_key(key, &mut form);
                if matches!(self.view, ConfigView::Menu) {
                    self.view = ConfigView::AddWorkspaceConfirm(form);
                }
                r
            }
            ConfigView::DeleteWorkspaceConfirm(mut form) => {
                let r = self.handle_delete_workspace_confirm_key(key, &mut form);
                if matches!(self.view, ConfigView::Menu) {
                    self.view = ConfigView::DeleteWorkspaceConfirm(form);
                }
                r
            }
            ConfigView::SetDefaultWorkspace(mut sel) => {
                let r = self.handle_set_default_workspace_key(key, &mut sel);
                if matches!(self.view, ConfigView::Menu) {
                    self.view = ConfigView::SetDefaultWorkspace(sel);
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
                    if idx == 3 {
                        self.view = ConfigView::ModeStyleSubMenu { cursor: 0 };
                    } else if idx == 2 {
                        self.view = ConfigView::RenameAgent(RenameAgentForm::default());
                    } else if idx == 1 {
                        self.view = ConfigView::WorkspaceList;
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
            KeyCode::Char('x') => {
                if self.models.len() <= 1 {
                    self.message = Some((
                        "至少保留一个模型，当前仅剩 1 个模型，不能删除".to_string(),
                        true,
                    ));
                    self.view = ConfigView::ModelList;
                    return ActionResult::Navigate;
                }
                let model = self.models.get(self.list_cursor);
                let (model_id, model_name) = match model {
                    Some(model) => (model.id.clone(), model.name.clone()),
                    None => {
                        self.message = Some(("没有可删除的模型".to_string(), true));
                        return ActionResult::Navigate;
                    }
                };
                self.view = ConfigView::DeleteModelConfirm(DeleteModelConfirm {
                    model_id,
                    model_name,
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

    fn handle_workspace_list_key(&mut self, key: KeyCode) -> ActionResult {
        match key {
            KeyCode::Up => {
                if self.list_cursor > 0 {
                    self.list_cursor -= 1;
                }
                ActionResult::Navigate
            }
            KeyCode::Down => {
                if !self.workspaces.is_empty() && self.list_cursor < self.workspaces.len() - 1 {
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
                self.view = ConfigView::AddWorkspace(AddWorkspaceForm::default());
                ActionResult::Navigate
            }
            KeyCode::Char('x') => {
                if self.workspaces.len() <= 1 {
                    self.message = Some(("至少需要保留一个工作区，不能删除".to_string(), true));
                    self.view = ConfigView::WorkspaceList;
                    return ActionResult::Navigate;
                }
                let ws = self.workspaces.get(self.list_cursor).cloned();
                match ws {
                    Some(ws) => {
                        self.view = ConfigView::DeleteWorkspaceConfirm(DeleteWorkspaceConfirm {
                            workspace_id: ws.id,
                            workspace_name: ws.name,
                            submitted: false,
                        });
                    }
                    None => {
                        self.message = Some(("没有可删除的工作区".to_string(), true));
                        self.view = ConfigView::WorkspaceList;
                    }
                }
                ActionResult::Navigate
            }
            KeyCode::Char('d') => {
                self.view = ConfigView::SetDefaultWorkspace(SetDefaultWorkspaceSelect {
                    candidates: self.workspaces.clone(),
                    cursor: self
                        .list_cursor
                        .min(self.workspaces.len().saturating_sub(1)),
                    submitted: false,
                });
                ActionResult::Navigate
            }
            _ => ActionResult::Navigate,
        }
    }

    fn handle_workspace_form_key(
        &mut self,
        key: KeyCode,
        form: &mut AddWorkspaceForm,
    ) -> ActionResult {
        match key {
            KeyCode::Left => {
                self.view = ConfigView::WorkspaceList;
                ActionResult::Navigate
            }
            KeyCode::Esc => ActionResult::Exit,
            KeyCode::Enter => {
                let path = form.path.trim();
                if path.is_empty() {
                    self.message = Some(("路径不能为空".to_string(), true));
                } else if !std::path::Path::new(path).is_absolute() {
                    // v0.5.0 §6.1：路径必须是绝对路径。
                    self.message = Some(("路径必须是绝对路径".to_string(), true));
                } else if !std::path::Path::new(path).exists() {
                    // v0.5.0 §3.2/§6.1：路径不存在 → 确认步（允许确认后继续）。
                    self.view = ConfigView::AddWorkspaceConfirm(AddWorkspaceConfirm {
                        path: path.to_string(),
                        submitted: false,
                    });
                } else {
                    form.submitted = true;
                }
                ActionResult::Navigate
            }
            KeyCode::Backspace => {
                form.path.pop();
                ActionResult::Navigate
            }
            KeyCode::Char(c) => {
                form.path.push(c);
                ActionResult::Navigate
            }
            _ => ActionResult::Navigate,
        }
    }

    /// v0.5.0：目录不存在确认步 —— Enter/y 继续（允许保存），←/n 返回修改路径。
    fn handle_add_workspace_confirm_key(
        &mut self,
        key: KeyCode,
        form: &mut AddWorkspaceConfirm,
    ) -> ActionResult {
        match key {
            KeyCode::Left | KeyCode::Char('n') | KeyCode::Char('N') => {
                self.view = ConfigView::AddWorkspace(AddWorkspaceForm {
                    path: form.path.clone(),
                    submitted: false,
                });
                ActionResult::Navigate
            }
            KeyCode::Esc => ActionResult::Exit,
            KeyCode::Enter | KeyCode::Char('y') | KeyCode::Char('Y') => {
                form.submitted = true;
                ActionResult::Navigate
            }
            _ => ActionResult::Navigate,
        }
    }

    fn handle_delete_workspace_confirm_key(
        &mut self,
        key: KeyCode,
        form: &mut DeleteWorkspaceConfirm,
    ) -> ActionResult {
        match key {
            KeyCode::Left => {
                self.view = ConfigView::WorkspaceList;
                ActionResult::Navigate
            }
            KeyCode::Esc => ActionResult::Exit,
            KeyCode::Enter => {
                form.submitted = true;
                ActionResult::Navigate
            }
            _ => ActionResult::Navigate,
        }
    }

    fn handle_set_default_workspace_key(
        &mut self,
        key: KeyCode,
        sel: &mut SetDefaultWorkspaceSelect,
    ) -> ActionResult {
        match key {
            KeyCode::Up => {
                if sel.cursor > 0 {
                    sel.cursor -= 1;
                }
                ActionResult::Navigate
            }
            KeyCode::Down => {
                if !sel.candidates.is_empty() && sel.cursor < sel.candidates.len() - 1 {
                    sel.cursor += 1;
                }
                ActionResult::Navigate
            }
            KeyCode::Left => {
                self.view = ConfigView::WorkspaceList;
                ActionResult::Navigate
            }
            KeyCode::Esc => ActionResult::Exit,
            KeyCode::Enter => {
                sel.submitted = true;
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

    fn handle_delete_model_confirm_key(
        &mut self,
        key: KeyCode,
        form: &mut DeleteModelConfirm,
    ) -> ActionResult {
        match key {
            KeyCode::Enter => {
                form.submitted = true;
                ActionResult::Navigate
            }
            KeyCode::Left | KeyCode::Esc => {
                self.view = ConfigView::ModelList;
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

    fn handle_mode_style_submenu_key(&mut self, key: KeyCode, cursor: &mut usize) -> ActionResult {
        match key {
            KeyCode::Up => {
                if *cursor > 0 {
                    *cursor -= 1;
                }
                ActionResult::Navigate
            }
            KeyCode::Down => {
                if *cursor < MODE_STYLE_SUBMENU_LEN - 1 {
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
                // cursor 0 = UNNI 模式设置分组；1 = KEEP 模式设置分组。
                self.view = if *cursor == 0 {
                    ConfigView::UnniModeMenu { cursor: 0 }
                } else {
                    ConfigView::KeepModeMenu { cursor: 0 }
                };
                ActionResult::Navigate
            }
            _ => ActionResult::Navigate,
        }
    }

    fn handle_unni_mode_menu_key(&mut self, key: KeyCode, cursor: &mut usize) -> ActionResult {
        match key {
            KeyCode::Up => {
                if *cursor > 0 {
                    *cursor -= 1;
                }
                ActionResult::Navigate
            }
            KeyCode::Down => {
                // UNNI_MODE_MENU_LEN 当前为 1：`cursor < LEN - 1` 恒假会触发 clippy，
                // 改用 `cursor + 1 < LEN`（LEN 增长后语义不变）。
                if *cursor + 1 < UNNI_MODE_MENU_LEN {
                    *cursor += 1;
                }
                ActionResult::Navigate
            }
            KeyCode::Left => {
                self.view = ConfigView::ModeStyleSubMenu { cursor: 0 };
                ActionResult::Navigate
            }
            KeyCode::Esc => ActionResult::Exit,
            KeyCode::Right | KeyCode::Enter => {
                // 当前仅「思考输出」一项 → 开/关 二选。
                self.view = ConfigView::ShowThinkSelect(ShowThinkSelect {
                    cursor: 0,
                    submitted: false,
                });
                ActionResult::Navigate
            }
            _ => ActionResult::Navigate,
        }
    }

    fn handle_show_think_select_key(
        &mut self,
        key: KeyCode,
        sel: &mut ShowThinkSelect,
    ) -> ActionResult {
        match key {
            KeyCode::Up => {
                if sel.cursor > 0 {
                    sel.cursor -= 1;
                }
                ActionResult::Navigate
            }
            KeyCode::Down => {
                if sel.cursor < SHOW_THINK_OPTIONS_LEN - 1 {
                    sel.cursor += 1;
                }
                ActionResult::Navigate
            }
            KeyCode::Left => {
                self.view = ConfigView::UnniModeMenu { cursor: 0 };
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

    fn handle_keep_mode_menu_key(&mut self, key: KeyCode, cursor: &mut usize) -> ActionResult {
        match key {
            KeyCode::Up => {
                if *cursor > 0 {
                    *cursor -= 1;
                }
                ActionResult::Navigate
            }
            KeyCode::Down => {
                if *cursor < KEEP_MODE_MENU_LEN - 1 {
                    *cursor += 1;
                }
                ActionResult::Navigate
            }
            KeyCode::Left => {
                self.view = ConfigView::ModeStyleSubMenu { cursor: 1 };
                ActionResult::Navigate
            }
            KeyCode::Esc => ActionResult::Exit,
            KeyCode::Right | KeyCode::Enter => {
                let target = *cursor;
                self.view = ConfigView::KeepBudgetInput(KeepBudgetInput {
                    target,
                    input: String::new(),
                    submitted: false,
                });
                ActionResult::Navigate
            }
            _ => ActionResult::Navigate,
        }
    }

    fn handle_keep_budget_input_key(
        &mut self,
        key: KeyCode,
        form: &mut KeepBudgetInput,
    ) -> ActionResult {
        match key {
            KeyCode::Char(c) if c.is_ascii_digit() => {
                form.input.push(c);
                ActionResult::Navigate
            }
            KeyCode::Backspace => {
                form.input.pop();
                ActionResult::Navigate
            }
            KeyCode::Left => {
                self.view = ConfigView::KeepModeMenu {
                    cursor: form.target,
                };
                ActionResult::Navigate
            }
            KeyCode::Esc => {
                self.view = ConfigView::KeepModeMenu {
                    cursor: form.target,
                };
                ActionResult::Navigate
            }
            KeyCode::Enter => {
                if form.input.trim().is_empty() {
                    self.message = Some(("请输入数字（0 = 无限）".into(), true));
                } else {
                    form.submitted = true;
                }
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
                "← 返回上级    ↑↓ 选择    a 新增  x 删除  d 切默认    Esc 退出"
            }
            ConfigView::AddModelSelectTemplate { .. }
            | ConfigView::SetDefault(_)
            | ConfigView::ModeStyleSubMenu { .. }
            | ConfigView::UnniModeMenu { .. }
            | ConfigView::ShowThinkSelect(_)
            | ConfigView::KeepModeMenu { .. } => "← 返回上级    ↑↓ 选择    → 确认    Esc 退出设置",
            ConfigView::AddModel(_) => "← 取消        Tab 下一字段  Enter 确认    Esc 退出设置",
            ConfigView::DeleteModelConfirm(_) => "Enter 确认删除  ←/Esc 取消",
            ConfigView::KeepBudgetInput(_) => {
                "← 返回上级    数字输入  Backspace 删除  Enter 确认  Esc 取消"
            }
            ConfigView::RenameAgent(_) => "← 取消    Enter 确认    Esc 退出设置",
            ConfigView::WorkspaceList => {
                "← 返回上级    ↑↓ 选择    a 新增  x 删除  d 设置默认  Esc 退出"
            }
            ConfigView::AddWorkspace(_) => "← 返回    输入路径  Enter 提交  Esc 退出",
            ConfigView::AddWorkspaceConfirm(_) => {
                "Enter / y 继续保存    n / ← 返回修改    Esc 退出"
            }
            ConfigView::DeleteWorkspaceConfirm(_) => "Enter 确认删除  ←/Esc 取消",
            ConfigView::SetDefaultWorkspace(_) => "← 返回    ↑↓ 选择    Enter 确认",
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
        ConfigView::SetDefault(sel) => render_set_default(panel, frame, content_area, sel),
        ConfigView::DeleteModelConfirm(form) => {
            render_delete_model_confirm(panel, frame, content_area, form);
        }
        ConfigView::ModeStyleSubMenu { cursor } => {
            render_mode_style_submenu(panel, frame, content_area, *cursor);
        }
        ConfigView::UnniModeMenu { cursor } => {
            render_unni_mode_menu(panel, frame, content_area, *cursor);
        }
        ConfigView::ShowThinkSelect(sel) => {
            render_show_think_select(panel, frame, content_area, sel);
        }
        ConfigView::KeepModeMenu { cursor } => {
            render_keep_mode_menu(panel, frame, content_area, *cursor);
        }
        ConfigView::KeepBudgetInput(form) => {
            render_keep_budget_input(panel, frame, content_area, form);
        }
        ConfigView::RenameAgent(form) => {
            render_rename_agent(panel, frame, content_area, form);
        }
        ConfigView::WorkspaceList => {
            render_workspace_list(panel, frame, content_area);
        }
        ConfigView::AddWorkspace(form) => {
            render_add_workspace(panel, frame, content_area, form);
        }
        ConfigView::AddWorkspaceConfirm(form) => {
            render_add_workspace_confirm(panel, frame, content_area, form);
        }
        ConfigView::DeleteWorkspaceConfirm(form) => {
            render_delete_workspace_confirm(panel, frame, content_area, form);
        }
        ConfigView::SetDefaultWorkspace(sel) => {
            render_set_default_workspace(panel, frame, content_area, sel);
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
        Span::styled("[x] 删除  ", Style::default().fg(Color::Red)),
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

fn render_delete_model_confirm(
    _panel: &ConfigPanel,
    frame: &mut Frame,
    area: Rect,
    form: &DeleteModelConfirm,
) {
    use ratatui::style::{Color, Modifier, Style};
    use ratatui::text::{Line, Span};
    use ratatui::widgets::Paragraph;

    let lines = vec![
        Line::from(""),
        Line::from(vec![Span::styled(
            "确认删除模型:",
            Style::default().fg(Color::Red),
        )]),
        Line::from(""),
        Line::from(vec![Span::styled(
            format!("  {} ({})", form.model_name, form.model_id),
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        )]),
        Line::from(""),
        Line::from(vec![Span::styled(
            "  Enter 确认删除    ←/Esc 取消",
            Style::default().fg(Color::DarkGray),
        )]),
    ];
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

/// 通用「标题 + (名称, 描述) 列表」渲染（模式设置各层子菜单共用）。
fn render_select_list(
    frame: &mut Frame,
    area: Rect,
    title: &str,
    items: &[(&str, &str)],
    cursor: usize,
) {
    use ratatui::style::{Color, Modifier, Style};
    use ratatui::text::{Line, Span};
    use ratatui::widgets::Paragraph;

    let mut lines: Vec<Line> = vec![
        Line::from(""),
        Line::from(vec![Span::styled(title, Style::default().fg(Color::Gray))]),
        Line::from(""),
    ];
    for (i, (name, desc)) in items.iter().enumerate() {
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

/// 模式设置：按模式分组子菜单（UNNI 模式设置 / KEEP 模式设置）。
fn render_mode_style_submenu(_panel: &ConfigPanel, frame: &mut Frame, area: Rect, cursor: usize) {
    let items: [(&str, &str); MODE_STYLE_SUBMENU_LEN] = [
        ("UNNI 模式设置", "思考输出"),
        ("KEEP 模式设置", "Token/时间预算"),
    ];
    render_select_list(frame, area, "模式设置:", &items, cursor);
}

/// UNNI 模式设置分组（当前仅「思考输出」→ 开/关 二选）。
fn render_unni_mode_menu(_panel: &ConfigPanel, frame: &mut Frame, area: Rect, cursor: usize) {
    let items: [(&str, &str); UNNI_MODE_MENU_LEN] = [("思考输出", "开 / 关")];
    render_select_list(frame, area, "UNNI 模式设置:", &items, cursor);
}

/// 思考输出 开/关 二选（UNNI 分组内；只控制 TUI 渲染，不改 thinking 执行链）。
fn render_show_think_select(
    _panel: &ConfigPanel,
    frame: &mut Frame,
    area: Rect,
    sel: &ShowThinkSelect,
) {
    let items: [(&str, &str); SHOW_THINK_OPTIONS_LEN] =
        [("开", "显示思考输出"), ("关", "隐藏思考输出")];
    render_select_list(frame, area, "思考输出:", &items, sel.cursor);
}

/// KEEP 模式设置分组（Token 预算 / 时间预算）。
fn render_keep_mode_menu(_panel: &ConfigPanel, frame: &mut Frame, area: Rect, cursor: usize) {
    let items: [(&str, &str); KEEP_MODE_MENU_LEN] = [
        ("Token 预算", "0=无限, 最小 100K"),
        ("时间预算", "0=无限, 最小 5min"),
    ];
    render_select_list(frame, area, "KEEP 模式设置:", &items, cursor);
}
fn render_keep_budget_input(
    _panel: &ConfigPanel,
    frame: &mut Frame,
    area: Rect,
    form: &KeepBudgetInput,
) {
    use ratatui::style::{Color, Modifier, Style};
    use ratatui::text::{Line, Span};
    use ratatui::widgets::Paragraph;

    let (title, unit, minimum, minimum_label) = if form.target == 0 {
        ("Token 预算", "K", "100", "100K (100,000 token)")
    } else {
        ("时间预算", "min", "5", "5min (300 秒)")
    };
    let lines = vec![
        Line::from(""),
        Line::from(vec![Span::styled(
            format!("{title}:"),
            Style::default().fg(Color::Gray),
        )]),
        Line::from(vec![Span::styled(
            format!("单位 {unit}；最小值 {minimum_label}；输入 0 = 无限"),
            Style::default().fg(Color::DarkGray),
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
                format!("{minimum} 起 / 0=无限: "),
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(form.input.clone(), Style::default().fg(Color::White)),
            Span::styled("_", Style::default().fg(Color::Cyan)),
        ]),
        Line::from(""),
        Line::from(vec![Span::styled(
            "  Enter 确认    ← 返回上级    Esc 取消",
            Style::default().fg(Color::DarkGray),
        )]),
    ];
    frame.render_widget(Paragraph::new(lines), area);
}

fn render_workspace_list(panel: &ConfigPanel, frame: &mut Frame, area: Rect) {
    use ratatui::style::{Color, Modifier, Style};
    use ratatui::text::{Line, Span};
    use ratatui::widgets::Paragraph;

    let mut lines: Vec<Line> = vec![Line::from(vec![Span::styled(
        "▼ 工作区管理",
        Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD),
    )])];
    lines.push(Line::from(""));
    if panel.workspaces.is_empty() {
        lines.push(Line::from(vec![Span::styled(
            "  (无工作区, 按 a 新增)",
            Style::default().fg(Color::DarkGray),
        )]));
    } else {
        for (i, w) in panel.workspaces.iter().enumerate() {
            let mark = if w.is_default { "★" } else { " " };
            let selected = panel.list_cursor == i;
            let style = if selected {
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(Color::Gray)
            };
            lines.push(Line::from(vec![
                Span::styled(format!("  [{}] {}  ", mark, w.name), style),
                Span::styled(w.path.clone(), Style::default().fg(Color::DarkGray)),
            ]));
        }
    }
    lines.push(Line::from(""));
    lines.push(Line::from(vec![Span::styled(
        "  a 新增    x 删除    d 设置默认    ← 返回",
        Style::default().fg(Color::DarkGray),
    )]));
    frame.render_widget(Paragraph::new(lines), area);
}

fn render_add_workspace(
    _panel: &ConfigPanel,
    frame: &mut Frame,
    area: Rect,
    form: &AddWorkspaceForm,
) {
    use ratatui::style::{Color, Modifier, Style};
    use ratatui::text::{Line, Span};
    use ratatui::widgets::Paragraph;
    let lines: Vec<Line> = vec![
        Line::from(""),
        Line::from(vec![Span::styled(
            "请输入工作区绝对路径:",
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
            Span::styled(form.path.clone(), Style::default().fg(Color::White)),
            Span::styled("_", Style::default().fg(Color::Cyan)),
        ]),
        Line::from(""),
        Line::from(vec![Span::styled(
            "  Enter 提交    ← 返回",
            Style::default().fg(Color::DarkGray),
        )]),
    ];
    frame.render_widget(Paragraph::new(lines), area);
}

fn render_add_workspace_confirm(
    panel: &ConfigPanel,
    frame: &mut Frame,
    area: Rect,
    form: &AddWorkspaceConfirm,
) {
    use ratatui::style::{Color, Style};
    use ratatui::text::{Line, Span};
    use ratatui::widgets::Paragraph;
    let _ = panel;
    let lines: Vec<Line> = vec![
        Line::from(""),
        Line::from(vec![Span::styled(
            "该目录当前不存在，是否继续？",
            Style::default().fg(Color::Yellow),
        )]),
        Line::from(vec![
            Span::styled("  ", Style::default()),
            Span::styled(form.path.clone(), Style::default().fg(Color::DarkGray)),
        ]),
        Line::from(""),
        Line::from(vec![Span::styled(
            "  Enter / y 继续    n / ← 返回修改    Esc 取消",
            Style::default().fg(Color::DarkGray),
        )]),
    ];
    frame.render_widget(Paragraph::new(lines), area);
}

fn render_delete_workspace_confirm(
    _panel: &ConfigPanel,
    frame: &mut Frame,
    area: Rect,
    form: &DeleteWorkspaceConfirm,
) {
    use ratatui::style::{Color, Style};
    use ratatui::text::{Line, Span};
    use ratatui::widgets::Paragraph;
    let lines: Vec<Line> = vec![
        Line::from(""),
        Line::from(vec![Span::styled(
            format!("确定删除工作区: {} ?", form.workspace_name),
            Style::default().fg(Color::Red),
        )]),
        Line::from(""),
        Line::from(vec![Span::styled(
            "  Enter 确认    ← 取消",
            Style::default().fg(Color::DarkGray),
        )]),
    ];
    frame.render_widget(Paragraph::new(lines), area);
}

fn render_set_default_workspace(
    _panel: &ConfigPanel,
    frame: &mut Frame,
    area: Rect,
    sel: &SetDefaultWorkspaceSelect,
) {
    use ratatui::style::{Color, Modifier, Style};
    use ratatui::text::{Line, Span};
    use ratatui::widgets::Paragraph;
    let mut lines: Vec<Line> = vec![
        Line::from(""),
        Line::from(vec![Span::styled(
            "选择要设为默认的工作区:",
            Style::default().fg(Color::Gray),
        )]),
    ];
    for (i, w) in sel.candidates.iter().enumerate() {
        let selected = sel.cursor == i;
        let style = if selected {
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Color::Gray)
        };
        lines.push(Line::from(vec![
            Span::styled(if selected { "  ▶ " } else { "    " }, style),
            Span::styled(format!("{}  {}", w.name, w.path), style),
        ]));
    }
    lines.push(Line::from(""));
    lines.push(Line::from(vec![Span::styled(
        "  Enter 确认    ↑↓ 选择    ← 返回",
        Style::default().fg(Color::DarkGray),
    )]));
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
    fn menu_shows_4_items_without_removed_entries() {
        let panel = ConfigPanel::new();
        let text = render_to_text(&panel);

        assert!(text.contains("Model + Provider"), "menu item 1");
        assert!(text.contains('工'), "menu item 2");
        assert!(text.contains("Agent"), "menu item 3 (Agent prefix)");
        // CJK 宽字符在 ratatui buffer 中占 2 格，续格符号为空格 → 先去空格再断言。
        assert!(
            text.replace(' ', "").contains("模式设置"),
            "menu item 4, got: {text}"
        );
        assert!(!text.contains("Mode Style"), "old wording removed");
        assert!(!text.contains("协同模式风格"), "old wording removed");
        assert!(!text.contains("默认设置"), "default settings removed");
        assert!(!text.contains("上下文编辑"), "context editing removed");

        // v0.5.0：工作区管理已启用（原为「待后续」占位），4 个菜单项全部可用 → 无 disabled 标记。
        let pending_count = text.matches('(').count();
        assert_eq!(
            pending_count, 0,
            "no disabled item remains after workspace management enabled, got: {text}"
        );
    }

    #[test]
    fn mode_style_submenu_renders_groups_without_panic() {
        let mut panel = ConfigPanel::new();
        panel.expanded = Some(3);
        panel.view = ConfigView::ModeStyleSubMenu { cursor: 1 };
        let text = render_to_text(&panel).replace(' ', "");
        assert!(text.contains("模式设置:"), "submenu title: {text}");
        assert!(text.contains("UNNI模式设置"), "group 1: {text}");
        assert!(text.contains("KEEP模式设置"), "group 2: {text}");
        assert!(!text.contains("协作设置"), "old title removed: {text}");
        assert!(!text.contains("协同节点"), "node item removed: {text}");
        assert!(!text.contains("Mix"), "mix item removed: {text}");
        assert!(!text.contains("UNNI协同方式"), "style item removed: {text}");
    }

    #[test]
    fn unni_mode_menu_renders_think_output_item() {
        let mut panel = ConfigPanel::new();
        panel.expanded = Some(3);
        panel.view = ConfigView::UnniModeMenu { cursor: 0 };
        let text = render_to_text(&panel).replace(' ', "");
        assert!(text.contains("UNNI模式设置:"), "title: {text}");
        assert!(text.contains("思考输出"), "think output item: {text}");
    }

    #[test]
    fn keep_mode_menu_renders_budget_items() {
        let mut panel = ConfigPanel::new();
        panel.expanded = Some(3);
        panel.view = ConfigView::KeepModeMenu { cursor: 1 };
        let text = render_to_text(&panel).replace(' ', "");
        assert!(text.contains("KEEP模式设置:"), "title: {text}");
        assert!(text.contains("Token预算"), "token item: {text}");
        assert!(text.contains("时间预算"), "time item: {text}");
        assert!(!text.contains("KEEPToken"), "no double KEEP prefix: {text}");
    }

    #[test]
    fn show_think_select_renders_on_off() {
        let mut panel = ConfigPanel::new();
        panel.expanded = Some(3);
        panel.view = ConfigView::ShowThinkSelect(ShowThinkSelect {
            cursor: 0,
            submitted: false,
        });
        let text = render_to_text(&panel).replace(' ', "");
        assert!(text.contains("思考输出:"), "title: {text}");
        assert!(text.contains("显示思考输出"), "on option: {text}");
        assert!(text.contains("隐藏思考输出"), "off option: {text}");
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
    fn menu_right_on_mode_style_enters_submenu() {
        let mut p = ConfigPanel::new();

        for _ in 0..3 {
            p.handle_key(KeyCode::Down);
        }
        assert_eq!(p.menu_cursor, 3);
        p.handle_key(KeyCode::Right);
        assert!(matches!(p.view, ConfigView::ModeStyleSubMenu { .. }));
        assert_eq!(p.expanded, Some(3));
    }

    #[test]
    fn mode_style_submenu_enter_opens_mode_groups() {
        let mut p = ConfigPanel::new();
        p.expanded = Some(3);
        p.view = ConfigView::ModeStyleSubMenu { cursor: 0 };
        p.handle_key(KeyCode::Enter);
        assert!(matches!(p.view, ConfigView::UnniModeMenu { cursor: 0 }));

        p.view = ConfigView::ModeStyleSubMenu { cursor: 1 };
        p.handle_key(KeyCode::Enter);
        assert!(matches!(p.view, ConfigView::KeepModeMenu { cursor: 0 }));
    }

    #[test]
    fn unni_mode_menu_enter_opens_show_think_select() {
        let mut p = ConfigPanel::new();
        p.expanded = Some(3);
        p.view = ConfigView::UnniModeMenu { cursor: 0 };
        p.handle_key(KeyCode::Enter);
        assert!(matches!(
            p.view,
            ConfigView::ShowThinkSelect(ShowThinkSelect {
                cursor: 0,
                submitted: false
            })
        ));
    }

    #[test]
    fn unni_mode_menu_left_returns_to_mode_style_submenu() {
        let mut p = ConfigPanel::new();
        p.expanded = Some(3);
        p.view = ConfigView::UnniModeMenu { cursor: 0 };
        p.handle_key(KeyCode::Left);
        assert!(matches!(p.view, ConfigView::ModeStyleSubMenu { cursor: 0 }));
    }

    #[test]
    fn show_think_select_on_submits_save_show_think() {
        let mut p = ConfigPanel::new();
        // handle_key 的 mem::replace 恢复依赖 expanded（真实路径：从主菜单 → 模式设置进入）。
        p.expanded = Some(3);
        p.view = ConfigView::ShowThinkSelect(ShowThinkSelect {
            cursor: 0,
            submitted: false,
        });
        p.handle_key(KeyCode::Enter);
        assert_eq!(
            p.pending_db_request(),
            DbRequest::SaveShowThink { show: true }
        );
        p.clear_db_request();
        assert_eq!(p.pending_db_request(), DbRequest::None);
    }

    #[test]
    fn show_think_select_off_submits_save_show_think_false() {
        let mut p = ConfigPanel::new();
        p.expanded = Some(3);
        p.view = ConfigView::ShowThinkSelect(ShowThinkSelect {
            cursor: 0,
            submitted: false,
        });
        p.handle_key(KeyCode::Down);
        p.handle_key(KeyCode::Enter);
        if let ConfigView::ShowThinkSelect(sel) = &p.view {
            assert_eq!(sel.cursor, 1);
        } else {
            panic!("still ShowThinkSelect");
        }
        assert_eq!(
            p.pending_db_request(),
            DbRequest::SaveShowThink { show: false }
        );
    }

    #[test]
    fn show_think_select_left_returns_to_unni_menu() {
        let mut p = ConfigPanel::new();
        p.view = ConfigView::ShowThinkSelect(ShowThinkSelect {
            cursor: 1,
            submitted: false,
        });
        p.handle_key(KeyCode::Left);
        assert!(matches!(p.view, ConfigView::UnniModeMenu { cursor: 0 }));
    }

    #[test]
    fn two_level_navigation_reaches_save_show_think() {
        let mut p = ConfigPanel::new();
        // 模式设置 → UNNI 模式设置 → 思考输出 开/关 → 提交
        p.expanded = Some(3);
        p.view = ConfigView::ModeStyleSubMenu { cursor: 0 };
        p.handle_key(KeyCode::Enter);
        assert!(matches!(p.view, ConfigView::UnniModeMenu { .. }));
        p.handle_key(KeyCode::Enter);
        assert!(matches!(p.view, ConfigView::ShowThinkSelect(_)));
        p.handle_key(KeyCode::Enter);
        assert_eq!(
            p.pending_db_request(),
            DbRequest::SaveShowThink { show: true }
        );
    }

    #[test]
    fn keep_mode_menu_enter_opens_keep_budget_input() {
        let mut p = ConfigPanel::new();
        p.expanded = Some(3);
        p.view = ConfigView::KeepModeMenu { cursor: 0 };
        p.handle_key(KeyCode::Enter);
        assert!(matches!(
            p.view,
            ConfigView::KeepBudgetInput(KeepBudgetInput { target: 0, .. })
        ));

        p.view = ConfigView::KeepModeMenu { cursor: 1 };
        p.handle_key(KeyCode::Enter);
        assert!(matches!(
            p.view,
            ConfigView::KeepBudgetInput(KeepBudgetInput { target: 1, .. })
        ));
    }

    #[test]
    fn keep_mode_menu_left_returns_to_mode_style_submenu() {
        let mut p = ConfigPanel::new();
        p.expanded = Some(3);
        p.view = ConfigView::KeepModeMenu { cursor: 1 };
        p.handle_key(KeyCode::Left);
        assert!(matches!(p.view, ConfigView::ModeStyleSubMenu { cursor: 1 }));
    }

    #[test]
    fn keep_budget_input_submits_zero_and_values() {
        let mut p = ConfigPanel::new();
        p.view = ConfigView::KeepBudgetInput(KeepBudgetInput {
            target: 0,
            input: "0".into(),
            submitted: false,
        });
        p.handle_key(KeyCode::Enter);
        assert_eq!(
            p.pending_db_request(),
            DbRequest::SaveModeStyle {
                target: 0,
                value: "0".to_string()
            }
        );
        p.clear_db_request();
        assert_eq!(p.pending_db_request(), DbRequest::None);

        p.view = ConfigView::KeepBudgetInput(KeepBudgetInput {
            target: 1,
            input: "10".into(),
            submitted: false,
        });
        p.handle_key(KeyCode::Enter);
        assert_eq!(
            p.pending_db_request(),
            DbRequest::SaveModeStyle {
                target: 1,
                value: "10".to_string()
            }
        );
    }

    #[test]
    fn keep_budget_input_only_accepts_digits_and_backspace() {
        let mut p = ConfigPanel::new();
        p.view = ConfigView::KeepBudgetInput(KeepBudgetInput {
            target: 0,
            input: String::new(),
            submitted: false,
        });
        p.handle_key(KeyCode::Char('1'));
        p.handle_key(KeyCode::Char('x'));
        p.handle_key(KeyCode::Char('0'));
        if let ConfigView::KeepBudgetInput(form) = &p.view {
            assert_eq!(form.input, "10");
        } else {
            panic!();
        }
        p.handle_key(KeyCode::Backspace);
        if let ConfigView::KeepBudgetInput(form) = &p.view {
            assert_eq!(form.input, "1");
        } else {
            panic!();
        }
    }

    #[test]
    fn keep_budget_input_esc_cancels_to_keep_mode_menu() {
        let mut p = ConfigPanel::new();
        p.view = ConfigView::KeepBudgetInput(KeepBudgetInput {
            target: 1,
            input: "0".into(),
            submitted: false,
        });
        p.handle_key(KeyCode::Esc);
        assert!(matches!(p.view, ConfigView::KeepModeMenu { cursor: 1 }));
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
        assert!(text.contains("[x]"), "delete hotkey hint");
        assert!(text.contains("[d]"), "set default hotkey hint");
        assert!(!text.contains("[q]"), "quick add hotkey removed");
        assert!(!text.contains("[k]"), "change key hotkey removed");
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
    fn model_list_x_with_single_model_is_rejected() {
        let mut p = ConfigPanel::new();
        p.view = ConfigView::ModelList;
        p.models = vec![fake_model("only")];
        p.handle_key(KeyCode::Char('x'));
        assert!(matches!(p.view, ConfigView::ModelList));
        assert!(p.message.is_some());
    }

    #[test]
    fn model_list_x_enters_delete_confirm_and_submits() {
        let mut p = ConfigPanel::new();
        p.view = ConfigView::ModelList;
        p.models = vec![fake_model("a"), fake_model("b")];
        p.list_cursor = 1;
        p.handle_key(KeyCode::Char('x'));
        assert!(matches!(
            &p.view,
            ConfigView::DeleteModelConfirm(DeleteModelConfirm {
                model_id,
                ..
            }) if model_id == "b"
        ));
        assert_eq!(p.pending_db_request(), DbRequest::None);
        p.handle_key(KeyCode::Enter);
        assert_eq!(
            p.pending_db_request(),
            DbRequest::DeleteModel {
                model_id: "b".to_string()
            }
        );
    }

    #[test]
    fn model_list_delete_confirm_esc_cancels() {
        let mut p = ConfigPanel::new();
        p.models = vec![fake_model("a"), fake_model("b")];
        p.view = ConfigView::DeleteModelConfirm(DeleteModelConfirm {
            model_id: "a".into(),
            model_name: "a".into(),
            submitted: false,
        });
        p.handle_key(KeyCode::Esc);
        assert!(matches!(p.view, ConfigView::ModelList));
        assert_eq!(p.pending_db_request(), DbRequest::None);
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
        p.view = test_add_model_form(
            vec![
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
            0,
            false,
        );
        p.handle_key(KeyCode::Char('a'));
        p.handle_key(KeyCode::Char('b'));
        if let ConfigView::AddModel(f) = &p.view {
            assert_eq!(f.fields[0].value, "ab");
        } else {
            panic!("still AddModel");
        }
    }

    #[test]
    fn form_tab_moves_to_next_field() {
        let mut p = ConfigPanel::new();
        p.view = test_add_model_form(
            vec![
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
            0,
            false,
        );
        p.handle_key(KeyCode::Tab);
        if let ConfigView::AddModel(f) = &p.view {
            assert_eq!(f.field_cursor, 1);
        } else {
            panic!();
        }
    }

    #[test]
    fn form_backspace_deletes() {
        let mut p = ConfigPanel::new();
        p.view = test_add_model_form(
            vec![FormField {
                label: "x",
                value: "ab".into(),
                is_secret: false,
            }],
            0,
            false,
        );
        p.handle_key(KeyCode::Backspace);
        if let ConfigView::AddModel(f) = &p.view {
            assert_eq!(f.fields[0].value, "a");
        } else {
            panic!();
        }
    }

    #[test]
    fn form_left_cancels_to_model_list() {
        let mut p = ConfigPanel::new();
        p.view = test_add_model_form(
            vec![FormField {
                label: "x",
                value: String::new(),
                is_secret: false,
            }],
            0,
            false,
        );
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
        p.view = test_add_model_form(
            vec![
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
            1,
            false,
        );
        p.handle_key(KeyCode::Enter);
        if let ConfigView::AddModel(f) = &p.view {
            assert!(f.submitted);
            assert_eq!(f.field_cursor, 1);
        } else {
            panic!();
        }
    }

    #[test]
    fn pending_db_request_none_when_not_submitted() {
        let mut p = ConfigPanel::new();
        p.view = test_add_model_form(
            vec![
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
            0,
            false,
        );
        let req = p.pending_db_request();
        assert!(matches!(req, DbRequest::None));
    }

    #[test]
    fn pending_db_request_delete_model_when_submitted() {
        let mut p = ConfigPanel::new();
        p.view = ConfigView::DeleteModelConfirm(DeleteModelConfirm {
            model_id: "m1".into(),
            model_name: "model one".into(),
            submitted: true,
        });
        let req = p.pending_db_request();
        match req {
            DbRequest::DeleteModel { model_id } => assert_eq!(model_id, "m1"),
            other => panic!("unexpected {:?}", other),
        }
    }

    fn test_add_model_form(
        fields: Vec<FormField>,
        field_cursor: usize,
        submitted: bool,
    ) -> ConfigView {
        ConfigView::AddModel(AddModelForm {
            template_idx: None,
            fields,
            field_cursor,
            submitted,
        })
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

    fn fake_workspace(id: &str, is_default: bool) -> WorkspaceRow {
        WorkspaceRow {
            id: id.into(),
            name: id.into(),
            path: format!("/projects/{id}"),
            is_default,
        }
    }

    // ---- v0.5.0 工作区管理：菜单导航 / 列表 / 新增确认 / 删除 / 设置默认（任务书 §4）----

    #[test]
    fn menu_down_to_workspace_management_opens_workspace_list() {
        let mut p = ConfigPanel::new();
        p.handle_key(KeyCode::Down); // Model+Provider -> 工作区管理
        assert_eq!(p.menu_cursor, 1);
        p.handle_key(KeyCode::Right);
        assert!(matches!(p.view, ConfigView::WorkspaceList));
        assert_eq!(p.expanded, Some(1));
        // 空缓存触发 LoadWorkspaces（进入列表即默认显示列表）。
        assert_eq!(p.pending_db_request(), DbRequest::LoadWorkspaces);
    }

    #[test]
    fn workspace_list_left_returns_to_menu() {
        let mut p = ConfigPanel::new();
        p.view = ConfigView::WorkspaceList;
        p.expanded = Some(1);
        let r = p.handle_key(KeyCode::Left);
        assert!(matches!(p.view, ConfigView::Menu));
        assert!(p.expanded.is_none());
        assert!(matches!(r, ActionResult::Navigate));
    }

    #[test]
    fn workspace_list_shows_rows_and_default_marker() {
        let mut panel = ConfigPanel::new();
        panel.view = ConfigView::WorkspaceList;
        panel.workspaces = vec![fake_workspace("alpha", true), fake_workspace("beta", false)];
        let text = render_to_text(&panel).replace(' ', "");
        assert!(text.contains("alpha"), "行内容可见: {text}");
        assert!(text.contains("beta"), "行内容可见: {text}");
        assert!(text.contains("★"), "默认标记可见: {text}");
        assert!(text.contains("/projects/alpha"), "路径可见: {text}");
        assert!(text.contains('a') && text.contains('x') && text.contains('d'));
    }

    #[test]
    fn workspace_list_empty_shows_add_hint_and_loads() {
        let mut p = ConfigPanel::new();
        p.view = ConfigView::WorkspaceList;
        assert_eq!(p.pending_db_request(), DbRequest::LoadWorkspaces);
        let text = render_to_text(&p).replace(' ', "");
        assert!(text.contains("无工作区"), "空列表提示: {text}");
    }

    #[test]
    fn workspace_list_a_enters_add_form() {
        let mut p = ConfigPanel::new();
        p.view = ConfigView::WorkspaceList;
        p.handle_key(KeyCode::Char('a'));
        assert!(matches!(p.view, ConfigView::AddWorkspace(_)));
    }

    #[test]
    fn workspace_add_empty_path_shows_error_not_submitted() {
        let mut p = ConfigPanel::new();
        p.view = ConfigView::AddWorkspace(AddWorkspaceForm::default());
        p.handle_key(KeyCode::Enter);
        assert!(p.message.is_some(), "空路径应提示");
        assert!(matches!(p.view, ConfigView::AddWorkspace(_)));
        assert_eq!(p.pending_db_request(), DbRequest::None);
    }

    #[test]
    fn workspace_add_relative_path_is_rejected() {
        let mut p = ConfigPanel::new();
        let form = AddWorkspaceForm {
            path: "relative/path".into(),
            submitted: false,
        };
        p.view = ConfigView::AddWorkspace(form);
        p.handle_key(KeyCode::Enter);
        assert!(
            matches!(p.view, ConfigView::AddWorkspace(_)),
            "留在表单可重输"
        );
        assert_eq!(p.pending_db_request(), DbRequest::None);
        assert!(
            p.message
                .as_ref()
                .is_some_and(|(msg, is_error)| *is_error && msg.contains("绝对路径")),
            "相对路径拒绝提示: {:?}",
            p.message
        );
    }

    #[test]
    fn workspace_add_existing_path_submits_directly() {
        let dir = tempfile::tempdir().unwrap();
        let mut p = ConfigPanel::new();
        let form = AddWorkspaceForm {
            path: dir.path().to_string_lossy().to_string(),
            submitted: false,
        };
        p.view = ConfigView::AddWorkspace(form);
        p.handle_key(KeyCode::Enter);
        assert_eq!(
            p.pending_db_request(),
            DbRequest::SubmitAddWorkspace {
                path: dir.path().to_string_lossy().to_string()
            }
        );
        assert!(matches!(p.view, ConfigView::AddWorkspace(_)));
    }

    #[test]
    fn workspace_add_missing_path_enters_confirm_step() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("not-yet-created");
        let mut p = ConfigPanel::new();
        let form = AddWorkspaceForm {
            path: missing.to_string_lossy().to_string(),
            submitted: false,
        };
        p.view = ConfigView::AddWorkspace(form);
        p.handle_key(KeyCode::Enter);
        assert_eq!(p.pending_db_request(), DbRequest::None, "确认前不提交");
        match &p.view {
            ConfigView::AddWorkspaceConfirm(f) => {
                assert_eq!(f.path, missing.to_string_lossy());
                assert!(!f.submitted);
            }
            other => panic!("应进入确认步, got {other:?}"),
        }
        let text = render_to_text(&p).replace(' ', "");
        assert!(
            text.contains("该目录当前不存在，是否继续？"),
            "确认文案渲染: {text}"
        );
    }

    #[test]
    fn workspace_add_confirm_enter_submits_request() {
        let mut p = ConfigPanel::new();
        p.view = ConfigView::AddWorkspaceConfirm(AddWorkspaceConfirm {
            path: "/projects/pending".into(),
            submitted: false,
        });
        p.handle_key(KeyCode::Enter);
        assert_eq!(
            p.pending_db_request(),
            DbRequest::SubmitAddWorkspace {
                path: "/projects/pending".into()
            }
        );
    }

    #[test]
    fn workspace_add_confirm_y_and_n_keys() {
        let mut p = ConfigPanel::new();
        p.view = ConfigView::AddWorkspaceConfirm(AddWorkspaceConfirm {
            path: "/projects/pending".into(),
            submitted: false,
        });
        p.handle_key(KeyCode::Char('n'));
        assert!(
            matches!(p.view, ConfigView::AddWorkspace(_)),
            "n 返回修改路径"
        );
        assert_eq!(p.pending_db_request(), DbRequest::None);

        p.view = ConfigView::AddWorkspaceConfirm(AddWorkspaceConfirm {
            path: "/projects/pending".into(),
            submitted: false,
        });
        p.handle_key(KeyCode::Char('y'));
        assert_eq!(
            p.pending_db_request(),
            DbRequest::SubmitAddWorkspace {
                path: "/projects/pending".into()
            }
        );
    }

    #[test]
    fn workspace_list_x_blocks_single_workspace_delete() {
        let mut p = ConfigPanel::new();
        p.view = ConfigView::WorkspaceList;
        p.workspaces = vec![fake_workspace("only", true)];
        p.handle_key(KeyCode::Char('x'));
        assert!(matches!(p.view, ConfigView::WorkspaceList));
        assert!(p.message.is_some());
        assert_eq!(p.pending_db_request(), DbRequest::None);
    }

    #[test]
    fn workspace_list_x_enters_delete_confirm_and_submits() {
        let mut p = ConfigPanel::new();
        p.view = ConfigView::WorkspaceList;
        p.workspaces = vec![fake_workspace("alpha", true), fake_workspace("beta", false)];
        p.list_cursor = 1;
        p.handle_key(KeyCode::Char('x'));
        assert!(matches!(
            &p.view,
            ConfigView::DeleteWorkspaceConfirm(form) if form.workspace_id == "beta"
        ));
        assert_eq!(p.pending_db_request(), DbRequest::None);
        p.handle_key(KeyCode::Enter);
        assert_eq!(
            p.pending_db_request(),
            DbRequest::SubmitDeleteWorkspace { id: "beta".into() }
        );
    }

    #[test]
    fn workspace_delete_confirm_left_returns_to_list() {
        let mut p = ConfigPanel::new();
        p.workspaces = vec![fake_workspace("alpha", true)];
        p.view = ConfigView::DeleteWorkspaceConfirm(DeleteWorkspaceConfirm {
            workspace_id: "alpha".into(),
            workspace_name: "alpha".into(),
            submitted: false,
        });
        p.handle_key(KeyCode::Left);
        assert!(matches!(p.view, ConfigView::WorkspaceList));
        assert_eq!(p.pending_db_request(), DbRequest::None);
    }

    #[test]
    fn workspace_list_d_enters_set_default_and_submits_selection() {
        let mut p = ConfigPanel::new();
        p.view = ConfigView::WorkspaceList;
        p.workspaces = vec![fake_workspace("alpha", true), fake_workspace("beta", false)];
        p.handle_key(KeyCode::Char('d'));
        p.handle_key(KeyCode::Down); // alpha -> beta
        p.handle_key(KeyCode::Enter);
        assert_eq!(
            p.pending_db_request(),
            DbRequest::SubmitSetDefaultWorkspace { id: "beta".into() }
        );
        p.clear_db_request();
        assert_eq!(p.pending_db_request(), DbRequest::None);
    }

    #[test]
    fn workspace_set_default_left_returns_to_list() {
        let mut p = ConfigPanel::new();
        p.view = ConfigView::SetDefaultWorkspace(SetDefaultWorkspaceSelect {
            candidates: vec![fake_workspace("alpha", true)],
            cursor: 0,
            submitted: false,
        });
        p.handle_key(KeyCode::Left);
        assert!(matches!(p.view, ConfigView::WorkspaceList));
    }
}
