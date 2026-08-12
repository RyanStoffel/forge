//! Git working-tree status for the right sidebar's Git tab.
//!
//! Shells out to `git status --porcelain=v2 --branch` rather than linking
//! libgit2. The format is explicitly designed to be machine-parsed and stable,
//! one subprocess yields branch, ahead/behind, and per-file staged/unstaged
//! state together, and it avoids a heavy C build plus libgit2 version skew.
//! Swapping in `git2` later only requires reimplementing `status()`.

use std::collections::{HashMap, HashSet};
use std::fmt;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Change {
    Added,
    Modified,
    Deleted,
    Renamed,
    Copied,
    Untracked,
    Conflicted,
}

impl Change {
    fn from_code(c: char) -> Option<Self> {
        match c {
            'A' => Some(Self::Added),
            'M' => Some(Self::Modified),
            'D' => Some(Self::Deleted),
            'R' => Some(Self::Renamed),
            'C' => Some(Self::Copied),
            _ => None,
        }
    }

    fn from_name_status(c: char) -> Option<Self> {
        match c {
            'A' => Some(Self::Added),
            'M' | 'T' => Some(Self::Modified),
            'D' => Some(Self::Deleted),
            'R' => Some(Self::Renamed),
            'C' => Some(Self::Copied),
            'U' => Some(Self::Conflicted),
            _ => None,
        }
    }

    /// Single-letter code shown in the UI's fixed-width status column.
    pub fn code(self) -> &'static str {
        match self {
            Self::Added => "A",
            Self::Modified => "M",
            Self::Deleted => "D",
            Self::Renamed => "R",
            Self::Copied => "C",
            Self::Untracked => "?",
            Self::Conflicted => "U",
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DiffStat {
    pub additions: u32,
    pub deletions: u32,
}

#[derive(Clone, Debug)]
pub struct Entry {
    /// Repo-relative path.
    pub path: String,
    /// Previous repo-relative path for a rename or copy.
    pub previous_path: Option<String>,
    /// Change staged in the index, if any.
    pub staged: Option<Change>,
    /// Diffstat for the staged version of this path.
    pub staged_stat: Option<DiffStat>,
    /// Change present only in the working tree, if any.
    pub unstaged: Option<Change>,
    /// Diffstat for the unstaged version of this path.
    pub unstaged_stat: Option<DiffStat>,
}

impl Entry {
    pub fn is_conflicted(&self) -> bool {
        self.staged == Some(Change::Conflicted)
    }
}

#[derive(Clone, Debug, Default)]
pub struct Status {
    /// Canonical repository root. Status paths are relative to this directory.
    pub root: Option<PathBuf>,
    pub branch: Option<String>,
    pub upstream: Option<String>,
    pub branches: Vec<String>,
    pub ahead: u32,
    pub behind: u32,
    pub entries: Vec<Entry>,
    /// False when the path isn't inside a git work tree.
    pub is_repo: bool,
}

impl Status {
    pub fn staged_count(&self) -> usize {
        self.entries.iter().filter(|e| e.staged.is_some()).count()
    }

    pub fn unstaged_count(&self) -> usize {
        self.entries.iter().filter(|e| e.unstaged.is_some()).count()
    }

    pub fn is_clean(&self) -> bool {
        self.entries.is_empty()
    }
}

/// Compact per-workspace git state for the sidebar.
///
/// Deliberately drops the per-file entry list that [`Status`] carries: the
/// sidebar shows one line per workspace, and holding every changed path for
/// every open workspace is wasted memory.
#[derive(Clone, Debug, Default)]
pub struct Summary {
    pub branch: Option<String>,
    pub ahead: u32,
    pub behind: u32,
    /// Number of paths with staged and/or worktree changes.
    pub changed: usize,
    pub is_repo: bool,
}

impl Summary {
    pub fn is_dirty(&self) -> bool {
        self.changed > 0
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GitError {
    pub message: String,
}

impl fmt::Display for GitError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for GitError {}

pub fn summary(repo: &Path) -> Summary {
    let status = status(repo);
    Summary {
        branch: status.branch,
        ahead: status.ahead,
        behind: status.behind,
        changed: status.entries.len(),
        is_repo: status.is_repo,
    }
}

/// Vertical extent of a lane's line within a commit row, relative to the
/// row's own commit dot. The row height itself is a rendering concern
/// (`theme::graph::ROW`); this only records which half is drawn.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LineStyle {
    /// The lane has commits both above and below this row: draw the full
    /// row height.
    Full,
    /// This row is the newest commit of the lane, with nothing feeding it
    /// from above: draw only the bottom half.
    Newest,
    /// This row is the oldest commit of the lane, with nothing continuing
    /// below: draw only the top half.
    Oldest,
    /// This row is the lane's only commit: no line at all, just the dot.
    Isolated,
}

/// A non-owning lane segment crossing a commit row. Joined lanes can need
/// only one half of the row because the join corner supplies the other half.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LaneSegment {
    pub lane: usize,
    pub style: LineStyle,
}

/// How a lane joins another lane at a specific commit row. Both kinds are a
/// single positioned box with two borders and a rounded corner; see
/// `theme::graph` for the shared geometry constants.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum JoinKind {
    /// A merge commit's non-first parent enters this row from `other`,
    /// drawn in the row's bottom half, colored in `other`'s lane color.
    Merge,
    /// Two lanes converge on a shared ancestor at this row; `other` exits
    /// upward out of `lane`, drawn in the row's top half, colored in
    /// `other`'s lane color.
    BranchOut,
}

/// A join box drawn on a commit row, connecting `lane` (this commit's own
/// lane, where the dot sits) to `other`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Join {
    pub kind: JoinKind,
    pub lane: usize,
    pub other: usize,
}

/// Lanes beyond this fold into the last one and the gutter stops growing.
/// Mirrored by `theme::graph::MAX_LANES` on the rendering side.
const MAX_LANES: usize = 4;

