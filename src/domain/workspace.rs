use std::path::{Component, Path, PathBuf};

/// Whitelist of directories within which file operations are permitted.
#[derive(Clone)]
pub struct Workspace {
    roots: Vec<PathBuf>,
}

impl Workspace {
    /// Create a workspace rooted at the given (absolute) directories.
    pub fn new(roots: Vec<PathBuf>) -> Self {
        Self { roots }
    }

    /// A workspace rooted at the current working directory.
    pub fn current_dir() -> std::io::Result<Self> {
        Ok(Self::new(vec![std::env::current_dir()?]))
    }

    pub fn roots(&self) -> &[PathBuf] {
        &self.roots
    }

    /// Returns true if `path` resolves to a location inside one of the roots.
    ///
    /// Resolution is lexical (collapses `.`/`..` without touching the
    /// filesystem), so it also guards write targets that do not yet exist and
    /// blocks `../` escapes.
    pub fn contains(&self, path: &Path) -> bool {
        let resolved = self.resolve(path);
        self.roots.iter().any(|root| resolved.starts_with(root))
    }

    fn resolve(&self, path: &Path) -> PathBuf {
        let joined = if path.is_absolute() {
            path.to_path_buf()
        } else {
            // Relative paths are anchored to the first root.
            self.roots.first().cloned().unwrap_or_default().join(path)
        };
        normalize_lexically(&joined)
    }
}

/// Lexically normalize a path: collapse `.` and `..` without filesystem access.
fn normalize_lexically(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for comp in path.components() {
        match comp {
            Component::ParentDir => {
                out.pop();
            }
            Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allows_paths_inside_root_and_blocks_escapes() {
        let ws = Workspace::new(vec![PathBuf::from("/home/user/project")]);

        assert!(ws.contains(Path::new("/home/user/project/src/main.rs")));
        assert!(ws.contains(Path::new("notes.txt"))); // relative → anchored to root
        assert!(ws.contains(Path::new("/home/user/project/a/../b.txt")));

        assert!(!ws.contains(Path::new("/etc/passwd")));
        assert!(!ws.contains(Path::new("/home/user/project/../secret"))); // escape
        assert!(!ws.contains(Path::new("../../etc/passwd")));
        assert!(!ws.contains(Path::new("/home/user/project-evil/x"))); // sibling prefix
    }
}
