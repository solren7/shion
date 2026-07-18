//! TUI state + key handling, kept free of terminal I/O so it is unit-testable.
//! The event loop (`mod.rs`) feeds key events in and interprets the returned
//! [`Action`]s; rendering (`ui.rs`) reads the state.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use super::approver::{Answer, ApprovalPrompt};

/// Who a transcript entry belongs to (drives the prefix + styling).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    You,
    Agent,
    /// System notices (session started, …).
    Info,
    Error,
}

pub struct Entry {
    pub role: Role,
    pub text: String,
}

/// What the event loop should do in response to a key, beyond the state
/// mutation already applied.
#[derive(Debug, PartialEq, Eq)]
pub enum Action {
    /// Run a turn with this input.
    Submit(String),
    /// Start a fresh session (`/new` / `/clear`).
    NewSession,
    /// The user answered the approval modal.
    Answered(Answer),
    /// The user answered a mid-turn `ask_user` question (local mode): resolve
    /// it into the suspended turn instead of starting a new one.
    Answer(String),
    Quit,
}

pub struct App {
    pub session_id: String,
    pub entries: Vec<Entry>,
    pub input: String,
    /// Cursor as a char index into `input`.
    pub cursor: usize,
    /// Scroll offset in wrapped lines from the bottom; 0 = follow the tail.
    pub scroll_from_bottom: u16,
    pub in_flight: bool,
    /// A mid-turn `ask_user` question is pending: the next submit is its
    /// answer (allowed through even though a turn is in flight).
    pub awaiting_answer: bool,
    pub spinner: usize,
    pub modal: Option<ApprovalPrompt>,
}

impl App {
    pub fn new(session_id: String) -> Self {
        Self {
            session_id,
            entries: Vec::new(),
            input: String::new(),
            cursor: 0,
            scroll_from_bottom: 0,
            in_flight: false,
            awaiting_answer: false,
            spinner: 0,
            modal: None,
        }
    }

    pub fn push(&mut self, role: Role, text: impl Into<String>) {
        self.entries.push(Entry {
            role,
            text: text.into(),
        });
        // New content: snap back to following the tail.
        self.scroll_from_bottom = 0;
    }

    /// Handle one key press. Mutates the state and returns the action (if any)
    /// the event loop must carry out.
    pub fn on_key(&mut self, key: KeyEvent) -> Option<Action> {
        // The approval modal captures the keyboard while shown.
        if self.modal.is_some() {
            let answer = match key.code {
                KeyCode::Char('y') | KeyCode::Char('Y') => Some(Answer::Once),
                KeyCode::Char('s') | KeyCode::Char('S') => Some(Answer::Session),
                KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => Some(Answer::Deny),
                // Ctrl-C still quits even under a modal (the dropped reply
                // reads as a denial on the approver side).
                KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    return Some(Action::Quit);
                }
                _ => None,
            };
            if let Some(answer) = answer {
                if let Some(mut prompt) = self.modal.take()
                    && let Some(reply) = prompt.reply.take()
                {
                    let _ = reply.send(answer);
                }
                return Some(Action::Answered(answer));
            }
            return None;
        }

