use std::collections::HashSet;
use std::path::{Path, PathBuf};

use tracing::debug;

use crate::domain::skill::Skill;

/// How deep to recurse when discovering `SKILL.md` files (plugin trees nest
/// skills several levels down, e.g. `plugins/<mp>/plugins/<p>/skills/<name>/`).
const MAX_SCAN_DEPTH: usize = 8;

/// Discovers and holds skills loaded from a directory of `<name>/SKILL.md` files.
pub struct SkillRegistry {
    skills: Vec<Skill>,
}

impl SkillRegistry {
    pub fn new(skills: Vec<Skill>) -> Self {
        Self { skills }
    }

    /// Scan a single `dir` for `*/SKILL.md` files and parse each into a [`Skill`].
    /// A missing directory yields an empty registry (skills are optional).
    pub fn load_from_dir(dir: &Path) -> Self {
        let mut skills = Self::scan_dir(dir);
        skills.sort_by(|a, b| a.name.cmp(&b.name));
        Self::new(skills)
    }

    /// Load skills from multiple directories (e.g. shion's own `skills/`, the
    /// project's `.claude/skills/`, and the user's `~/.claude/skills/`). The
    /// first directory to define a given skill name wins, so workspace-local
    /// skills override globally-shared ones.
    pub fn load_from_dirs(dirs: &[PathBuf]) -> Self {
        let mut skills = Vec::new();
        let mut seen = HashSet::new();
        for dir in dirs {
            for skill in Self::scan_dir(dir) {
                if seen.insert(skill.name.clone()) {
                    skills.push(skill);
                }
            }
        }
        skills.sort_by(|a, b| a.name.cmp(&b.name));
        Self::new(skills)
    }

    fn scan_dir(dir: &Path) -> Vec<Skill> {
        let mut skills = Vec::new();
        let Ok(entries) = std::fs::read_dir(dir) else {
            debug!(?dir, "no skills directory; skipped");
            return skills;
        };
        for entry in entries.flatten() {
            let manifest = entry.path().join("SKILL.md");
            if !manifest.is_file() {
                continue;
            }
            match std::fs::read_to_string(&manifest) {
                Ok(content) => match Skill::parse(&content) {
                    Some(skill) => {
                        debug!(name = %skill.name, ?dir, "loaded skill");
                        skills.push(skill);
                    }
                    None => warn!(?manifest, "SKILL.md missing valid frontmatter; skipped"),
                },
                Err(e) => warn!(?manifest, %e, "failed to read SKILL.md"),
            }
        }
        skills
    }

    pub fn len(&self) -> usize {
        self.skills.len()
    }

    /// A capped `- name: description` catalog for the system prompt: lists up to
    /// `max` skills, noting how many more exist (use the `skill` tool to list all).
    pub fn catalog_capped(&self, max: usize) -> String {
        if self.skills.len() <= max {
            return self.catalog();
        }
        let shown = self
            .skills
            .iter()
            .take(max)
            .map(|s| format!("- {}: {}", s.name, s.description))
            .collect::<Vec<_>>()
            .join("\n");
        format!(
            "{shown}\n- …and {} more — call the `skill` tool with action=list to see all.",
            self.skills.len() - max
        )
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

    #[test]
    fn first_directory_wins_on_name_collision() {
        let base = std::env::temp_dir().join("shion_skill_dirs_test");
        let local = base.join("local");
        let global = base.join("global");
        for (dir, body) in [(&local, "LOCAL version"), (&global, "GLOBAL version")] {
            let d = dir.join("dup");
            std::fs::create_dir_all(&d).unwrap();
            std::fs::write(
                d.join("SKILL.md"),
                format!("---\nname: dup\ndescription: d\n---\n{body}"),
            )
            .unwrap();
        }

        let reg = SkillRegistry::load_from_dirs(&[local.clone(), global.clone()]);
        assert_eq!(reg.len(), 1);
        assert!(reg.get("dup").unwrap().instructions.contains("LOCAL"));

        let _ = std::fs::remove_dir_all(&base);
    }
}
