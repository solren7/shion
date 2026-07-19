//! Configurable permission policy (roadmap §3): a pure rule engine that decides
//! whether a side-effecting action is auto-allowed, hard-denied, or escalated to
//! interactive approval — consulted *before* the interactive [`Approver`].
//!
//! Pure: no I/O and no config parsing. `config.rs` parses the `[policy]` table
//! from config.toml into these types (via the `parse` helpers here), and
//! `agent::policy_approver::PolicyApprover` wraps the interactive approver and
//! consults a [`Policy`] on every non-`Safe` request.
//!
//! Layering: the policy sits *above* each tool's own hardline floor (shell's
//! refused patterns, HA's blocked domains). Those short-circuit inside the tool
//! before any approver is consulted, so no policy `Allow` rule can unlock them —
//! the policy can only make the gate stricter than a tool's floor, never looser.
//!
//! [`Approver`]: crate::domain::approval::Approver

use crate::domain::approval::{ActionRef, ApprovalRequest, Risk};

/// The class of action a rule applies to (mirrors [`ActionRef`]'s variants).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Category {
    Shell,
    File,
    Network,
    HomeAssistant,
}

impl Category {
    /// Parse a config string (`shell` / `file` / `network` / `homeassistant`).
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_lowercase().as_str() {
            "shell" => Some(Self::Shell),
            "file" => Some(Self::File),
            "network" | "net" => Some(Self::Network),
            "homeassistant" | "ha" => Some(Self::HomeAssistant),
            _ => None,
        }
    }
}

/// How a rule's `value` is compared against the action's target string.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Matcher {
    Prefix,
    Suffix,
    Exact,
    Contains,
}

impl Matcher {
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_lowercase().as_str() {
            "prefix" => Some(Self::Prefix),
            "suffix" => Some(Self::Suffix),
            "exact" => Some(Self::Exact),
            "contains" => Some(Self::Contains),
            _ => None,
        }
    }

    fn matches(&self, value: &str, target: &str) -> bool {
        match self {
            Matcher::Prefix => target.starts_with(value),
            Matcher::Suffix => target.ends_with(value),
            Matcher::Exact => target == value,
            Matcher::Contains => target.contains(value),
        }
    }
}

/// Filesystem access kind a `file` rule scopes to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Access {
    Read,
    Write,
}

impl Access {
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_lowercase().as_str() {
            "read" => Some(Self::Read),
            "write" => Some(Self::Write),
            _ => None,
        }
    }
}

/// What a matching rule does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Effect {
    Allow,
    Deny,
}

impl Effect {
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_lowercase().as_str() {
            "allow" => Some(Self::Allow),
            "deny" => Some(Self::Deny),
            _ => None,
        }
    }
}

/// The policy's decision for a request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// Auto-allow without prompting.
    Allow,
    /// Hard-deny without prompting.
    Deny,
    /// Escalate to the interactive approver (the current behavior).
    Ask,
}

impl Verdict {
    /// Parse the `default_normal` config value (`ask` / `deny` / `allow`).
    pub fn parse_default(s: &str) -> Option<Self> {
        match s.trim().to_lowercase().as_str() {
            "ask" => Some(Self::Ask),
            "deny" => Some(Self::Deny),
            "allow" => Some(Self::Allow),
            _ => None,
        }
    }
}

/// One policy rule. Built by `config.rs` from a `[[policy.rule]]` table.
#[derive(Debug, Clone)]
pub struct Rule {
    /// Channel scope (`feishu` / `telegram` / `cli` / …); `None` = all channels.
    pub channels: Option<Vec<String>>,
    pub category: Category,
    pub matcher: Matcher,
    pub value: String,
    /// `file`-only: restrict to reads or writes; `None` = either.
    pub access: Option<Access>,
    pub effect: Effect,
    /// Allow rules don't grant `Risk::Dangerous` actions unless this is set.
    pub include_dangerous: bool,
    /// Allow rules apply only within a real session turn unless this is set:
    /// an `unattended = true` allow also grants in no-session contexts (the
    /// briefing sweep's tool-capable turn). Deny rules ignore this — they are
    /// unconditional everywhere. The narrow channel of roadmap §3.
    pub unattended: bool,
}

