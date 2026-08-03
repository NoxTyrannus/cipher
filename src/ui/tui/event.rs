use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TuiAction {
    ForwardTab,

    BackwardTab,

    Cancel,

    Char(char),

    Submit,

    Backspace,

    ScrollUp,

    ScrollDown,

    Quit,

    Ignore,
}

pub fn key_event_to_action(key: KeyEvent) -> TuiAction {
    if key.kind != KeyEventKind::Press {
        return TuiAction::Ignore;
    }

    if key.modifiers.contains(KeyModifiers::CONTROL) {
        return match key.code {
            KeyCode::Char('c') | KeyCode::Char('d') => TuiAction::Quit,
            _ => TuiAction::Ignore,
        };
    }
    match key.code {
        KeyCode::Tab => TuiAction::ForwardTab,
        KeyCode::BackTab => TuiAction::BackwardTab,
        KeyCode::Esc => TuiAction::Cancel,
        KeyCode::Enter => TuiAction::Submit,
        KeyCode::Backspace => TuiAction::Backspace,
        KeyCode::PageUp => TuiAction::ScrollUp,
        KeyCode::PageDown => TuiAction::ScrollDown,
        KeyCode::Char(c) => TuiAction::Char(c),
        _ => TuiAction::Ignore,
    }
}

pub const BACKTAB_SENTINEL: &str = "\u{11}<backtab>";

pub const TAB_STR: &str = "\t";

pub const QUIT_SENTINEL: &str = "/exit";

#[cfg(test)]
mod tests {
    use super::*;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn ctrl(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL)
    }

    #[test]
    fn tab_maps_to_forward() {
        assert_eq!(
            key_event_to_action(key(KeyCode::Tab)),
            TuiAction::ForwardTab
        );
    }

    #[test]
    fn backtab_maps_to_backward() {
        assert_eq!(
            key_event_to_action(key(KeyCode::BackTab)),
            TuiAction::BackwardTab
        );
    }

    #[test]
    fn esc_maps_to_cancel() {
        assert_eq!(key_event_to_action(key(KeyCode::Esc)), TuiAction::Cancel);
    }

    #[test]
    fn enter_maps_to_submit() {
        assert_eq!(key_event_to_action(key(KeyCode::Enter)), TuiAction::Submit);
    }

    #[test]
    fn char_maps_to_char() {
        assert_eq!(
            key_event_to_action(key(KeyCode::Char('a'))),
            TuiAction::Char('a')
        );
    }

    #[test]
    fn ctrl_c_ctrl_d_quit() {
        assert_eq!(key_event_to_action(ctrl('c')), TuiAction::Quit);
        assert_eq!(key_event_to_action(ctrl('d')), TuiAction::Quit);
    }

    #[test]
    fn ctrl_other_ignored() {
        assert_eq!(key_event_to_action(ctrl('x')), TuiAction::Ignore);
    }

    #[test]
    fn backspace_maps() {
        assert_eq!(
            key_event_to_action(key(KeyCode::Backspace)),
            TuiAction::Backspace
        );
    }

    fn key_release(code: KeyCode) -> KeyEvent {
        KeyEvent::new_with_kind(code, KeyModifiers::NONE, KeyEventKind::Release)
    }

    #[test]
    fn release_events_ignored() {
        assert_eq!(
            key_event_to_action(key_release(KeyCode::Char('a'))),
            TuiAction::Ignore
        );
        assert_eq!(
            key_event_to_action(key_release(KeyCode::Tab)),
            TuiAction::Ignore
        );
        assert_eq!(
            key_event_to_action(key_release(KeyCode::Enter)),
            TuiAction::Ignore
        );
    }

    #[test]
    fn sentinels_nonempty() {
        assert!(!BACKTAB_SENTINEL.is_empty());
        assert!(!QUIT_SENTINEL.is_empty());
        assert_eq!(TAB_STR, "\t");
    }
}
