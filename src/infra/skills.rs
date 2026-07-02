//! Filesystem-backed skill store — the single source of truth for governed
//! skills (roadmap §9).
//!
//! Skills are durable personal data (peers of memory/kanban), so they live as
//! `SKILL.md` files under `~/.shion/skills/<name>/`, not in the disposable
//! `shion.db`. Files are editable, shareable, and lock-free: every governance
//! action works while the gateway holds the Turso db lock.
//!
//! Layout under the root:
//! - `<name>/SKILL.md` — an **active** skill, loaded into the runtime
//!   `SkillRegistry` (the root is one of its scan directories).
//! - `.candidates/<name>/SKILL.md` — a reviewer **proposal**, invisible to the
//!   runtime until the operator promotes it (`shion skill promote`). The dot
//!   prefix keeps the registry's directory scan from ever loading it.
//! - `.candidates/<name>/.history/<ts>.md` — prior candidate versions, rolled
//!   on overwrite so a re-extraction never silently destroys the last proposal.
//!
//! The [`SkillRepository`] impl is the automated write path (the reflective
//! reviewer): `save` only ever writes a candidate — it never touches an active
//! file. Operator actions (promote/reject/protect/disable) are inherent
//! methods, used by the CLI directly.

use std::fs;
use std::path::{Path, PathBuf};

use async_trait::async_trait;
use tracing::{debug, warn};

use crate::domain::{
    repository::SkillRepository,
    skill::{SOURCE_REVIEWER, Skill, valid_skill_name},
};

/// Directory (under the store root) holding reviewer proposals.
const CANDIDATES_DIR: &str = ".candidates";
/// Directory (under a candidate) holding rolled prior versions.
const HISTORY_DIR: &str = ".history";
/// Marker file: the one-time import of legacy `shion.db` skills already ran.
const DB_IMPORT_MARKER: &str = ".imported-from-db";

pub struct FsSkillStore {
    root: PathBuf,
}

impl FsSkillStore {
    pub fn new(root: PathBuf) -> Self {
        Self { root }
    }