impl Rule {
    /// Whether this rule is in scope for `action` on `channel` (ignores `value`).
    fn applies(&self, action: &ActionRef, channel: Option<&str>) -> bool {
        if self.category != category_of(action) {
            return false;
        }
        if let Some(allowed) = &self.channels {
            match channel {
                Some(c) if allowed.iter().any(|x| x == c) => {}
                _ => return false,
            }
        }
        // File access scope: a `write` rule applies only to writes, etc.
        if let (Some(want), ActionRef::File { write, .. }) = (self.access, action)
            && (want == Access::Write) != *write
        {
            return false;
        }
        true
    }

    /// Whether this rule's `value`/`matcher` matches the action's target.
    fn matches(&self, action: &ActionRef) -> bool {
        match action {
            // Network matches on the host, with dotted-boundary suffix matching
            // so `suffix github.com` does not also match `evilgithub.com`.
            ActionRef::Network { url } => {
                let host = host_of(url);
                match self.matcher {
                    Matcher::Suffix => {
                        let want = self.value.trim_start_matches('.');
                        host == want || host.ends_with(&format!(".{want}"))
                    }
                    other => other.matches(&self.value, &host),
                }
            }
            ActionRef::Shell { command } => self.matcher.matches(&self.value, command),
            ActionRef::File { path, .. } => {
                self.matcher.matches(&self.value, &path.to_string_lossy())
            }
            ActionRef::Service { domain, service } => self
                .matcher
                .matches(&self.value, &format!("{domain}.{service}")),
        }
    }
}

/// A verdict plus which rule produced it (`None` = fell through to a default).
/// The rule index is into the policy's rule list as configured — `komo policy
/// list` shows the same numbering, so a `check` result points at a real line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Decision {
    pub verdict: Verdict,
    pub rule: Option<usize>,
}

impl Decision {
    fn fallback(verdict: Verdict) -> Self {
        Self {
            verdict,
            rule: None,
        }
    }
}

/// A resolved permission policy: an ordered rule list plus the fallback verdict
/// for a `Risk::Normal` action that no rule matches.
#[derive(Debug, Clone)]
pub struct Policy {
    rules: Vec<Rule>,
    default_normal: Verdict,
}

impl Policy {
    pub fn new(rules: Vec<Rule>, default_normal: Verdict) -> Self {
        Self {
            rules,
            default_normal,
        }
    }

    /// The configured rules, in evaluation-list order (for `komo policy list`).
    pub fn rules(&self) -> &[Rule] {
        &self.rules
    }

    /// The fallback verdict for an unmatched `Risk::Normal` action.
    pub fn default_normal(&self) -> Verdict {
        self.default_normal
    }

