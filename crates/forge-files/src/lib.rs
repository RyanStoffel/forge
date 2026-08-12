//! Gitignore-aware file tree for the right sidebar's Files tab. Eagerly
//! scans the whole tree (bounded by `max_depth`) using only the workspace
//! root's `.gitignore`; nested `.gitignore` files and live filesystem
//! watching are fast-follows (see docs/mvp-plan.md).

use std::path::{Path, PathBuf};

use ignore::gitignore::{Gitignore, GitignoreBuilder};

pub struct FileNode {
    pub name: String,
    pub path: PathBuf,
    pub is_dir: bool,
    pub children: Vec<FileNode>,
}

const DEFAULT_MAX_DEPTH: usize = 8;
const ALWAYS_SKIP: &[&str] = &[".git"];

pub fn scan(root: &Path) -> FileNode {
    let mut builder = GitignoreBuilder::new(root);
    builder.add(root.join(".gitignore"));
    let gitignore = builder.build().unwrap_or_else(|_| Gitignore::empty());
    build_node(root, &gitignore, 0)
}

fn build_node(path: &Path, gitignore: &Gitignore, depth: usize) -> FileNode {
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| path.to_string_lossy().to_string());
    let is_dir = path.is_dir();

    let mut children = Vec::new();
    if is_dir && depth < DEFAULT_MAX_DEPTH {
        if let Ok(entries) = std::fs::read_dir(path) {
            let mut entries: Vec<PathBuf> =
                entries.filter_map(|e| e.ok()).map(|e| e.path()).collect();
            entries.sort_by(|a, b| {
                let a_dir = a.is_dir();
                let b_dir = b.is_dir();
                b_dir
                    .cmp(&a_dir)
                    .then_with(|| a.file_name().cmp(&b.file_name()))
            });

            for entry in entries {
                let entry_name = entry
                    .file_name()
                    .map(|n| n.to_string_lossy().to_string())
                    .unwrap_or_default();
                if ALWAYS_SKIP.contains(&entry_name.as_str()) {
                    continue;
                }
                let entry_is_dir = entry.is_dir();
                if gitignore.matched(&entry, entry_is_dir).is_ignore() {
                    continue;
                }
                children.push(build_node(&entry, gitignore, depth + 1));
            }
        }
    }

    FileNode {
        name,
        path: path.to_path_buf(),
        is_dir,
        children,
    }
}
