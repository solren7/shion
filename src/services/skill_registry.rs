use std::collections::HashSet;
use std::path::{Path, PathBuf};

use tracing::{debug, warn};

use crate::domain::skill::Skill;

/// Discovers skills from a set of `<name>/SKILL.md` directories.
///
/// When built via [`load_from_dirs`](Self::load_from_dirs) the registry holds
/// only the directory list and **re-scans on every query**, so a skill
/// installed, promoted, enabled, or disabled on disk is reflected the next time
/// the `skill` tool runs — no gateway restart needed (the filesystem is the
/// source of truth, matching `FsSkillStore` and the `komo skill` CLI). Reads
/// touch only a handful of small files, so live scanning is cheap. A registry
/// built via [`new`](Self::new) instead holds a fixed list and never re-scans
/// (used by tests).
pub struct SkillRegistry {
    /// Directories re-scanned on each query. Empty ⇒ this is a static registry
    /// backed by `static_skills`.
    dirs: Vec<PathBuf>,
    /// Fixed skill list, used only when `dirs` is empty.
    static_skills: Vec<Skill>,
}

impl SkillRegistry {
    /// A static registry over a fixed skill list — never re-scans disk.
    /// Test-only: production builds the live, disk-backed registry via
    /// [`load_from_dirs`](Self::load_from_dirs).
    #[cfg(test)]
    pub fn new(skills: Vec<Skill>) -> Self {
        Self {
            dirs: Vec::new(),
            static_skills: skills,
        }
    }

    /// A live registry over multiple directories (e.g. komo's own `skills/`,
    /// the project's `.claude/skills/`, and the user's `~/.claude/skills/`).
    /// Each query re-scans these, so on-disk changes appear without a restart.
    /// The first directory to define a given skill name wins, so workspace-local
    /// skills override globally-shared ones.
    pub fn load_from_dirs(dirs: &[PathBuf]) -> Self {
        Self {
            dirs: dirs.to_vec(),
            static_skills: Vec::new(),
        }
    }

