//! Design tokens for Forge's UI chrome.
//!
//! Implements the palette/spacing system specified in
//! `docs/figma-design-brief.md`: a cool-neutral dark base with layered surface
//! elevation expressed as lightness steps (not shadows), a restrained blue
//! accent, and a shared 5-state status scale reused by panes, the workspace
//! list, and eventually Kanban cards.
//!
//! These cover UI chrome only. Terminal *content* colors come from the
//! standard xterm-256 palette and are intentionally not themed here.

// The token set is intentionally complete rather than minimal: it mirrors the
// documented design system, so scales have all their steps and the 5-state
// status colors exist before the agent/Kanban work that consumes them. Keeping
// them here avoids re-deriving values ad hoc at each call site later.
#![allow(dead_code)]

use gpui::{rgb, rgba, Hsla, Rgba};

/// Surfaces.
///
/// Modeled on cmux: the titlebar, sidebars and terminal all share **one**
/// opaque background, so the window reads as a single surface rather than a
/// content area bracketed by distinct chrome. Separation comes from the
/// selection fill and hairline dividers, not from competing greys. Everything
/// here is fully opaque — no vibrancy.
pub mod surface {
    /// The single unified window background.
    pub const BASE: u32 = 0x1b1c20;
    /// Alias kept for call sites that mean "chrome"; identical to BASE by
    /// design, so chrome and content never diverge in tone.
    pub const RAISED: u32 = BASE;
    /// Floating surfaces (command palette, popovers) lift slightly, since they
    /// overlap content and need an edge.
    pub const OVERLAY: u32 = 0x23252a;
    /// Row hover: only one visible elevation step above the base.
    pub const HOVER: u32 = 0x26282e;
    /// Pressed/secondary active state for controls that aren't the primary
    /// selection.
    pub const ACTIVE: u32 = 0x2c2f36;
    /// Quiet inset controls such as segmented-control tracks and editor gutter.
    pub const INSET: u32 = 0x16171a;
    /// Fill for a section that is *actionable* — staged changes, merge
    /// conflicts. The only thing in the panel allowed a fill of its own, which
    /// is what makes it read as the actionable region at a glance.
    pub const SECTION: u32 = 0x1e2025;
}

/// The primary selection fill: a solid, saturated blue covering the whole row,
/// as cmux does, rather than a thin accent bar.
pub mod selection {
    pub const BG: u32 = 0x0a84ff;
    pub const TEXT: u32 = 0xffffff;
    /// Secondary text on the selection fill.
    pub const TEXT_MUTED: u32 = 0xcae4ff;
}

pub mod border {
    /// Structural dividers between regions. Hairline, since regions share a
    /// background and the divider is the only thing separating them.
    pub const DEFAULT: u32 = 0x34363e;
    /// Within-region separators; nearly invisible by design.
    pub const SUBTLE: u32 = 0x26282e;
    /// Border on an unfocused pane.
    pub const PANE_IDLE: u32 = 0x26282e;
    /// Outline of a section card (staged changes, merge conflicts).
    pub const SECTION: u32 = 0x2c2f36;
}

pub mod text {
    /// Primary content: filenames, branch name, focused input text.
    pub const DEFAULT: u32 = 0xeceef1;
    /// Active section labels, commit-detail filenames.
    pub const STRONG: u32 = 0xc9cdd4;
    /// Secondary information: commit subjects, counts, secondary glyphs.
    pub const MUTED: u32 = 0x9ba0a8;
    /// Tertiary: section labels when passive, inline text actions, chevrons.
    pub const DIM: u32 = 0x6b6f78;
    /// Quaternary: directory paths, short sha, author initials, timestamps.
    pub const FAINT: u32 = 0x5f636b;
    /// Text on an accent-filled surface.
    pub const ON_ACCENT: u32 = 0xffffff;
}

/// Less saturated than the old `0x0a84ff` so it survives being used at 12%
/// fill and 1px border weight, and so it stops out-shouting the git status
/// colors.
pub const ACCENT: u32 = 0x4c8df6;

pub mod accent {
    pub const HOVER: u32 = 0x5f9bf8;
    pub const PRESSED: u32 = 0x3b7ae4;
    /// Disabled / busy primary button.
    pub const MUTED: u32 = 0x2b3a52;
    /// Selection wash over a row (8-digit RGBA).
    pub const WASH: u32 = 0x4c8df61a;
}

