//! Install a skill from a remote source into the active skill store.
//!
//! Shared by the operator CLI (`komo skill install`) and the approved `skill`
//! tool `install` action. Two source shapes are supported:
//!
//! - a **git repository** (`owner/repo`, `owner/repo/subpath`, a GitHub
//!   `tree` URL, or any `*.git` / `git@…` URL) — shallow-cloned, then the skill
//!   directory (SKILL.md + any scripts/`references/`) is copied whole, so
//!   multi-file skills install intact;
//! - a **single raw `SKILL.md` URL** (or a GitHub `blob` link to one) — fetched
//!   over HTTP for one-file skills.
//!
//! The whole fetch is staged in a temp dir and only copied into the store once a
//! valid `SKILL.md` is located, so a failed clone/fetch never leaves a
//! half-written skill. Installs land **active** (governance decision: an install
//! is either an operator CLI action or an approved tool call — a human is always
//! in the loop). With the live [`SkillRegistry`](crate::services::skill_registry),
//! the agent sees the new skill on its next `skill` list — no gateway restart.

use std::path::{Component, Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use crate::domain::skill::Skill;
use crate::infra::skills::FsSkillStore;

/// Outcome of a successful install.
pub struct Installed {
    pub name: String,
    pub description: String,
    pub path: PathBuf,
    /// Number of files copied into the store (1 for a single-file skill, more
    /// for a skill that ships scripts/references).
    pub files: usize,
}

/// Where a skill comes from, resolved from the user-supplied source string.
#[derive(Debug, PartialEq)]
enum Source {
    /// A single raw `SKILL.md`, fetched over HTTP.
    SingleFile(String),
    /// A git repository, shallow-cloned, with an optional branch and in-repo
    /// subpath pointing at the skill directory.
    Git {
        repo: String,
        branch: Option<String>,
        subpath: Option<String>,
    },
}

/// Fetch a skill from `source` and install it as an active skill in `store`.
pub async fn install(store: &FsSkillStore, source: &str) -> anyhow::Result<Installed> {
    let resolved = resolve_source(source)?;
    // Stage into a temp dir removed on drop, so a failed fetch/clone or an
    // invalid manifest never leaves anything behind in the store.
    let stage = TempDir::new()?;
    let skill_dir = match resolved {
        Source::SingleFile(url) => {
            let body = fetch_text(&url).await?;
            Skill::parse(&body).ok_or_else(|| {
                anyhow::anyhow!("fetched file is not a valid SKILL.md (missing frontmatter): {url}")
            })?;
            let dir = stage.path().join("skill");
            std::fs::create_dir_all(&dir)?;
            std::fs::write(dir.join("SKILL.md"), body)?;
            dir
        }
        Source::Git {
            repo,
            branch,
            subpath,
        } => {
            let checkout = stage.path().join("repo");
            git_clone(&repo, branch.as_deref(), &checkout)?;
            locate_skill_dir(&checkout, subpath.as_deref())?
        }
    };

    let (skill, files) = store.install_active_dir(&skill_dir)?;
    Ok(Installed {
        path: store.active_path(&skill.name),
        name: skill.name,
        description: skill.description,
        files,
    })
}

/// Resolve a user-supplied source string into a [`Source`]. Recognizes GitHub
/// shorthands and URLs, raw `SKILL.md` links, and generic git URLs.
fn resolve_source(raw: &str) -> anyhow::Result<Source> {
    let raw = raw.trim();
    if raw.is_empty() {
        anyhow::bail!("empty skill source");
    }

    // SSH git remote: clone verbatim.
    if raw.starts_with("git@") {
        return Ok(Source::Git {
            repo: raw.to_string(),
            branch: None,
            subpath: None,
        });
    }

    if raw.starts_with("http://") || raw.starts_with("https://") {
        // GitHub web URLs carry structure (tree/blob/branch/subpath) we unpack.
        let github = raw
            .strip_prefix("https://github.com/")
            .or_else(|| raw.strip_prefix("http://github.com/"));
        if let Some(rest) = github {
            let segs: Vec<&str> = rest.trim_end_matches('/').split('/').collect();
            if segs.len() >= 2 {
                let owner = segs[0];
                let repo = segs[1].trim_end_matches(".git");
                let repo_url = format!("https://github.com/{owner}/{repo}.git");
                if segs.len() >= 4 && (segs[2] == "tree" || segs[2] == "blob") {
                    let branch = segs[3].to_string();
                    let sub = segs[4..].join("/");
                    if segs[2] == "blob" && sub.to_lowercase().ends_with(".md") {
                        // A link straight to one file → raw fetch.
                        return Ok(Source::SingleFile(format!(
                            "https://raw.githubusercontent.com/{owner}/{repo}/{branch}/{sub}"
                        )));
                    }
                    return Ok(Source::Git {
                        repo: repo_url,
                        branch: Some(branch),
                        subpath: (!sub.is_empty()).then_some(sub),
                    });
                }
                return Ok(Source::Git {
                    repo: repo_url,
                    branch: None,
                    subpath: None,
                });
            }
        }
        // Non-GitHub URL: a direct SKILL.md, a git repo, or ambiguous.
        if raw.to_lowercase().ends_with(".md") {
            return Ok(Source::SingleFile(raw.to_string()));
        }
        return Ok(Source::Git {
            repo: raw.to_string(),
            branch: None,
            subpath: None,
        });
    }

    // Shorthand `owner/repo` or `owner/repo/subpath` — GitHub assumed. A repo is
    // always the first two segments; the rest is the in-repo skill path.
    let segs: Vec<&str> = raw.split('/').filter(|s| !s.is_empty()).collect();
    if segs.len() >= 2 {
        let owner = segs[0];
        let repo = segs[1].trim_end_matches(".git");
        let sub = segs[2..].join("/");
        return Ok(Source::Git {
            repo: format!("https://github.com/{owner}/{repo}.git"),
            branch: None,
            subpath: (!sub.is_empty()).then_some(sub),
        });
    }

    anyhow::bail!(
        "unrecognized skill source `{raw}` — use owner/repo[/subpath], a GitHub URL, or a raw SKILL.md URL"
    )
}

/// Shallow-clone `repo` (optionally at `branch`) into `dest`.
fn git_clone(repo: &str, branch: Option<&str>, dest: &Path) -> anyhow::Result<()> {
    let mut cmd = Command::new("git");
    cmd.arg("clone").arg("--depth").arg("1").arg("--quiet");
    if let Some(b) = branch {
        cmd.arg("--branch").arg(b);
    }
    cmd.arg(repo).arg(dest);
    let out = cmd.output().map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            anyhow::anyhow!(
                "`git` not found on PATH — install git to fetch skills from a repository"
            )
        } else {
            anyhow::anyhow!("failed to run git clone: {e}")
        }
    })?;
    if !out.status.success() {
        anyhow::bail!(
            "git clone failed for {repo}: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(())
}

/// Find the skill directory inside a checkout. With `subpath`, it points there;
/// otherwise the repo root is the skill, or — if exactly one exists — the sole
/// `SKILL.md` anywhere in the tree. Multiple skills without a subpath is an
/// error that lists the choices.
fn locate_skill_dir(root: &Path, subpath: Option<&str>) -> anyhow::Result<PathBuf> {
    if let Some(sub) = subpath {
        let target = safe_join(root, sub)?;
        // The subpath may name the skill dir or the SKILL.md itself.
        if target.join("SKILL.md").is_file() {
            return Ok(target);
        }
        if target.file_name().is_some_and(|n| n == "SKILL.md") && target.is_file() {
            if let Some(parent) = target.parent() {
                return Ok(parent.to_path_buf());
            }
            anyhow::bail!("`{sub}` has no parent directory");
        }
        anyhow::bail!("no SKILL.md at `{sub}` in the repository");
    }

    if root.join("SKILL.md").is_file() {
        return Ok(root.to_path_buf());
    }

    let mut found = find_skill_dirs(root, 3);
    match found.len() {
        0 => anyhow::bail!("no SKILL.md found in the repository"),
        1 => Ok(found.remove(0)),
        _ => {
            let choices: Vec<String> = found
                .iter()
                .filter_map(|p| p.strip_prefix(root).ok())
                .map(|r| r.display().to_string())
                .collect();
            anyhow::bail!(
                "repository has multiple skills — install one with owner/repo/<subpath>:\n  {}",
                choices.join("\n  ")
            )
        }
    }
}

/// Directories (under `root`, to `max_depth`) that contain a `SKILL.md`,
/// skipping dot-directories like `.git`.
fn find_skill_dirs(root: &Path, max_depth: usize) -> Vec<PathBuf> {
    let mut out = Vec::new();
    fn walk(dir: &Path, depth: usize, out: &mut Vec<PathBuf>) {
        if dir.join("SKILL.md").is_file() {
            out.push(dir.to_path_buf());
        }
        if depth == 0 {
            return;
        }
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            if entry.file_name().to_string_lossy().starts_with('.') {
                continue;
            }
            if entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                walk(&entry.path(), depth - 1, out);
            }
        }
    }
    walk(root, max_depth, &mut out);
    out.sort();
    out
}

