use std::sync::Arc;

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::json;

use crate::{
    domain::{
        approval::{ApprovalRequest, Approver},
        repository::SkillRepository,
        skill::{SOURCE_LEARNED, Skill},
        tool::Tool,
    },
    infra::skills::FsSkillStore,
    services::skill_registry::SkillRegistry,
};

#[derive(Deserialize)]
struct SkillArgs {
    action: String,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    instructions: Option<String>,
    #[serde(default)]
    source: Option<String>,
}

/// Lets the model discover, load, author, and install skills (progressive
/// disclosure): `list` returns the catalog; `view` returns a skill's full
/// instruction body, which the model then follows; `learn` distills a reusable
/// procedure into a **candidate** skill (the on-demand analog of the reflective
/// reviewer's passive extraction — same triage ladder); `install` fetches a
/// skill from a git repo or a raw SKILL.md URL and — once the operator approves
/// — installs it **active** (a human is always in the loop for third-party code).
pub struct SkillTool {
    registry: Arc<SkillRegistry>,
    store: Arc<FsSkillStore>,
    approver: Arc<dyn Approver>,
}

impl SkillTool {
    pub fn new(
        registry: Arc<SkillRegistry>,
        store: Arc<FsSkillStore>,
        approver: Arc<dyn Approver>,
    ) -> Self {
        Self {
            registry,
            store,
            approver,
        }
    }
}

