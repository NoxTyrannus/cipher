//! #13（v0.5.6）：API key 单行掩码输入组件（CLI 向导与 TUI 配置面板共用，一处真源）。
//!
//! 交互规格（用户 2026-09-19 拍板，逐字冻结）：
//! - 逐字符键入：内部 buffer 追加；显示 = 已有字符全部 `*` + 刚键入字符明文（末位明文），
//!   输入第二位时第一位应已变 `*`。
//! - Backspace：不回显；显示退为全 `*`（无明文位——只有"键入"动作才产生末位明文）。
//! - 整段粘贴（bracketed paste）：追加全部字符；显示全部 `*`（连末位也不露），
//!   `*` 位数与粘贴字符数一致。
//! - Enter：提交（空输入拒绝由调用方沿用既有文案语义）；Ctrl+C：中止（返回 Err，
//!   与 dialoguer 时代行为一致）。
//! - v0.5.4 的「已接收：…回车确认 / r 重填」确认环已整体删除（位数实时可见，确认环失去意义）。
//!
//! 安全边界：明文仅存在于显示层与内存 buffer；本模块不写任何日志（不得把 key
//! 写进 tracing）；持久化行为由调用方既有路径决定。
//!
//! 退化边界（任务书 §3）：终端不支持 bracketed paste 时，粘贴退化为逐字符键入
//! 显示（末位明文效果）——可接受。

use crate::common::error::AgentError;

/// 掩码输入事件（键处理状态机的输入枚举）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MaskedInputEvent {
    /// 逐字符键入（产生末位明文）。
    Char(char),
    /// 整段粘贴（bracketed paste；全掩码显示，连末位也不露）。
    Paste(String),
    /// 退格删除末字符（不回显，显示退为全 `*`）。
    Backspace,
    /// 提交。
    Enter,
    /// 中止（Ctrl+C）。
    CtrlC,
}

/// 状态机结果：继续输入 / 提交（携带 buffer 原文）/ 中止。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MaskedInputOutcome {
    Continue,
    Submit(String),
    Abort,
}

/// 一次事件后的完整步进结果（buffer + 显示串 + 末位明文标记 + 结果）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MaskedInputStep {
    /// 事件后的新 buffer。
    pub buffer: String,
    /// 事件后应显示的掩码串。
    pub display: String,
    /// 末位是否明文（仅 Char 事件置 true；供 TUI 在无键事件的重绘间保持显示）。
    pub last_input_reveal: bool,
    pub outcome: MaskedInputOutcome,
}

/// 显示渲染纯函数：全部 `*`（宽度按字符数）+ 可选末位明文。
/// - 空 buffer → 空串；`last_input_reveal=true` → 前 n-1 个 `*` + 末位明文；
/// - `false` → n 个 `*`（n = buffer 字符数，非字节数）。
pub fn render_masked_display(buffer: &str, last_input_reveal: bool) -> String {
    let n = buffer.chars().count();
    if n == 0 {
        return String::new();
    }
    let stars = "*".repeat(n - 1);
    if last_input_reveal {
        let last = buffer.chars().next_back().expect("n > 0");
        format!("{stars}{last}")
    } else {
        format!("{stars}*")
    }
}