/// Shared 5-state status scale. Used for pane focus, workspace activity, and
/// (later) agent task state, so one color always means one thing.
pub mod status {
    pub const IDLE: u32 = 0x646b76;
    pub const RUNNING: u32 = 0x4c9aff;
    pub const ATTENTION: u32 = 0xe0a458;
    pub const ERROR: u32 = 0xe0605e;
    pub const DONE: u32 = 0x56c288;
}

/// Git status colors, shared between the Git tab and file-tree filename tint.
/// Re-tuned to a common perceived lightness so no single status shouts.
/// These appear ONLY on the one-character status code and on diffstat
/// numbers — never on a file icon.
pub mod git {
    pub const ADDED: u32 = 0x6cc07a;
    pub const MODIFIED: u32 = 0xd9a441;
    pub const DELETED: u32 = 0xe07a70;
    pub const RENAMED: u32 = 0x6b9fe0;
    pub const UNTRACKED: u32 = 0x6cc07a;
    pub const CONFLICTED: u32 = 0xb48ce6;
}

/// Conflict card and error banner.
pub mod danger {
    pub const SURFACE: u32 = 0x231b1d;
    pub const BORDER: u32 = 0x4a2f33;
    pub const TEXT: u32 = 0xe0a8a4;
}

/// Commit-graph geometry and lane colors.
pub mod graph {
    /// Cycled by lane index. Lane 0 is always the checked-out branch, so it
    /// gets the accent; the rest borrow from the git status hues rather than
    /// introducing new colors.
    pub const LANES: [u32; 4] = [0x4c8df6, 0xd9a441, 0x6cc07a, 0xb48ce6];
    /// Lanes beyond this fold into the last one and the gutter stops growing.
    pub const MAX_LANES: usize = 4;
    /// Lane centre x = ORIGIN + PITCH * lane_index.
    pub const ORIGIN: f32 = 10.0;
    pub const PITCH: f32 = 12.0;
    /// Gutter width = PITCH * lanes + PAD.
    pub const PAD: f32 = 6.0;
    pub const LINE: f32 = 1.5;
    pub const DOT: f32 = 7.0;
    /// Ring drawn in the row's own background so the lane line does not read
    /// as passing through the dot.
    pub const DOT_RING: f32 = 2.0;
    /// Corner radius of a merge / branch-out join.
    pub const JOIN_RADIUS: f32 = 8.0;
    /// Row height a join is computed against.
    pub const ROW: f32 = 26.0;
}

/// Compact, developer-tool spacing scale (px).
pub mod space {
    pub const XXS: f32 = 2.0;
    pub const XS: f32 = 4.0;
    pub const SM: f32 = 6.0;
    pub const MD: f32 = 8.0;
    pub const ML: f32 = 10.0;
    pub const LG: f32 = 12.0;
    pub const XL: f32 = 16.0;
    pub const XXL: f32 = 24.0;
}

/// Small radii only — sharp and tool-like, not consumer-rounded.
pub mod radius {
    pub const SM: f32 = 3.0;
    pub const MD: f32 = 5.0;
    pub const LG: f32 = 8.0;
}

pub mod font_size {
    /// Section headers, badges, status bar.
    pub const XS: f32 = 10.0;
    /// Secondary rows, metadata.
    pub const SM: f32 = 11.0;
    /// Default UI text and tree rows.
    pub const MD: f32 = 12.0;
    /// Primary labels, palette input.
    pub const LG: f32 = 13.0;
}

/// Fixed row metrics keep list columns aligned without relying on gaps.
pub mod row {
    /// Single-line list rows (file tree, process list, git entries).
    pub const HEIGHT: f32 = 22.0;
    /// Workspace entries: identity, Git state, and full path metadata.
    pub const WORKSPACE_HEIGHT: f32 = 72.0;
    /// Fixed leading slot for chevrons/status glyphs.
    pub const ICON_SLOT: f32 = 14.0;
    /// Fixed slot for two-character git status codes.
    pub const GIT_CODE_SLOT: f32 = 22.0;
}

pub fn color(hex: u32) -> Rgba {
    rgb(hex)
}

/// Build a color from 8-digit `0xRRGGBBAA`, for translucent surfaces.
pub fn color_a(hex: u32) -> Rgba {
    rgba(hex)
}

pub fn hsla(hex: u32) -> Hsla {
    Hsla::from(rgb(hex))
}
