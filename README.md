<div align="center">

# Forge

**A fast, native workspace for terminals, code, Git, and coding agents.**

Built entirely with Rust and [GPUI](https://www.gpui.rs/).

> Forge is an early, macOS-first work in progress. The name and product surface may still evolve.

</div>

## Why Forge exists

Modern agent-assisted development is powerful, but the daily workflow is fragmented. A task starts in a terminal, moves into VS Code, continues in Claude Code or another agent harness, detours through a Git client, and ends up tracked in a separate task app. Each tool can be excellent on its own; the constant context switching is not.

I am building Forge because I do not want to juggle my work across all of them.

Forge is intended to be the single native application where I can:

- run a real terminal and the command-line tools I already use;
- inspect and edit code without moving to another window;
- launch, observe, and manage multiple coding agents;
- understand workspace and Git state at a glance; and
- move from an agent's task to its terminal, files, diff, and review context directly.

This is deliberately opinionated software. Forge is not trying to reproduce every feature of every terminal, editor, or project manager. It is being shaped around a fast, keyboard-first workflow with native-feeling experiences for each job.

## Product principles

- **One workspace, not several loosely connected apps.** Terminal sessions, files, Git state, and agents share the same project context.
- **Native-feeling workflows.** Each surface should feel purpose-built rather than like a web view embedded in a shell.
- **Speed is a feature.** Rust, GPUI, event-driven rendering, and background work keep interaction responsive and idle overhead low.
- **Keyboard first, mouse friendly.** Core navigation has a direct keyboard path without making pointer interaction an afterthought.
- **Agent agnostic.** Claude Code, Codex, and other CLI agents run through normal PTYs instead of a proprietary execution layer.
- **Local by default.** Source code and terminal sessions stay on the machine; integrations are explicit.

## Current state

Forge already provides a usable native application shell:

| Area | Available now | In progress |
|---|---|---|
| Terminal | Real PTYs, ANSI/VT rendering, scrollback, horizontal and vertical splits, pane focus and resize | Nested split trees, draggable ratios, pane zoom |
| Editor | Rope-backed buffers, normal/insert/command modes, core modal motions and edits, save/quit commands | Visual modes, complete operator grammar, tree-sitter, LSP |
| Workspaces | Multiple project roots, branch and process context, rename and switching | Session restore, persisted layouts, notifications |
| Files | Gitignore-aware tree and file opening | Nested ignore rules and filesystem watching |
| Git | Status, staging, commits, branches, fetch/pull/push, stash, discard, diffs, and history graph | Deeper review workflows and remote/PR integration |
| Processes | Pane-scoped process inspection | Process actions and richer resource history |
| Agents | Any CLI agent can run in a terminal pane; a dedicated agent surface exists | Structured adapters, attention states, notifications, and task board |
| GitHub | First-run browser sign-in through GitHub CLI with Git credential setup | Native device OAuth and issue/PR surfaces |
| Updates | Signed-checksum edge-release detection, bottom-left prompt, in-place install, and restart | Signed/notarized app bundles and stable channels |

The central thesis is already testable: terminal work, code editing, project navigation, and Git operations can happen in one fast native window. Agent orchestration and the task board are the next major product layer.

## Architecture

Forge is a Cargo workspace split around product boundaries:

```text
crates/
├── forge/             GPUI application shell and composition
├── forge-terminal/    PTY lifecycle and VT terminal state
├── forge-workspace/   workspace and pane models
├── forge-editor/      modal editor state and rope-backed buffers
├── forge-files/       gitignore-aware project tree
├── forge-git/         Git status, history, diffs, and mutations
└── forge-proc/        pane-scoped process inspection
```

The binary owns presentation and input routing. Reusable engines remain independent of GPUI where practical, which keeps their behavior testable and prevents UI concerns from leaking into terminal, editor, Git, and process state.

### Technology

- **Rust** for predictable performance, memory safety, and a cohesive native stack.
- **GPUI** for GPU-accelerated desktop UI and fine-grained control over interaction.
- **portable-pty + vt100** for native pseudo-terminals and terminal state.
- **ropey** for editor text storage.
- **sysinfo** for process inspection.
- **Git's stable porcelain formats** for repository state and operations.

## Getting started

### Requirements

- macOS
- Xcode Command Line Tools
- a stable Rust toolchain
- [GitHub CLI](https://cli.github.com/) for GitHub onboarding and repository credentials

```bash
git clone https://github.com/RyanStoffel/forge.git
cd forge
cargo run -p forge --release
```

Forge opens the current directory as its first workspace. On first launch it detects an existing GitHub CLI session or offers GitHub's browser-based sign-in flow. Connecting GitHub is optional.

### Core shortcuts

| Shortcut | Action |
|---|---|
| `⌘1` / `⌘2` / `⌘3` | Terminal / Editor / Agents |
| `⌘E` | Cycle primary views |
| `⌘K` | Open file and workspace palette |
| `⌘D` | Split panes horizontally |
| `⇧⌘D` | Split panes vertically |
| `⌘[` / `⌘]` | Focus previous / next pane |
| `⌘W` | Close focused pane |
| `⌘R` | Rename active workspace |

The editor intentionally implements a focused Vim-like subset today rather than claiming full Vim compatibility.

## Updates and releases

Every push to `main` builds both Apple Silicon and Intel macOS binaries and refreshes the `edge` GitHub Release. CI embeds the source revision into each binary, publishes a SHA-256 digest, and the app checks that release without blocking the UI.

When a newer revision is available, Forge shows an update action at the bottom of the workspace sidebar. Selecting it downloads the architecture-matched binary, verifies its digest, atomically replaces the executable, and restarts Forge.

This edge channel matches the project's current work-in-progress stage. Production distribution still requires app bundling, code signing, notarization, rollback hardening, and a stable release channel.

## Roadmap

Near-term work is tracked in [GitHub Issues](https://github.com/RyanStoffel/forge/issues) and the repository's GitHub Project. Major themes:

1. structured agent adapters and attention notifications;
2. a native task board linked to workspaces, branches, panes, and agents;
3. completion of the modal editing grammar;
4. tree-sitter syntax and LSP intelligence;
5. session persistence, settings, and keymap configuration;
6. production-grade packaging, signing, and update channels; and
7. deeper GitHub issue and pull-request workflows.

## Contributing

Forge is early and its architecture is still moving. Before opening a large change, start with an issue that describes the user-facing problem, the intended workflow, and the smallest complete behavior that solves it.

For changes:

1. keep engine behavior outside the GPUI shell when it does not depend on presentation;
2. preserve event-driven rendering and keep filesystem, process, network, and Git work off the render thread;
3. add tests for observable behavior changes;
4. run `cargo fmt --all -- --check`, `cargo clippy --workspace --all-targets -- -D warnings`, and `cargo test --workspace`; and
5. verify UI behavior in the running app.

## Security and privacy

Forge executes shells and developer tools with the current user's permissions. Treat commands and agent actions with the same care as commands run directly in a terminal.

GitHub onboarding delegates authentication and token storage to the official GitHub CLI. Forge reads public account metadata from `gh api user`; it does not read, copy, or persist the OAuth token. Updates are downloaded only from this repository's GitHub Release and must match the published SHA-256 digest before installation.

Please report security-sensitive findings privately to the repository owner rather than opening a public issue.
