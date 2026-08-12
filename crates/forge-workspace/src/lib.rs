//! Workspace list + single-level pane splits. A workspace is a project
//! directory holding one or more terminal panes arranged in a single row or
//! column. Nested/mixed split trees (real tmux-style multiplexing) are a
//! fast-follow — see docs/mvp-plan.md build order.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::Result;
use forge_terminal::{OutputNotifier, TerminalPane};

pub struct Pane {
    pub terminal: TerminalPane,
    /// Last output generation the UI has painted, used to skip repaints when
    /// no new PTY output has arrived.
    pub last_seen_generation: u64,
}

impl Pane {
    fn new(terminal: TerminalPane) -> Self {
        Self {
            terminal,
            last_seen_generation: 0,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Layout {
    /// Panes side by side.
    Row,
    /// Panes stacked top to bottom.
    Column,
}

pub struct Workspace {
    /// Directory basename; the fallback label.
    pub name: String,
    /// Set by the user via rename. Once present it wins permanently — an
    /// explicit name should never be overwritten by process activity.
    pub custom_name: Option<String>,
    /// Derived from the focused pane's foreground process, e.g. "claude" or
    /// "nvim". `None` while the pane sits at an idle shell.
    pub process_name: Option<String>,
    /// Root the workspace was opened with; stable and shown in the sidebar.
    pub path: PathBuf,
    /// Current directory of the focused foreground process. Git follows this
    /// so `cd` into/out of repositories updates the info panel naturally.
    pub current_path: PathBuf,
    pub branch: Option<String>,
    pub git: Option<forge_git::Summary>,
    pub panes: Vec<Pane>,
    pub layout: Layout,
    pub focused: usize,
}

impl Workspace {
    pub fn open(
        path: impl Into<PathBuf>,
        rows: u16,
        cols: u16,
        on_output: Option<OutputNotifier>,
    ) -> Result<Self> {
        let path = path.into();
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| path.to_string_lossy().to_string());
        let terminal = TerminalPane::spawn(&path, rows, cols, on_output)?;
        Ok(Self {
            name,
            custom_name: None,
            process_name: None,
            current_path: path.clone(),
            path,
            branch: None,
            git: None,
            panes: vec![Pane::new(terminal)],
            layout: Layout::Row,
            focused: 0,
        })
    }

    /// Label shown in the sidebar.
    ///
    /// Precedence: an explicit rename, then the running process, then the
    /// directory name. So a workspace reads as its project until an agent or
    /// editor starts, then reflects that, unless the user has named it.
    pub fn display_name(&self) -> &str {
        self.custom_name
            .as_deref()
            .or(self.process_name.as_deref())
            .unwrap_or(&self.name)
    }

    /// Compact label for the workspace rail. Unnamed workspaces are path-first;
    /// an active tool adds identity without hiding the directory it belongs to.
    pub fn sidebar_label(&self, max_chars: usize) -> String {
        if let Some(name) = &self.custom_name {
            return truncate_end(name, max_chars);
        }

        let process = self
            .process_name
            .as_deref()
            .map(process_display_name)
            .map(|process| truncate_end(&process, 10));
        let prefix_len = process
            .as_ref()
            .map(|process| process.chars().count() + 3)
            .unwrap_or(0);
        let path = display_path(&self.path, max_chars.saturating_sub(prefix_len));
        match process {
            Some(process) => format!("{process} - {path}"),
            None => path,
        }
    }

    pub fn rename(&mut self, name: impl Into<String>) {
        let name = name.into();
        let trimmed = name.trim();
        // An empty rename clears back to automatic naming rather than leaving
        // a blank row.
        self.custom_name = if trimmed.is_empty() {
            None
        } else {
            Some(trimmed.to_string())
        };
    }

    pub fn is_renamed(&self) -> bool {
        self.custom_name.is_some()
    }

    pub fn refresh_git(&mut self) {
        let summary = forge_git::summary(&self.current_path);
        self.branch = summary.branch.clone();
        self.git = Some(summary);
    }

    pub fn git_path(&self) -> &Path {
        &self.current_path
    }

    pub fn set_current_path(&mut self, path: Option<PathBuf>) {
        if let Some(path) = path.filter(|path| path.is_dir()) {
            self.current_path = path;
        }
    }

    /// Adopt `process` as the dynamic label, ignoring plain shells so an idle
    /// pane keeps showing the project name instead of "zsh".
    pub fn set_process_name(&mut self, process: Option<String>) {
        self.process_name = match process {
            Some(name) if !is_shell(&name) => Some(name),
            _ => None,
        };
    }
}

/// Shells are treated as "nothing running": showing `zsh` for every idle
/// workspace would be noise, not information.
fn truncate_end(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        return text.to_string();
    }
    if max_chars <= 3 {
        return ".".repeat(max_chars);
    }
    format!(
        "{}...",
        text.chars().take(max_chars - 3).collect::<String>()
    )
}

fn process_display_name(name: &str) -> String {
    let base = name.rsplit('/').next().unwrap_or(name).to_ascii_lowercase();
    match base.as_str() {
        "pi" => "π".into(),
        "claude" => "Claude".into(),
        "codex" => "Codex".into(),
        _ => name.rsplit('/').next().unwrap_or(name).to_string(),
    }
}

fn is_shell(name: &str) -> bool {
    const SHELLS: &[&str] = &[
        "sh", "bash", "zsh", "fish", "dash", "ksh", "tcsh", "csh", "nu", "elvish", "xonsh", "login",
    ];
    let base = name.rsplit('/').next().unwrap_or(name);
    let base = base.strip_prefix('-').unwrap_or(base);
    SHELLS.contains(&base)
}

/// Render a path as a compact workspace identity.
///
/// The uninteresting absolute/home prefix is represented by `../`, then the
/// beginning of the useful path is preserved. Overlong labels end in `...`,
/// matching Finder-style filename truncation rather than hiding the parent
/// directories that distinguish neighboring workspaces.
pub fn display_path(path: &Path, max_chars: usize) -> String {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let text = match &home {
        Some(home) if path == home => "~".to_string(),
        Some(home) if path.starts_with(home) => path
            .strip_prefix(home)
            .ok()
            .map(|rest| format!("../{}", rest.to_string_lossy()))
            .unwrap_or_else(|| path.to_string_lossy().to_string()),
        _ if path.is_absolute() => format!(
            "../{}",
            path.to_string_lossy()
                .trim_start_matches(std::path::MAIN_SEPARATOR)
        ),
        _ => path.to_string_lossy().to_string(),
    };

    let len = text.chars().count();
    if len <= max_chars {
        return text;
    }
    if max_chars <= 3 {
        return ".".repeat(max_chars);
    }

    let head: String = text.chars().take(max_chars - 3).collect();
    format!("{head}...")
}

/// Full path shown as workspace metadata. Home is represented by `~`, the
/// directory slash is retained, and only the end may be elided.
pub fn full_display_path(path: &Path, max_chars: usize) -> String {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let mut text = match &home {
        Some(home) if path == home => "~".to_string(),
        Some(home) if path.starts_with(home) => path
            .strip_prefix(home)
            .ok()
            .map(|rest| format!("~/{}", rest.to_string_lossy()))
            .unwrap_or_else(|| path.to_string_lossy().to_string()),
        _ => path.to_string_lossy().to_string(),
    };
    if !text.ends_with('/') {
        text.push('/');
    }
    truncate_end(&text, max_chars)
}

impl Workspace {
    /// Re-read the current git branch. Cheap enough to call on focus /
    /// periodic refresh; real filesystem-watch-driven updates are a
    /// fast-follow.
    pub fn refresh_branch(&mut self) {
        self.branch = git_branch(&self.path);
    }