/// One row in the repository history graph.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Commit {
    pub id: String,
    pub short_id: String,
    pub parents: Vec<String>,
    /// ASCII graph lane emitted by `git log --graph` (`*`, `| *`, ...).
    /// Superseded by `lane` for rendering; kept only because it's cheap to
    /// carry and useful for debugging `git log` output directly.
    pub graph: String,
    /// Connector-only graph rows emitted between commits (`|\\`, `|/`, ...).
    pub connectors: Vec<String>,
    pub refs: Vec<String>,
    /// True when a ref decoration on this commit is the checked-out `HEAD`.
    pub is_head: bool,
    /// True when this commit is reachable from the local main branch.
    pub merged_to_main: bool,
    /// Graph column this commit's dot sits in, computed by `assign_lanes`.
    pub lane: usize,
    /// How far this commit's own lane line extends within its row.
    pub line_style: LineStyle,
    /// Other active lanes crossing this commit row.
    pub pass_through: Vec<LaneSegment>,
    /// Merge/branch-out joins drawn on this row, computed by `assign_lanes`.
    pub joins: Vec<Join>,
    pub subject: String,
    pub author: String,
    pub relative_date: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileChange {
    pub path: String,
    pub previous_path: Option<String>,
    pub change: Change,
}

/// Recent commits across local and remote refs, in graph/date order.
pub fn history(repo: &Path, limit: usize) -> Vec<Commit> {
    let output = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args([
            "log",
            "--graph",
            "--date-order",
            "--all",
            "--decorate=short",
            "--date=relative",
            "--pretty=format:%x1e%H%x1f%h%x1f%P%x1f%D%x1f%s%x1f%an%x1f%ar",
            &format!("-n{}", limit.max(1)),
        ])
        .output();

    let Ok(output) = output else {
        return Vec::new();
    };
    if !output.status.success() {
        return Vec::new();
    }
    let Ok(text) = String::from_utf8(output.stdout) else {
        return Vec::new();
    };
    let mut commits = parse_history(&text);
    assign_lanes(&mut commits);
    let mainline = mainline_commits(repo);
    for commit in &mut commits {
        commit.merged_to_main = mainline.contains(&commit.id);
    }
    commits
}

fn parse_history(text: &str) -> Vec<Commit> {
    let mut commits: Vec<Commit> = Vec::new();
    for line in text.lines() {
        let Some((graph, fields)) = line.split_once('\u{1e}') else {
            let connector = line.trim_end();
            if !connector.is_empty() {
                if let Some(commit) = commits.last_mut() {
                    commit.connectors.push(connector.to_string());
                }
            }
            continue;
        };
        let mut fields = fields.split('\u{1f}');
        let Some(id) = fields.next() else { continue };
        let Some(short_id) = fields.next() else {
            continue;
        };
        let Some(parents) = fields.next() else {
            continue;
        };
        let Some(refs) = fields.next() else { continue };
        let Some(subject) = fields.next() else {
            continue;
        };
        let Some(author) = fields.next() else {
            continue;
        };
        let Some(relative_date) = fields.next() else {
            continue;
        };
        let refs: Vec<String> = refs
            .split(", ")
            .filter(|label| !label.is_empty())
            .map(str::to_string)
            .collect();
        let is_head = refs.iter().any(|label| label.starts_with("HEAD"));
        commits.push(Commit {
            id: id.to_string(),
            short_id: short_id.to_string(),
            parents: parents.split_whitespace().map(str::to_string).collect(),
            graph: graph.trim_end().to_string(),
            connectors: Vec::new(),
            refs,
            is_head,
            merged_to_main: false,
            lane: 0,
            line_style: LineStyle::Isolated,
            pass_through: Vec::new(),
            joins: Vec::new(),
            subject: subject.to_string(),
            author: author.to_string(),
            relative_date: relative_date.to_string(),
        });
    }
    commits
}

/// Assigns logical graph columns newest-to-oldest, then folds those columns
/// into the four display lanes stored on each commit.
///
/// Logical columns are intentionally unbounded. Multiple parents may share
/// display lane 3, but they must retain separate pending commit ids or one
/// branch is lost and later reappears as a disconnected tip.
fn assign_lanes(commits: &mut [Commit]) {
    // Reserve logical lane zero for the checked-out branch even when a newer
    // commit from another `--all` ref appears first.
    let head = commits
        .iter()
        .find(|commit| commit.is_head)
        .map(|commit| commit.id.clone());
    let mut columns: Vec<Option<String>> = head.clone().into_iter().map(Some).collect();
    let mut head_pending = head.is_some();

    for commit in commits.iter_mut() {
        let before = active_lanes(&columns, head_pending);
        let matches = columns
            .iter()
            .enumerate()
            .filter_map(|(lane, target)| {
                (target.as_deref() == Some(commit.id.as_str())).then_some(lane)
            })
            .collect::<Vec<_>>();
        let entered = !matches.is_empty() && !(head_pending && commit.is_head);
        let mut logical_joins = Vec::new();

        let logical_lane = if let Some((&lane, rest)) = matches.split_first() {
            for &other in rest {
                logical_joins.push(Join {
                    kind: JoinKind::BranchOut,
                    lane,
                    other,
                });
                columns[other] = None;
            }
            lane
        } else {
            alloc_lane(&mut columns)
        };
        commit.lane = display_lane(logical_lane);
        if commit.is_head {
            head_pending = false;
        }

        let terminal = commit.parents.is_empty();
        commit.line_style = match (entered, terminal) {
            (false, false) => LineStyle::Newest,
            (false, true) => LineStyle::Isolated,
            (true, false) => LineStyle::Full,
            (true, true) => LineStyle::Oldest,
        };

        if terminal {
            columns[logical_lane] = None;
        } else {
            columns[logical_lane] = Some(commit.parents[0].clone());
            for extra in &commit.parents[1..] {
                let other = columns
                    .iter()
                    .position(|target| target.as_deref() == Some(extra.as_str()))
                    .unwrap_or_else(|| {
                        let other = alloc_lane(&mut columns);
                        columns[other] = Some(extra.clone());
                        other
                    });
                if other != logical_lane {
                    logical_joins.push(Join {
                        kind: JoinKind::Merge,
                        lane: logical_lane,
                        other,
                    });
                }
            }
        }

        let after = active_lanes(&columns, head_pending);
        let mut crossing = before.union(&after).copied().collect::<Vec<_>>();
        crossing.sort_unstable();
        let mut display_segments = HashMap::new();
        for other in crossing {
            if other == logical_lane {
                continue;
            }
            let active_before = before.contains(&other);
            let active_after = after.contains(&other);
            let merge = logical_joins
                .iter()
                .any(|join| join.other == other && join.kind == JoinKind::Merge);
            let branch_out = logical_joins
                .iter()
                .any(|join| join.other == other && join.kind == JoinKind::BranchOut);
            let style = match (merge, branch_out, active_before, active_after) {
                // Keep an already-active merge lane full-height behind its
                // rounded lower corner so the midpoint cannot open a seam.
                (true, false, true, true) => LineStyle::Full,
                (true, false, true, false) => LineStyle::Oldest,
                (true, false, false, _) => continue,
                // The corner supplies the upper half of a branch-out.
                (false, true, _, true) => LineStyle::Newest,
                (false, true, _, false) => continue,
                (true, true, _, _) => continue,
                (false, false, true, true) => LineStyle::Full,
                (false, false, true, false) => LineStyle::Oldest,
                (false, false, false, true) => LineStyle::Newest,
                (false, false, false, false) => continue,
            };
            let lane = display_lane(other);
            if lane == commit.lane {
                continue;
            }
            display_segments
                .entry(lane)
                .and_modify(|current| *current = combine_line_styles(*current, style))
                .or_insert(style);
        }
        commit.pass_through = display_segments
            .into_iter()
            .map(|(lane, style)| LaneSegment { lane, style })
            .collect();
        commit
            .pass_through
            .sort_unstable_by_key(|segment| segment.lane);

        commit.joins.clear();
        for join in logical_joins {
            let join = Join {
                kind: join.kind,
                lane: display_lane(join.lane),
                other: display_lane(join.other),
            };
            if join.lane != join.other && !commit.joins.contains(&join) {
                commit.joins.push(join);
            }
        }
        // Shared-row joins overlap from the commit lane outward. Paint the
        // widest first so shorter inner joins remain visible on top.
        commit
            .joins
            .sort_unstable_by_key(|join| std::cmp::Reverse(join.lane.abs_diff(join.other)));
    }
}