        match key.code {
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                Some(Action::Quit)
            }
            // Ctrl-D quits only on an empty input, shell-style.
            KeyCode::Char('d')
                if key.modifiers.contains(KeyModifiers::CONTROL) && self.input.is_empty() =>
            {
                Some(Action::Quit)
            }
            KeyCode::Enter => {
                let text = self.input.trim().to_string();
                if text.is_empty() {
                    return None;
                }
                if text == "/new" || text == "/clear" {
                    self.clear_input();
                    return Some(Action::NewSession);
                }
                if self.in_flight {
                    // A pending clarify question lets the input through as its
                    // answer — the suspended turn continues with it.
                    if self.awaiting_answer {
                        self.clear_input();
                        self.awaiting_answer = false;
                        return Some(Action::Answer(text));
                    }
                    // One turn at a time; keep the draft so nothing is lost.
                    return None;
                }
                self.clear_input();
                Some(Action::Submit(text))
            }
            KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                let at = self.byte_cursor();
                self.input.insert(at, c);
                self.cursor += 1;
                None
            }
            KeyCode::Backspace => {
                if self.cursor > 0 {
                    self.cursor -= 1;
                    let at = self.byte_cursor();
                    self.input.remove(at);
                }
                None
            }
            KeyCode::Delete => {
                if self.cursor < self.input.chars().count() {
                    let at = self.byte_cursor();
                    self.input.remove(at);
                }
                None
            }
            KeyCode::Left => {
                self.cursor = self.cursor.saturating_sub(1);
                None
            }
            KeyCode::Right => {
                self.cursor = (self.cursor + 1).min(self.input.chars().count());
                None
            }
            KeyCode::Home => {
                self.cursor = 0;
                None
            }
            KeyCode::End => {
                self.cursor = self.input.chars().count();
                None
            }
            KeyCode::Up => {
                self.scroll_from_bottom = self.scroll_from_bottom.saturating_add(1);
                None
            }
            KeyCode::Down => {
                self.scroll_from_bottom = self.scroll_from_bottom.saturating_sub(1);
                None
            }
            KeyCode::PageUp => {
                self.scroll_from_bottom = self.scroll_from_bottom.saturating_add(10);
                None
            }
            KeyCode::PageDown => {
                self.scroll_from_bottom = self.scroll_from_bottom.saturating_sub(10);
                None
            }
            _ => None,
        }
    }

    /// Byte offset of the char cursor (input is UTF-8; CJK chars are multibyte).
    fn byte_cursor(&self) -> usize {
        self.input
            .char_indices()
            .nth(self.cursor)
            .map(|(i, _)| i)
            .unwrap_or(self.input.len())
    }

    fn clear_input(&mut self) {
        self.input.clear();
        self.cursor = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn ctrl(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL)
    }

    fn type_str(app: &mut App, s: &str) {
        for c in s.chars() {
            app.on_key(key(KeyCode::Char(c)));
        }
    }

    #[test]
    fn typing_and_multibyte_editing_keep_utf8_boundaries() {
        let mut app = App::new("s".into());
        type_str(&mut app, "你好a");
        assert_eq!(app.input, "你好a");
        // Backspace removes whole chars, not bytes.
        app.on_key(key(KeyCode::Backspace));
        app.on_key(key(KeyCode::Backspace));
        assert_eq!(app.input, "你");
        // Insert mid-string via Left.
        type_str(&mut app, "们");
        app.on_key(key(KeyCode::Left));
        type_str(&mut app, "x");
        assert_eq!(app.input, "你x们");
    }

    #[test]
    fn enter_submits_and_clears_but_not_while_in_flight() {
        let mut app = App::new("s".into());
        type_str(&mut app, "hello");
        assert_eq!(
            app.on_key(key(KeyCode::Enter)),
            Some(Action::Submit("hello".into()))
        );
        assert!(app.input.is_empty());

        app.in_flight = true;
        type_str(&mut app, "queued?");
        assert_eq!(app.on_key(key(KeyCode::Enter)), None, "one turn at a time");
        assert_eq!(app.input, "queued?", "draft preserved");
    }

    #[test]
    fn pending_clarify_lets_a_mid_turn_submit_through_as_answer() {
        let mut app = App::new("s".into());
        app.in_flight = true;
        app.awaiting_answer = true;
        type_str(&mut app, "蓝色");
        assert_eq!(
            app.on_key(key(KeyCode::Enter)),
            Some(Action::Answer("蓝色".into()))
        );
        assert!(app.input.is_empty());
        assert!(!app.awaiting_answer, "one answer per question");
        // The next mid-turn submit is back to being blocked.
        type_str(&mut app, "more");
        assert_eq!(app.on_key(key(KeyCode::Enter)), None);
    }

    #[test]
    fn slash_new_is_a_new_session_even_mid_turn() {
        let mut app = App::new("s".into());
        app.in_flight = true;
        type_str(&mut app, "/new");
        assert_eq!(app.on_key(key(KeyCode::Enter)), Some(Action::NewSession));
    }

    #[test]
    fn modal_captures_keys_and_replies() {
        let (tx, rx) = tokio::sync::oneshot::channel();
        let mut app = App::new("s".into());
        app.modal = Some(ApprovalPrompt {
            summary: "run shell".into(),
            detail: None,
            dangerous: false,
            reply: Some(tx),
        });
        // Ordinary typing is captured by the modal.
        assert_eq!(app.on_key(key(KeyCode::Char('x'))), None);
        assert!(app.input.is_empty());
        // Answering resolves the oneshot and closes the modal.
        assert_eq!(
            app.on_key(key(KeyCode::Char('y'))),
            Some(Action::Answered(Answer::Once))
        );
        assert!(app.modal.is_none());
        assert_eq!(rx.blocking_recv(), Ok(Answer::Once));
    }

    #[test]
    fn ctrl_c_quits_everywhere_ctrl_d_only_on_empty_input() {
        let mut app = App::new("s".into());
        assert_eq!(app.on_key(ctrl('d')), Some(Action::Quit));
        type_str(&mut app, "draft");
        assert_eq!(app.on_key(ctrl('d')), None, "Ctrl-D with a draft is inert");
        assert_eq!(app.on_key(ctrl('c')), Some(Action::Quit));
    }
}