    /// Split the currently focused pane, spawning a new shell at the
    /// workspace's root and focusing it. All panes in a workspace share one
    /// layout direction in this pass.
    pub fn split(
        &mut self,
        layout: Layout,
        rows: u16,
        cols: u16,
        on_output: Option<OutputNotifier>,
    ) -> Result<()> {
        let terminal = TerminalPane::spawn(&self.path, rows, cols, on_output)?;
        self.panes.push(Pane::new(terminal));
        self.layout = layout;
        self.focused = self.panes.len() - 1;
        Ok(())
    }

    pub fn close_focused(&mut self) {
        if self.panes.len() <= 1 {
            return;
        }
        self.panes.remove(self.focused);
        if self.focused >= self.panes.len() {
            self.focused = self.panes.len() - 1;
        }
    }

    pub fn focus_next(&mut self) {
        if !self.panes.is_empty() {
            self.focused = (self.focused + 1) % self.panes.len();
        }
    }

    pub fn focus_prev(&mut self) {
        if !self.panes.is_empty() {
            self.focused = (self.focused + self.panes.len() - 1) % self.panes.len();
        }
    }

    pub fn focused_pane(&self) -> Option<&Pane> {
        self.panes.get(self.focused)
    }

    pub fn focused_pane_mut(&mut self) -> Option<&mut Pane> {
        self.panes.get_mut(self.focused)
    }
}

fn git_branch(path: &Path) -> Option<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(path)
        .arg("rev-parse")
        .arg("--abbrev-ref")
        .arg("HEAD")
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let branch = String::from_utf8(output.stdout).ok()?.trim().to_string();
    if branch.is_empty() || branch == "HEAD" {
        None
    } else {
        Some(branch)
    }
}