fn active_lanes(columns: &[Option<String>], head_pending: bool) -> HashSet<usize> {
    columns
        .iter()
        .enumerate()
        .filter_map(|(lane, target)| {
            (target.is_some() && !(head_pending && lane == 0)).then_some(lane)
        })
        .collect()
}

fn display_lane(logical_lane: usize) -> usize {
    logical_lane.min(MAX_LANES - 1)
}

fn combine_line_styles(a: LineStyle, b: LineStyle) -> LineStyle {
    if a == b {
        a
    } else if a == LineStyle::Isolated {
        b
    } else if b == LineStyle::Isolated {
        a
    } else {
        LineStyle::Full
    }
}

/// Reuses the lowest free logical column, growing without a topology cap.
/// `display_lane` folds columns into the final visible lane separately.
fn alloc_lane(columns: &mut Vec<Option<String>>) -> usize {
    if let Some(free) = columns.iter().position(Option::is_none) {
        return free;
    }
    columns.push(None);
    columns.len() - 1
}

fn mainline_commits(repo: &Path) -> HashSet<String> {
    let main_ref = [
        "refs/heads/main",
        "refs/remotes/origin/main",
        "refs/heads/master",
        "refs/remotes/origin/master",
        "refs/heads/trunk",
    ]
    .into_iter()
    .find(|candidate| {
        Command::new("git")
            .arg("-C")
            .arg(repo)
            .args(["show-ref", "--verify", "--quiet", candidate])
            .status()
            .is_ok_and(|status| status.success())
    });
    let Some(main_ref) = main_ref else {
        return HashSet::new();
    };
    let Ok(output) = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["rev-list", "--max-count=10000", main_ref])
        .output()
    else {
        return HashSet::new();
    };
    if !output.status.success() {
        return HashSet::new();
    }
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(str::to_string)
        .collect()
}

pub fn commit_changes(repo: &Path, commit: &Commit) -> Vec<FileChange> {
    let output = if let Some(parent) = commit.parents.first() {
        Command::new("git")
            .arg("-C")
            .arg(repo)
            .args(["diff", "--name-status", "-z", "-M", parent, &commit.id])
            .output()
    } else {
        Command::new("git")
            .arg("-C")
            .arg(repo)
            .args([
                "diff-tree",
                "--root",
                "--no-commit-id",
                "--name-status",
                "-r",
                "-z",
                "-M",
                &commit.id,
            ])
            .output()
    };
    output
        .ok()
        .filter(|output| output.status.success())
        .map(|output| parse_name_status(&output.stdout))
        .unwrap_or_default()
}

pub fn working_diff(repo: &Path, path: &str, untracked: bool) -> String {
    let output = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["diff", "--no-ext-diff", "--no-color", "--", path])
        .output();
    let text = output
        .ok()
        .filter(|output| output.status.success())
        .map(|output| String::from_utf8_lossy(&output.stdout).into_owned())
        .unwrap_or_default();
    if !text.is_empty() || !untracked {
        return text;
    }

    let absolute = repository_root(repo)
        .unwrap_or_else(|| repo.to_path_buf())
        .join(path);
    std::fs::read_to_string(absolute)
        .map(|contents| {
            contents
                .lines()
                .map(|line| format!("+{line}"))
                .collect::<Vec<_>>()
                .join("\n")
        })
        .unwrap_or_default()
}

pub fn staged_diff(repo: &Path, path: &str) -> String {
    Command::new("git")
        .arg("-C")
        .arg(repo)
        .args([
            "diff",
            "--cached",
            "--no-ext-diff",
            "--no-color",
            "--",
            path,
        ])
        .output()
        .ok()
        .filter(|output| output.status.success())
        .map(|output| String::from_utf8_lossy(&output.stdout).into_owned())
        .unwrap_or_default()
}

pub fn commit_diff(repo: &Path, commit: &Commit, path: &str) -> String {
    let mut command = Command::new("git");
    command.arg("-C").arg(repo);
    if let Some(parent) = commit.parents.first() {
        command.args([
            "diff",
            "--no-ext-diff",
            "--no-color",
            "-M",
            parent,
            &commit.id,
            "--",
            path,
        ]);
    } else {
        command.args([
            "show",
            "--format=",
            "--no-ext-diff",
            "--no-color",
            "--root",
            &commit.id,
            "--",
            path,
        ]);
    }
    let output = command.output();
    output
        .ok()
        .filter(|output| output.status.success())
        .map(|output| String::from_utf8_lossy(&output.stdout).into_owned())
        .unwrap_or_default()
}

pub fn repository_root(repo: &Path) -> Option<PathBuf> {
    Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["rev-parse", "--show-toplevel"])
        .output()
        .ok()
        .filter(|output| output.status.success())
        .and_then(|output| String::from_utf8(output.stdout).ok())
        .map(|path| PathBuf::from(path.trim()))
}