/// 键处理状态机纯函数：(当前 buffer, 事件) → (新 buffer, 显示串, 末位明文标记, 结果)。
/// 每个事件自身即可完全决定显示——Char 后末位明文、Paste/Backspace 后全掩码，
/// 因此无需携带先验 reveal 状态。
pub fn apply_masked_input_event(buffer: &str, event: MaskedInputEvent) -> MaskedInputStep {
    match event {
        MaskedInputEvent::Char(c) => {
            let mut next = String::with_capacity(buffer.len() + c.len_utf8());
            next.push_str(buffer);
            next.push(c);
            MaskedInputStep {
                display: render_masked_display(&next, true),
                last_input_reveal: true,
                buffer: next,
                outcome: MaskedInputOutcome::Continue,
            }
        }
        MaskedInputEvent::Paste(text) => {
            // 主代理审阅裁定（v0.5.6）：剥掉粘贴内容首尾空白（\r \n \t 空格）。
            // 从文档/剪贴板复制的 key 常带行尾换行——原样入 buffer 会使 `*` 位数
            // 比真实 key 多 1-2（破坏"位数一致"核对），且提交含隐形空白致 ping 失败；
            // 旧 dialoguer 时代粘贴换行被当作 Enter 消化，不剥属行为退化。
            // key 内部字符不动；仅 secret 语义（本组件只用于 secret）。
            let text = text.trim_matches(|c: char| c == '\r' || c == '\n' || c == '\t' || c == ' ');
            let mut next = String::with_capacity(buffer.len() + text.len());
            next.push_str(buffer);
            next.push_str(text);
            MaskedInputStep {
                display: render_masked_display(&next, false),
                last_input_reveal: false,
                buffer: next,
                outcome: MaskedInputOutcome::Continue,
            }
        }
        MaskedInputEvent::Backspace => {
            let next: String = match buffer.char_indices().next_back() {
                Some((idx, _)) => buffer[..idx].to_string(),
                None => String::new(),
            };
            MaskedInputStep {
                display: render_masked_display(&next, false),
                last_input_reveal: false,
                buffer: next,
                outcome: MaskedInputOutcome::Continue,
            }
        }
        // 提交/中止：buffer 不变；显示定格为全掩码（不新增明文泄露面）。
        MaskedInputEvent::Enter => MaskedInputStep {
            display: render_masked_display(buffer, false),
            last_input_reveal: false,
            buffer: buffer.to_string(),
            outcome: MaskedInputOutcome::Submit(buffer.to_string()),
        },
        MaskedInputEvent::CtrlC => MaskedInputStep {
            display: render_masked_display(buffer, false),
            last_input_reveal: false,
            buffer: buffer.to_string(),
            outcome: MaskedInputOutcome::Abort,
        },
    }
}

/// crossterm 终端事件 → 掩码输入事件（CLI raw-mode 壳专用映射；
/// TUI 面板为 KeyCode 粒度，自行构造事件枚举后走同一状态机）。
pub fn crossterm_event_to_masked(event: crossterm::event::Event) -> Option<MaskedInputEvent> {
    use crossterm::event::{Event, KeyCode, KeyEventKind, KeyModifiers};
    match event {
        Event::Key(key) if key.kind == KeyEventKind::Press => {
            if key.modifiers.contains(KeyModifiers::CONTROL) {
                return match key.code {
                    KeyCode::Char('c') | KeyCode::Char('C') => Some(MaskedInputEvent::CtrlC),
                    _ => None,
                };
            }
            match key.code {
                // 仅接受无修饰 / Shift 的字符（Alt/Cmd 组合键不作为键入）。
                KeyCode::Char(c)
                    if key.modifiers == KeyModifiers::NONE
                        || key.modifiers == KeyModifiers::SHIFT =>
                {
                    Some(MaskedInputEvent::Char(c))
                }
                KeyCode::Enter => Some(MaskedInputEvent::Enter),
                KeyCode::Backspace => Some(MaskedInputEvent::Backspace),
                _ => None,
            }
        }
        Event::Paste(text) => Some(MaskedInputEvent::Paste(text)),
        _ => None,
    }
}

/// raw-mode + bracketed paste 的局部化终端状态：进入时启用、任何退出路径
/// （含错误与 Ctrl+C 中止）恢复原状。
struct RawPasteGuard(());

impl RawPasteGuard {
    fn new() -> Result<Self, AgentError> {
        use crossterm::event::EnableBracketedPaste;
        use crossterm::execute;
        use crossterm::terminal::enable_raw_mode;
        enable_raw_mode()
            .map_err(|e| AgentError::Io(format!("masked input enable_raw_mode: {e}")))?;
        if let Err(e) = execute!(std::io::stdout(), EnableBracketedPaste) {
            let _ = crossterm::terminal::disable_raw_mode();
            return Err(AgentError::Io(format!(
                "masked input EnableBracketedPaste: {e}"
            )));
        }
        Ok(Self(()))
    }
}

impl Drop for RawPasteGuard {
    fn drop(&mut self) {
        use crossterm::event::DisableBracketedPaste;
        use crossterm::execute;
        let _ = execute!(std::io::stdout(), DisableBracketedPaste);
        let _ = crossterm::terminal::disable_raw_mode();
    }
}