pub struct WorkspaceManager {
    pub workspaces: Vec<Workspace>,
    pub active: usize,
}

impl WorkspaceManager {
    pub fn new() -> Self {
        Self {
            workspaces: Vec::new(),
            active: 0,
        }
    }

    pub fn add(
        &mut self,
        path: impl Into<PathBuf>,
        rows: u16,
        cols: u16,
        on_output: Option<OutputNotifier>,
    ) -> Result<()> {
        let workspace = Workspace::open(path, rows, cols, on_output)?;
        self.workspaces.push(workspace);
        self.active = self.workspaces.len() - 1;
        Ok(())
    }

    pub fn push(&mut self, workspace: Workspace) {
        self.workspaces.push(workspace);
        self.active = self.workspaces.len() - 1;
    }

    pub fn select(&mut self, index: usize) {
        if index < self.workspaces.len() {
            self.active = index;
        }
    }

    pub fn active_workspace(&self) -> Option<&Workspace> {
        self.workspaces.get(self.active)
    }

    pub fn active_workspace_mut(&mut self) -> Option<&mut Workspace> {
        self.workspaces.get_mut(self.active)
    }
}

impl Default for WorkspaceManager {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn home_prefix_is_compacted() {
        let home = std::env::var("HOME").unwrap();
        let p = PathBuf::from(&home).join("Developer/personal/better-instagram");
        assert_eq!(
            display_path(&p, 80),
            "../Developer/personal/better-instagram"
        );
    }

    #[test]
    fn bare_home_is_just_tilde() {
        let home = PathBuf::from(std::env::var("HOME").unwrap());
        assert_eq!(display_path(&home, 80), "~");
    }

    #[test]
    fn absolute_prefix_is_compacted() {
        assert_eq!(
            display_path(Path::new("/usr/local/bin"), 80),
            "../usr/local/bin"
        );
    }

    #[test]
    fn long_paths_keep_the_useful_prefix_and_end_in_dots() {
        let home = std::env::var("HOME").unwrap();
        let p = PathBuf::from(&home).join("Developer/personal/better-instagram");
        assert_eq!(display_path(&p, 29), "../Developer/personal/bett...");
    }

    #[test]
    fn exact_length_path_is_not_truncated() {
        let text = display_path(Path::new("/abc"), 6);
        assert_eq!(text, "../abc");
    }

    #[test]
    fn full_path_keeps_home_prefix_and_directory_slash() {
        let home = std::env::var("HOME").unwrap();
        let path = PathBuf::from(home).join("Developer/personal/better-instagram");
        assert_eq!(
            full_display_path(&path, 80),
            "~/Developer/personal/better-instagram/"
        );
    }

