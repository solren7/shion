use std::sync::Arc;

use async_trait::async_trait;
use serde::Deserialize;

use crate::domain::{
    llm::LlmClient,
    memory::{Memory, MemoryKind, MemoryRepository},
    message::{Message, Role},
    repository::SkillRepository,
    reviewer::{ReviewOutcome, Reviewer, SELF_REVIEW_PROMPT},
    session::Session,
    skill::Skill,
};

pub struct ReflectiveReviewer {
    llm: Arc<dyn LlmClient>,
    memories: Arc<dyn MemoryRepository>,
    skills: Arc<dyn SkillRepository>,
}

impl ReflectiveReviewer {
    pub fn new(
        llm: Arc<dyn LlmClient>,
        memories: Arc<dyn MemoryRepository>,
        skills: Arc<dyn SkillRepository>,
    ) -> Self {
        Self {
            llm,
            memories,
            skills,
        }
    }
}

#[async_trait]
impl Reviewer for ReflectiveReviewer {
    async fn review(&self, session: &Session) -> anyhow::Result<ReviewOutcome> {
        let prompt = review_prompt(session);
        let review_session = Session {
            id: format!("review-{}", session.id),
            messages: vec![Message::user(prompt)],
            created_at: time::OffsetDateTime::now_utc().unix_timestamp(),
        };

        let reply = self.llm.complete(&review_session).await?;
        let Some(suggestions) = parse_suggestions(&reply)? else {
            return Ok(ReviewOutcome::default());
        };

        let mut outcome = ReviewOutcome::default();
        for suggestion in suggestions.memories {
            if should_skip(&suggestion.content) {
                continue;
            }
            let memory = Memory::new(
                suggestion.kind.unwrap_or(MemoryKind::User),
                suggestion.content,
            );
            self.memories.save(&memory).await?;
            outcome.memories_written.push(memory.id);
        }

        for suggestion in suggestions.skills {
            if should_skip(&suggestion.instructions) {
                continue;
            }
            let existing = self.skills.find(&suggestion.name).await?;
            let skill = Skill {
                name: suggestion.name,
                description: suggestion
                    .description
                    .or_else(|| existing.as_ref().map(|s| s.description.clone()))
                    .unwrap_or_default(),
                instructions: suggestion.instructions,
                protected: existing.map(|s| s.protected).unwrap_or(false),
            };
            self.skills.save(&skill).await?;
            outcome.skills_written.push(skill.name);
        }

        Ok(outcome)
    }
}

fn review_prompt(session: &Session) -> String {
    let transcript = session
        .messages
        .iter()
        .map(render_message)
        .collect::<Vec<_>>()
        .join("\n\n");

    format!(
        "{SELF_REVIEW_PROMPT}\n\nReturn only JSON in this exact shape:\n\
         {{\"memories\":[{{\"kind\":\"user|feedback|project|reference\",\"content\":\"...\"}}],\
         \"skills\":[{{\"name\":\"class-level-skill-name\",\"description\":\"...\",\
         \"instructions\":\"full patched skill body\"}}]}}\n\
         Use empty arrays when nothing durable should be written.\n\n\
         Session transcript:\n{transcript}"
    )
}

fn render_message(message: &Message) -> String {
    let role = match message.role {
        Role::System => "system",
        Role::User => "user",
        Role::Assistant => "assistant",
        Role::Tool => "tool",
    };
    format!("{role}: {}", message.content)
}

#[derive(Debug, Deserialize)]
struct ReviewSuggestions {
    #[serde(default)]
    memories: Vec<MemorySuggestion>,
    #[serde(default)]
    skills: Vec<SkillSuggestion>,
}

#[derive(Debug, Deserialize)]
struct MemorySuggestion {
    #[serde(default)]
    kind: Option<MemoryKind>,
    content: String,
}

#[derive(Debug, Deserialize)]
struct SkillSuggestion {
    name: String,
    #[serde(default)]
    description: Option<String>,
    instructions: String,
}

fn parse_suggestions(reply: &str) -> anyhow::Result<Option<ReviewSuggestions>> {
    let json = extract_json(reply).trim();
    if json.is_empty() {
        return Ok(None);
    }
    Ok(Some(serde_json::from_str(json)?))
}

fn extract_json(reply: &str) -> &str {
    if let Some(start) = reply.find("```json") {
        let after_fence = &reply[start + "```json".len()..];
        if let Some(end) = after_fence.find("```") {
            return &after_fence[..end];
        }
    }
    if let Some(start) = reply.find("```") {
        let after_fence = &reply[start + "```".len()..];
        if let Some(end) = after_fence.find("```") {
            return &after_fence[..end];
        }
    }
    reply
}

fn should_skip(content: &str) -> bool {
    let text = content.to_lowercase();
    [
        "command not found",
        "missing credential",
        "missing credentials",
        "package not installed",
        "tool is broken",
        "tool broke",
        "retry fixed",
        "transient",
    ]
    .iter()
    .any(|needle| text.contains(needle))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_fenced_json() {
        let parsed = parse_suggestions(
            "```json\n{\"memories\":[{\"kind\":\"user\",\"content\":\"prefers concise replies\"}],\"skills\":[]}\n```",
        )
        .unwrap()
        .unwrap();

        assert_eq!(parsed.memories.len(), 1);
        assert_eq!(parsed.memories[0].kind, Some(MemoryKind::User));
    }

    #[test]
    fn skips_environment_failures() {
        assert!(should_skip("npm failed with command not found"));
        assert!(!should_skip("User asked for concise status updates"));
    }
}