/// raw-mode 交互壳：crossterm 读键循环 + 单行重绘（前缀 prompt + 显示串 + 行尾光标）。
/// - Enter → `Ok(buffer 原文)`（可能为空——空拒绝语义与文案由调用方沿用；
///   返回值不 trim，与 dialoguer::Password 语义一致）。
/// - Ctrl+C → `Err`（与 dialoguer 时代一致，走调用方现有中止路径）。
pub fn read_masked_line(prompt: &str) -> Result<String, AgentError> {
    let _guard = RawPasteGuard::new()?;
    let mut buffer = String::new();
    let mut last_input_reveal = false;
    loop {
        redraw_masked_line(prompt, &render_masked_display(&buffer, last_input_reveal))
            .map_err(|e| AgentError::Io(format!("masked input redraw: {e}")))?;
        let event = crossterm::event::read()
            .map_err(|e| AgentError::Io(format!("masked input read: {e}")))?;
        let Some(masked_event) = crossterm_event_to_masked(event) else {
            continue;
        };
        let step = apply_masked_input_event(&buffer, masked_event);
        match step.outcome {
            MaskedInputOutcome::Continue => {
                buffer = step.buffer;
                last_input_reveal = step.last_input_reveal;
            }
            MaskedInputOutcome::Submit(_) => {
                // 定格为全掩码后换行让出提示行。
                finalize_masked_line(&step.display)
                    .map_err(|e| AgentError::Io(format!("masked input finalize: {e}")))?;
                return Ok(buffer);
            }
            MaskedInputOutcome::Abort => {
                let _ = finalize_masked_line(&step.display);
                return Err(AgentError::Parse("用户中止输入 (Ctrl+C)".to_string()));
            }
        }
    }
}

/// 重绘提示行：光标归零 → 清当前行 → 写 prompt + 显示串（光标自然落在行尾）。
fn redraw_masked_line(prompt: &str, display: &str) -> std::io::Result<()> {
    use crossterm::cursor::MoveToColumn;
    use crossterm::execute;
    use crossterm::terminal::{Clear, ClearType};
    use std::io::Write;
    let mut stdout = std::io::stdout();
    execute!(stdout, MoveToColumn(0), Clear(ClearType::CurrentLine))?;
    write!(stdout, "{prompt}{display}")?;
    stdout.flush()
}