    /// Evaluate `request` for a turn on `channel` (`None` when no session is in
    /// scope, e.g. a maintenance sweep), reporting which rule matched — also the
    /// dry-run surface behind `komo policy check`.
    ///
    /// Deny rules take precedence over allow rules regardless of order; with no
    /// rule matching, `Risk::Normal` falls to `default_normal` and
    /// `Risk::Dangerous` always falls to [`Verdict::Ask`] (only an explicit
    /// `include_dangerous` allow rule grants a dangerous action).
    ///
    /// `Risk::Safe` gets **deny-only** evaluation: deny rules can block a
    /// read-only action (network fetch, file read), but nothing ever escalates
    /// it to a prompt — an unmatched safe action stays allowed. Allow rules are
    /// meaningless for safe actions and are skipped.
    ///
    /// **Unattended contexts** (`channel = None`) only grant through an allow
    /// rule explicitly marked `unattended`; a `default_normal = allow` fallback
    /// degrades to [`Verdict::Ask`] there — no-session grants are always an
    /// explicit opt-in, never a default.
    pub fn decide(&self, request: &ApprovalRequest, channel: Option<&str>) -> Decision {
        let Some(action) = request.action.as_ref() else {
            // No structured resource to match on; risk-only fallback.
            return Decision::fallback(self.default_for(request.risk, channel));
        };

        for (i, rule) in self.rules.iter().enumerate() {
            if rule.effect == Effect::Deny && rule.applies(action, channel) && rule.matches(action)
            {
                return Decision {
                    verdict: Verdict::Deny,
                    rule: Some(i),
                };
            }
        }
        if request.risk == Risk::Safe {
            // Deny-only for read-only actions: no allow rules, no escalation.
            return Decision::fallback(Verdict::Allow);
        }
        for (i, rule) in self.rules.iter().enumerate() {
            if rule.effect == Effect::Allow && rule.applies(action, channel) && rule.matches(action)
            {
                if request.risk == Risk::Dangerous && !rule.include_dangerous {
                    continue;
                }
                // No session in scope: only explicitly-unattended allows grant.
                if channel.is_none() && !rule.unattended {
                    continue;
                }
                return Decision {
                    verdict: Verdict::Allow,
                    rule: Some(i),
                };
            }
        }
        Decision::fallback(self.default_for(request.risk, channel))
    }

    fn default_for(&self, risk: Risk, channel: Option<&str>) -> Verdict {
        match risk {
            Risk::Safe => Verdict::Allow,
            // A default can never grant unattended (channel = None) — only an
            // explicit `unattended` rule does; degrade a would-be Allow to Ask.
            Risk::Normal if channel.is_none() && self.default_normal == Verdict::Allow => {
                Verdict::Ask
            }
            Risk::Normal => self.default_normal,
            Risk::Dangerous => Verdict::Ask,
        }
    }
}

impl Default for Policy {
    /// The empty policy: no rules, `Normal` actions ask. Identical behavior to
    /// having no policy at all — i.e. the current interactive-only flow.
    fn default() -> Self {
        Self::new(Vec::new(), Verdict::Ask)
    }
}

fn category_of(action: &ActionRef) -> Category {
    match action {
        ActionRef::Shell { .. } => Category::Shell,
        ActionRef::File { .. } => Category::File,
        ActionRef::Network { .. } => Category::Network,
        ActionRef::Service { .. } => Category::HomeAssistant,
    }
}