    /// The shion-owned skills home: `~/.shion/skills`.
    pub fn default_root() -> PathBuf {
        crate::config::shion_home().join("skills")
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    fn candidates_root(&self) -> PathBuf {
        self.root.join(CANDIDATES_DIR)
    }

    pub fn active_path(&self, name: &str) -> PathBuf {
        self.root.join(name).join("SKILL.md")
    }

    pub fn candidate_path(&self, name: &str) -> PathBuf {
        self.candidates_root().join(name).join("SKILL.md")
    }

    /// Rolled prior versions of a candidate (file names, oldest first) — the
    /// lightweight edit history `skill inspect` shows. Only the reviewer path
    /// rolls history; hand-edited active files are the user's own to version.
    pub fn candidate_history(&self, name: &str) -> Vec<String> {
        let dir = self.candidates_root().join(name).join(HISTORY_DIR);
        let Ok(entries) = fs::read_dir(dir) else {
            return Vec::new();
        };
        let mut names: Vec<String> = entries
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    /// Active skills (the governed subset the registry loads from this root —
    /// workspace/`~/.claude` skill dirs are governed by their own repos).
    pub fn list_active(&self) -> Vec<Skill> {
        scan_dir(&self.root)
    }

    /// Reviewer proposals awaiting triage.
    pub fn list_candidates(&self) -> Vec<Skill> {
        scan_dir(&self.candidates_root())
    }

    pub fn find_active(&self, name: &str) -> Option<Skill> {
        valid_skill_name(name)
            .then(|| read_skill(&self.active_path(name)))
            .flatten()
    }

    pub fn find_candidate(&self, name: &str) -> Option<Skill> {
        valid_skill_name(name)
            .then(|| read_skill(&self.candidate_path(name)))
            .flatten()
    }

    /// Promote a candidate to active (accepting an update proposal overwrites
    /// the active file). The candidate directory is removed afterwards.
    pub fn promote(&self, name: &str) -> anyhow::Result<Skill> {
        let Some(skill) = self.find_candidate(name) else {
            anyhow::bail!("no candidate skill named `{name}`");
        };
        self.write_active(&skill)?;
        fs::remove_dir_all(self.candidates_root().join(name))?;
        Ok(skill)
    }

    /// Reject (delete) a candidate. Unlike memories there is no usage signal to
    /// earn, so nothing is kept.
    pub fn reject(&self, name: &str) -> anyhow::Result<()> {
        if self.find_candidate(name).is_none() {
            anyhow::bail!("no candidate skill named `{name}`");
        }
        fs::remove_dir_all(self.candidates_root().join(name))?;
        Ok(())
    }

    /// Flip an active skill's `protected` flag (operator-only path).
    pub fn set_protected(&self, name: &str, on: bool) -> anyhow::Result<Skill> {
        self.update_active(name, |s| s.protected = on)
    }

    /// Flip an active skill's `disabled` flag (operator-only path).
    pub fn set_disabled(&self, name: &str, on: bool) -> anyhow::Result<Skill> {
        self.update_active(name, |s| s.disabled = on)
    }

    fn update_active(
        &self,
        name: &str,
        mutate: impl FnOnce(&mut Skill),
    ) -> anyhow::Result<Skill> {
        let Some(mut skill) = self.find_active(name) else {
            anyhow::bail!("no active skill named `{name}` in {}", self.root.display());
        };
        mutate(&mut skill);
        self.write_active(&skill)?;
        Ok(skill)
    }

    fn write_active(&self, skill: &Skill) -> anyhow::Result<()> {
        let path = self.active_path(&skill.name);
        fs::create_dir_all(path.parent().expect("skill path has a parent"))?;
        fs::write(&path, render(skill))?;
        Ok(())
    }

    /// One-time import of skills a pre-filesystem shion accumulated in
    /// `shion.db` (the reviewer used to write there; the runtime never read
    /// it). They land as **candidates** — previously-invisible extractions get
    /// a triage pass instead of silently activating. A marker file makes this
    /// a no-op forever after, even if the db rows outlive it.
    pub fn import_legacy_db(&self, skills: Vec<Skill>) -> anyhow::Result<usize> {
        let marker = self.root.join(DB_IMPORT_MARKER);
        if marker.exists() {
            return Ok(0);
        }
        let mut imported = 0;
        for mut skill in skills {
            if !valid_skill_name(&skill.name) {
                warn!(name = %skill.name, "legacy db skill has an unusable name; skipped");
                continue;
            }
            skill.source = SOURCE_REVIEWER.to_string();
            if self.write_candidate(&skill).is_ok() {
                imported += 1;
            }
        }
        fs::create_dir_all(&self.root)?;
        fs::write(&marker, "legacy shion.db skills were imported as candidates\n")?;
        Ok(imported)
    }

    /// Write (or overwrite) a candidate proposal, rolling any existing one into
    /// its `.history/` first.
    fn write_candidate(&self, skill: &Skill) -> anyhow::Result<()> {
        let path = self.candidate_path(&skill.name);
        let dir = path.parent().expect("candidate path has a parent");
        fs::create_dir_all(dir)?;
        if path.exists() {
            let history = dir.join(HISTORY_DIR);
            fs::create_dir_all(&history)?;
            let ts = time::OffsetDateTime::now_utc().unix_timestamp();
            fs::rename(&path, history.join(format!("{ts}.md")))?;
        }
        fs::write(&path, render(skill))?;
        Ok(())
    }
}

/// The automated write path. `find`/`list` expose **active** skills (what the
/// reviewer needs for description fallback and the protected check); `save`
/// writes a **candidate** — automated extraction never takes effect directly
/// (same governance ladder as memory candidates), and a protected active skill
/// refuses even the proposal.
#[async_trait]
impl SkillRepository for FsSkillStore {
    async fn find(&self, name: &str) -> anyhow::Result<Option<Skill>> {
        Ok(self.find_active(name))
    }

    async fn list(&self) -> anyhow::Result<Vec<Skill>> {
        Ok(self.list_active())
    }

    async fn save(&self, skill: &Skill) -> anyhow::Result<()> {
        if !valid_skill_name(&skill.name) {
            anyhow::bail!(
                "invalid skill name `{}` (letters, digits, `-`/`_`/`.` only)",
                skill.name
            );
        }
        if self.find_active(&skill.name).is_some_and(|s| s.protected) {
            anyhow::bail!(
                "skill `{}` is protected — not writing a proposal (operator edits only)",
                skill.name
            );
        }
        self.write_candidate(skill)
    }
}

fn read_skill(path: &Path) -> Option<Skill> {
    let content = fs::read_to_string(path).ok()?;
    Skill::parse(&content)
}

/// Scan `dir` for `<name>/SKILL.md` entries (same shape as the runtime
/// registry's scan). Dot-prefixed entries never match: a dot-dir has no
/// `SKILL.md` of its own and dot names are rejected at parse level too.
fn scan_dir(dir: &Path) -> Vec<Skill> {
    let mut skills = Vec::new();
    let Ok(entries) = fs::read_dir(dir) else {
        debug!(?dir, "no skills directory; skipped");
        return skills;
    };
    for entry in entries.flatten() {
        if entry.file_name().to_string_lossy().starts_with('.') {
            continue;
        }
        let manifest = entry.path().join("SKILL.md");
        if !manifest.is_file() {
            continue;
        }
        match read_skill(&manifest) {
            Some(skill) => skills.push(skill),
            None => warn!(?manifest, "SKILL.md missing valid frontmatter; skipped"),
        }
    }
    skills.sort_by(|a, b| a.name.cmp(&b.name));
    skills
}

/// Render a skill back to `SKILL.md`: identity frontmatter, governance keys
/// only when set (hand-written files stay minimal), then the body.
fn render(skill: &Skill) -> String {
    let mut front = format!("---\nname: {}\n", skill.name);
    if !skill.description.is_empty() {
        front.push_str(&format!("description: {}\n", skill.description));
    }
    if skill.source != crate::domain::skill::SOURCE_USER {
        front.push_str(&format!("source: {}\n", skill.source));
    }
    if skill.protected {
        front.push_str("protected: true\n");
    }
    if skill.disabled {
        front.push_str("disabled: true\n");
    }
    front.push_str(&format!(
        "updated_at: {}\n",
        time::OffsetDateTime::now_utc()
            .format(&time::format_description::well_known::Rfc3339)
            .unwrap_or_default()
    ));
    format!("{front}---\n\n{}\n", skill.instructions)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store(name: &str) -> FsSkillStore {
        let root = std::env::temp_dir().join(name);
        let _ = fs::remove_dir_all(&root);
        FsSkillStore::new(root)
    }

    fn skill(name: &str) -> Skill {
        Skill {
            name: name.to_string(),
            description: format!("does {name}"),
            instructions: format!("How to {name}."),
            protected: false,
            disabled: false,
            source: SOURCE_REVIEWER.to_string(),
        }
    }

    #[tokio::test]
    async fn save_writes_a_candidate_not_an_active_skill() {
        let store = store("shion_skillstore_candidate");
        store.save(&skill("sync-cal")).await.unwrap();

        assert!(store.find_active("sync-cal").is_none());
        let cand = store.find_candidate("sync-cal").unwrap();
        assert_eq!(cand.source, SOURCE_REVIEWER);
        assert_eq!(store.list_candidates().len(), 1);
        assert!(store.list_active().is_empty());
    }

    #[tokio::test]
    async fn promote_moves_candidate_to_active() {
        let store = store("shion_skillstore_promote");
        store.save(&skill("sync-cal")).await.unwrap();

        store.promote("sync-cal").unwrap();
        assert!(store.find_candidate("sync-cal").is_none());
        let active = store.find_active("sync-cal").unwrap();
        assert_eq!(active.description, "does sync-cal");
        // Round-trips through render/parse.
        assert!(active.instructions.contains("How to sync-cal."));
    }

    #[tokio::test]
    async fn reject_deletes_the_candidate() {
        let store = store("shion_skillstore_reject");
        store.save(&skill("sync-cal")).await.unwrap();
        store.reject("sync-cal").unwrap();
        assert!(store.find_candidate("sync-cal").is_none());
        assert!(store.reject("sync-cal").is_err());
    }

    #[tokio::test]
    async fn candidate_overwrite_rolls_history() {
        let store = store("shion_skillstore_history");
        store.save(&skill("sync-cal")).await.unwrap();
        let mut v2 = skill("sync-cal");
        v2.instructions = "v2 body".to_string();
        store.save(&v2).await.unwrap();

        assert!(
            store
                .find_candidate("sync-cal")
                .unwrap()
                .instructions
                .contains("v2 body")
        );
        let history = store
            .candidates_root()
            .join("sync-cal")
            .join(HISTORY_DIR);
        assert_eq!(fs::read_dir(history).unwrap().count(), 1);
    }

    #[tokio::test]
    async fn protected_active_skill_refuses_proposals() {
        let store = store("shion_skillstore_protected");
        store.save(&skill("sync-cal")).await.unwrap();
        store.promote("sync-cal").unwrap();
        store.set_protected("sync-cal", true).unwrap();

        let err = store.save(&skill("sync-cal")).await.unwrap_err();
        assert!(err.to_string().contains("protected"));
        assert!(store.find_candidate("sync-cal").is_none());
    }

    #[tokio::test]
    async fn save_rejects_path_escaping_names() {
        let store = store("shion_skillstore_names");
        let mut bad = skill("ok");
        bad.name = "../escape".to_string();
        assert!(store.save(&bad).await.is_err());
    }

    #[tokio::test]
    async fn disable_and_enable_roundtrip() {
        let store = store("shion_skillstore_disable");
        store.save(&skill("sync-cal")).await.unwrap();
        store.promote("sync-cal").unwrap();

        let s = store.set_disabled("sync-cal", true).unwrap();
        assert!(s.disabled);
        assert!(store.find_active("sync-cal").unwrap().disabled);
        let s = store.set_disabled("sync-cal", false).unwrap();
        assert!(!s.disabled);
    }

    #[test]
    fn legacy_import_lands_candidates_once() {
        let store = store("shion_skillstore_import");
        let n = store.import_legacy_db(vec![skill("old-a"), skill("old-b")]);
        assert_eq!(n.unwrap(), 2);
        assert_eq!(store.list_candidates().len(), 2);
        // Marker makes the second import a no-op.
        let n = store.import_legacy_db(vec![skill("old-c")]);
        assert_eq!(n.unwrap(), 0);
        assert_eq!(store.list_candidates().len(), 2);
    }
}