/// 提示行收尾：定格显示串（Enter/Ctrl+C 路径均为全掩码）后换行。
/// raw mode 下 `\n` 不自动回车，故显式写 `\r\n`。
fn finalize_masked_line(display: &str) -> std::io::Result<()> {
    use crossterm::cursor::MoveToColumn;
    use crossterm::execute;
    use crossterm::terminal::{Clear, ClearType};
    use std::io::Write;
    let mut stdout = std::io::stdout();
    execute!(stdout, MoveToColumn(0), Clear(ClearType::CurrentLine))?;
    write!(stdout, "{display}\r\n")?;
    stdout.flush()
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- 显示渲染纯函数 ----

    #[test]
    fn render_empty_buffer_is_empty() {
        assert_eq!(render_masked_display("", true), "");
        assert_eq!(render_masked_display("", false), "");
    }

    #[test]
    fn render_single_char_reveal_is_plaintext() {
        // 仅 1 位：0 个 * + 末位明文。
        assert_eq!(render_masked_display("k", true), "k");
    }

    #[test]
    fn render_five_chars_reveal_last_only() {
        // 规格示例：输入 5 位后显示 ****k。
        assert_eq!(render_masked_display("skabk", true), "****k");
    }

    #[test]
    fn render_hidden_is_all_stars_by_char_count() {
        // 退格/粘贴态：全 * 无明文位。
        assert_eq!(render_masked_display("skabk", false), "*****");
        assert_eq!(render_masked_display("k", false), "*");
    }

    #[test]
    fn render_counts_chars_not_bytes() {
        // key 理论 ASCII，但函数按字符数即可（CJK 每字符一个 * 位）。
        assert_eq!(render_masked_display("中a", false), "**");
        assert_eq!(render_masked_display("中a", true), "*a");
    }

    // ---- 键处理状态机（规格 1-4 全部分支）----

    #[test]
    fn first_char_shows_plaintext() {
        let s = apply_masked_input_event("", MaskedInputEvent::Char('a'));
        assert_eq!(s.buffer, "a");
        assert_eq!(s.display, "a");
        assert!(s.last_input_reveal);
        assert_eq!(s.outcome, MaskedInputOutcome::Continue);
    }

    #[test]
    fn second_char_masks_the_first() {
        // 用户规格：输入第二位的时候第一位应该变成 *。
        let s = apply_masked_input_event("a", MaskedInputEvent::Char('b'));
        assert_eq!(s.buffer, "ab");
        assert_eq!(s.display, "*b");
    }

    #[test]
    fn consecutive_typing_shows_only_last_char() {
        let mut buffer = String::new();
        for c in "sk-abc".chars() {
            buffer = apply_masked_input_event(&buffer, MaskedInputEvent::Char(c)).buffer;
        }
        let s = apply_masked_input_event(&buffer, MaskedInputEvent::Char('k'));
        assert_eq!(s.buffer, "sk-abck");
        assert_eq!(s.display, "******k", "仅末位明文，其余全 *");
    }

    #[test]
    fn paste_appends_all_and_masks_everything() {
        // 整段粘贴：全部 *，连末位也不露；* 位数与 buffer 字符数一致。
        let s = apply_masked_input_event("sk-", MaskedInputEvent::Paste("abcdef".into()));
        assert_eq!(s.buffer, "sk-abcdef");
        assert_eq!(s.display, "*".repeat(9));
        assert!(!s.last_input_reveal);
    }

    #[test]
    fn paste_trims_surrounding_whitespace_artifacts() {
        // 主代理审阅裁定：复制的 key 常带行尾 \r\n——剥首尾空白（\r\n\t 空格），
        // 位数与真实 key 一致、提交无隐形空白；内部字符不动。
        let s = apply_masked_input_event("", MaskedInputEvent::Paste("sk-abc123\r\n".into()));
        assert_eq!(s.buffer, "sk-abc123");
        assert_eq!(s.display, "*".repeat(9));
        let s = apply_masked_input_event("", MaskedInputEvent::Paste(" \tsk-abc123\t ".into()));
        assert_eq!(s.buffer, "sk-abc123");
        // 内部空白保留（不误伤理论含内部空白的 key）。
        let s = apply_masked_input_event("", MaskedInputEvent::Paste("ab cd".into()));
        assert_eq!(s.buffer, "ab cd");
    }

    #[test]
    fn paste_from_empty_star_count_equals_pasted_chars() {
        let s = apply_masked_input_event("", MaskedInputEvent::Paste("12345".into()));
        assert_eq!(s.buffer, "12345");
        assert_eq!(s.display, "*****", "* 位数与粘贴字符数一致");
    }

    #[test]
    fn typing_after_paste_reveals_only_typed_char() {
        // 粘贴 5 位后键入 k → *****k（明文只属于"键入"动作）。
        let pasted = apply_masked_input_event("", MaskedInputEvent::Paste("12345".into()));
        let s = apply_masked_input_event(&pasted.buffer, MaskedInputEvent::Char('k'));
        assert_eq!(s.display, "*****k");
        assert!(s.last_input_reveal);
    }

    #[test]
    fn backspace_pops_and_shows_all_stars() {
        // 键入态（末位明文）退格 → 不回显，显示退为全 *。
        let s = apply_masked_input_event("sk-abk", MaskedInputEvent::Backspace);
        assert_eq!(s.buffer, "sk-ab");
        assert_eq!(s.display, "*****");
        assert!(!s.last_input_reveal);
    }

    #[test]
    fn backspace_on_empty_stays_empty() {
        let s = apply_masked_input_event("", MaskedInputEvent::Backspace);
        assert_eq!(s.buffer, "");
        assert_eq!(s.display, "");
        assert_eq!(s.outcome, MaskedInputOutcome::Continue);
    }

    #[test]
    fn backspace_after_paste_still_all_stars() {
        let pasted = apply_masked_input_event("", MaskedInputEvent::Paste("12345".into()));
        let s = apply_masked_input_event(&pasted.buffer, MaskedInputEvent::Backspace);
        assert_eq!(s.buffer, "1234");
        assert_eq!(s.display, "****");
        assert!(!s.last_input_reveal);
    }

    #[test]
    fn enter_submits_buffer_verbatim() {
        let s = apply_masked_input_event("sk-x", MaskedInputEvent::Enter);
        assert_eq!(s.outcome, MaskedInputOutcome::Submit("sk-x".into()));
        assert_eq!(s.display, "****", "提交时定格全掩码");
        assert_eq!(s.buffer, "sk-x");
    }

    #[test]
    fn enter_on_empty_submits_empty_for_caller_policy() {
        // 空拒绝语义（"API key 不能为空"）由调用方沿用；组件返回空 Submit 不越权改文案。
        let s = apply_masked_input_event("", MaskedInputEvent::Enter);
        assert_eq!(s.outcome, MaskedInputOutcome::Submit(String::new()));
    }

    #[test]
    fn ctrl_c_aborts() {
        let s = apply_masked_input_event("sk-x", MaskedInputEvent::CtrlC);
        assert_eq!(s.outcome, MaskedInputOutcome::Abort);
        assert_eq!(s.buffer, "sk-x", "中止不吞 buffer（仅不再使用）");
    }

    #[test]
    fn mixed_sequence_type_paste_backspace() {
        // 键入 → 粘贴 → 退格 → 键入：终态 ****k。
        let mut buffer = String::new();
        buffer = apply_masked_input_event(&buffer, MaskedInputEvent::Char('s')).buffer;
        buffer = apply_masked_input_event(&buffer, MaskedInputEvent::Paste("-abc".into())).buffer;
        buffer = apply_masked_input_event(&buffer, MaskedInputEvent::Backspace).buffer;
        let s = apply_masked_input_event(&buffer, MaskedInputEvent::Char('k'));
        assert_eq!(s.buffer, "s-abk");
        assert_eq!(s.display, "****k");
    }

    // ---- crossterm 事件映射（CLI raw-mode 壳）----

    #[test]
    fn maps_ctrl_c_to_abort_event() {
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        let ev =
            crossterm::event::Event::Key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL));
        assert_eq!(crossterm_event_to_masked(ev), Some(MaskedInputEvent::CtrlC));
    }

    #[test]
    fn maps_plain_char_enter_backspace_paste() {
        use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
        assert_eq!(
            crossterm_event_to_masked(Event::Key(KeyEvent::new(
                KeyCode::Char('a'),
                KeyModifiers::NONE
            ))),
            Some(MaskedInputEvent::Char('a'))
        );
        // Shift 修饰（大写键入）按普通字符接受。
        assert_eq!(
            crossterm_event_to_masked(Event::Key(KeyEvent::new(
                KeyCode::Char('A'),
                KeyModifiers::SHIFT
            ))),
            Some(MaskedInputEvent::Char('A'))
        );
        assert_eq!(
            crossterm_event_to_masked(Event::Key(KeyEvent::new(
                KeyCode::Enter,
                KeyModifiers::NONE
            ))),
            Some(MaskedInputEvent::Enter)
        );
        assert_eq!(
            crossterm_event_to_masked(Event::Key(KeyEvent::new(
                KeyCode::Backspace,
                KeyModifiers::NONE
            ))),
            Some(MaskedInputEvent::Backspace)
        );
        assert_eq!(
            crossterm_event_to_masked(Event::Paste("abc".into())),
            Some(MaskedInputEvent::Paste("abc".into()))
        );
    }

    #[test]
    fn ignores_ctrl_other_alt_release_and_non_input_events() {
        use crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
        // Ctrl+其他键、Alt+字符：不作为键入。
        assert_eq!(
            crossterm_event_to_masked(Event::Key(KeyEvent::new(
                KeyCode::Char('v'),
                KeyModifiers::CONTROL
            ))),
            None
        );
        assert_eq!(
            crossterm_event_to_masked(Event::Key(KeyEvent::new(
                KeyCode::Char('a'),
                KeyModifiers::ALT
            ))),
            None
        );
        // Release/Repeat 事件忽略（Windows kitty-enhancement 键盘下会成对上报）。
        assert_eq!(
            crossterm_event_to_masked(Event::Key(KeyEvent::new_with_kind(
                KeyCode::Char('a'),
                KeyModifiers::NONE,
                KeyEventKind::Release
            ))),
            None
        );
        // 方向键等其他键码忽略。
        assert_eq!(
            crossterm_event_to_masked(Event::Key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE))),
            None
        );
    }
}
