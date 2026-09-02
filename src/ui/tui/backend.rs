use crate::common::AgentError;
use crate::mode_runtime::{ModeKind, ModeResponse};
use crate::ui::backend::UiBackend;
use crate::ui::tui::event::{
    key_event_to_action, TuiAction, BACKTAB_SENTINEL, QUIT_SENTINEL, TAB_STR,
};
use crate::ui::tui::render;
use crate::ui::tui::state::TuiState;
use async_trait::async_trait;
use crossterm::event::{self, Event};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;
use std::io::Stdout;
use std::str::FromStr;

struct TerminalGuard {
    terminal: Terminal<CrosstermBackend<Stdout>>,
}

impl TerminalGuard {
    fn new() -> Result<Self, AgentError> {
        enable_raw_mode().map_err(|e| AgentError::Io(format!("enable_raw_mode: {e}")))?;

        match Self::enter_alt_and_build() {
            Ok(terminal) => Ok(Self { terminal }),
            Err(e) => {
                let _ = disable_raw_mode();
                Err(e)
            }
        }
    }

    fn enter_alt_and_build() -> Result<Terminal<CrosstermBackend<Stdout>>, AgentError> {
        let mut stdout = std::io::stdout();
        execute!(stdout, EnterAlternateScreen)
            .map_err(|e| AgentError::Io(format!("EnterAlternateScreen: {e}")))?;
        let backend = CrosstermBackend::new(stdout);
        match Terminal::new(backend) {
            Ok(t) => Ok(t),
            Err(e) => {
                let _ = execute!(std::io::stdout(), LeaveAlternateScreen);
                Err(AgentError::Io(format!("Terminal::new: {e}")))
            }
        }
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let _ = execute!(self.terminal.backend_mut(), LeaveAlternateScreen);
    }
}

pub struct TuiBackend {
    guard: Option<TerminalGuard>,
    state: TuiState,
}

impl TuiBackend {
    pub fn new() -> Self {
        Self {
            guard: None,
            state: TuiState::new(),
        }
    }

    fn redraw(&mut self) -> Result<(), AgentError> {
        use std::io::IsTerminal;
        if !std::io::stdout().is_terminal() {
            return Ok(());
        }

        let Self { guard, state, .. } = self;
        if guard.is_none() {
            *guard = Some(TerminalGuard::new()?);
        }
        let g = guard.as_mut().expect("just initialized");
        g.terminal
            .draw(|f| render::render(state, f))
            .map_err(|e| AgentError::Io(format!("terminal draw: {e}")))?;
        Ok(())
    }
}

impl Default for TuiBackend {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl UiBackend for TuiBackend {
    async fn show_mode_status(&mut self, mode_name: &str, status: &str) -> Result<(), AgentError> {
        self.state.current_mode = ModeKind::from_str(mode_name).unwrap_or(ModeKind::Unni);
        let _ = status;
        self.redraw()
    }

    async fn show_response(&mut self, response: &ModeResponse) -> Result<(), AgentError> {
        if response.awaiting_confirmation {
            self.state.push_request(response.text.clone());
        } else {
            self.state.push_assistant(response.text.clone());
        }
        self.redraw()
    }

    async fn show_error(&mut self, error: &str) -> Result<(), AgentError> {
        self.state.set_error(error.to_string());
        self.redraw()
    }

    async fn wait_for_input(&mut self) -> Result<String, AgentError> {
        self.redraw()?;
        loop {
            let ev = event::read().map_err(|e| AgentError::Io(format!("event::read: {e}")))?;
            if let Event::Key(key) = ev {
                match key_event_to_action(key) {
                    TuiAction::ForwardTab => return Ok(TAB_STR.to_string()),
                    TuiAction::BackwardTab => return Ok(BACKTAB_SENTINEL.to_string()),
                    TuiAction::Quit => return Ok(QUIT_SENTINEL.to_string()),
                    TuiAction::Char(c) => {
                        self.state.input_push(c);
                        self.redraw()?;
                    }
                    TuiAction::Backspace => {
                        self.state.input_backspace();
                        self.redraw()?;
                    }
                    TuiAction::ScrollUp | TuiAction::ScrollDown => continue,
                    TuiAction::Submit => {
                        let input = self.state.take_input();
                        if !input.is_empty() {
                            self.state.push_user(input.clone());
                        }
                        self.redraw()?;
                        return Ok(input);
                    }
                    TuiAction::Ignore => continue,
                }
            }
        }
    }
}
