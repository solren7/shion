use std::path::Path;

use tracing::{debug, warn};

use crate::domain::skill::Skill;

/// Discovers and holds skills loaded from a directory of `<name>/SKILL.md` files.
pub struct SkillRegistry {
    skills: Vec<Skill>,
}

impl SkillRegistry {
    pub fn new(skills: Vec<Skill>) -> Self {
        Self { skills }
    }

    /// Scan `dir` for `*/SKILL.md` files and parse each into a [`Skill`].
    /// A missing directory yields an empty registry (skills are optional).
    pub fn load_from_dir(dir: &Path) -> Self {
        let mut skills = Vec::new();
        let Ok(entries) = std::fs::read_dir(dir) else {
            debug!(?dir, "no skills directory; skills disabled");
            return Self::new(skills);
        };

        for entry in entries.flatten() {
            let manifest = entry.path().join("SKILL.md");
            if !manifest.is_file() {
                continue;
            }
            match std::fs::read_to_string(&manifest) {
                Ok(content) => match Skill::parse(&content) {
                    Some(skill) => {
                        debug!(name = %skill.name, "loaded skill");
                        skills.push(skill);
                    }
                    None => warn!(?manifest, "SKILL.md missing valid frontmatter; skipped"),
                },
                Err(e) => warn!(?manifest, %e, "failed to read SKILL.md"),
            }
        }
        skills.sort_by(|a, b| a.name.cmp(&b.name));
        Self::new(skills)
    }

    pub fn list(&self) -> &[Skill] {
        &self.skills
    }

    pub fn get(&self, name: &str) -> Option<&Skill> {
        self.skills.iter().find(|s| s.name == name)
    }

    pub fn is_empty(&self) -> bool {
        self.skills.is_empty()
    }

    /// A `- name: description` catalog for injection into the system prompt.
    pub fn catalog(&self) -> String {
        self.skills
            .iter()
            .map(|s| format!("- {}: {}", s.name, s.description))
            .collect::<Vec<_>>()
            .join("\n")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loads_skills_from_directory() {
        let dir = std::env::temp_dir().join("shion_skill_reg_test");
        let skill_dir = dir.join("greet");
        std::fs::create_dir_all(&skill_dir).unwrap();
        std::fs::write(
            skill_dir.join("SKILL.md"),
            "---\nname: greet\ndescription: Say hello nicely\n---\nGreet the user warmly.",
        )
        .unwrap();

        let reg = SkillRegistry::load_from_dir(&dir);
        assert_eq!(reg.list().len(), 1);
        assert_eq!(reg.get("greet").unwrap().description, "Say hello nicely");
        assert!(reg.catalog().contains("greet: Say hello nicely"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn missing_directory_is_empty() {
        let reg = SkillRegistry::load_from_dir(Path::new("/nonexistent/shion/skills"));
        assert!(reg.is_empty());
    }
}