    /// The current skills, sorted by name. Live-scans `dirs` when set (first
    /// directory wins on a name clash), otherwise returns the static list.
    fn snapshot(&self) -> Vec<Skill> {
        if self.dirs.is_empty() {
            return self.static_skills.clone();
        }
        let mut skills = Vec::new();
        let mut seen = HashSet::new();
        for dir in &self.dirs {
            for skill in Self::scan_dir(dir) {
                if seen.insert(skill.name.clone()) {
                    skills.push(skill);
                }
            }
        }
        skills.sort_by(|a, b| a.name.cmp(&b.name));
        skills
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

    /// A capped `- name: description` catalog for the system prompt: lists up to
    /// `max` skills, noting how many more exist (use the `skill` tool to list all).
    pub fn catalog_capped(&self, max: usize) -> String {
        let snapshot = self.snapshot();
        let enabled: Vec<&Skill> = snapshot.iter().filter(|s| !s.disabled).collect();
        let shown = enabled
            .iter()
            .take(max)
            .map(|s| format!("- {}: {}", s.name, s.description))
            .collect::<Vec<_>>()
            .join("\n");
        if enabled.len() <= max {
            return shown;
        }
        format!(
            "{shown}\n- …and {} more — call the `skill` tool with action=list to see all.",
            enabled.len() - max
        )
    }

    /// Look up by name, including disabled skills — the `skill` tool answers a
    /// `view` on a disabled skill with its state rather than "not found".
    pub fn get(&self, name: &str) -> Option<Skill> {
        self.snapshot().into_iter().find(|s| s.name == name)
    }

    /// No usable (enabled) skills — gates the system-prompt catalog note.
    pub fn is_empty(&self) -> bool {
        !self.snapshot().iter().any(|s| !s.disabled)
    }

    /// A `- name: description` catalog for injection into the system prompt.
    pub fn catalog(&self) -> String {
        self.snapshot()
            .iter()
            .filter(|s| !s.disabled)
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
        let dir = std::env::temp_dir().join("komo_skill_reg_test");
        let skill_dir = dir.join("greet");
        std::fs::create_dir_all(&skill_dir).unwrap();
        std::fs::write(
            skill_dir.join("SKILL.md"),
            "---\nname: greet\ndescription: Say hello nicely\n---\nGreet the user warmly.",
        )
        .unwrap();

        let reg = SkillRegistry::load_from_dirs(std::slice::from_ref(&dir));
        assert_eq!(reg.catalog().lines().count(), 1);
        assert_eq!(reg.get("greet").unwrap().description, "Say hello nicely");
        assert!(reg.catalog().contains("greet: Say hello nicely"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A skill written to disk *after* the registry is built must appear on the
    /// next query without reconstructing the registry — this is the no-restart
    /// hot-reload behavior (the bug that made `skill list` miss a freshly
    /// installed skill while `komo skill list` saw it).
    #[test]
    fn rescans_disk_so_new_skills_appear_without_restart() {
        let dir = std::env::temp_dir().join("komo_skill_hot_reload_test");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let reg = SkillRegistry::load_from_dirs(std::slice::from_ref(&dir));
        assert!(reg.is_empty());
        assert!(reg.get("late").is_none());

        // Install a skill after construction, as an approved `file` write would.
        let late = dir.join("late");
        std::fs::create_dir_all(&late).unwrap();
        std::fs::write(
            late.join("SKILL.md"),
            "---\nname: late\ndescription: Arrived after startup\n---\nDo the thing.",
        )
        .unwrap();

        // No reconstruction — the same registry now sees it.
        assert!(!reg.is_empty());
        assert_eq!(
            reg.get("late").unwrap().description,
            "Arrived after startup"
        );
        assert!(reg.catalog().contains("late: Arrived after startup"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A registry built from an explicit list (tests) is static — no disk.
    #[test]
    fn static_registry_from_new_does_not_scan_disk() {
        let reg = SkillRegistry::new(vec![Skill {
            name: "fixed".into(),
            description: "d".into(),
            instructions: "b".into(),
            protected: false,
            disabled: false,
            source: "user".into(),
        }]);
        assert_eq!(reg.get("fixed").unwrap().instructions, "b");
        assert!(reg.catalog().contains("fixed"));
    }

    #[test]
    fn missing_directory_is_empty() {
        let reg = SkillRegistry::load_from_dirs(&[PathBuf::from("/nonexistent/komo/skills")]);
        assert!(reg.is_empty());
    }

    #[test]
    fn disabled_skills_are_hidden_from_the_catalog_but_still_resolvable() {
        let dir = std::env::temp_dir().join("komo_skill_disabled_test");
        for (name, disabled) in [("alive", false), ("paused", true)] {
            let d = dir.join(name);
            std::fs::create_dir_all(&d).unwrap();
            std::fs::write(
                d.join("SKILL.md"),
                format!("---\nname: {name}\ndescription: d\ndisabled: {disabled}\n---\nbody"),
            )
            .unwrap();
        }

        let reg = SkillRegistry::load_from_dirs(std::slice::from_ref(&dir));
        assert!(reg.catalog().contains("alive"));
        assert!(!reg.catalog().contains("paused"));
        assert!(!reg.is_empty());
        // Still resolvable so the `skill` tool can explain its state.
        assert!(reg.get("paused").unwrap().disabled);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn all_disabled_counts_as_empty() {
        let reg = SkillRegistry::new(vec![Skill {
            name: "paused".into(),
            description: "d".into(),
            instructions: "b".into(),
            protected: false,
            disabled: true,
            source: "user".into(),
        }]);
        assert!(reg.is_empty());
        assert!(reg.catalog().is_empty());
    }

    #[test]
    fn first_directory_wins_on_name_collision() {
        let base = std::env::temp_dir().join("komo_skill_dirs_test");
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
        assert_eq!(reg.catalog().lines().count(), 1);
        assert!(reg.get("dup").unwrap().instructions.contains("LOCAL"));

        let _ = std::fs::remove_dir_all(&base);
    }
}