fn run_git(repo: &Path, args: &[&str]) -> Result<(), GitError> {
    let output = Command::new("git")
        .arg("--literal-pathspecs")
        .arg("-C")
        .arg(repo)
        .args(args)
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()
        .map_err(|error| GitError {
            message: format!("Unable to run git: {error}"),
        })?;
    if output.status.success() {
        return Ok(());
    }
    let diagnostic = String::from_utf8_lossy(&output.stderr).trim().to_string();
    Err(GitError {
        message: if diagnostic.is_empty() {
            format!("git exited with {}", output.status)
        } else {
            diagnostic
        },
    })
}

pub fn initialize(repo: &Path) -> Result<(), GitError> {
    run_git(repo, &["init"])
}

pub fn local_branches(repo: &Path) -> Vec<String> {
    Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["for-each-ref", "--format=%(refname:short)", "refs/heads"])
        .output()
        .ok()
        .filter(|output| output.status.success())
        .map(|output| {
            String::from_utf8_lossy(&output.stdout)
                .lines()
                .filter(|branch| !branch.is_empty())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

pub fn switch_branch(repo: &Path, branch: &str) -> Result<(), GitError> {
    run_git(repo, &["switch", branch])
}

pub fn fetch(repo: &Path) -> Result<(), GitError> {
    run_git(repo, &["fetch", "--all", "--prune"])
}

pub fn pull_fast_forward(repo: &Path) -> Result<(), GitError> {
    run_git(repo, &["pull", "--ff-only"])
}

pub fn push(repo: &Path) -> Result<(), GitError> {
    run_git(repo, &["push"])
}

pub fn stash_all(repo: &Path) -> Result<(), GitError> {
    run_git(repo, &["stash", "push", "--include-untracked"])
}

pub fn stash_pop(repo: &Path) -> Result<(), GitError> {
    run_git(repo, &["stash", "pop"])
}

fn move_to_trash(path: &Path) -> Result<(), GitError> {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .ok_or_else(|| GitError {
            message: "Unable to locate the Trash folder".into(),
        })?;
    let trash = home.join(".Trash");
    std::fs::create_dir_all(&trash).map_err(|error| GitError {
        message: format!("Unable to open the Trash folder: {error}"),
    })?;
    let name = path.file_name().ok_or_else(|| GitError {
        message: "Unable to determine the changed file name".into(),
    })?;
    let mut destination = trash.join(name);
    if destination.exists() {
        let suffix = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let name = name.to_string_lossy();
        destination = trash.join(format!("{name}-{suffix}"));
    }
    std::fs::rename(path, &destination).map_err(|error| GitError {
        message: format!("Unable to move {} to Trash: {error}", path.display()),
    })
}

pub fn discard_worktree(
    repo: &Path,
    path: &str,
    previous_path: Option<&str>,
    untracked: bool,
) -> Result<(), GitError> {
    let root = repository_root(repo).ok_or_else(|| GitError {
        message: "Repository changed; refresh and try again".into(),
    })?;
    if untracked || previous_path.is_some() {
        let absolute = root.join(path);
        if absolute.exists() {
            move_to_trash(&absolute)?;
        }
    }
    if let Some(previous_path) = previous_path {
        run_git(repo, &["restore", "--worktree", "--", previous_path])
    } else if untracked {
        Ok(())
    } else {
        run_git(repo, &["restore", "--worktree", "--", path])
    }
}

pub fn stage(repo: &Path, path: &str) -> Result<(), GitError> {
    stage_entry(repo, path, None)
}

pub fn stage_entry(repo: &Path, path: &str, previous_path: Option<&str>) -> Result<(), GitError> {
    let mut args = vec!["add", "--all", "--", path];
    if let Some(previous_path) = previous_path {
        args.push(previous_path);
    }
    run_git(repo, &args)
}

pub fn stage_all(repo: &Path) -> Result<(), GitError> {
    run_git(repo, &["add", "--all", "--", "."])
}

fn has_head(repo: &Path) -> bool {
    Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["rev-parse", "--verify", "--quiet", "HEAD"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

pub fn unstage(repo: &Path, path: &str) -> Result<(), GitError> {
    unstage_entry(repo, path, None)
}

pub fn unstage_entry(repo: &Path, path: &str, previous_path: Option<&str>) -> Result<(), GitError> {
    let mut paths = vec![path];
    if let Some(previous_path) = previous_path {
        paths.push(previous_path);
    }
    if has_head(repo) {
        let mut args = vec!["restore", "--staged", "--"];
        args.extend(paths);
        run_git(repo, &args)
    } else {
        let mut args = vec!["rm", "--cached", "--force", "--ignore-unmatch", "-r", "--"];
        args.extend(paths);
        run_git(repo, &args)
    }
}

pub fn unstage_all(repo: &Path) -> Result<(), GitError> {
    if has_head(repo) {
        run_git(repo, &["restore", "--staged", "--", "."])
    } else {
        run_git(repo, &["read-tree", "--empty"])
    }
}

pub fn commit_staged(repo: &Path, message: &str) -> Result<(), GitError> {
    if message.trim().is_empty() {
        return Err(GitError {
            message: "Commit message cannot be empty".into(),
        });
    }
    let mut child = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["commit", "--file=-", "--cleanup=strip"])
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| GitError {
            message: format!("Unable to run git: {error}"),
        })?;
    child
        .stdin
        .take()
        .expect("piped stdin")
        .write_all(message.as_bytes())
        .map_err(|error| GitError {
            message: format!("Unable to send commit message to git: {error}"),
        })?;
    let output = child.wait_with_output().map_err(|error| GitError {
        message: format!("Unable to wait for git: {error}"),
    })?;
    if output.status.success() {
        Ok(())
    } else {
        let diagnostic = String::from_utf8_lossy(&output.stderr).trim().to_string();
        Err(GitError {
            message: if diagnostic.is_empty() {
                format!("git exited with {}", output.status)
            } else {
                diagnostic
            },
        })
    }
}

fn parse_name_status(bytes: &[u8]) -> Vec<FileChange> {
    let fields: Vec<&str> = bytes
        .split(|byte| *byte == 0)
        .filter(|field| !field.is_empty())
        .filter_map(|field| std::str::from_utf8(field).ok())
        .collect();
    let mut changes = Vec::new();
    let mut index = 0;
    while index < fields.len() {
        let status = fields[index];
        index += 1;
        let Some(change) = status.chars().next().and_then(Change::from_name_status) else {
            continue;
        };
        let renamed = matches!(change, Change::Renamed | Change::Copied);
        if renamed {
            let (Some(previous), Some(path)) = (fields.get(index), fields.get(index + 1)) else {
                break;
            };
            changes.push(FileChange {
                path: (*path).to_string(),
                previous_path: Some((*previous).to_string()),
                change,
            });
            index += 2;
        } else if let Some(path) = fields.get(index) {
            changes.push(FileChange {
                path: (*path).to_string(),
                previous_path: None,
                change,
            });
            index += 1;
        }
    }
    changes
}

pub fn status(repo: &Path) -> Status {
    let output = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args([
            "status",
            "--porcelain=v2",
            "--branch",
            "-z",
            "--untracked-files=normal",
        ])
        .output();

    let Ok(output) = output else {
        return Status::default();
    };
    if !output.status.success() {
        return Status::default();
    }
    let mut status = parse_z(&output.stdout);
    status.root = repository_root(repo);
    if let Some(root) = status.root.as_deref() {
        status.branches = local_branches(root);
        let staged_stats = diffstats(root, true);
        let unstaged_stats = diffstats(root, false);
        for entry in &mut status.entries {
            entry.staged_stat = staged_stats.get(&entry.path).copied();
            entry.unstaged_stat = unstaged_stats.get(&entry.path).copied();
        }
    }
    status
}

fn diffstats(repo: &Path, staged: bool) -> HashMap<String, DiffStat> {
    let mut command = Command::new("git");
    command.arg("-C").arg(repo).arg("diff");
    if staged {
        command.arg("--cached");
    }
    let Ok(output) = command.args(["--numstat", "-z"]).output() else {
        return HashMap::new();
    };
    if !output.status.success() {
        return HashMap::new();
    }
    parse_numstat(&output.stdout)
}

fn parse_numstat(bytes: &[u8]) -> HashMap<String, DiffStat> {
    let mut fields = bytes.split(|byte| *byte == 0);
    let mut stats = HashMap::new();
    while let Some(record) = fields.next() {
        if record.is_empty() {
            continue;
        }
        let mut columns = record.splitn(3, |byte| *byte == b'\t');
        let additions = columns
            .next()
            .and_then(|value| std::str::from_utf8(value).ok())
            .and_then(|value| value.parse().ok());
        let deletions = columns
            .next()
            .and_then(|value| std::str::from_utf8(value).ok())
            .and_then(|value| value.parse().ok());
        let Some(path) = columns.next() else { continue };
        let path = if path.is_empty() {
            // With `-z`, rename records put old and new paths in the next two
            // NUL-delimited fields. The new path is the status row's key.
            let _previous = fields.next();
            fields.next().unwrap_or_default()
        } else {
            path
        };
        let (Some(additions), Some(deletions)) = (additions, deletions) else {
            continue;
        };
        stats.insert(
            String::from_utf8_lossy(path).into_owned(),
            DiffStat {
                additions,
                deletions,
            },
        );
    }
    stats
}

#[cfg(test)]
fn parse(text: &str) -> Status {
    let mut status = Status {
        is_repo: true,
        ..Default::default()
    };

    for line in text.lines() {
        if !parse_status_header(&mut status, line) {
            if let Some(entry) = parse_entry(line) {
                status.entries.push(entry);
            }
        }
    }

    status.entries.sort_by(|a, b| a.path.cmp(&b.path));
    status
}

fn parse_z(bytes: &[u8]) -> Status {
    let mut status = Status {
        is_repo: true,
        ..Default::default()
    };
    let mut records = bytes
        .split(|byte| *byte == 0)
        .filter(|record| !record.is_empty());
    while let Some(record) = records.next() {
        let record = String::from_utf8_lossy(record);
        if parse_status_header(&mut status, &record) {
            continue;
        }
        if record.starts_with("2 ") {
            let mut entry = parse_entry(&record);
            if let (Some(entry), Some(previous)) = (entry.as_mut(), records.next()) {
                entry.previous_path = Some(String::from_utf8_lossy(previous).into_owned());
            }
            if let Some(entry) = entry {
                status.entries.push(entry);
            }
        } else if let Some(entry) = parse_entry(&record) {
            status.entries.push(entry);
        }
    }
    status.entries.sort_by(|a, b| a.path.cmp(&b.path));
    status
}

fn parse_status_header(status: &mut Status, line: &str) -> bool {
    if let Some(rest) = line.strip_prefix("# branch.head ") {
        if rest != "(detached)" {
            status.branch = Some(rest.to_string());
        }
    } else if let Some(rest) = line.strip_prefix("# branch.upstream ") {
        status.upstream = Some(rest.to_string());
    } else if let Some(rest) = line.strip_prefix("# branch.ab ") {
        // Format: "+<ahead> -<behind>"
        for token in rest.split_whitespace() {
            if let Some(n) = token.strip_prefix('+') {
                status.ahead = n.parse().unwrap_or(0);
            } else if let Some(n) = token.strip_prefix('-') {
                status.behind = n.parse().unwrap_or(0);
            }
        }
    } else {
        return false;
    }
    true
}

fn parse_entry(line: &str) -> Option<Entry> {
    let mut parts = line.split(' ');
    match parts.next()? {
        // Ordinary change: `1 <XY> <sub> <mH> <mI> <mW> <hH> <hI> <path>`
        "1" => {
            let xy = parts.next()?;
            let path = line.splitn(9, ' ').nth(8)?.to_string();
            Some(entry_from_xy(xy, path, None))
        }
        // Rename/copy: same as ordinary plus a score field, and the path
        // field is `<path>\t<origPath>`.
        "2" => {
            let xy = parts.next()?;
            let tail = line.splitn(10, ' ').nth(9)?;
            let mut paths = tail.split('\t');
            let path = paths.next()?.to_string();
            let previous_path = paths.next().map(str::to_string);
            Some(entry_from_xy(xy, path, previous_path))
        }
        // Unmerged: `u <XY> ...` — always a conflict.
        "u" => {
            let _xy = parts.next()?;
            let path = line.splitn(11, ' ').nth(10)?.to_string();
            Some(Entry {
                path,
                previous_path: None,
                staged: Some(Change::Conflicted),
                staged_stat: None,
                unstaged: None,
                unstaged_stat: None,
            })
        }
        // Untracked: `? <path>`
        "?" => Some(Entry {
            path: line.strip_prefix("? ")?.to_string(),
            previous_path: None,
            staged: None,
            staged_stat: None,
            unstaged: Some(Change::Untracked),
            unstaged_stat: None,
        }),
        _ => None,
    }
}

fn entry_from_xy(xy: &str, path: String, previous_path: Option<String>) -> Entry {
    let mut chars = xy.chars();
    let index = chars.next().unwrap_or('.');
    let worktree = chars.next().unwrap_or('.');
    Entry {
        path,
        previous_path,
        staged: Change::from_code(index),
        staged_stat: None,
        unstaged: Change::from_code(worktree),
        unstaged_stat: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_history_graph_rows_and_refs() {
        let mut commits = parse_history(
            "* \u{1e}abc123456789\u{1f}abc1234\u{1f}parent1\u{1f}HEAD -> main, origin/main\u{1f}Polish UI\u{1f}Ryan\u{1f}2 hours ago\n\
             | * \u{1e}def987654321\u{1f}def9876\u{1f}parent2\u{1f}feature\u{1f}Add graph\u{1f}Sam\u{1f}yesterday\n",
        );
        assign_lanes(&mut commits);
        assert_eq!(commits.len(), 2);
        assert_eq!(commits[0].graph, "*");
        assert_eq!(commits[0].refs, ["HEAD -> main", "origin/main"]);
        assert_eq!(commits[0].subject, "Polish UI");
        assert!(commits[0].is_head);
        assert_eq!(commits[0].lane, 0);
        assert_eq!(commits[0].line_style, LineStyle::Newest);
        assert!(!commits[1].is_head);
        assert_eq!(commits[1].lane, 1);
        assert!(commits[0].connectors.is_empty());
        assert_eq!(commits[1].graph, "| *");
        assert_eq!(commits[1].author, "Sam");
    }

    #[test]
    fn head_keeps_lane_zero_when_another_ref_is_newer() {
        let mut commits = parse_history(
            "* \u{1e}other\u{1f}other\u{1f}other-parent\u{1f}feature\u{1f}Newer elsewhere\u{1f}A\u{1f}now\n\
             * \u{1e}head\u{1f}head\u{1f}head-parent\u{1f}HEAD -> main\u{1f}Checked out\u{1f}B\u{1f}today\n",
        );
        assign_lanes(&mut commits);

        assert_eq!(commits[0].lane, 1);
        assert!(commits[0].pass_through.is_empty());
        assert_eq!(commits[1].lane, 0);
        assert_eq!(commits[1].line_style, LineStyle::Newest);
        assert_eq!(
            commits[1].pass_through,
            [LaneSegment {
                lane: 1,
                style: LineStyle::Full
            }]
        );
    }

    #[test]
    fn active_merge_lane_stays_full_height_behind_its_corner() {
        // A newer tip already opened lane 1 before HEAD's merge reaches it.
        // The full-height rail behind the lower merge corner prevents the
        // corner radius from opening a seam at the row midpoint.
        let mut commits = parse_history(
            "* \u{1e}tip\u{1f}tip\u{1f}feature\u{1f}feature-tip\u{1f}Newer tip\u{1f}A\u{1f}now\n\
             * \u{1e}merge\u{1f}merge\u{1f}main-parent feature\u{1f}HEAD -> main\u{1f}Merge feature\u{1f}B\u{1f}today\n",
        );
        assign_lanes(&mut commits);

        assert_eq!(commits[0].lane, 1);
        assert_eq!(commits[1].lane, 0);
        assert_eq!(
            commits[1].joins,
            [Join {
                kind: JoinKind::Merge,
                lane: 0,
                other: 1
            }]
        );
        assert_eq!(
            commits[1].pass_through,
            [LaneSegment {
                lane: 1,
                style: LineStyle::Full
            }]
        );
    }

    #[test]
    fn shared_row_joins_render_widest_first() {
        let mut commits = parse_history(
            "* \u{1e}tip-one\u{1f}tip1\u{1f}root\u{1f}feature-one\u{1f}Tip one\u{1f}A\u{1f}now\n\
             * \u{1e}tip-two\u{1f}tip2\u{1f}root\u{1f}feature-two\u{1f}Tip two\u{1f}B\u{1f}now\n\
             * \u{1e}head\u{1f}head\u{1f}root\u{1f}HEAD -> main\u{1f}Head\u{1f}C\u{1f}today\n\
             * \u{1e}root\u{1f}root\u{1f}\u{1f}\u{1f}Shared root\u{1f}D\u{1f}yesterday\n",
        );
        assign_lanes(&mut commits);

        assert_eq!(commits[3].lane, 0);
        assert_eq!(
            commits[3].joins,
            [
                Join {
                    kind: JoinKind::BranchOut,
                    lane: 0,
                    other: 2
                },
                Join {
                    kind: JoinKind::BranchOut,
                    lane: 0,
                    other: 1
                }
            ]
        );
    }

    #[test]
    fn parses_numstat_for_paths_and_renames_without_counting_binaries() {
        let stats =
            parse_numstat(b"12\t3\tsrc/main.rs\0-\t-\tassets/logo.png\x004\t5\t\0old.rs\0new.rs\0");
        assert_eq!(
            stats.get("src/main.rs"),
            Some(&DiffStat {
                additions: 12,
                deletions: 3
            })
        );
        assert_eq!(
            stats.get("new.rs"),
            Some(&DiffStat {
                additions: 4,
                deletions: 5
            })
        );
        assert!(!stats.contains_key("assets/logo.png"));
        assert!(!stats.contains_key("old.rs"));
    }

    #[test]
    fn preserves_connector_rows_after_their_commit() {
        let commits = parse_history(
            "* \u{1e}abc\u{1f}abc\u{1f}parent\u{1f}\u{1f}Merge\u{1f}A\u{1f}now\n|\\\n\
             | * \u{1e}def\u{1f}def\u{1f}parent\u{1f}\u{1f}Side\u{1f}B\u{1f}now\n|/\n",
        );
        assert_eq!(commits.len(), 2);
        assert_eq!(commits[0].connectors, ["|\\"]);
        assert_eq!(commits[1].connectors, ["|/"]);
    }

    #[test]
    fn merge_and_fork_produce_matching_join_pair() {
        // Newest to oldest: M merges F into the mainline parent P; F and P
        // both descend from the shared ancestor FB, which is where the
        // feature branch's lane folds back into the mainline lane.
        let mut commits = parse_history(
            "* \u{1e}M\u{1f}m\u{1f}P F\u{1f}\u{1f}Merge feature\u{1f}A\u{1f}now\n\
             * \u{1e}P\u{1f}p\u{1f}FB\u{1f}\u{1f}Mainline commit\u{1f}A\u{1f}now\n\
             * \u{1e}F\u{1f}f\u{1f}FB\u{1f}\u{1f}Feature commit\u{1f}B\u{1f}now\n\
             * \u{1e}FB\u{1f}fb\u{1f}\u{1f}\u{1f}Root\u{1f}A\u{1f}now\n",
        );
        assign_lanes(&mut commits);

        assert_eq!(commits[0].lane, 0);
        assert_eq!(commits[0].line_style, LineStyle::Newest);
        assert_eq!(
            commits[0].joins,
            [Join {
                kind: JoinKind::Merge,
                lane: 0,
                other: 1
            }]
        );

        assert_eq!(commits[1].lane, 0);
        assert_eq!(commits[1].line_style, LineStyle::Full);
        assert!(commits[1].joins.is_empty());
        assert_eq!(
            commits[1].pass_through,
            [LaneSegment {
                lane: 1,
                style: LineStyle::Full
            }]
        );

        assert_eq!(commits[2].lane, 1);
        assert_eq!(commits[2].line_style, LineStyle::Full);
        assert!(commits[2].joins.is_empty());
        assert_eq!(
            commits[2].pass_through,
            [LaneSegment {
                lane: 0,
                style: LineStyle::Full
            }]
        );

        assert_eq!(commits[3].lane, 0);
        assert_eq!(commits[3].line_style, LineStyle::Oldest);
        assert_eq!(
            commits[3].joins,
            [Join {
                kind: JoinKind::BranchOut,
                lane: 0,
                other: 1
            }]
        );
    }

    #[test]
    fn lanes_beyond_max_fold_into_the_last_column() {
        // Five independent, still-open branch tips (parents that never
        // appear) force five simultaneously live lanes.
        let mut commits = parse_history(
            "* \u{1e}B1\u{1f}b1\u{1f}X1\u{1f}\u{1f}b1\u{1f}A\u{1f}now\n\
             * \u{1e}B2\u{1f}b2\u{1f}X2\u{1f}\u{1f}b2\u{1f}A\u{1f}now\n\
             * \u{1e}B3\u{1f}b3\u{1f}X3\u{1f}\u{1f}b3\u{1f}A\u{1f}now\n\
             * \u{1e}B4\u{1f}b4\u{1f}X4\u{1f}\u{1f}b4\u{1f}A\u{1f}now\n\
             * \u{1e}B5\u{1f}b5\u{1f}X5\u{1f}\u{1f}b5\u{1f}A\u{1f}now\n",
        );
        assign_lanes(&mut commits);
        assert_eq!(
            commits.iter().map(|c| c.lane).collect::<Vec<_>>(),
            [0, 1, 2, 3, 3]
        );
    }

    #[test]
    fn folded_octopus_parents_keep_separate_logical_targets() {
        let mut commits = parse_history(
            "* \u{1e}merge\u{1f}merge\u{1f}P0 P1 P2 P3 P4\u{1f}\u{1f}Octopus merge\u{1f}A\u{1f}now\n\
             * \u{1e}P0\u{1f}p0\u{1f}\u{1f}\u{1f}Parent zero\u{1f}A\u{1f}now\n\
             * \u{1e}P1\u{1f}p1\u{1f}\u{1f}\u{1f}Parent one\u{1f}A\u{1f}now\n\
             * \u{1e}P2\u{1f}p2\u{1f}\u{1f}\u{1f}Parent two\u{1f}A\u{1f}now\n\
             * \u{1e}P3\u{1f}p3\u{1f}\u{1f}\u{1f}Parent three\u{1f}A\u{1f}now\n\
             * \u{1e}P4\u{1f}p4\u{1f}\u{1f}\u{1f}Parent four\u{1f}A\u{1f}now\n",
        );
        assign_lanes(&mut commits);

        assert_eq!(commits[0].joins.len(), 3);
        assert_eq!(
            commits.iter().map(|commit| commit.lane).collect::<Vec<_>>(),
            [0, 0, 1, 2, 3, 3]
        );
        assert_eq!(commits[4].line_style, LineStyle::Oldest);
        assert_eq!(commits[5].line_style, LineStyle::Oldest);
    }

    #[test]
    fn reads_history_from_the_real_repository() {
        let commits = history(Path::new("."), 5);
        assert!(!commits.is_empty());
        assert!(commits.len() <= 5);
        assert!(!commits[0].short_id.is_empty());
    }

    #[test]
    fn parses_nul_delimited_name_status() {
        let changes = parse_name_status(b"M\0src/main.rs\0R100\0old.rs\0new.rs\0");
        assert_eq!(changes.len(), 2);
        assert_eq!(changes[0].path, "src/main.rs");
        assert_eq!(changes[0].change, Change::Modified);
        assert_eq!(changes[1].path, "new.rs");
        assert_eq!(changes[1].previous_path.as_deref(), Some("old.rs"));
        assert_eq!(changes[1].change, Change::Renamed);
    }

    #[test]
    fn commit_file_list_and_diff_use_the_same_parent_for_merges() {
        let root = std::env::temp_dir().join(format!(
            "forge-git-merge-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let git = |args: &[&str]| {
            let status = Command::new("git")
                .arg("-C")
                .arg(&root)
                .args(args)
                .status()
                .unwrap();
            assert!(status.success(), "git {args:?} failed");
        };

        git(&["init", "-q", "-b", "main"]);
        git(&["config", "user.name", "Forge Test"]);
        git(&["config", "user.email", "forge@example.invalid"]);
        std::fs::write(root.join("base.txt"), "base\n").unwrap();
        git(&["add", "."]);
        git(&["commit", "-q", "-m", "base"]);
        git(&["checkout", "-q", "-b", "feature"]);
        std::fs::write(root.join("feature.txt"), "feature\n").unwrap();
        git(&["add", "."]);
        git(&["commit", "-q", "-m", "feature"]);
        git(&["checkout", "-q", "main"]);
        std::fs::write(root.join("main.txt"), "main\n").unwrap();
        git(&["add", "."]);
        git(&["commit", "-q", "-m", "main"]);
        git(&["merge", "-q", "--no-ff", "feature", "-m", "merge feature"]);

        let merge = history(&root, 1).remove(0);
        assert_eq!(merge.parents.len(), 2);
        let changes = commit_changes(&root, &merge);
        assert!(changes.iter().any(|change| change.path == "feature.txt"));
        let diff = commit_diff(&root, &merge, "feature.txt");
        assert!(diff.contains("+feature"));

        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn stage_unstage_and_commit_operations_work() {
        let root = std::env::temp_dir().join(format!(
            "forge-git-operations-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let git = |args: &[&str]| {
            let result = Command::new("git")
                .arg("-C")
                .arg(&root)
                .args(args)
                .status()
                .unwrap();
            assert!(result.success(), "git {args:?} failed");
        };

        git(&["init", "-q", "-b", "main"]);
        git(&["config", "user.name", "Forge Test"]);
        git(&["config", "user.email", "forge@example.invalid"]);
        std::fs::write(root.join("tracked.txt"), "one\n").unwrap();
        stage_all(&root).unwrap();
        commit_staged(&root, "initial").unwrap();
        git(&["branch", "feature"]);
        assert_eq!(local_branches(&root), ["feature", "main"]);
        switch_branch(&root, "feature").unwrap();
        assert_eq!(status(&root).branch.as_deref(), Some("feature"));
        switch_branch(&root, "main").unwrap();

        std::fs::write(root.join("tracked.txt"), "discard me\n").unwrap();
        discard_worktree(&root, "tracked.txt", None, false).unwrap();
        assert_eq!(
            std::fs::read_to_string(root.join("tracked.txt")).unwrap(),
            "one\n"
        );

        std::fs::write(root.join("tracked.txt"), "two\n").unwrap();
        stage(&root, "tracked.txt").unwrap();
        assert_eq!(status(&root).staged_count(), 1);
        unstage(&root, "tracked.txt").unwrap();
        assert_eq!(status(&root).staged_count(), 0);

        stage_all(&root).unwrap();
        unstage_all(&root).unwrap();
        assert_eq!(status(&root).staged_count(), 0);
        stage_all(&root).unwrap();
        commit_staged(&root, "update").unwrap();
        assert!(status(&root).is_clean());
        assert_eq!(
            repository_root(&root).as_deref(),
            Some(root.canonicalize().unwrap().as_path())
        );

        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn empty_commit_message_is_rejected() {
        let error = commit_staged(Path::new("."), "  ").unwrap_err();
        assert_eq!(error.message, "Commit message cannot be empty");
    }

    #[test]
    fn parses_branch_and_ahead_behind() {
        let s = parse("# branch.head main\n# branch.upstream origin/main\n# branch.ab +3 -2\n");
        assert_eq!(s.branch.as_deref(), Some("main"));
        assert_eq!(s.upstream.as_deref(), Some("origin/main"));
        assert_eq!(s.ahead, 3);
        assert_eq!(s.behind, 2);
        assert!(s.is_clean());
    }

    #[test]
    fn detached_head_reports_no_branch() {
        let s = parse("# branch.head (detached)\n");
        assert_eq!(s.branch, None);
    }

    #[test]
    fn parses_ordinary_staged_and_unstaged() {
        // staged modify + worktree modify
        let s = parse("1 MM N... 100644 100644 100644 abc def src/main.rs\n");
        assert_eq!(s.entries.len(), 1);
        let e = &s.entries[0];
        assert_eq!(e.path, "src/main.rs");
        assert_eq!(e.staged, Some(Change::Modified));
        assert_eq!(e.unstaged, Some(Change::Modified));
        assert_eq!(s.staged_count(), 1);
        assert_eq!(s.unstaged_count(), 1);
    }

    #[test]
    fn dot_means_no_change_on_that_side() {
        let s = parse("1 .M N... 100644 100644 100644 abc def a/b.txt\n");
        let e = &s.entries[0];
        assert_eq!(e.staged, None);
        assert_eq!(e.unstaged, Some(Change::Modified));
    }

    #[test]
    fn parses_untracked_and_rename() {
        let s =
            parse("? new_file.rs\n2 R. N... 100644 100644 100644 abc def R100 new.rs\told.rs\n");
        assert_eq!(s.entries.len(), 2);
        let renamed = s.entries.iter().find(|e| e.path == "new.rs").unwrap();
        assert_eq!(renamed.staged, Some(Change::Renamed));
        assert_eq!(renamed.previous_path.as_deref(), Some("old.rs"));
        let untracked = s.entries.iter().find(|e| e.path == "new_file.rs").unwrap();
        assert_eq!(untracked.unstaged, Some(Change::Untracked));
    }

    #[test]
    fn parses_conflict() {
        let s = parse("u UU N... 100644 100644 100644 100644 a b c both.rs\n");
        assert_eq!(s.entries.len(), 1);
        assert!(s.entries[0].is_conflicted());
    }

    #[test]
    fn reads_the_real_repository_it_lives_in() {
        // Guards the parser against drift in git's actual v2 output, which the
        // hand-written fixtures above can't catch on their own.
        let s = status(Path::new("."));
        assert!(s.is_repo, "crate dir should be inside a git work tree");
        assert!(
            s.branch.is_some() || s.is_repo,
            "expected a branch or a valid detached-head read"
        );
    }

    #[test]
    fn summary_counts_changes_without_keeping_entries() {
        let s = parse(
            "# branch.head main\n\
             # branch.ab +1 -0\n\
             1 M. N... 100644 100644 100644 a b one.rs\n\
             ? two.rs\n",
        );
        let summary = Summary {
            branch: s.branch.clone(),
            ahead: s.ahead,
            behind: s.behind,
            changed: s.entries.len(),
            is_repo: s.is_repo,
        };
        assert_eq!(summary.changed, 2);
        assert!(summary.is_dirty());
        assert_eq!(summary.ahead, 1);
        assert_eq!(summary.branch.as_deref(), Some("main"));
    }

    #[test]
    fn clean_tree_is_not_dirty() {
        let s = parse("# branch.head main\n");
        assert_eq!(s.entries.len(), 0);
        let summary = Summary {
            changed: s.entries.len(),
            ..Default::default()
        };
        assert!(!summary.is_dirty());
    }

    #[test]
    fn non_repo_path_is_reported_not_a_repo() {
        let s = status(Path::new("/"));
        assert!(!s.is_repo);
    }

    #[test]
    fn nul_status_preserves_newlines_and_rename_paths() {
        let status = parse_z(
            b"# branch.head main\0? dir/line\nname.txt\0\
              2 R. N... 100644 100644 100644 a b R100 new name.txt\0old name.txt\0",
        );
        assert_eq!(status.branch.as_deref(), Some("main"));
        assert_eq!(status.entries.len(), 2);
        assert_eq!(status.entries[0].path, "dir/line\nname.txt");
        let renamed = status
            .entries
            .iter()
            .find(|entry| entry.path == "new name.txt")
            .unwrap();
        assert_eq!(renamed.previous_path.as_deref(), Some("old name.txt"));
    }

    #[test]
    fn paths_with_spaces_survive() {
        let s = parse("1 .M N... 100644 100644 100644 abc def my dir/my file.txt\n");
        assert_eq!(s.entries[0].path, "my dir/my file.txt");
    }
}
