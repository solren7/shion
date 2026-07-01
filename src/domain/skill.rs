/// A named capability package: lightweight metadata (`name` + `description`)
/// plus a full instruction body loaded on demand (progressive disclosure).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Skill {
    pub name: String,
    pub description: String,
    pub instructions: String,
    pub protected: bool,
}

impl Skill {
    /// Parse a `SKILL.md` document: YAML-ish frontmatter (`name`, `description`)
    /// fenced by `---`, followed by the instruction body.
    pub fn parse(content: &str) -> Option<Skill> {
        let rest = content.trim_start().strip_prefix("---")?;
        let fence = rest.find("\n---")?;
        let front = &rest[..fence];
        let body = rest[fence + "\n---".len()..]
            .trim_start_matches(|c| c == '-')
            .trim_start_matches(['\n', '\r'])
            .trim()
            .to_string();

        let mut name = None;
        let mut description = None;
        for line in front.lines() {
            if let Some(v) = line.strip_prefix("name:") {
                name = Some(unquote(v.trim()));
            } else if let Some(v) = line.strip_prefix("description:") {
                description = Some(unquote(v.trim()));
            }
        }

        let name = name?;
        if name.is_empty() {
            return None;
        }
        Some(Skill {
            name,
            description: description.unwrap_or_default(),
            instructions: body,
            protected: false,
        })
    }
}

fn unquote(s: &str) -> String {
    s.trim_matches(|c| c == '"' || c == '\'').to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_frontmatter_and_body() {
        let doc = "---\nname: summarize-file\ndescription: \"Summarize a file\"\n---\n\nStep 1. Read it.\nStep 2. Summarize.\n";
        let skill = Skill::parse(doc).unwrap();
        assert_eq!(skill.name, "summarize-file");
        assert_eq!(skill.description, "Summarize a file");
        assert!(skill.instructions.starts_with("Step 1."));
        assert!(skill.instructions.contains("Step 2."));
    }

    #[test]
    fn rejects_document_without_frontmatter() {
        assert!(Skill::parse("no frontmatter here").is_none());
    }
}