    #[test]
    fn full_path_truncates_only_at_the_end() {
        let home = std::env::var("HOME").unwrap();
        let path = PathBuf::from(home).join("Developer/personal/better-instagram");
        let label = full_display_path(&path, 29);
        assert_eq!(label, "~/Developer/personal/bette...");
        assert_eq!(label.chars().count(), 29);
    }

    fn workspace_fixture() -> Workspace {
        Workspace {
            name: "better-instagram".into(),
            custom_name: None,
            process_name: None,
            path: PathBuf::from("/tmp/better-instagram"),
            current_path: PathBuf::from("/tmp/better-instagram"),
            branch: None,
            git: None,
            panes: Vec::new(),
            layout: Layout::Row,
            focused: 0,
        }
    }

    #[test]
    fn git_path_follows_a_valid_terminal_working_directory() {
        let mut ws = workspace_fixture();
        assert_eq!(ws.git_path(), Path::new("/tmp/better-instagram"));
        ws.set_current_path(Some(PathBuf::from("/tmp")));
        assert_eq!(ws.git_path(), Path::new("/tmp"));
    }

    #[test]
    fn missing_terminal_working_directory_is_ignored() {
        let mut ws = workspace_fixture();
        ws.set_current_path(Some(PathBuf::from("/definitely/not/a/real/path")));
        assert_eq!(ws.git_path(), Path::new("/tmp/better-instagram"));
    }

    #[test]
    fn name_falls_back_to_directory() {
        assert_eq!(workspace_fixture().display_name(), "better-instagram");
    }

    #[test]
    fn running_process_overrides_directory_name() {
        let mut ws = workspace_fixture();
        ws.set_process_name(Some("claude".into()));
        assert_eq!(ws.display_name(), "claude");
    }

    #[test]
    fn sidebar_label_is_path_first_and_adds_known_process_identity() {
        let mut ws = workspace_fixture();
        assert_eq!(ws.sidebar_label(80), "../tmp/better-instagram");
        ws.set_process_name(Some("pi".into()));
        assert_eq!(ws.sidebar_label(80), "π - ../tmp/better-instagram");
    }

    #[test]
    fn long_process_names_cannot_consume_the_path_label() {
        let mut ws = workspace_fixture();
        ws.set_process_name(Some("extremely-long-agent-process".into()));
        let label = ws.sidebar_label(29);
        assert_eq!(label.chars().count(), 29);
        assert!(label.starts_with("extreme... - ../"), "{label}");
        assert_ne!(label, "...");
    }

    #[test]
    fn explicit_name_replaces_the_automatic_sidebar_label() {
        let mut ws = workspace_fixture();
        ws.set_process_name(Some("pi".into()));
        ws.rename("Instagram");
        assert_eq!(ws.sidebar_label(80), "Instagram");
    }

    #[test]
    fn idle_shells_do_not_become_the_name() {
        let mut ws = workspace_fixture();
        for shell in ["zsh", "bash", "-zsh", "/bin/fish"] {
            ws.set_process_name(Some(shell.into()));
            assert_eq!(
                ws.display_name(),
                "better-instagram",
                "{shell} should not label the workspace"
            );
        }
    }

    #[test]
    fn explicit_rename_outranks_running_process() {
        let mut ws = workspace_fixture();
        ws.rename("Instagram");
        ws.set_process_name(Some("nvim".into()));
        assert_eq!(ws.display_name(), "Instagram");
        assert!(ws.is_renamed());
    }

    #[test]
    fn blank_rename_reverts_to_automatic_naming() {
        let mut ws = workspace_fixture();
        ws.rename("Temp");
        ws.rename("   ");
        assert!(!ws.is_renamed());
        assert_eq!(ws.display_name(), "better-instagram");
    }

    #[test]
    fn rename_trims_surrounding_whitespace() {
        let mut ws = workspace_fixture();
        ws.rename("  Instagram  ");
        assert_eq!(ws.display_name(), "Instagram");
    }
}
