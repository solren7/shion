//! Approver for the TUI: routes an approval request to the event loop as a
//! modal and awaits the user's keypress.
//!
//! Mirrors `CliApprover`'s policy (`Risk::Safe` runs without asking; `y`
//! allows once, `s` allows and remembers the scope key for the session,
//! anything else denies) — but where `CliApprover` blocks reading stdin, this
//! sends an [`ApprovalPrompt`] over a channel and suspends on a `oneshot`, so
//! the terminal stays owned by the TUI. Concurrent requests (a round's tool
//! calls run in parallel) simply queue in the channel; the event loop shows
//! one modal at a time.

use std::collections::HashSet;
use std::sync::Mutex;

use async_trait::async_trait;
use tokio::sync::{mpsc, oneshot};

use crate::domain::approval::{ApprovalRequest, Approver, Risk};

/// The user's answer to an approval modal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Answer {
    /// Allow this one action.
    Once,
    /// Allow and remember the scope key for the rest of the session.
    Session,
    Deny,
}

/// One approval rendered as a modal. `reply` is taken (`Option`) when the
/// user answers; dropping it unanswered reads as a denial on the waiting side.
pub struct ApprovalPrompt {
    pub summary: String,
    pub detail: Option<String>,
    pub dangerous: bool,
    pub reply: Option<oneshot::Sender<Answer>>,
}

pub struct TuiApprover {
    session_allowed: Mutex<HashSet<String>>,
    prompts: mpsc::UnboundedSender<ApprovalPrompt>,
}

impl TuiApprover {
    pub fn new(prompts: mpsc::UnboundedSender<ApprovalPrompt>) -> Self {
        Self {
            session_allowed: Mutex::new(HashSet::new()),
            prompts,
        }
    }
}

#[async_trait]
impl Approver for TuiApprover {
    async fn approve(&self, request: &ApprovalRequest) -> bool {
        if request.risk == Risk::Safe {
            return true;
        }
        if let Some(key) = &request.scope_key
            && self.session_allowed.lock().unwrap().contains(key)
        {
            return true;
        }

        let (tx, rx) = oneshot::channel();
        let prompt = ApprovalPrompt {
            summary: request.summary.clone(),
            detail: request.detail.clone(),
            dangerous: request.risk == Risk::Dangerous,
            reply: Some(tx),
        };
        // The TUI gone (channel closed) means no one can answer: deny.
        if self.prompts.send(prompt).is_err() {
            return false;
        }
        match rx.await {
            Ok(Answer::Once) => true,
            Ok(Answer::Session) => {
                if let Some(key) = &request.scope_key {
                    self.session_allowed.lock().unwrap().insert(key.clone());
                }
                true
            }
            // Explicit deny, or the modal was dropped unanswered (quit).
            Ok(Answer::Deny) | Err(_) => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::approval::ApprovalRequest;

    fn normal(summary: &str, scope_key: Option<&str>) -> ApprovalRequest {
        let mut r = ApprovalRequest::normal(summary);
        r.scope_key = scope_key.map(str::to_string);
        r
    }

    #[tokio::test]
    async fn safe_requests_never_prompt() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let approver = TuiApprover::new(tx);
        assert!(approver.approve(&ApprovalRequest::safe("read")).await);
        assert!(rx.try_recv().is_err(), "no modal for a safe action");
    }

    #[tokio::test]
    async fn session_answer_caches_the_scope_key() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let approver = std::sync::Arc::new(TuiApprover::new(tx));

        // First request prompts; answer "session".
        let a = approver.clone();
        let fut = tokio::spawn(async move { a.approve(&normal("run", Some("shell:ls"))).await });
        let mut prompt = rx.recv().await.expect("modal shown");
        prompt.reply.take().unwrap().send(Answer::Session).unwrap();
        assert!(fut.await.unwrap());

        // Same scope key again: allowed with no modal.
        assert!(approver.approve(&normal("run", Some("shell:ls"))).await);
        assert!(rx.try_recv().is_err(), "cached scope must not re-prompt");
    }

    #[tokio::test]
    async fn dropped_modal_reads_as_denial() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let approver = std::sync::Arc::new(TuiApprover::new(tx));
        let a = approver.clone();
        let fut = tokio::spawn(async move { a.approve(&normal("rm -rf", None)).await });
        let prompt = rx.recv().await.expect("modal shown");
        drop(prompt); // quit without answering
        assert!(!fut.await.unwrap());
    }
}
