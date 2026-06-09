use std::sync::Arc;

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::json;

use crate::{domain::tool::Tool, services::skill_registry::SkillRegistry};

#[derive(Deserialize)]
struct SkillArgs {
    action: String,
    #[serde(default)]
    name: Option<String>,
}

/// Lets the model discover and load skills (progressive disclosure): `list`
/// returns the catalog; `view` returns a skill's full instruction body, which
/// the model then follows.
pub struct SkillTool {
    registry: Arc<SkillRegistry>,
}

impl SkillTool {
    pub fn new(registry: Arc<SkillRegistry>) -> Self {
        Self { registry }
    }
}

#[async_trait]
impl Tool for SkillTool {
    fn name(&self) -> &'static str {
        "skill"
    }

    fn description(&self) -> &'static str {
        "Discover and load skills (reusable instruction playbooks). \
         action=\"list\" returns available skills; action=\"view\" returns a \
         named skill's full instructions, which you should then follow."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "action": {
                    "type": "string",
                    "enum": ["list", "view"],
                    "description": "Whether to list skills or view one."
                },
                "name": {
                    "type": "string",
                    "description": "Skill name to view (required for action=view)."
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
                    Some(skill) => Ok(format!(
                        "# Skill: {}\n{}\n\n{}",
                        skill.name, skill.description, skill.instructions
                    )),
                    None => Err(anyhow::anyhow!(
                        "skill `{name}` not found; use action=list to see available skills"
                    )),
                }
            }
            other => Err(anyhow::anyhow!(
                "unknown action `{other}` (expected list/view)"
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
        }]))
    }

    #[tokio::test]
    async fn lists_and_views_skills() {
        let tool = SkillTool::new(registry());

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
    async fn view_unknown_skill_errors() {
        let tool = SkillTool::new(registry());
        let err = tool
            .execute(json!({ "action": "view", "name": "nope" }).to_string())
            .await
            .unwrap_err();
        assert!(err.to_string().contains("not found"));
    }
}
