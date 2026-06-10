use std::collections::HashSet;
use std::io::{self, Write};
use std::sync::Mutex;

use crate::domain::approval::{ApprovalRequest, Approver, Risk};

/// What the user answered at the approval prompt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Answer {
    /// Allow this one action only.
    Once,
    /// Allow this action and remember its scope key for the rest of the session.
    Session,
    Deny,
}

fn parse_answer(input: &str) -> Answer {
    match input.trim().to_lowercase().as_str() {
        "y" | "yes" => Answer::Once,
        "s" | "session" => Answer::Session,
        _ => Answer::Deny,
    }
}

/// Interactive approver, modeled on hermes-agent's approval policy:
/// - `Risk::Safe` actions (read-only commands) are allowed without prompting.
/// - everything else prompts with `[y/s/N]`, where `s` allows the action and
///   caches its scope key so the same kind of action skips the prompt for the
///   rest of the session.
pub struct CliApprover {
    session_allowed: Mutex<HashSet<String>>,
}

impl CliApprover {
    pub fn new() -> Self {
        Self {
            session_allowed: Mutex::new(HashSet::new()),
        }
    }
}

impl Default for CliApprover {
    fn default() -> Self {
        Self::new()
    }
}

impl Approver for CliApprover {
    fn approve(&self, request: &ApprovalRequest) -> bool {
        if request.risk == Risk::Safe {
            return true;
        }

        // Session cache: the user already said "allow for this session" for
        // this kind of action.
        if let Some(key) = &request.scope_key {
            if self.session_allowed.lock().unwrap().contains(key) {
                println!("✓ auto-approved (session): {}", request.summary);
                return true;
            }
        }

        match request.risk {
            Risk::Safe => unreachable!("handled above"),
            Risk::Dangerous => {
                print!("\n🛑 DANGEROUS — request to {}", request.summary);
                if let Some(detail) = &request.detail {
                    print!("\n   ({detail})");
                }
                print!("\n   Approve? [y]es once / [s]ession / [N]o ");
            }
            Risk::Normal => {
                print!(
                    "\n⚠  Approve request to {}? [y]es once / [s]ession / [N]o ",
                    request.summary
                );
            }
        }

        if io::stdout().flush().is_err() {
            return false;
        }

        let mut answer = String::new();
        if io::stdin().read_line(&mut answer).is_err() {
            return false;
        }

        match parse_answer(&answer) {
            Answer::Once => true,
            Answer::Session => {
                if let Some(key) = &request.scope_key {
                    self.session_allowed.lock().unwrap().insert(key.clone());
                }
                true
            }
            Answer::Deny => false,
        }
    }
}

/// Non-interactive approver for unattended contexts (the gateway): there is no
/// human at a TTY to consent, so every approval-gated action is denied — even
/// `Risk::Safe` ones. This mirrors hermes disabling interactive/dangerous
/// toolsets in its cron/gateway context — tools that never request approval
/// still work; everything gated is refused.
pub struct DenyApprover;

impl Approver for DenyApprover {
    fn approve(&self, request: &ApprovalRequest) -> bool {
        tracing::warn!(
            summary = %request.summary,
            "approval auto-denied (non-interactive gateway)"
        );
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn answers_parse_to_once_session_or_deny() {
        assert_eq!(parse_answer("y\n"), Answer::Once);
        assert_eq!(parse_answer("YES"), Answer::Once);
        assert_eq!(parse_answer("s\n"), Answer::Session);
        assert_eq!(parse_answer("Session"), Answer::Session);
        assert_eq!(parse_answer(""), Answer::Deny);
        assert_eq!(parse_answer("n"), Answer::Deny);
        assert_eq!(parse_answer("whatever"), Answer::Deny);
    }

    #[test]
    fn safe_requests_skip_the_prompt() {
        let approver = CliApprover::new();
        assert!(approver.approve(&ApprovalRequest::safe("run shell command: ls")));
    }

    #[test]
    fn session_cache_short_circuits_the_prompt() {
        let approver = CliApprover::new();
        approver
            .session_allowed
            .lock()
            .unwrap()
            .insert("file:write".to_string());
        let request =
            ApprovalRequest::normal("write 5 bytes to /tmp/x").with_scope_key("file:write");
        assert!(approver.approve(&request));
    }

    #[test]
    fn deny_approver_denies_even_safe_requests() {
        assert!(!DenyApprover.approve(&ApprovalRequest::safe("run shell command: ls")));
    }
}