/// Extract the lowercase host from a URL, dependency-free: strip the scheme, cut
/// at the first `/`, `:`, `?`, or `#`.
fn host_of(url: &str) -> String {
    let after_scheme = url.split_once("://").map(|(_, r)| r).unwrap_or(url);
    after_scheme
        .split(['/', ':', '?', '#'])
        .next()
        .unwrap_or("")
        .to_lowercase()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn shell(cmd: &str, risk: Risk) -> ApprovalRequest {
        let mut req = ApprovalRequest::normal(format!("run: {cmd}"));
        req.risk = risk;
        req.with_action(ActionRef::Shell {
            command: cmd.to_string(),
        })
    }

    fn file_write(path: &str) -> ApprovalRequest {
        ApprovalRequest::normal("write").with_action(ActionRef::File {
            path: PathBuf::from(path),
            write: true,
        })
    }

    fn rule(category: Category, matcher: Matcher, value: &str, effect: Effect) -> Rule {
        Rule {
            channels: None,
            category,
            matcher,
            value: value.to_string(),
            access: None,
            effect,
            include_dangerous: false,
            unattended: false,
        }
    }

    #[test]
    fn unattended_grants_only_through_an_explicit_unattended_rule() {
        let mut r = rule(Category::Shell, Matcher::Prefix, "curl ", Effect::Allow);
        // Plain allow: grants in a session, not unattended.
        let p = Policy::new(vec![r.clone()], Verdict::Ask);
        assert_eq!(
            p.decide(&shell("curl http://x", Risk::Normal), Some("cli"))
                .verdict,
            Verdict::Allow
        );
        assert_eq!(
            p.decide(&shell("curl http://x", Risk::Normal), None)
                .verdict,
            Verdict::Ask,
            "non-unattended allow must not grant without a session"
        );
        // Opt-in: grants unattended too.
        r.unattended = true;
        let p = Policy::new(vec![r], Verdict::Ask);
        let d = p.decide(&shell("curl http://x", Risk::Normal), None);
        assert_eq!(d.verdict, Verdict::Allow);
        assert_eq!(d.rule, Some(0));
    }

    #[test]
    fn default_allow_never_grants_unattended() {
        let p = Policy::new(Vec::new(), Verdict::Allow);
        assert_eq!(
            p.decide(&shell("ls", Risk::Normal), Some("cli")).verdict,
            Verdict::Allow
        );
        assert_eq!(
            p.decide(&shell("ls", Risk::Normal), None).verdict,
            Verdict::Ask,
            "a default can never be an unattended grant"
        );
    }

    #[test]
    fn safe_actions_get_deny_only_evaluation() {
        let net = |url: &str| {
            ApprovalRequest::safe("fetch").with_action(ActionRef::Network {
                url: url.to_string(),
            })
        };
        let p = Policy::new(
            vec![
                // An allow rule must be irrelevant to safe actions…
                rule(
                    Category::Network,
                    Matcher::Suffix,
                    "github.com",
                    Effect::Allow,
                ),
                rule(
                    Category::Network,
                    Matcher::Suffix,
                    "internal.corp",
                    Effect::Deny,
                ),
            ],
            // …and so must default_normal: even Deny leaves unmatched safe alone.
            Verdict::Deny,
        );
        let denied = p.decide(&net("https://api.internal.corp/x"), Some("cli"));
        assert_eq!(denied.verdict, Verdict::Deny);
        assert_eq!(denied.rule, Some(1));

        let unmatched = p.decide(&net("https://example.com"), Some("cli"));
        assert_eq!(unmatched.verdict, Verdict::Allow);
        assert_eq!(unmatched.rule, None);
    }

    #[test]
    fn decide_reports_the_matching_rule_index() {
        let p = Policy::new(
            vec![
                rule(Category::Shell, Matcher::Prefix, "cargo ", Effect::Allow),
                rule(Category::Shell, Matcher::Prefix, "git ", Effect::Allow),
            ],
            Verdict::Ask,
        );
        let d = p.decide(&shell("git status", Risk::Normal), Some("cli"));
        assert_eq!(d.verdict, Verdict::Allow);
        assert_eq!(d.rule, Some(1));
    }

    #[test]
    fn empty_policy_asks_for_normal_and_dangerous() {
        let p = Policy::default();
        assert_eq!(
            p.decide(&shell("ls", Risk::Normal), Some("cli")).verdict,
            Verdict::Ask
        );
        assert_eq!(
            p.decide(&shell("rm -rf x", Risk::Dangerous), Some("cli"))
                .verdict,
            Verdict::Ask
        );
    }

    #[test]
    fn allow_rule_matches_command_prefix() {
        let p = Policy::new(
            vec![rule(
                Category::Shell,
                Matcher::Prefix,
                "cargo ",
                Effect::Allow,
            )],
            Verdict::Ask,
        );
        assert_eq!(
            p.decide(&shell("cargo build", Risk::Normal), Some("cli"))
                .verdict,
            Verdict::Allow
        );
        assert_eq!(
            p.decide(&shell("npm install", Risk::Normal), Some("cli"))
                .verdict,
            Verdict::Ask
        );
    }

    #[test]
    fn deny_rule_beats_allow_regardless_of_order() {
        let p = Policy::new(
            vec![
                rule(Category::Shell, Matcher::Prefix, "git ", Effect::Allow),
                rule(Category::Shell, Matcher::Contains, "push", Effect::Deny),
            ],
            Verdict::Ask,
        );
        assert_eq!(
            p.decide(&shell("git push origin", Risk::Normal), Some("cli"))
                .verdict,
            Verdict::Deny
        );
    }

    #[test]
    fn allow_rule_does_not_grant_dangerous_without_opt_in() {
        let p = Policy::new(
            vec![rule(Category::Shell, Matcher::Prefix, "rm ", Effect::Allow)],
            Verdict::Ask,
        );
        assert_eq!(
            p.decide(&shell("rm file", Risk::Dangerous), Some("cli"))
                .verdict,
            Verdict::Ask
        );

        let mut allow_dangerous = rule(Category::Shell, Matcher::Prefix, "rm ", Effect::Allow);
        allow_dangerous.include_dangerous = true;
        let p = Policy::new(vec![allow_dangerous], Verdict::Ask);
        assert_eq!(
            p.decide(&shell("rm file", Risk::Dangerous), Some("cli"))
                .verdict,
            Verdict::Allow
        );
    }

    #[test]
    fn file_write_prefix_and_access_scope() {
        let mut r = rule(
            Category::File,
            Matcher::Prefix,
            "/home/me/proj",
            Effect::Allow,
        );
        r.access = Some(Access::Write);
        let p = Policy::new(vec![r], Verdict::Ask);
        assert_eq!(
            p.decide(&file_write("/home/me/proj/src/x.rs"), Some("cli"))
                .verdict,
            Verdict::Allow
        );
        assert_eq!(
            p.decide(&file_write("/etc/passwd"), Some("cli")).verdict,
            Verdict::Ask
        );
    }

    #[test]
    fn channel_scope_limits_a_rule() {
        let mut r = rule(Category::Shell, Matcher::Prefix, "cargo ", Effect::Allow);
        r.channels = Some(vec!["cli".to_string()]);
        let p = Policy::new(vec![r], Verdict::Ask);
        assert_eq!(
            p.decide(&shell("cargo build", Risk::Normal), Some("cli"))
                .verdict,
            Verdict::Allow
        );
        assert_eq!(
            p.decide(&shell("cargo build", Risk::Normal), Some("feishu"))
                .verdict,
            Verdict::Ask
        );
        // No session in scope → a channel-scoped rule never matches.
        assert_eq!(
            p.decide(&shell("cargo build", Risk::Normal), None).verdict,
            Verdict::Ask
        );
    }

    #[test]
    fn network_suffix_matches_on_dot_boundary() {
        let net = |url: &str| {
            ApprovalRequest::normal("fetch").with_action(ActionRef::Network {
                url: url.to_string(),
            })
        };
        let p = Policy::new(
            vec![rule(
                Category::Network,
                Matcher::Suffix,
                "github.com",
                Effect::Allow,
            )],
            Verdict::Ask,
        );
        assert_eq!(
            p.decide(&net("https://api.github.com/repos"), Some("cli"))
                .verdict,
            Verdict::Allow
        );
        assert_eq!(
            p.decide(&net("https://github.com"), Some("cli")).verdict,
            Verdict::Allow
        );
        // Not a real subdomain — must not match.
        assert_eq!(
            p.decide(&net("https://evilgithub.com"), Some("cli"))
                .verdict,
            Verdict::Ask
        );
    }

    #[test]
    fn default_normal_can_deny() {
        let p = Policy::new(Vec::new(), Verdict::Deny);
        assert_eq!(
            p.decide(&shell("ls", Risk::Normal), Some("feishu")).verdict,
            Verdict::Deny
        );
        // Dangerous still asks regardless of default_normal.
        assert_eq!(
            p.decide(&shell("rm x", Risk::Dangerous), Some("feishu"))
                .verdict,
            Verdict::Ask
        );
    }
}
