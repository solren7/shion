//! [`PolicyApprover`] — the configurable permission layer (roadmap §3).
//!
//! A decorator over the interactive approver (`CliApprover` / `ChatApprover`):
//! it consults the resolved [`Policy`] first, and only escalates to the wrapped
//! approver when the policy returns [`Verdict::Ask`]. This keeps the per-action
//! decision logic in one configurable place instead of scattered `if/else` in
//! each tool, while leaving each tool's own hardline floor untouched below it.
//!
//! Same composition shape as `agent::daemon::WorkdayGated` decorating a
//! `Maintenance`.

use std::sync::Arc;

use async_trait::async_trait;
use tracing::info;

use crate::domain::{
    approval::{ApprovalRequest, Approver, Risk},
    policy::{Policy, Verdict},
};
use crate::services::tool_execution::current_session;

/// Wraps an [`Approver`], applying a [`Policy`] before falling back to it.
pub struct PolicyApprover {
    policy: Policy,
    inner: Arc<dyn Approver>,
}

impl PolicyApprover {
    /// Wrap `inner` with `policy`. Returns the trait object the tools depend on.
    pub fn wrap(policy: Policy, inner: Arc<dyn Approver>) -> Arc<dyn Approver> {
        Arc::new(Self { policy, inner })
    }
}

/// The channel a session id belongs to: the part before `:` (`feishu:oc_x` →
/// `feishu`), or `cli` for the REPL's bare uuid session ids.
fn channel_of(session_id: &str) -> String {
    match session_id.split_once(':') {
        Some((platform, _)) => platform.to_string(),
        None => "cli".to_string(),
    }
}

#[async_trait]
impl Approver for PolicyApprover {
    async fn approve(&self, request: &ApprovalRequest) -> bool {
        let channel = current_session().map(|c| channel_of(&c.session_id));

        // Read-only actions get deny-only evaluation: a deny rule can block a
        // network fetch / file read, but nothing escalates one to a prompt — an
        // unmatched safe action stays allowed without consulting the inner
        // approver (which would auto-pass it anyway).
        if request.risk == Risk::Safe {
            let decision = self.policy.decide(request, channel.as_deref());
            if decision.verdict == Verdict::Deny {
                info!(summary = %request.summary, channel = ?channel, rule = ?decision.rule,
                      "policy: denied (safe action)");
                return false;
            }
            return true;
        }

        let decision = self.policy.decide(request, channel.as_deref());
        match decision.verdict {
            Verdict::Deny => {
                info!(summary = %request.summary, channel = ?channel, rule = ?decision.rule,
                      "policy: denied");
                false
            }
            // Auto-allow only within a real session turn. With no session in
            // scope (maintenance sweep, aux sub-agent) there is no human and no
            // channel to scope to, so we never grant unattended — fall through to
            // the inner approver, which denies in that case.
            Verdict::Allow if channel.is_some() => {
                info!(summary = %request.summary, channel = ?channel, rule = ?decision.rule,
                      "policy: auto-allowed");
                true
            }
            Verdict::Allow | Verdict::Ask => self.inner.approve(request).await,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::approval::ActionRef;
    use crate::domain::policy::{Category, Effect, Matcher, Rule};
    use crate::services::tool_execution::{SessionContext, with_session};
    use std::sync::Mutex;

    struct Recording {
        asked: Mutex<bool>,
        answer: bool,
    }
    #[async_trait]
    impl Approver for Recording {
        async fn approve(&self, _request: &ApprovalRequest) -> bool {
            *self.asked.lock().unwrap() = true;
            self.answer
        }
    }

    fn allow_rule(value: &str) -> Rule {
        Rule {
            channels: None,
            category: Category::Shell,
            matcher: Matcher::Prefix,
            value: value.to_string(),
            access: None,
            effect: Effect::Allow,
            include_dangerous: false,
        }
    }

    fn shell_req() -> ApprovalRequest {
        ApprovalRequest::normal("run: cargo build").with_action(ActionRef::Shell {
            command: "cargo build".to_string(),
        })
    }

    #[tokio::test]
    async fn auto_allow_skips_inner_within_a_session() {
        let inner = Arc::new(Recording {
            asked: Mutex::new(false),
            answer: false,
        });
        let approver = PolicyApprover::wrap(
            Policy::new(vec![allow_rule("cargo ")], Verdict::Ask),
            inner.clone(),
        );
        let ctx = SessionContext::detached("cli-session");
        let allowed = with_session(ctx, approver.approve(&shell_req())).await;
        assert!(allowed);
        assert!(!*inner.asked.lock().unwrap(), "inner must not be consulted");
    }

    #[tokio::test]
    async fn allow_without_session_falls_through_to_inner() {
        let inner = Arc::new(Recording {
            asked: Mutex::new(false),
            answer: false,
        });
        let approver = PolicyApprover::wrap(
            Policy::new(vec![allow_rule("cargo ")], Verdict::Ask),
            inner.clone(),
        );
        // No `with_session`: a sweep-like context. Allow must not auto-grant.
        let allowed = approver.approve(&shell_req()).await;
        assert!(!allowed);
        assert!(*inner.asked.lock().unwrap(), "inner should be consulted");
    }

    #[tokio::test]
    async fn safe_action_is_blocked_by_a_deny_rule_without_asking() {
        let inner = Arc::new(Recording {
            asked: Mutex::new(false),
            answer: true,
        });
        let mut deny = allow_rule("");
        deny.category = Category::Network;
        deny.matcher = Matcher::Suffix;
        deny.value = "internal.corp".to_string();
        deny.effect = Effect::Deny;
        let approver = PolicyApprover::wrap(Policy::new(vec![deny], Verdict::Ask), inner.clone());

        let req = ApprovalRequest::safe("fetch").with_action(ActionRef::Network {
            url: "https://api.internal.corp/secrets".to_string(),
        });
        let ctx = SessionContext::detached("cli-session");
        assert!(!with_session(ctx, approver.approve(&req)).await);
        assert!(!*inner.asked.lock().unwrap(), "safe deny must not prompt");
    }

    #[tokio::test]
    async fn unmatched_safe_action_passes_without_consulting_inner() {
        let inner = Arc::new(Recording {
            asked: Mutex::new(false),
            answer: false,
        });
        let approver = PolicyApprover::wrap(Policy::default(), inner.clone());
        let req = ApprovalRequest::safe("fetch").with_action(ActionRef::Network {
            url: "https://example.com".to_string(),
        });
        // Even with no session in scope (sweep/aux), safe stays allowed.
        assert!(approver.approve(&req).await);
        assert!(!*inner.asked.lock().unwrap());
    }

    #[tokio::test]
    async fn ask_delegates_to_inner() {
        let inner = Arc::new(Recording {
            asked: Mutex::new(false),
            answer: true,
        });
        let approver = PolicyApprover::wrap(Policy::default(), inner.clone());
        let ctx = SessionContext::detached("cli-session");
        let allowed = with_session(ctx, approver.approve(&shell_req())).await;
        assert!(allowed);
        assert!(*inner.asked.lock().unwrap());
    }
}