/// Join `sub` onto `root`, rejecting any component that would escape the
/// checkout (`..`, absolute prefixes). Guards a repo-supplied subpath.
fn safe_join(root: &Path, sub: &str) -> anyhow::Result<PathBuf> {
    let mut p = root.to_path_buf();
    for comp in Path::new(sub).components() {
        match comp {
            Component::Normal(c) => p.push(c),
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                anyhow::bail!("invalid subpath `{sub}`")
            }
        }
    }
    Ok(p)
}

/// Fetch a text body over HTTP (raw SKILL.md).
async fn fetch_text(url: &str) -> anyhow::Result<String> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .user_agent("komo-skill-installer")
        .build()?;
    let resp = client
        .get(url)
        .send()
        .await
        .map_err(|e| anyhow::anyhow!("fetching {url} failed: {e}"))?;
    let status = resp.status();
    let body = resp.text().await.unwrap_or_default();
    if !status.is_success() {
        anyhow::bail!("fetching {url} returned HTTP {status}");
    }
    Ok(body)
}

/// A temp directory removed when dropped — cleans up the staging area whether
/// the install succeeds or fails.
struct TempDir(PathBuf);

impl TempDir {
    fn new() -> anyhow::Result<Self> {
        let path =
            std::env::temp_dir().join(format!("komo-skill-install-{}", uuid::Uuid::now_v7()));
        std::fs::create_dir_all(&path)?;
        Ok(Self(path))
    }
    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_owner_repo_shorthand() {
        assert_eq!(
            resolve_source("solren7/komo").unwrap(),
            Source::Git {
                repo: "https://github.com/solren7/komo.git".into(),
                branch: None,
                subpath: None,
            }
        );
    }

