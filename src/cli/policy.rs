//! `komo policy` — inspect and dry-run the permission policy (roadmap §3).
//!
//! `list` shows the resolved rules exactly as the `PolicyApprover` will apply
//! them (invalid config entries are already filtered out, and reported); the
//! rule numbers here are the ones `check` cites. `check` dry-runs one action
//! through `Policy::decide` with the same risk classification the real tools
//! use, so what it prints is what a turn would do. Pure config parsing — no
//! db, no gateway.

use std::path::PathBuf;

use crate::config::{ConfigSnapshot, PolicyReport};
use crate::domain::approval::{ActionRef, ApprovalRequest, Risk};
use crate::domain::policy::{Access, Category, Effect, Matcher, Rule, Verdict};

/// Render the resolved policy: defaults, rules in evaluation order, and any
/// config entries that failed to parse.
pub fn list(config: &ConfigSnapshot) -> anyhow::Result<()> {
    let PolicyReport {
        policy,
        invalid,
        configured,
    } = &config.runtime.policy;

    if !configured {
        println!(
            "No [policy] table in {} — every Normal/Dangerous action asks interactively.",
            config.runtime.home.join("config.toml").display()
        );
        return Ok(());
    }

    println!("default_normal: {}", verdict_str(policy.default_normal()));
    println!("(Dangerous always asks unless a rule sets include_dangerous; Safe is deny-only)");

    if policy.rules().is_empty() {
        println!("\nno rules configured");
    } else {
        println!("\nrules (deny rules always win over allow):");
        for (i, r) in policy.rules().iter().enumerate() {
            println!("  #{i} {}", rule_str(r));
        }
    }

    if !invalid.is_empty() {
        println!(
            "\n✗ {} invalid [[policy.rule]] entr{} ignored (config order: {})",
            invalid.len(),
            if invalid.len() == 1 { "y" } else { "ies" },
            invalid
                .iter()
                .map(|i| format!("#{i}"))
                .collect::<Vec<_>>()
                .join(", ")
        );
    }
    Ok(())
}

/// Dry-run one action through the policy and explain the outcome.
pub fn check(
    config: &ConfigSnapshot,
    category: &str,
    target: &str,
    channel: Option<&str>,
    dangerous: bool,
    write: bool,
) -> anyhow::Result<()> {
    let Some(cat) = Category::parse(category) else {
        anyhow::bail!(
            "unknown category `{category}` (expected shell | file | network | homeassistant)"
        );
    };

    // Mirror the risk each real tool would attach, so the dry run matches a turn.
    let (action, risk) = match cat {
        Category::Shell => (
            ActionRef::Shell {
                command: target.to_string(),
            },
            if dangerous {
                Risk::Dangerous
            } else {
                Risk::Normal
            },
        ),
        Category::File => (
            ActionRef::File {
                path: PathBuf::from(target),
                write,
            },
            if write { Risk::Normal } else { Risk::Safe },
        ),
        Category::Network => (
            ActionRef::Network {
                url: target.to_string(),
            },
            Risk::Safe,
        ),
        Category::HomeAssistant => {
            let Some((domain, service)) = target.split_once('.') else {
                anyhow::bail!("homeassistant target must be `domain.service` (e.g. light.turn_on)");
            };
            (
                ActionRef::Service {
                    domain: domain.to_string(),
                    service: service.to_string(),
                },
                Risk::Normal,
            )
        }
    };

    let mut request = ApprovalRequest::normal(format!("check {category} {target}"));
    request.risk = risk;
    let request = request.with_action(action);

    let policy = &config.runtime.policy.policy;
    let decision = policy.decide(&request, channel);

    let risk_str = match risk {
        Risk::Safe => "safe (read-only)",
        Risk::Normal => "normal",
        Risk::Dangerous => "dangerous",
    };
    println!(
        "action:  {category} {target}  [risk: {risk_str}{}]",
        channel
            .map(|c| format!(", channel: {c}"))
            .unwrap_or_else(|| ", no session (unattended context)".to_string())
    );

    match (decision.verdict, decision.rule) {
        (Verdict::Deny, Some(i)) => {
            println!("verdict: DENY — hard-blocked, no prompt");
            println!("matched: #{i} {}", rule_str(&policy.rules()[i]));
        }
        (Verdict::Allow, Some(i)) => {
            println!("verdict: ALLOW — auto-allowed inside a session turn (no prompt)");
            println!("matched: #{i} {}", rule_str(&policy.rules()[i]));
            println!(
                "note:    with no session in scope (sweep/aux), this still falls to ask → deny"
            );
        }
        (Verdict::Allow, None) if risk == Risk::Safe => {
            println!(
                "verdict: ALLOW — read-only action, no deny rule matches (deny-only evaluation)"
            );
            println!(
                "note:    allow rules never apply to safe actions; only a deny rule can block this"
            );
        }
        (Verdict::Allow, None) => {
            println!("verdict: ALLOW — default_normal = allow (no rule matched)");
        }
        (Verdict::Ask, _) => {
            println!(
                "verdict: ASK — escalates to interactive approval (/approve in chat, y/N at the CLI)"
            );
            if risk == Risk::Dangerous {
                println!(
                    "note:    dangerous actions auto-allow only via a rule with include_dangerous = true"
                );
            }
        }
        (Verdict::Deny, None) => {
            println!("verdict: DENY — default_normal = deny (no rule matched)");
        }
    }
    Ok(())
}

fn verdict_str(v: Verdict) -> &'static str {
    match v {
        Verdict::Allow => "allow",
        Verdict::Deny => "deny",
        Verdict::Ask => "ask",
    }
}

/// One-line rendering of a rule, mirroring its config shape.
fn rule_str(r: &Rule) -> String {
    let mut parts = vec![
        match r.effect {
            Effect::Allow => "allow".to_string(),
            Effect::Deny => "deny ".to_string(),
        },
        format!("{:<14}", category_str(r.category)),
        format!("{} \"{}\"", matcher_str(r.matcher), r.value),
    ];
    if let Some(a) = r.access {
        parts.push(format!(
            "access={}",
            match a {
                Access::Read => "read",
                Access::Write => "write",
            }
        ));
    }
    if let Some(c) = &r.channels {
        parts.push(format!("channels={}", c.join(",")));
    }
    if r.include_dangerous {
        parts.push("include_dangerous".to_string());
    }
    if r.unattended {
        parts.push("unattended".to_string());
    }
    parts.join("  ")
}

fn category_str(c: Category) -> &'static str {
    match c {
        Category::Shell => "shell",
        Category::File => "file",
        Category::Network => "network",
        Category::HomeAssistant => "homeassistant",
    }
}

fn matcher_str(m: Matcher) -> &'static str {
    match m {
        Matcher::Prefix => "prefix",
        Matcher::Suffix => "suffix",
        Matcher::Exact => "exact",
        Matcher::Contains => "contains",
    }
}
