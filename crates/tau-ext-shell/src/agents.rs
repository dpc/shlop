//! `AGENTS.md` discovery used at `SessionStarted` time.

use std::fs;
use std::path::{Path, PathBuf};

pub(crate) struct DiscoveredAgentsFile {
    pub(crate) file_path: PathBuf,
    pub(crate) content: String,
}

pub(crate) fn discover_session_agents_files() -> Vec<DiscoveredAgentsFile> {
    let mut roots = Vec::new();
    if let Some(home) = dirs::home_dir() {
        roots.push(home.join(".agents"));
    }
    if let Ok(cwd) = std::env::current_dir() {
        roots.extend(ancestor_dirs(&cwd));
    }
    discover_agents_files_from_roots(roots)
}

#[cfg(test)]
pub(crate) fn discover_agents_files_from(cwd: &Path) -> Vec<DiscoveredAgentsFile> {
    discover_agents_files_from_roots(ancestor_dirs(cwd))
}

pub(crate) fn discover_agents_files_from_roots(
    roots: impl IntoIterator<Item = PathBuf>,
) -> Vec<DiscoveredAgentsFile> {
    let mut seen = std::collections::HashSet::new();
    let mut discovered = Vec::new();
    for dir in roots {
        let candidate = dir.join("AGENTS.md");
        let Ok(metadata) = fs::metadata(&candidate) else {
            continue;
        };
        if !metadata.is_file() {
            continue;
        }

        let Ok(content) = fs::read_to_string(&candidate) else {
            continue;
        };
        if content.trim().is_empty() {
            continue;
        }

        let file_path = candidate.canonicalize().unwrap_or(candidate);
        if !seen.insert(file_path.clone()) {
            continue;
        }
        discovered.push(DiscoveredAgentsFile { file_path, content });
    }

    discovered
}

fn ancestor_dirs(cwd: &Path) -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    let mut current = cwd.to_path_buf();
    loop {
        dirs.push(current.clone());
        let Some(parent) = current.parent() else {
            break;
        };
        if parent == current {
            break;
        }
        current = parent.to_path_buf();
    }
    dirs.reverse();
    dirs
}