    #[test]
    fn resolves_owner_repo_subpath_shorthand() {
        assert_eq!(
            resolve_source("solren7/komo/skills/summarize-file").unwrap(),
            Source::Git {
                repo: "https://github.com/solren7/komo.git".into(),
                branch: None,
                subpath: Some("skills/summarize-file".into()),
            }
        );
    }

    #[test]
    fn resolves_github_tree_url_with_branch_and_subpath() {
        assert_eq!(
            resolve_source("https://github.com/o/r/tree/main/skills/foo").unwrap(),
            Source::Git {
                repo: "https://github.com/o/r.git".into(),
                branch: Some("main".into()),
                subpath: Some("skills/foo".into()),
            }
        );
    }

    #[test]
    fn resolves_github_blob_md_url_to_raw_single_file() {
        assert_eq!(
            resolve_source("https://github.com/o/r/blob/main/skills/foo/SKILL.md").unwrap(),
            Source::SingleFile(
                "https://raw.githubusercontent.com/o/r/main/skills/foo/SKILL.md".into()
            )
        );
    }

    #[test]
    fn resolves_raw_md_url_to_single_file() {
        let url = "https://raw.githubusercontent.com/o/r/main/SKILL.md";
        assert_eq!(
            resolve_source(url).unwrap(),
            Source::SingleFile(url.to_string())
        );
    }

    #[test]
    fn resolves_git_ssh_and_dotgit_urls() {
        assert!(matches!(
            resolve_source("git@github.com:o/r.git").unwrap(),
            Source::Git { .. }
        ));
        assert!(matches!(
            resolve_source("https://gitlab.example.com/o/r.git").unwrap(),
            Source::Git { .. }
        ));
    }

    #[test]
    fn empty_and_bare_sources_error() {
        assert!(resolve_source("").is_err());
        assert!(resolve_source("justoneword").is_err());
    }

    #[test]
    fn safe_join_rejects_escaping_subpaths() {
        let root = Path::new("/tmp/checkout");
        assert!(safe_join(root, "../etc").is_err());
        assert!(safe_join(root, "/etc/passwd").is_err());
        assert_eq!(
            safe_join(root, "skills/foo").unwrap(),
            root.join("skills").join("foo")
        );
    }

    #[test]
    fn locate_finds_single_skill_and_flags_multiple() {
        let base = std::env::temp_dir().join(format!("komo-locate-{}", uuid::Uuid::now_v7()));
        let one = base.join("one");
        std::fs::create_dir_all(one.join("skills/a")).unwrap();
        std::fs::write(one.join("skills/a/SKILL.md"), "---\nname: a\n---\nx").unwrap();
        // A repo with one skill, not at the root, resolves to it.
        assert_eq!(locate_skill_dir(&one, None).unwrap(), one.join("skills/a"));
        // Subpath resolution.
        assert_eq!(
            locate_skill_dir(&one, Some("skills/a")).unwrap(),
            one.join("skills/a")
        );

        let many = base.join("many");
        std::fs::create_dir_all(many.join("skills/a")).unwrap();
        std::fs::create_dir_all(many.join("skills/b")).unwrap();
        std::fs::write(many.join("skills/a/SKILL.md"), "---\nname: a\n---\nx").unwrap();
        std::fs::write(many.join("skills/b/SKILL.md"), "---\nname: b\n---\nx").unwrap();
        assert!(locate_skill_dir(&many, None).is_err());

        let _ = std::fs::remove_dir_all(&base);
    }
}