#[async_trait]
impl Tool for SkillTool {
    fn name(&self) -> &'static str {
        "skill"
    }

    fn description(&self) -> &'static str {
        "Discover, load, author, and install skills (reusable instruction \
         playbooks). action=\"list\" returns available skills; action=\"view\" \
         returns a named skill's full instructions, which you should then \
         follow; action=\"learn\" saves a reusable procedure you just worked out \
         as a candidate skill for the operator to review (only learn durable, \
         reusable know-how, not one-off facts); action=\"install\" fetches a \
         skill the user points you at (a git repo or a SKILL.md URL) and \
         installs it after the operator approves."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "action": {
                    "type": "string",
                    "enum": ["list", "view", "learn", "install"],
                    "description": "Whether to list skills, view one, learn a new one, or install one."
                },
                "name": {
                    "type": "string",
                    "description": "Skill name — required for action=view and action=learn. \
                     For learn it doubles as the on-disk directory name: letters, digits, \
                     `-`/`_`/`.` only (a short, class-level slug like `sync-calendar`)."
                },
                "description": {
                    "type": "string",
                    "description": "One-line summary of what the skill does and when to use it \
                     (action=learn). Optional but strongly recommended."
                },
                "instructions": {
                    "type": "string",
                    "description": "The full skill body — the step-by-step reusable procedure \
                     (required for action=learn)."
                },
                "source": {
                    "type": "string",
                    "description": "Where to fetch the skill from (required for action=install): \
                     `owner/repo`, `owner/repo/subpath`, a GitHub URL, any `*.git`/`git@` URL, \
                     or a link straight to a raw SKILL.md."
                }
            },
            "required": ["action"]
        })
    }

    async fn execute(&self, input: String) -> anyhow::Result<String> {
        let args: SkillArgs = serde_json::from_str(&input)
            .map_err(|e| anyhow::anyhow!("invalid skill arguments: {e}"))?;

        match args.action.as_str() {
            "list" => {
                if self.registry.is_empty() {
                    Ok("(no skills installed)".to_string())
                } else {
                    Ok(self.registry.catalog())
                }
            }
            "view" => {
                let name = args
                    .name
                    .ok_or_else(|| anyhow::anyhow!("`name` is required for action=view"))?;
                match self.registry.get(&name) {
                    // A clear terminal answer, not an error: the model should
                    // move on, not retry other spellings.
                    Some(skill) if skill.disabled => Ok(format!(
                        "skill `{}` is disabled by the operator and cannot be used.",
                        skill.name
                    )),
                    Some(skill) => Ok(format!(
                        "# Skill: {}\n{}\n\n{}",
                        skill.name, skill.description, skill.instructions
                    )),
                    None => Err(anyhow::anyhow!(
                        "skill `{name}` not found; use action=list to see available skills"
                    )),
                }
            }
            "learn" => {
                let name = args
                    .name
                    .ok_or_else(|| anyhow::anyhow!("`name` is required for action=learn"))?;
                let instructions = args.instructions.ok_or_else(|| {
                    anyhow::anyhow!("`instructions` is required for action=learn")
                })?;
                if instructions.trim().is_empty() {
                    return Err(anyhow::anyhow!("`instructions` must not be empty"));
                }
                let skill = Skill {
                    name: name.clone(),
                    description: args.description.unwrap_or_default(),
                    instructions,
                    protected: false,
                    disabled: false,
                    source: SOURCE_LEARNED.to_string(),
                };
                // `save` writes a *candidate* (never an active skill): the same
                // triage ladder as the reviewer, and it refuses a protected
                // active skill or a path-escaping name. A candidate is invisible
                // to the runtime until promoted, so the reply must not imply it's
                // usable this turn.
                self.store.save(&skill).await?;
                Ok(format!(
                    "Learned `{name}` as a candidate skill. Review it with \
                     `komo skill inspect {name}`, then `komo skill promote {name}` \
                     to activate (usable on the agent's next `skill` list once promoted)."
                ))
            }
            "install" => {
                let source = args
                    .source
                    .ok_or_else(|| anyhow::anyhow!("`source` is required for action=install"))?;
                // Installing third-party code is side-effecting and never
                // unattended: gate it through the approver (session-scoped, so a
                // `/approve session` covers a batch). Denied ⇒ nothing written.
                let request = ApprovalRequest::normal(format!(
                    "Install skill from `{source}` into the active skill store"
                ))
                .with_scope_key("skill:install".to_string());
                if !self.approver.approve(&request).await {
                    return Ok("Skill install rejected by user; nothing was installed.".to_string());
                }
                let installed = crate::infra::skill_install::install(&self.store, &source).await?;
                let about = if installed.description.is_empty() {
                    String::new()
                } else {
                    format!(" — {}", installed.description)
                };
                Ok(format!(
                    "Installed `{}` ({} file(s)){about}. It's active now: use \
                     `skill` view/list to load it (no restart needed).",
                    installed.name, installed.files
                ))
            }
            other => Err(anyhow::anyhow!(
                "unknown action `{other}` (expected list/view/learn/install)"
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::skill::Skill;

    fn registry() -> Arc<SkillRegistry> {
        Arc::new(SkillRegistry::new(vec![Skill {
            name: "greet".to_string(),
            description: "Say hello".to_string(),
            instructions: "Greet the user warmly.".to_string(),
            protected: false,
            disabled: false,
            source: "user".to_string(),
        }]))
    }

    /// A throwaway on-disk store rooted in a unique temp dir.
    fn store(tag: &str) -> Arc<FsSkillStore> {
        let root = std::env::temp_dir().join(format!("komo_skilltool_{tag}"));
        let _ = std::fs::remove_dir_all(&root);
        Arc::new(FsSkillStore::new(root))
    }

    /// A no-op approver — install is the only action that consults it, and the
    /// non-install tests never reach that path.
    struct DenyAll;
    #[async_trait]
    impl Approver for DenyAll {
        async fn approve(&self, _request: &ApprovalRequest) -> bool {
            false
        }
    }

    fn approver() -> Arc<dyn Approver> {
        Arc::new(DenyAll)
    }

    fn tool_with(tag: &str) -> (SkillTool, Arc<FsSkillStore>) {
        let store = store(tag);
        (SkillTool::new(registry(), store.clone(), approver()), store)
    }

    #[tokio::test]
    async fn lists_and_views_skills() {
        let tool = SkillTool::new(registry(), store("listview"), approver());

        let list = tool
            .execute(json!({ "action": "list" }).to_string())
            .await
            .unwrap();
        assert!(list.contains("greet: Say hello"));

        let view = tool
            .execute(json!({ "action": "view", "name": "greet" }).to_string())
            .await
            .unwrap();
        assert!(view.contains("Greet the user warmly."));
    }

    #[tokio::test]
    async fn view_disabled_skill_reports_state_without_instructions() {
        let tool = SkillTool::new(
            Arc::new(SkillRegistry::new(vec![Skill {
                name: "paused".to_string(),
                description: "d".to_string(),
                instructions: "secret steps".to_string(),
                protected: false,
                disabled: true,
                source: "user".to_string(),
            }])),
            store("disabled"),
            approver(),
        );

        let view = tool
            .execute(json!({ "action": "view", "name": "paused" }).to_string())
            .await
            .unwrap();
        assert!(view.contains("disabled by the operator"));
        assert!(!view.contains("secret steps"));
    }

    #[tokio::test]
    async fn install_is_refused_when_the_approver_denies() {
        let (tool, store) = tool_with("install_denied");
        // DenyAll approver ⇒ short-circuits before any fetch; nothing installed.
        let out = tool
            .execute(json!({ "action": "install", "source": "owner/repo" }).to_string())
            .await
            .unwrap();
        assert!(out.contains("rejected"));
        assert!(store.list_active().is_empty());
    }

    #[tokio::test]
    async fn install_requires_a_source() {
        let (tool, _store) = tool_with("install_nosource");
        let err = tool
            .execute(json!({ "action": "install" }).to_string())
            .await
            .unwrap_err();
        assert!(err.to_string().contains("source"));
    }

    #[tokio::test]
    async fn view_unknown_skill_errors() {
        let tool = SkillTool::new(registry(), store("unknown"), approver());
        let err = tool
            .execute(json!({ "action": "view", "name": "nope" }).to_string())
            .await
            .unwrap_err();
        assert!(err.to_string().contains("not found"));
    }

    #[tokio::test]
    async fn learn_writes_a_candidate() {
        let (tool, store) = tool_with("learn_candidate");
        let reply = tool
            .execute(
                json!({
                    "action": "learn",
                    "name": "sync-cal",
                    "description": "Sync the calendar",
                    "instructions": "Step 1. Open the calendar.\nStep 2. Sync."
                })
                .to_string(),
            )
            .await
            .unwrap();
        assert!(reply.contains("candidate"));

        // Lands as a candidate (not active), tagged with `learned` provenance.
        assert!(store.find_active("sync-cal").is_none());
        let cand = store.find_candidate("sync-cal").unwrap();
        assert_eq!(cand.source, crate::domain::skill::SOURCE_LEARNED);
        assert_eq!(cand.description, "Sync the calendar");
        assert!(cand.instructions.contains("Step 2. Sync."));
    }

    #[tokio::test]
    async fn learn_requires_name_and_instructions() {
        let (tool, _) = tool_with("learn_missing");
        assert!(
            tool.execute(json!({ "action": "learn", "instructions": "x" }).to_string())
                .await
                .is_err()
        );
        assert!(
            tool.execute(json!({ "action": "learn", "name": "x" }).to_string())
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn learn_rejects_path_escaping_name() {
        let (tool, _) = tool_with("learn_badname");
        let err = tool
            .execute(
                json!({ "action": "learn", "name": "../escape", "instructions": "body" })
                    .to_string(),
            )
            .await
            .unwrap_err();
        assert!(err.to_string().contains("invalid skill name"));
    }

    #[tokio::test]
    async fn learn_refuses_protected_active_skill() {
        let (tool, store) = tool_with("learn_protected");
        // Seed an active, protected skill of the same name.
        store
            .save(&Skill {
                name: "guarded".to_string(),
                description: "d".to_string(),
                instructions: "orig".to_string(),
                protected: false,
                disabled: false,
                source: crate::domain::skill::SOURCE_LEARNED.to_string(),
            })
            .await
            .unwrap();
        store.promote("guarded").unwrap();
        store.set_protected("guarded", true).unwrap();

        let err = tool
            .execute(
                json!({ "action": "learn", "name": "guarded", "instructions": "new body" })
                    .to_string(),
            )
            .await
            .unwrap_err();
        assert!(err.to_string().contains("protected"));
        assert!(store.find_candidate("guarded").is_none());
    }
}
