//! Forge application shell: native terminal, modal editor, Git tools,
//! workspace management, agent surface, account onboarding, and updates.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
#[cfg(target_os = "macos")]
use std::sync::{
    atomic::{AtomicBool, AtomicIsize, Ordering},
    OnceLock,
};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use futures::channel::mpsc;
use futures::StreamExt;

#[cfg(target_os = "macos")]
use cocoa::{
    appkit::{NSApp, NSEventModifierFlags, NSMenu, NSMenuItem},
    base::{id, nil},
    foundation::{NSAutoreleasePool, NSString},
};
#[cfg(target_os = "macos")]
use objc::{
    class,
    declare::ClassDecl,
    msg_send,
    runtime::{Class, Object, Sel},
    sel, sel_impl,
};

mod assets;
mod github;
mod theme;
mod updater;

use forge_files::FileNode;
use forge_workspace::{Layout, Workspace, WorkspaceManager};
use gpui::{
    div, font, prelude::*, px, rgb, rgba, size, uniform_list, white, AnyElement, App, Application,
    Bounds, ClipboardItem, Context, ElementId, FocusHandle, Focusable, Font, FontFallbacks,
    FontWeight, Hsla, KeyDownEvent, Keystroke, MouseButton, Render, ScrollStrategy, SharedString,
    StyledText, TextAlign, TextRun, Timer, TitlebarOptions, UniformListScrollHandle, Window,
    WindowBounds, WindowOptions,
};
use gpui::{img, point, svg, Image, ImageFormat, ObjectFit};

const ROWS: u16 = 45;
const COLS: u16 = 160;
const FONT_SIZE: f32 = 13.0;
const LINE_HEIGHT: f32 = 18.0;
const SIDEBAR_WIDTH: f32 = 244.0;
const SIDEBAR_MIN_WIDTH: f32 = 180.0;
const SIDEBAR_MAX_WIDTH: f32 = 480.0;
const INFO_PANEL_WIDTH: f32 = 240.0;
const INFO_PANEL_MIN_WIDTH: f32 = 180.0;
const INFO_PANEL_MAX_WIDTH: f32 = 480.0;
const TOP_BAR_HEIGHT: f32 = 42.0;
/// Left inset reserved for the macOS traffic lights, so the window decoration
/// occupies the titlebar strip up to where the first tab begins.
const TRAFFIC_LIGHT_INSET: f32 = 78.0;
/// How often the Processes tab refreshes while visible. A refresh reads every
/// process on the system, so this stays slow and only runs when the tab is open.
const PROCESS_REFRESH_INTERVAL: Duration = Duration::from_millis(1500);
const PADDING: f32 = 8.0;
const MIN_ROWS: u16 = 8;
const MIN_COLS: u16 = 20;
const DEFAULT_FG: u32 = theme::text::DEFAULT;
const TERMINAL_BG: u32 = theme::surface::BASE;
/// Preferred terminal/editor font, first match wins.
///
/// Nerd Font *Mono* variants are used deliberately: their patched icons are
/// squeezed to a single cell advance, which is what a fixed grid renderer
/// needs. The proportional (non-Mono) variants draw icons double-width and
/// would desynchronize our column math from the PTY's.
///
/// GPUI resolves the first family that exists and keeps the rest as a
/// per-glyph fallback cascade, so this degrades cleanly to Menlo when no Nerd
/// Font is installed, with emoji picking up the system color font.
const FONT_STACK: &[&str] = &[
    "JetBrainsMono Nerd Font Mono",
    "FiraCode Nerd Font Mono",
    "Symbols Nerd Font Mono",
    "MesloLGS NF",
    "Menlo",
    "Apple Color Emoji",
];
/// How long the output-wake loop yields the main-thread executor after
/// requesting a repaint.
///
/// This is deliberately *shorter* than a 120Hz frame (8.33ms): the goal is
/// only to give the presenter room to draw, not to act as the frame clock.
/// Sleeping a full frame means always racing the vsync deadline, which costs
/// a dropped frame whenever the wake lands late (measured: 107fps with 16.7ms
/// stalls). Yielding half a frame keeps us ready ahead of every vsync and
/// lets the compositor set the pace.
const OUTPUT_WAKE_YIELD: Duration = Duration::from_micros(4000);
/// Minimum spacing between sidebar git/foreground-process refreshes. These
/// shell out and read process state, so they are driven by terminal activity
/// but rate-limited far below the frame rate.
const SIDEBAR_META_INTERVAL: Duration = Duration::from_millis(1200);
/// Character budget for the path line under a workspace name.
const PATH_MAX_CHARS: usize = 29;

#[cfg(target_os = "macos")]
static WORKSPACE_RENAME_CHOSEN: AtomicBool = AtomicBool::new(false);
#[cfg(target_os = "macos")]
static GIT_MENU_CHOICE: AtomicIsize = AtomicIsize::new(-1);

#[cfg(target_os = "macos")]
fn workspace_menu_target_class() -> &'static Class {
    static CLASS: OnceLock<&'static Class> = OnceLock::new();
    CLASS.get_or_init(|| {
        extern "C" fn rename_workspace(_: &Object, _: Sel, _: id) {
            WORKSPACE_RENAME_CHOSEN.store(true, Ordering::Release);
        }

        let mut declaration = ClassDecl::new("ForgeWorkspaceContextMenuTarget", class!(NSObject))
            .expect("workspace menu target class should register once");
        unsafe {
            declaration.add_method(
                sel!(renameWorkspace:),
                rename_workspace as extern "C" fn(&Object, Sel, id),
            );
        }
        declaration.register()
    })
}

#[cfg(target_os = "macos")]
fn show_workspace_context_menu() -> bool {
    WORKSPACE_RENAME_CHOSEN.store(false, Ordering::Release);
    unsafe {
        let pool = NSAutoreleasePool::new(nil);
        let menu = NSMenu::new(nil);
        menu.setAutoenablesItems(false as _);

        let title = NSString::alloc(nil).init_str("Rename Workspace…");
        let key = NSString::alloc(nil).init_str("r");
        let item = NSMenuItem::alloc(nil).initWithTitle_action_keyEquivalent_(
            title,
            sel!(renameWorkspace:),
            key,
        );
        item.setKeyEquivalentModifierMask_(NSEventModifierFlags::NSCommandKeyMask);
        let target: id = msg_send![workspace_menu_target_class(), new];
        item.setTarget_(target);
        menu.addItem_(item);

        let app = NSApp();
        let event: id = msg_send![app, currentEvent];
        let window: id = msg_send![app, keyWindow];
        let view: id = msg_send![window, contentView];
        let _: () =
            msg_send![class!(NSMenu), popUpContextMenu: menu withEvent: event forView: view];

        let _: () = msg_send![target, release];
        let _: () = msg_send![item, release];
        let _: () = msg_send![title, release];
        let _: () = msg_send![key, release];
        let _: () = msg_send![menu, release];
        pool.drain();
    }
    WORKSPACE_RENAME_CHOSEN.load(Ordering::Acquire)
}

#[cfg(target_os = "macos")]
fn git_menu_target_class() -> &'static Class {
    static CLASS: OnceLock<&'static Class> = OnceLock::new();
    CLASS.get_or_init(|| {
        extern "C" fn choose_git_menu_item(_: &Object, _: Sel, sender: id) {
            let tag: isize = unsafe { msg_send![sender, tag] };
            GIT_MENU_CHOICE.store(tag, Ordering::Release);
        }

        let mut declaration = ClassDecl::new("ForgeGitContextMenuTarget", class!(NSObject))
            .expect("git menu target class should register once");
        unsafe {
            declaration.add_method(
                sel!(chooseGitMenuItem:),
                choose_git_menu_item as extern "C" fn(&Object, Sel, id),
            );
        }
        declaration.register()
    })
}

#[cfg(target_os = "macos")]
fn show_git_native_menu(items: &[Option<String>]) -> Option<usize> {
    GIT_MENU_CHOICE.store(-1, Ordering::Release);
    unsafe {
        let pool = NSAutoreleasePool::new(nil);
        let menu = NSMenu::new(nil);
        menu.setAutoenablesItems(false as _);
        let target: id = msg_send![git_menu_target_class(), new];

        for (index, title) in items.iter().enumerate() {
            if let Some(title) = title {
                let title = NSString::alloc(nil).init_str(title);
                let key = NSString::alloc(nil).init_str("");
                let item = NSMenuItem::alloc(nil).initWithTitle_action_keyEquivalent_(
                    title,
                    sel!(chooseGitMenuItem:),
                    key,
                );
                let _: () = msg_send![item, setTag: index as isize];
                item.setTarget_(target);
                menu.addItem_(item);
                let _: () = msg_send![item, release];
                let _: () = msg_send![title, release];
                let _: () = msg_send![key, release];
            } else {
                menu.addItem_(NSMenuItem::separatorItem(nil));
            }
        }

        let app = NSApp();
        let event: id = msg_send![app, currentEvent];
        let window: id = msg_send![app, keyWindow];
        let view: id = msg_send![window, contentView];
        let _: () =
            msg_send![class!(NSMenu), popUpContextMenu: menu withEvent: event forView: view];

        let _: () = msg_send![target, release];
        let _: () = msg_send![menu, release];
        pool.drain();
    }
    usize::try_from(GIT_MENU_CHOICE.load(Ordering::Acquire)).ok()
}

#[cfg(target_os = "macos")]
fn confirm_git_discard(target: &str, count: usize) -> bool {
    unsafe {
        let pool = NSAutoreleasePool::new(nil);
        let alert: id = msg_send![class!(NSAlert), new];
        let title = if count == 1 {
            format!("Discard changes in {target}?")
        } else {
            format!("Discard the {count} reviewed changes?")
        };
        let detail = "Tracked changes will be restored. Untracked and moved files will be moved to the Trash.";
        let title = NSString::alloc(nil).init_str(&title);
        let detail = NSString::alloc(nil).init_str(detail);
        let confirm = NSString::alloc(nil).init_str("Discard Changes");
        let cancel = NSString::alloc(nil).init_str("Cancel");
        let _: () = msg_send![alert, setMessageText: title];
        let _: () = msg_send![alert, setInformativeText: detail];
        let _: id = msg_send![alert, addButtonWithTitle: confirm];
        let _: id = msg_send![alert, addButtonWithTitle: cancel];
        let response: isize = msg_send![alert, runModal];
        let _: () = msg_send![alert, release];
        let _: () = msg_send![title, release];
        let _: () = msg_send![detail, release];
        let _: () = msg_send![confirm, release];
        let _: () = msg_send![cancel, release];
        pool.drain();
        response == 1000
    }
}

struct Forge {
    workspaces: WorkspaceManager,
    focus_handle: FocusHandle,
    show_workspace_sidebar: bool,
    show_info_panel: bool,
    workspace_sidebar_width: f32,
    info_panel_width: f32,
    sidebar_resize: Option<SidebarResize>,
    file_tree: Option<FileNode>,
    expanded_dirs: HashSet<PathBuf>,
    selected_file: Option<PathBuf>,
    palette_open: bool,
    palette_query: String,
    palette_selected: usize,
    active_view: ViewMode,
    editor: Option<forge_editor::Editor>,
    editor_pending: Option<char>,
    /// Monospace cell width, measured once. Font size is fixed in this pass,
    /// so this never needs invalidating; if font settings become dynamic this
    /// must be cleared when they change.
    char_width: Option<f32>,
    /// Built once when the palette opens rather than per keystroke: sourcing
    /// it walks the whole file tree and allocates a label per file.
    palette_cache: Vec<PaletteItem>,
    editor_scroll: UniformListScrollHandle,
    frame_stats: Option<FrameStats>,
    /// Handed to every spawned PTY so its reader thread can wake the UI.
    output_notifier: forge_terminal::OutputNotifier,
    info_tab: InfoTab,
    /// Index of the workspace being renamed, plus its in-progress text.
    renaming: Option<(usize, String)>,
    /// Monotonic request ids discard background results after a fast workspace
    /// switch, avoiding stale trees/history without blocking or cancellation.
    file_tree_request: u64,
    git_request: u64,
    sidebar_meta_refreshing: bool,
    sidebar_meta_pending: bool,
    workspace_add_in_flight: bool,
    /// Debounces background sidebar metadata work. Nothing that shells out or
    /// walks the filesystem is allowed to run on GPUI's render thread.
    last_meta_refresh: Instant,
    git_status: Option<forge_git::Status>,
    git_history: Vec<forge_git::Commit>,
    git_selection: Option<GitSelection>,
    git_changes: Vec<forge_git::FileChange>,
    git_detail_loading: bool,
    git_detail_request: u64,
    git_diff: Option<GitDiffView>,
    git_diff_request: u64,
    git_commit_message: String,
    git_filter: String,
    git_filter_visible: bool,
    git_input: Option<GitInput>,
    git_merge_collapsed: bool,
    git_staged_collapsed: bool,
    git_changes_collapsed: bool,
    git_history_collapsed: bool,
    git_operation_in_flight: bool,
    git_operation_error: Option<String>,
    git_failed_operation: Option<GitMutation>,
    processes: Vec<forge_proc::ProcessInfo>,
    /// Held only while the Processes tab is visible. Dropping it cancels the
    /// refresh loop, preserving the app's zero-wakeup idle behavior.
    process_task: Option<gpui::Task<()>>,
    github_state: GitHubState,
    /// Held only while a device-flow sign-in is in progress; dropping it
    /// cancels the poll loop, which is how "Cancel" in the Profile tab works.
    github_sign_in_task: Option<gpui::Task<()>>,
    github_avatar: Option<Arc<Image>>,
    update_state: UpdateState,
}

/// Frame instrumentation, enabled with `FORGE_FPS=1`. Reports achieved frame
/// rate and the slowest frame in each window, so render-side regressions show
/// up as a rising max rather than only as an average dip.
struct FrameStats {
    window_start: Instant,
    last_frame: Instant,
    frames: u32,
    max_frame: Duration,
    total_render: Duration,
}

impl FrameStats {
    fn new() -> Self {
        let now = Instant::now();
        Self {
            window_start: now,
            last_frame: now,
            frames: 0,
            max_frame: Duration::ZERO,
            total_render: Duration::ZERO,
        }
    }

    fn record(&mut self, render_time: Duration) {
        let now = Instant::now();
        let delta = now.duration_since(self.last_frame);
        self.last_frame = now;
        self.frames += 1;
        self.max_frame = self.max_frame.max(delta);
        self.total_render += render_time;

        let elapsed = now.duration_since(self.window_start);
        if elapsed >= Duration::from_secs(1) {
            let fps = self.frames as f64 / elapsed.as_secs_f64();
            let avg_render = self.total_render.as_secs_f64() * 1000.0 / self.frames.max(1) as f64;
            eprintln!(
                "forge/fps: {fps:6.1} fps | avg render {avg_render:5.2}ms | worst gap {:5.2}ms",
                self.max_frame.as_secs_f64() * 1000.0
            );
            self.window_start = now;
            self.frames = 0;
            self.max_frame = Duration::ZERO;
            self.total_render = Duration::ZERO;
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ViewMode {
    Terminal,
    Editor,
    Agents,
    Profile,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ResizeSide {
    Workspace,
    InfoPanel,
}

#[derive(Clone, Copy, Debug)]
struct SidebarResize {
    side: ResizeSide,
    anchor_x: f32,
    start_width: f32,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum GitSelection {
    Commit(String),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum GitInput {
    CommitMessage,
    Filter,
}

#[derive(Clone, Debug)]
struct GitDiscardTarget {
    path: String,
    previous_path: Option<String>,
    untracked: bool,
}

#[derive(Clone, Debug)]
enum GitMutation {
    Initialize,
    Stage {
        path: String,
        previous_path: Option<String>,
    },
    Unstage {
        path: String,
        previous_path: Option<String>,
    },
    StageAll,
    UnstageAll,
    Commit(String),
    SwitchBranch(String),
    Fetch,
    Pull,
    Push,
    StashAll,
    StashPop,
    Discard(GitDiscardTarget),
    DiscardAll(Vec<GitDiscardTarget>),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum GitSection {
    Merge,
    Staged,
    Changes,
    History,
}

struct GitDiffView {
    title: String,
    lines: Vec<String>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum InfoTab {
    Files,
    Git,
    Processes,
}

impl InfoTab {
    fn label(self) -> &'static str {
        match self {
            Self::Files => "Files",
            Self::Git => "Git",
            Self::Processes => "Processes",
        }
    }

    fn icon(self) -> &'static str {
        match self {
            Self::Files => "icons/folder.svg",
            Self::Git => "icons/git-branch.svg",
            Self::Processes => "icons/info.svg",
        }
    }
}

#[derive(Clone, Debug)]
enum GitHubState {
    Checking,
    SignedOut,
    SigningIn,
    AwaitingDevice(github::DeviceAuthorization),
    Connected(github::Account),
    Failed(String),
}

#[derive(Clone, Debug)]
enum UpdateState {
    Disabled,
    Checking,
    Current,
    Available(updater::Release),
    /// A newer edge release exists, but Homebrew owns this installation, so
    /// Forge only surfaces a read-only notice instead of an install button.
    HomebrewUpdateAvailable(updater::Release),
    Installing,
    Failed(String),
}

#[derive(Clone)]
enum PaletteAction {
    SelectWorkspace(usize),
    OpenFile(PathBuf),
}

struct PaletteItem {
    label: String,
    kind: &'static str,
    action: PaletteAction,
}

struct WorkspaceMetaUpdate {
    index: usize,
    process_name: Option<String>,
    current_path: Option<PathBuf>,
    git: forge_git::Summary,
}

impl Forge {
    fn new(cx: &mut Context<Self>, initial_paths: Vec<PathBuf>) -> Self {
        // Event-driven repaint. A timer poll caps the frame rate at its own
        // interval and still wakes on an idle shell; instead each PTY reader
        // thread signals here the moment it parses output.
        //
        // Capacity 1 + `try_send` gives free coalescing: while a wake is
        // already queued, further signals are dropped rather than queueing a
        // redundant repaint per output chunk.
        let (tx, mut rx) = mpsc::channel::<()>(1);
        let tx = Arc::new(Mutex::new(tx));
        let output_notifier: forge_terminal::OutputNotifier = Arc::new(move || {
            if let Ok(mut tx) = tx.lock() {
                let _ = tx.try_send(());
            }
        });

        let mut workspaces = WorkspaceManager::new();
        for path in initial_paths {
            if let Err(err) =
                workspaces.add(path.clone(), ROWS, COLS, Some(Arc::clone(&output_notifier)))
            {
                eprintln!(
                    "forge: failed to open workspace {}: {err:#}",
                    path.display()
                );
            }
        }

        cx.spawn(async move |this, cx| {
            while rx.next().await.is_some() {
                let alive = this
                    .update(cx, |forge, cx| {
                        let (visible_changed, changed_workspaces) = forge.poll_terminal_output();
                        if !changed_workspaces.is_empty() {
                            forge.refresh_sidebar_meta(changed_workspaces, false, cx);
                        }
                        if visible_changed {
                            cx.notify();
                        }
                    })
                    .is_ok();
                if !alive {
                    break;
                }
                // Yield the main-thread executor. Without this, a flooding
                // PTY refills the wake channel as fast as it drains and this
                // loop starves the presenter, requesting far more repaints
                // than ever get drawn (measured: 0.2fps).
                Timer::after(OUTPUT_WAKE_YIELD).await;
            }
        })
        .detach();

        let mut this = Self {
            workspaces,
            focus_handle: cx.focus_handle(),
            show_workspace_sidebar: true,
            show_info_panel: false,
            workspace_sidebar_width: SIDEBAR_WIDTH,
            info_panel_width: INFO_PANEL_WIDTH,
            sidebar_resize: None,
            file_tree: None,
            expanded_dirs: HashSet::new(),
            selected_file: None,
            palette_open: false,
            palette_query: String::new(),
            palette_selected: 0,
            active_view: ViewMode::Terminal,
            editor: None,
            editor_pending: None,
            char_width: None,
            palette_cache: Vec::new(),
            editor_scroll: UniformListScrollHandle::new(),
            frame_stats: std::env::var("FORGE_FPS").is_ok().then(FrameStats::new),
            output_notifier,
            info_tab: InfoTab::Files,
            renaming: None,
            file_tree_request: 0,
            git_request: 0,
            sidebar_meta_refreshing: false,
            sidebar_meta_pending: false,
            workspace_add_in_flight: false,
            last_meta_refresh: Instant::now() - SIDEBAR_META_INTERVAL,
            git_status: None,
            git_history: Vec::new(),
            git_selection: None,
            git_changes: Vec::new(),
            git_detail_loading: false,
            git_detail_request: 0,
            git_diff: None,
            git_diff_request: 0,
            git_commit_message: String::new(),
            git_filter: String::new(),
            git_filter_visible: false,
            git_input: None,
            git_merge_collapsed: false,
            git_staged_collapsed: false,
            git_changes_collapsed: false,
            git_history_collapsed: false,
            git_operation_in_flight: false,
            git_operation_error: None,
            git_failed_operation: None,
            processes: Vec::new(),
            process_task: None,
            github_state: GitHubState::Checking,
            github_sign_in_task: None,
            github_avatar: None,
            update_state: if updater::checks_enabled() {
                UpdateState::Checking
            } else {
                UpdateState::Disabled
            },
        };
        if !github::onboarding_completed() {
            this.active_view = ViewMode::Profile;
            let _ = github::complete_onboarding();
        }
        this.refresh_file_tree(cx);
        let all = (0..this.workspaces.workspaces.len()).collect();
        this.refresh_sidebar_meta(all, true, cx);
        this.refresh_github_account(cx);
        this.check_for_updates(cx);
        this
    }

    fn refresh_github_account(&mut self, cx: &mut Context<Self>) {
        self.github_state = GitHubState::Checking;
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_spawn(async move { github::lookup_account() })
                .await;
            let _ = this.update(cx, |forge, cx| {
                forge.github_state = match result {
                    Ok(github::AccountLookup::Connected(account)) => {
                        let _ = github::complete_onboarding();
                        forge.refresh_avatar(cx);
                        GitHubState::Connected(account)
                    }
                    Ok(github::AccountLookup::SignedOut) => GitHubState::SignedOut,
                    Ok(github::AccountLookup::Failed(message)) => GitHubState::Failed(message),
                    Err(error) => GitHubState::Failed(error.to_string()),
                };
                cx.notify();
            });
        })
        .detach();
    }

    /// Runs GitHub's OAuth device flow end to end: request a code, show it,
    /// poll until the browser step completes, then fetch the profile and
    /// store the token. Kept cancellable by holding the `Task` in
    /// `github_sign_in_task` — dropping it (see `cancel_github_sign_in`)
    /// stops the poll loop at its next await point.
    fn begin_github_sign_in(&mut self, cx: &mut Context<Self>) {
        if matches!(
            self.github_state,
            GitHubState::SigningIn | GitHubState::AwaitingDevice(_)
        ) {
            return;
        }
        self.github_state = GitHubState::SigningIn;
        cx.notify();

        let task = cx.spawn(async move |this, cx| {
            let device = cx
                .background_spawn(async move { github::request_device_authorization() })
                .await;
            let device = match device {
                Ok(device) => device,
                Err(error) => {
                    let _ = this.update(cx, |forge, cx| {
                        forge.github_sign_in_task = None;
                        forge.github_state = GitHubState::Failed(error.to_string());
                        cx.notify();
                    });
                    return;
                }
            };

            let shown = this.update(cx, |forge, cx| {
                forge.github_state = GitHubState::AwaitingDevice(device.clone());
                cx.notify();
            });
            if shown.is_err() {
                return;
            }

            let verification_uri = device.verification_uri.clone();
            cx.background_spawn(async move { github::open_verification_uri(&verification_uri) })
                .await;

            let mut interval = device.interval;
            let token = loop {
                if Instant::now() >= device.expires_at {
                    break Err(anyhow::anyhow!(
                        "The one-time code expired. Try connecting again."
                    ));
                }
                Timer::after(interval).await;
                let device_code = device.device_code.clone();
                let outcome = cx
                    .background_spawn(async move { github::poll_device_token(&device_code) })
                    .await;
                match outcome {
                    Ok(github::DevicePoll::Authorized(token)) => break Ok(token),
                    Ok(github::DevicePoll::Pending) => continue,
                    Ok(github::DevicePoll::SlowDown) => {
                        interval += Duration::from_secs(5);
                        continue;
                    }
                    Err(error) => break Err(error),
                }
            };

            let token = match token {
                Ok(token) => token,
                Err(error) => {
                    let _ = this.update(cx, |forge, cx| {
                        forge.github_sign_in_task = None;
                        forge.github_state = GitHubState::Failed(error.to_string());
                        cx.notify();
                    });
                    return;
                }
            };

            let result = cx
                .background_spawn(async move { github::complete_sign_in(&token) })
                .await;
            let _ = this.update(cx, |forge, cx| {
                forge.github_sign_in_task = None;
                forge.github_state = match result {
                    Ok(account) => {
                        let _ = github::complete_onboarding();
                        forge.refresh_avatar(cx);
                        GitHubState::Connected(account)
                    }
                    Err(error) => GitHubState::Failed(error.to_string()),
                };
                cx.notify();
            });
        });
        self.github_sign_in_task = Some(task);
    }

    /// Stops an in-progress device-flow sign-in. Dropping the task cancels
    /// its poll loop; GitHub expires the unused device code on its own.
    fn cancel_github_sign_in(&mut self, cx: &mut Context<Self>) {
        self.github_sign_in_task = None;
        self.github_state = GitHubState::SignedOut;
        cx.notify();
    }

    fn sign_out_github(&mut self, cx: &mut Context<Self>) {
        self.github_sign_in_task = None;
        self.github_avatar = None;
        self.github_state = GitHubState::Checking;
        cx.notify();
        cx.spawn(async move |this, cx| {
            let result = cx.background_spawn(async move { github::sign_out() }).await;
            let _ = this.update(cx, |forge, cx| {
                forge.github_state = match result {
                    Ok(()) => GitHubState::SignedOut,
                    Err(error) => GitHubState::Failed(error.to_string()),
                };
                cx.notify();
            });
        })
        .detach();
    }

    /// Fetches the avatar bitmap for the connected account. Best-effort: a
    /// failure here leaves the initials placeholder rather than blocking or
    /// disturbing `github_state`.
    fn refresh_avatar(&mut self, cx: &mut Context<Self>) {
        let GitHubState::Connected(account) = &self.github_state else {
            return;
        };
        let Some(url) = account.avatar_url.clone() else {
            return;
        };
        cx.spawn(async move |this, cx| {
            let fetched = cx
                .background_spawn(async move { github::fetch_avatar_bytes(&url) })
                .await;
            let Ok((content_type, bytes)) = fetched else {
                return;
            };
            let format = match content_type.as_str() {
                "image/jpeg" | "image/jpg" => ImageFormat::Jpeg,
                "image/webp" => ImageFormat::Webp,
                "image/gif" => ImageFormat::Gif,
                "image/bmp" => ImageFormat::Bmp,
                "image/tiff" => ImageFormat::Tiff,
                _ => ImageFormat::Png,
            };
            let image = Arc::new(Image::from_bytes(format, bytes));
            let _ = this.update(cx, |forge, cx| {
                forge.github_avatar = Some(image);
                cx.notify();
            });
        })
        .detach();
    }

    fn check_for_updates(&mut self, cx: &mut Context<Self>) {
        if !updater::checks_enabled() {
            self.update_state = UpdateState::Disabled;
            return;
        }
        self.update_state = UpdateState::Checking;
        cx.spawn(async move |this, cx| {
            let result = cx.background_spawn(async move { updater::check() }).await;
            let _ = this.update(cx, |forge, cx| {
                forge.update_state = match result {
                    Ok(Some(release)) => {
                        if updater::self_install_enabled() {
                            UpdateState::Available(release)
                        } else {
                            UpdateState::HomebrewUpdateAvailable(release)
                        }
                    }
                    Ok(None) => UpdateState::Current,
                    Err(error) => UpdateState::Failed(error.to_string()),
                };
                cx.notify();
            });
        })
        .detach();
    }

    fn install_update(&mut self, cx: &mut Context<Self>) {
        let UpdateState::Available(release) = &self.update_state else {
            return;
        };
        let release = release.clone();
        self.update_state = UpdateState::Installing;
        cx.notify();
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_spawn(async move { updater::install(&release) })
                .await;
            let _ = this.update(cx, |forge, cx| match result {
                Ok(()) => match updater::restart() {
                    Ok(()) => std::process::exit(0),
                    Err(error) => {
                        forge.update_state = UpdateState::Failed(error.to_string());
                        cx.notify();
                    }
                },
                Err(error) => {
                    forge.update_state = UpdateState::Failed(error.to_string());
                    cx.notify();
                }
            });
        })
        .detach();
    }

    fn select_workspace(&mut self, ix: usize, cx: &mut Context<Self>) {
        if ix == self.workspaces.active || ix >= self.workspaces.workspaces.len() {
            return;
        }
        self.workspaces.select(ix);
        self.git_diff = None;
        self.git_diff_request = self.git_diff_request.wrapping_add(1);
        // Paint the new terminal immediately. Trees and Git data arrive on
        // background workers and are guarded against rapid-switch races.
        self.refresh_file_tree(cx);
        self.refresh_sidebar_meta(vec![ix], true, cx);
        self.sync_info_tab(cx);
        cx.notify();
    }

    fn add_workspace(&mut self, cx: &mut Context<Self>) {
        if self.workspace_add_in_flight {
            return;
        }
        let Some(path) = std::env::var_os("HOME").map(PathBuf::from) else {
            return;
        };
        self.workspace_add_in_flight = true;
        cx.notify();

        let notifier = Arc::clone(&self.output_notifier);
        cx.spawn(async move |this, cx| {
            // PTY allocation and shell startup can involve filesystem and
            // login-shell work. Never make the event handler wait for it.
            let opened = cx
                .background_spawn(async move { Workspace::open(path, ROWS, COLS, Some(notifier)) })
                .await;
            let _ = this.update(cx, |forge, cx| {
                forge.workspace_add_in_flight = false;
                match opened {
                    Ok(workspace) => {
                        forge.workspaces.push(workspace);
                        let active = forge.workspaces.active;
                        forge.refresh_file_tree(cx);
                        forge.refresh_sidebar_meta(vec![active], true, cx);
                        forge.sync_info_tab(cx);
                    }
                    Err(err) => eprintln!("forge: failed to open workspace: {err:#}"),
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// Advance every pane's seen-generation, reporting whether any pane in the
    /// *visible* workspace produced output. Background workspaces still get
    /// their counters advanced so switching to them doesn't trigger a spurious
    /// repaint, but their output alone never forces one.
    fn poll_terminal_output(&mut self) -> (bool, Vec<usize>) {
        let active = self.workspaces.active;
        let mut visible_changed = false;
        let mut changed_workspaces = Vec::new();
        for (ix, ws) in self.workspaces.workspaces.iter_mut().enumerate() {
            let mut workspace_changed = false;
            for pane in ws.panes.iter_mut() {
                let generation = pane.terminal.generation();
                if generation != pane.last_seen_generation {
                    pane.last_seen_generation = generation;
                    workspace_changed = true;
                }
            }
            if workspace_changed {
                visible_changed |= ix == active;
                changed_workspaces.push(ix);
            }
        }
        (visible_changed, changed_workspaces)
    }

    /// Refresh process title, current directory, and compact Git state without
    /// ever blocking GPUI's event/render thread.
    fn refresh_sidebar_meta(
        &mut self,
        mut indices: Vec<usize>,
        force: bool,
        cx: &mut Context<Self>,
    ) {
        if self.sidebar_meta_refreshing {
            self.sidebar_meta_pending |= force;
            return;
        }
        indices.sort_unstable();
        indices.dedup();
        let targets = indices
            .into_iter()
            .filter_map(|index| {
                let ws = self.workspaces.workspaces.get(index)?;
                let pid = ws
                    .focused_pane()
                    .and_then(|pane| pane.terminal.foreground_pid());
                Some((index, pid, ws.current_path.clone()))
            })
            .collect::<Vec<_>>();
        if targets.is_empty() {
            return;
        }

        let delay = if force {
            Duration::ZERO
        } else {
            SIDEBAR_META_INTERVAL.saturating_sub(self.last_meta_refresh.elapsed())
        };
        self.sidebar_meta_refreshing = true;
        cx.spawn(async move |this, cx| {
            if !delay.is_zero() {
                Timer::after(delay).await;
            }
            let updates = cx
                .background_spawn(async move {
                    let mut probe = forge_proc::ForegroundProbe::new();
                    targets
                        .into_iter()
                        .map(|(index, pid, fallback_path)| {
                            let info = pid.and_then(|pid| probe.inspect(pid));
                            let current_path = info
                                .as_ref()
                                .and_then(|info| info.cwd.clone())
                                .or(Some(fallback_path));
                            let git = current_path
                                .as_deref()
                                .map(forge_git::summary)
                                .unwrap_or_default();
                            WorkspaceMetaUpdate {
                                index,
                                process_name: info.map(|info| info.name),
                                current_path,
                                git,
                            }
                        })
                        .collect::<Vec<_>>()
                })
                .await;

            let _ = this.update(cx, |forge, cx| {
                let active = forge.workspaces.active;
                let old_active_path = forge
                    .workspaces
                    .active_workspace()
                    .map(|ws| ws.current_path.clone());
                for update in updates {
                    if let Some(ws) = forge.workspaces.workspaces.get_mut(update.index) {
                        ws.set_process_name(update.process_name);
                        ws.set_current_path(update.current_path);
                        ws.branch = update.git.branch.clone();
                        ws.git = Some(update.git);
                    }
                }
                let active_path_changed = old_active_path
                    != forge
                        .workspaces
                        .workspaces
                        .get(active)
                        .map(|ws| ws.current_path.clone());
                forge.last_meta_refresh = Instant::now();
                forge.sidebar_meta_refreshing = false;

                if active_path_changed && forge.show_info_panel && forge.info_tab == InfoTab::Git {
                    forge.refresh_git_status(cx);
                }
                if std::mem::take(&mut forge.sidebar_meta_pending) {
                    let all = (0..forge.workspaces.workspaces.len()).collect();
                    forge.refresh_sidebar_meta(all, true, cx);
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn begin_rename(&mut self, ix: usize, cx: &mut Context<Self>) {
        if let Some(ws) = self.workspaces.workspaces.get(ix) {
            // Existing custom names are editable in place. Automatic path or
            // process labels start empty so the user doesn't have to erase a
            // generated value before typing the name they want.
            self.renaming = Some((ix, ws.custom_name.clone().unwrap_or_default()));
            cx.notify();
        }
    }

    fn commit_rename(&mut self, cx: &mut Context<Self>) {
        if let Some((ix, text)) = self.renaming.take() {
            if let Some(ws) = self.workspaces.workspaces.get_mut(ix) {
                ws.rename(text);
            }
        }
        cx.notify();
    }

    fn cancel_rename(&mut self, cx: &mut Context<Self>) {
        self.renaming = None;
        cx.notify();
    }

    fn handle_rename_key(&mut self, ks: &Keystroke, cx: &mut Context<Self>) {
        match ks.key.as_str() {
            "escape" => self.cancel_rename(cx),
            "enter" => self.commit_rename(cx),
            "backspace" => {
                if let Some((_, text)) = self.renaming.as_mut() {
                    text.pop();
                }
                cx.notify();
            }
            _ => {
                if !ks.modifiers.platform && !ks.modifiers.control {
                    if let Some(ch) = ks.key_char.as_ref() {
                        if let Some((_, text)) = self.renaming.as_mut() {
                            text.push_str(ch);
                        }
                        cx.notify();
                    }
                }
            }
        }
    }

    fn refresh_file_tree(&mut self, cx: &mut Context<Self>) {
        let needed = (self.show_info_panel && self.info_tab == InfoTab::Files) || self.palette_open;
        self.file_tree_request = self.file_tree_request.wrapping_add(1);
        let request = self.file_tree_request;
        if !needed {
            self.file_tree = None;
            return;
        }
        let Some(root) = self.workspaces.active_workspace().map(|ws| ws.path.clone()) else {
            self.file_tree = None;
            return;
        };
        if self.file_tree.as_ref().map(|tree| &tree.path) == Some(&root) {
            return;
        }

        self.file_tree = None;
        cx.spawn(async move |this, cx| {
            let scan_root = root.clone();
            let tree = cx
                .background_spawn(async move { forge_files::scan(&scan_root) })
                .await;
            let _ = this.update(cx, |forge, cx| {
                let still_active = forge
                    .workspaces
                    .active_workspace()
                    .is_some_and(|ws| ws.path == root);
                if forge.file_tree_request == request && still_active {
                    forge.file_tree = Some(tree);
                    if forge.palette_open {
                        forge.palette_cache = forge.palette_items();
                    }
                    cx.notify();
                }
            });
        })
        .detach();
    }

    fn toggle_workspace_sidebar(&mut self, cx: &mut Context<Self>) {
        self.show_workspace_sidebar = !self.show_workspace_sidebar;
        cx.notify();
    }

    fn toggle_info_panel(&mut self, cx: &mut Context<Self>) {
        self.show_info_panel = !self.show_info_panel;
        self.sync_info_tab(cx);
        cx.notify();
    }

    fn begin_sidebar_resize(&mut self, side: ResizeSide, anchor_x: f32, cx: &mut Context<Self>) {
        let start_width = match side {
            ResizeSide::Workspace => self.workspace_sidebar_width,
            ResizeSide::InfoPanel => self.info_panel_width,
        };
        self.sidebar_resize = Some(SidebarResize {
            side,
            anchor_x,
            start_width,
        });
        cx.notify();
    }

    fn update_sidebar_resize(&mut self, pointer_x: f32, cx: &mut Context<Self>) {
        let Some(drag) = self.sidebar_resize else {
            return;
        };
        let dx = pointer_x - drag.anchor_x;
        match drag.side {
            ResizeSide::Workspace => {
                self.workspace_sidebar_width =
                    (drag.start_width + dx).clamp(SIDEBAR_MIN_WIDTH, SIDEBAR_MAX_WIDTH);
            }
            ResizeSide::InfoPanel => {
                self.info_panel_width =
                    (drag.start_width - dx).clamp(INFO_PANEL_MIN_WIDTH, INFO_PANEL_MAX_WIDTH);
            }
        }
        cx.notify();
    }

    fn end_sidebar_resize(&mut self, cx: &mut Context<Self>) {
        if self.sidebar_resize.take().is_some() {
            cx.notify();
        }
    }

    fn render_resize_handle(&self, side: ResizeSide, cx: &mut Context<Self>) -> AnyElement {
        div()
            .id(match side {
                ResizeSide::Workspace => "resize-workspace-sidebar",
                ResizeSide::InfoPanel => "resize-info-panel",
            })
            .w(px(5.0))
            .h_full()
            .flex_shrink_0()
            .cursor(gpui::CursorStyle::ResizeLeftRight)
            .hover(|style| style.bg(theme::color(theme::surface::HOVER)))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, event: &gpui::MouseDownEvent, _, cx| {
                    this.begin_sidebar_resize(side, f32::from(event.position.x), cx);
                }),
            )
            .into_any_element()
    }

    fn select_info_tab(&mut self, tab: InfoTab, cx: &mut Context<Self>) {
        self.info_tab = tab;
        self.show_info_panel = true;
        self.sync_info_tab(cx);
        cx.notify();
    }

    /// Start/stop per-tab data sources so nothing polls while hidden.
    fn sync_info_tab(&mut self, cx: &mut Context<Self>) {
        let visible = self.show_info_panel;

        if visible && self.info_tab == InfoTab::Files {
            self.refresh_file_tree(cx);
        }

        if visible && self.info_tab == InfoTab::Git {
            self.refresh_git_status(cx);
        }

        if visible && self.info_tab == InfoTab::Processes {
            if self.process_task.is_none() {
                // One long-lived monitor shared with the background task:
                // sysinfo derives CPU% from the delta against its previous
                // sample, so recreating it per pass would peg CPU at 0.
                let monitor = Arc::new(Mutex::new(forge_proc::ProcessMonitor::new()));
                self.process_task = Some(cx.spawn(async move |this, cx| loop {
                    let roots = this
                        .update(cx, |forge, _| forge.pane_shell_pids())
                        .unwrap_or_default();
                    let monitor = Arc::clone(&monitor);
                    let sampled = cx
                        .background_spawn(async move {
                            monitor
                                .lock()
                                .map(|mut m| m.refresh(&roots))
                                .unwrap_or_default()
                        })
                        .await;
                    if this
                        .update(cx, |forge, cx| {
                            forge.processes = sampled;
                            cx.notify();
                        })
                        .is_err()
                    {
                        break;
                    }
                    Timer::after(PROCESS_REFRESH_INTERVAL).await;
                }));
            }
        } else {
            self.process_task = None;
        }
    }

    fn pane_shell_pids(&self) -> Vec<u32> {
        self.workspaces
            .active_workspace()
            .map(|ws| ws.panes.iter().filter_map(|p| p.terminal.pid()).collect())
            .unwrap_or_default()
    }

    fn refresh_git_status(&mut self, cx: &mut Context<Self>) {
        self.git_request = self.git_request.wrapping_add(1);
        let request = self.git_request;
        let Some(path) = self
            .workspaces
            .active_workspace()
            .map(|ws| ws.git_path().to_path_buf())
        else {
            self.git_status = None;
            self.git_history.clear();
            self.git_selection = None;
            self.git_changes.clear();
            self.git_detail_loading = false;
            return;
        };

        let same_repository = self
            .git_status
            .as_ref()
            .and_then(|status| status.root.as_ref())
            .is_some_and(|root| path.starts_with(root));
        if !same_repository {
            self.git_status = None;
            self.git_history.clear();
            self.git_selection = None;
            self.git_diff = None;
            self.git_diff_request = self.git_diff_request.wrapping_add(1);
            self.git_changes.clear();
            self.git_detail_loading = false;
            self.git_commit_message.clear();
            self.git_filter.clear();
            self.git_filter_visible = false;
            self.git_input = None;
            self.git_operation_error = None;
            self.git_failed_operation = None;
        }
        cx.spawn(async move |this, cx| {
            let query_path = path.clone();
            let (status, history) = cx
                .background_spawn(async move {
                    let status = forge_git::status(&query_path);
                    let history = if status.is_repo {
                        forge_git::history(&query_path, 50)
                    } else {
                        Vec::new()
                    };
                    (status, history)
                })
                .await;
            let _ = this.update(cx, |forge, cx| {
                let still_active = forge
                    .workspaces
                    .active_workspace()
                    .is_some_and(|ws| ws.git_path() == path);
                if forge.git_request == request && still_active {
                    forge.git_status = Some(status);
                    forge.git_history = history;
                    cx.notify();
                }
            });
        })
        .detach();
    }

    fn git_repository(&self) -> Option<PathBuf> {
        self.git_status
            .as_ref()
            .and_then(|status| status.root.clone())
            .or_else(|| {
                self.workspaces
                    .active_workspace()
                    .map(|workspace| workspace.git_path().to_path_buf())
            })
    }

    fn show_git_branch_menu(&mut self, cx: &mut Context<Self>) {
        let Some(status) = self.git_status.as_ref() else {
            return;
        };
        let branches = status.branches.clone();
        if branches.is_empty() {
            return;
        }
        let current = status.branch.clone();
        let items = branches
            .iter()
            .map(|branch| {
                Some(if Some(branch.as_str()) == current.as_deref() {
                    format!("✓  {branch}")
                } else {
                    branch.clone()
                })
            })
            .collect::<Vec<_>>();
        #[cfg(target_os = "macos")]
        if let Some(index) = show_git_native_menu(&items) {
            if let Some(branch) = branches.get(index) {
                if Some(branch.as_str()) != current.as_deref() {
                    self.run_git_mutation(GitMutation::SwitchBranch(branch.clone()), cx);
                }
            }
        }
    }

    fn show_git_more_menu(&mut self, cx: &mut Context<Self>) {
        let items = vec![
            Some("Fetch".to_string()),
            Some("Pull (Fast-forward Only)".to_string()),
            Some("Push".to_string()),
            None,
            Some("Stash All Changes".to_string()),
            Some("Pop Stash".to_string()),
            None,
            Some("Copy Repository Path".to_string()),
            Some("Refresh Git Status".to_string()),
        ];
        #[cfg(target_os = "macos")]
        if let Some(index) = show_git_native_menu(&items) {
            match index {
                0 => self.run_git_mutation(GitMutation::Fetch, cx),
                1 => self.run_git_mutation(GitMutation::Pull, cx),
                2 => self.run_git_mutation(GitMutation::Push, cx),
                4 => self.run_git_mutation(GitMutation::StashAll, cx),
                5 => self.run_git_mutation(GitMutation::StashPop, cx),
                7 => {
                    if let Some(repo) = self.git_repository() {
                        cx.write_to_clipboard(ClipboardItem::new_string(
                            repo.display().to_string(),
                        ));
                    }
                }
                8 => self.refresh_git_status(cx),
                _ => {}
            }
        }
    }

    fn toggle_git_filter(&mut self, cx: &mut Context<Self>) {
        self.git_filter_visible = !self.git_filter_visible;
        if self.git_filter_visible {
            self.git_input = Some(GitInput::Filter);
        } else {
            self.git_filter.clear();
            self.git_input = None;
        }
        cx.notify();
    }

    fn focus_git_input(&mut self, input: GitInput, cx: &mut Context<Self>) {
        self.git_input = Some(input);
        cx.notify();
    }

    fn handle_git_input_key(&mut self, ks: &Keystroke, cx: &mut Context<Self>) {
        let Some(input) = self.git_input else { return };
        match ks.key.as_str() {
            "escape" => {
                if input == GitInput::Filter {
                    self.git_filter_visible = false;
                    self.git_filter.clear();
                }
                self.git_input = None;
                cx.notify();
            }
            "backspace" => {
                match input {
                    GitInput::CommitMessage => {
                        self.git_commit_message.pop();
                    }
                    GitInput::Filter => {
                        self.git_filter.pop();
                    }
                }
                cx.notify();
            }
            "enter" if input == GitInput::CommitMessage && ks.modifiers.platform => {
                self.commit_git_message(cx);
            }
            "enter" if input == GitInput::CommitMessage => {
                self.git_commit_message.push('\n');
                cx.notify();
            }
            "v" if ks.modifiers.platform => {
                if let Some(text) = cx.read_from_clipboard().and_then(|item| item.text()) {
                    match input {
                        GitInput::CommitMessage => self.git_commit_message.push_str(&text),
                        GitInput::Filter => {
                            self.git_filter.push_str(&text.replace(['\r', '\n'], ""))
                        }
                    }
                    cx.notify();
                }
            }
            "c" if ks.modifiers.platform => {
                let text = match input {
                    GitInput::CommitMessage => &self.git_commit_message,
                    GitInput::Filter => &self.git_filter,
                };
                cx.write_to_clipboard(ClipboardItem::new_string(text.clone()));
            }
            "x" if ks.modifiers.platform => {
                let text = match input {
                    GitInput::CommitMessage => std::mem::take(&mut self.git_commit_message),
                    GitInput::Filter => std::mem::take(&mut self.git_filter),
                };
                cx.write_to_clipboard(ClipboardItem::new_string(text));
                cx.notify();
            }
            "a" if ks.modifiers.platform => {
                // The lightweight GPUI field has no selection model yet;
                // clearing here preserves the common Cmd-A then type workflow.
                match input {
                    GitInput::CommitMessage => self.git_commit_message.clear(),
                    GitInput::Filter => self.git_filter.clear(),
                }
                cx.notify();
            }
            _ if !ks.modifiers.platform && !ks.modifiers.control => {
                if let Some(text) = ks.key_char.as_ref() {
                    match input {
                        GitInput::CommitMessage => self.git_commit_message.push_str(text),
                        GitInput::Filter => self.git_filter.push_str(text),
                    }
                    cx.notify();
                }
            }
            _ => {}
        }
    }

    fn dismiss_git_error(&mut self, cx: &mut Context<Self>) {
        self.git_operation_error = None;
        self.git_failed_operation = None;
        cx.notify();
    }

    fn retry_git_operation(&mut self, cx: &mut Context<Self>) {
        if let Some(operation) = self.git_failed_operation.clone() {
            self.run_git_mutation(operation, cx);
        }
    }

    fn blur_git_input(&mut self, cx: &mut Context<Self>) {
        if self.git_input.take().is_some() {
            cx.notify();
        }
    }

    fn toggle_git_section(&mut self, section: GitSection, cx: &mut Context<Self>) {
        let collapsed = match section {
            GitSection::Merge => &mut self.git_merge_collapsed,
            GitSection::Staged => &mut self.git_staged_collapsed,
            GitSection::Changes => &mut self.git_changes_collapsed,
            GitSection::History => &mut self.git_history_collapsed,
        };
        *collapsed = !*collapsed;
        cx.notify();
    }

    fn commit_git_message(&mut self, cx: &mut Context<Self>) {
        let message = self.git_commit_message.trim();
        let can_commit = !message.is_empty()
            && self
                .git_status
                .as_ref()
                .is_some_and(|status| status.staged_count() > 0);
        if can_commit {
            self.run_git_mutation(GitMutation::Commit(message.to_string()), cx);
        }
    }

    fn run_git_mutation(&mut self, mutation: GitMutation, cx: &mut Context<Self>) {
        if self.git_operation_in_flight {
            return;
        }
        #[cfg(target_os = "macos")]
        match &mutation {
            GitMutation::Discard(target) if !confirm_git_discard(&target.path, 1) => {
                return;
            }
            GitMutation::DiscardAll(targets)
                if !targets.is_empty() && !confirm_git_discard("changes", targets.len()) =>
            {
                return;
            }
            _ => {}
        }
        let Some(repo) = self.git_repository() else {
            return;
        };
        self.git_operation_in_flight = true;
        self.git_operation_error = None;
        self.git_failed_operation = None;
        cx.notify();

        cx.spawn(async move |this, cx| {
            let operation = mutation.clone();
            let operation_repo = repo.clone();
            let result = cx
                .background_spawn(async move {
                    match &operation {
                        GitMutation::Initialize => forge_git::initialize(&operation_repo),
                        GitMutation::Stage {
                            path,
                            previous_path,
                        } => {
                            forge_git::stage_entry(&operation_repo, path, previous_path.as_deref())
                        }
                        GitMutation::Unstage {
                            path,
                            previous_path,
                        } => forge_git::unstage_entry(
                            &operation_repo,
                            path,
                            previous_path.as_deref(),
                        ),
                        GitMutation::StageAll => forge_git::stage_all(&operation_repo),
                        GitMutation::UnstageAll => forge_git::unstage_all(&operation_repo),
                        GitMutation::Commit(message) => {
                            forge_git::commit_staged(&operation_repo, message)
                        }
                        GitMutation::SwitchBranch(branch) => {
                            forge_git::switch_branch(&operation_repo, branch)
                        }
                        GitMutation::Fetch => forge_git::fetch(&operation_repo),
                        GitMutation::Pull => forge_git::pull_fast_forward(&operation_repo),
                        GitMutation::Push => forge_git::push(&operation_repo),
                        GitMutation::StashAll => forge_git::stash_all(&operation_repo),
                        GitMutation::StashPop => forge_git::stash_pop(&operation_repo),
                        GitMutation::Discard(target) => forge_git::discard_worktree(
                            &operation_repo,
                            &target.path,
                            target.previous_path.as_deref(),
                            target.untracked,
                        ),
                        GitMutation::DiscardAll(targets) => targets.iter().try_for_each(|target| {
                            forge_git::discard_worktree(
                                &operation_repo,
                                &target.path,
                                target.previous_path.as_deref(),
                                target.untracked,
                            )
                        }),
                    }
                })
                .await;
            let _ = this.update(cx, |forge, cx| {
                forge.git_operation_in_flight = false;
                match result {
                    Ok(()) => {
                        forge.git_operation_error = None;
                        forge.git_failed_operation = None;
                        if matches!(mutation, GitMutation::Commit(_)) {
                            forge.git_commit_message.clear();
                            forge.git_input = None;
                        }
                        let still_active =
                            forge.git_repository().is_some_and(|active| active == repo);
                        if still_active {
                            forge.refresh_git_status(cx);
                        }
                    }
                    Err(error) => {
                        forge.git_operation_error = Some(error.message);
                        forge.git_failed_operation = Some(mutation);
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn open_working_git_diff(&mut self, path: String, staged: bool, cx: &mut Context<Self>) {
        self.git_input = None;
        let Some(repo) = self.git_repository() else {
            return;
        };
        let untracked = !staged
            && self.git_status.as_ref().is_some_and(|status| {
                status.entries.iter().any(|entry| {
                    entry.path == path && entry.unstaged == Some(forge_git::Change::Untracked)
                })
            });
        let title = format!(
            "{} — {}",
            path,
            if staged { "Staged Changes" } else { "Changes" }
        );
        self.git_diff_request = self.git_diff_request.wrapping_add(1);
        let request = self.git_diff_request;
        cx.spawn(async move |this, cx| {
            let query_path = path.clone();
            let text = cx
                .background_spawn(async move {
                    if staged {
                        forge_git::staged_diff(&repo, &query_path)
                    } else {
                        forge_git::working_diff(&repo, &query_path, untracked)
                    }
                })
                .await;
            let _ = this.update(cx, |forge, cx| {
                if forge.git_diff_request == request {
                    forge.git_diff = Some(GitDiffView {
                        title,
                        lines: text.lines().map(str::to_string).collect(),
                    });
                    forge.active_view = ViewMode::Editor;
                    cx.notify();
                }
            });
        })
        .detach();
    }

    fn select_git_commit(&mut self, id: String, cx: &mut Context<Self>) {
        self.git_input = None;
        let selection = GitSelection::Commit(id.clone());
        if self.git_selection.as_ref() == Some(&selection) {
            self.git_selection = None;
            self.git_changes.clear();
            self.git_detail_loading = false;
            self.git_detail_request = self.git_detail_request.wrapping_add(1);
            cx.notify();
            return;
        }

        let Some(commit) = self
            .git_history
            .iter()
            .find(|commit| commit.id == id)
            .cloned()
        else {
            return;
        };
        let Some(path) = self
            .workspaces
            .active_workspace()
            .map(|workspace| workspace.git_path().to_path_buf())
        else {
            return;
        };
        self.git_selection = Some(selection);
        self.git_changes.clear();
        self.git_detail_loading = true;
        self.git_detail_request = self.git_detail_request.wrapping_add(1);
        let request = self.git_detail_request;
        cx.notify();

        cx.spawn(async move |this, cx| {
            let changes = cx
                .background_spawn(async move { forge_git::commit_changes(&path, &commit) })
                .await;
            let _ = this.update(cx, |forge, cx| {
                if forge.git_detail_request == request
                    && forge.git_selection == Some(GitSelection::Commit(id))
                {
                    forge.git_changes = changes;
                    forge.git_detail_loading = false;
                    cx.notify();
                }
            });
        })
        .detach();
    }

    fn open_git_diff(&mut self, path: String, cx: &mut Context<Self>) {
        self.git_input = None;
        let Some(repo) = self
            .workspaces
            .active_workspace()
            .map(|workspace| workspace.git_path().to_path_buf())
        else {
            return;
        };
        let selection = self.git_selection.clone();
        let untracked = self.git_status.as_ref().is_some_and(|status| {
            status.entries.iter().any(|entry| {
                entry.path == path && entry.unstaged == Some(forge_git::Change::Untracked)
            })
        });
        let commit = match &selection {
            Some(GitSelection::Commit(id)) => self
                .git_history
                .iter()
                .find(|commit| &commit.id == id)
                .cloned(),
            _ => None,
        };
        let title = match &commit {
            Some(commit) => format!("{} — {}", path, commit.short_id),
            None => format!("{} — Working Tree", path),
        };
        self.git_diff_request = self.git_diff_request.wrapping_add(1);
        let request = self.git_diff_request;

        cx.spawn(async move |this, cx| {
            let query_path = path.clone();
            let text = cx
                .background_spawn(async move {
                    if let Some(commit) = commit {
                        forge_git::commit_diff(&repo, &commit, &query_path)
                    } else {
                        forge_git::working_diff(&repo, &query_path, untracked)
                    }
                })
                .await;
            let _ = this.update(cx, |forge, cx| {
                if forge.git_diff_request == request {
                    forge.git_diff = Some(GitDiffView {
                        title,
                        lines: text.lines().map(str::to_string).collect(),
                    });
                    forge.active_view = ViewMode::Editor;
                    forge.editor_scroll = UniformListScrollHandle::new();
                    cx.notify();
                }
            });
        })
        .detach();
    }

    fn toggle_dir(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        if !self.expanded_dirs.remove(&path) {
            self.expanded_dirs.insert(path);
        }
        cx.notify();
    }

    fn open_file(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        match forge_editor::Editor::open(path.clone()) {
            Ok(editor) => {
                self.git_diff = None;
                self.editor = Some(editor);
                self.active_view = ViewMode::Editor;
                self.selected_file = Some(path);
            }
            Err(err) => eprintln!("forge: failed to open {}: {err:#}", path.display()),
        }
        cx.notify();
    }

    fn handle_key_down(
        &mut self,
        event: &KeyDownEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let ks = &event.keystroke;

        // Rename captures all input while active, so typing a name can't
        // leak through to the terminal.
        if self.renaming.is_some() {
            return self.handle_rename_key(ks, cx);
        }

        if self.palette_open {
            return self.handle_palette_key(ks, cx);
        }

        if self.git_input.is_some() {
            return self.handle_git_input_key(ks, cx);
        }

        if ks.modifiers.platform {
            match ks.key.as_str() {
                "k" => return self.open_palette(cx),
                "r" | "R" => {
                    let active = self.workspaces.active;
                    return self.begin_rename(active, cx);
                }
                "1" => return self.set_active_view(ViewMode::Terminal, cx),
                "2" => return self.set_active_view(ViewMode::Editor, cx),
                "3" => return self.set_active_view(ViewMode::Agents, cx),
                "4" => return self.set_active_view(ViewMode::Profile, cx),
                "e" => return self.toggle_primary_view(cx),
                "d" if ks.modifiers.shift => return self.split_active(Layout::Column, cx),
                "d" => return self.split_active(Layout::Row, cx),
                "w" => return self.close_focused_pane(cx),
                "]" => return self.focus_next_pane(cx),
                "[" => return self.focus_prev_pane(cx),
                _ => {}
            }
        }

        if should_close_editor(self.active_view, ks.key.as_str()) {
            return self.close_editor(cx);
        }

        if self.active_view == ViewMode::Editor {
            if self.git_diff.is_some() {
                return;
            }
            return self.handle_editor_key(ks, cx);
        }

        if let Some(bytes) = translate_keystroke(ks) {
            if let Some(ws) = self.workspaces.active_workspace_mut() {
                if let Some(pane) = ws.focused_pane_mut() {
                    let _ = pane.terminal.write_input(&bytes);
                }
            }
        }
        self.scroll_cursor_into_view();
        cx.notify();
    }

    fn set_active_view(&mut self, view: ViewMode, cx: &mut Context<Self>) {
        self.active_view = view;
        cx.notify();
    }

    fn close_editor(&mut self, cx: &mut Context<Self>) {
        self.editor = None;
        self.git_diff = None;
        self.selected_file = None;
        self.editor_pending = None;
        self.editor_scroll = UniformListScrollHandle::new();
        self.active_view = ViewMode::Terminal;
        cx.notify();
    }

    /// Fast two-surface switch, analogous to toggling between source and its
    /// running program. Cmd-1/Cmd-2 remain available for direct selection.
    fn toggle_primary_view(&mut self, cx: &mut Context<Self>) {
        self.set_active_view(next_primary_view(self.active_view), cx);
    }

    /// Keep the cursor line inside the virtualized viewport after any motion.
    fn scroll_cursor_into_view(&self) {
        if let Some(editor) = &self.editor {
            let (line, _) = editor.cursor_line_col();
            self.editor_scroll
                .scroll_to_item(line, ScrollStrategy::Center);
        }
    }

    fn handle_editor_key(&mut self, ks: &Keystroke, cx: &mut Context<Self>) {
        let Some(editor) = self.editor.as_mut() else {
            return;
        };
        let symbol = ks.key_char.clone().unwrap_or_else(|| ks.key.clone());

        match editor.mode {
            forge_editor::Mode::Insert => match ks.key.as_str() {
                "escape" => editor.exit_insert(),
                "enter" => editor.insert_newline(),
                "backspace" => editor.backspace(),
                "tab" => {
                    editor.insert_char(' ');
                    editor.insert_char(' ');
                }
                _ => {
                    if !ks.modifiers.platform && !ks.modifiers.control {
                        if let Some(ch) = ks.key_char.as_ref().and_then(|s| s.chars().next()) {
                            editor.insert_char(ch);
                        }
                    }
                }
            },
            forge_editor::Mode::Command => match ks.key.as_str() {
                "escape" => editor.exit_command(),
                "enter" => {
                    if editor.execute_command() == forge_editor::CommandOutcome::Quit {
                        self.active_view = ViewMode::Terminal;
                    }
                }
                "backspace" => editor.command_backspace(),
                _ => {
                    if let Some(ch) = ks.key_char.as_ref().and_then(|s| s.chars().next()) {
                        editor.command_push(ch);
                    }
                }
            },
            forge_editor::Mode::Normal => {
                if ks.key.as_str() == "escape" {
                    self.editor_pending = None;
                } else if let Some(pending) = self.editor_pending.take() {
                    match (pending, symbol.as_str()) {
                        ('d', "d") => editor.delete_line(),
                        ('y', "y") => editor.yank_line(),
                        ('g', "g") => editor.move_doc_start(),
                        _ => {}
                    }
                } else {
                    match symbol.as_str() {
                        "h" | "left" => editor.move_left(),
                        "l" | "right" => editor.move_right(),
                        "j" | "down" => editor.move_down(),
                        "k" | "up" => editor.move_up(),
                        "0" => editor.move_line_start(),
                        "$" => editor.move_line_end(),
                        "g" => self.editor_pending = Some('g'),
                        "G" => editor.move_doc_end(),
                        "i" => editor.enter_insert(),
                        "a" => editor.enter_insert_after(),
                        "o" => editor.enter_insert_line_below(),
                        "O" => editor.enter_insert_line_above(),
                        "x" => editor.delete_char(),
                        "d" => self.editor_pending = Some('d'),
                        "y" => self.editor_pending = Some('y'),
                        "p" => editor.paste_after(),
                        ":" => editor.enter_command(),
                        _ => {}
                    }
                }
            }
        }
        cx.notify();
    }

    fn split_active(&mut self, layout: Layout, cx: &mut Context<Self>) {
        let notifier = Arc::clone(&self.output_notifier);
        if let Some(ws) = self.workspaces.active_workspace_mut() {
            if let Err(err) = ws.split(layout, ROWS, COLS, Some(notifier)) {
                eprintln!("forge: failed to split pane: {err:#}");
            }
        }
        cx.notify();
    }

    fn close_focused_pane(&mut self, cx: &mut Context<Self>) {
        if let Some(ws) = self.workspaces.active_workspace_mut() {
            ws.close_focused();
        }
        cx.notify();
    }

    fn focus_next_pane(&mut self, cx: &mut Context<Self>) {
        if let Some(ws) = self.workspaces.active_workspace_mut() {
            ws.focus_next();
        }
        cx.notify();
    }

    fn focus_prev_pane(&mut self, cx: &mut Context<Self>) {
        if let Some(ws) = self.workspaces.active_workspace_mut() {
            ws.focus_prev();
        }
        cx.notify();
    }

    fn open_palette(&mut self, cx: &mut Context<Self>) {
        self.palette_open = true;
        self.palette_query.clear();
        self.palette_selected = 0;
        self.palette_cache = self.palette_items();
        self.refresh_file_tree(cx);
        cx.notify();
    }

    fn close_palette(&mut self, cx: &mut Context<Self>) {
        self.palette_open = false;
        self.palette_query.clear();
        self.palette_selected = 0;
        self.palette_cache = Vec::new();
        cx.notify();
    }

    fn palette_items(&self) -> Vec<PaletteItem> {
        let mut items = Vec::new();
        for (ix, ws) in self.workspaces.workspaces.iter().enumerate() {
            items.push(PaletteItem {
                label: ws.name.clone(),
                kind: "WORKSPACE",
                action: PaletteAction::SelectWorkspace(ix),
            });
        }
        if let Some(root) = &self.file_tree {
            collect_files(root, &root.path, &mut items);
        }
        items
    }

    /// Filter the cached item list. Returns indices into `palette_cache` so
    /// callers can resolve an action without cloning every candidate.
    fn filtered_palette_indices(&self) -> Vec<usize> {
        let query = self.palette_query.to_lowercase();
        self.palette_cache
            .iter()
            .enumerate()
            .filter(|(_, item)| query.is_empty() || item.label.to_lowercase().contains(&query))
            .map(|(ix, _)| ix)
            .take(20)
            .collect()
    }

    fn handle_palette_key(&mut self, ks: &Keystroke, cx: &mut Context<Self>) {
        match ks.key.as_str() {
            "escape" => self.close_palette(cx),
            "enter" => self.activate_palette_selection(cx),
            "up" => {
                self.palette_selected = self.palette_selected.saturating_sub(1);
                cx.notify();
            }
            "down" => {
                let count = self.filtered_palette_indices().len();
                if count > 0 {
                    self.palette_selected = (self.palette_selected + 1).min(count - 1);
                }
                cx.notify();
            }
            "backspace" => {
                self.palette_query.pop();
                self.palette_selected = 0;
                cx.notify();
            }
            _ => {
                if !ks.modifiers.platform && !ks.modifiers.control {
                    if let Some(ch) = ks.key_char.as_ref() {
                        self.palette_query.push_str(ch);
                        self.palette_selected = 0;
                        cx.notify();
                    }
                }
            }
        }
    }

    fn activate_palette_selection(&mut self, cx: &mut Context<Self>) {
        let action = self
            .filtered_palette_indices()
            .get(self.palette_selected)
            .and_then(|&ix| self.palette_cache.get(ix))
            .map(|item| item.action.clone());

        if let Some(action) = action {
            match action {
                PaletteAction::SelectWorkspace(ix) => self.select_workspace(ix, cx),
                PaletteAction::OpenFile(path) => {
                    self.show_info_panel = true;
                    let root = self.workspaces.active_workspace().map(|ws| ws.path.clone());
                    let mut parent = path.parent().map(|p| p.to_path_buf());
                    while let Some(dir) = parent {
                        self.expanded_dirs.insert(dir.clone());
                        if root.as_deref() == Some(dir.as_path()) {
                            break;
                        }
                        parent = dir.parent().map(|p| p.to_path_buf());
                    }
                    self.open_file(path, cx);
                }
            }
        }
        self.close_palette(cx);
    }

    fn render_sidebar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let active = self.workspaces.active;
        let items = self
            .workspaces
            .workspaces
            .iter()
            .enumerate()
            .map(|(ix, ws)| {
                let selected = ix == active;
                let renaming = self.renaming.as_ref().filter(|(r, _)| *r == ix);
                let label = ws.sidebar_label(PATH_MAX_CHARS);
                let full_path = forge_workspace::full_display_path(&ws.path, PATH_MAX_CHARS);
                let git = ws.git.clone();

                div()
                    .id(("workspace", ix))
                    .flex()
                    .flex_col()
                    .flex_shrink_0()
                    .justify_center()
                    .gap(px(theme::space::XS))
                    .w_full()
                    .h(px(theme::row::WORKSPACE_HEIGHT))
                    .px(px(theme::space::ML))
                    .py(px(theme::space::MD))
                    .rounded(px(theme::radius::MD))
                    .cursor_pointer()
                    .when(selected, |d| d.bg(theme::color(theme::surface::ACTIVE)))
                    .when(!selected, |d| {
                        d.hover(|s| s.bg(theme::color(theme::surface::HOVER)))
                    })
                    .child(match renaming {
                        Some((_, text)) => div()
                            .w_full()
                            .min_w(px(0.0))
                            .truncate()
                            .h(px(20.0))
                            .px(px(theme::space::XS))
                            .rounded(px(theme::radius::SM))
                            .bg(theme::color(theme::surface::BASE))
                            .border_1()
                            .border_color(theme::color(theme::ACCENT))
                            .font(mono_font())
                            .text_size(px(theme::font_size::MD))
                            .text_color(theme::color(theme::text::DEFAULT))
                            .child(format!("{text}\u{258f}"))
                            .into_any_element(),
                        None => div()
                            .w_full()
                            .font(mono_font())
                            .text_size(px(theme::font_size::MD))
                            .font_weight(FontWeight::MEDIUM)
                            .text_color(theme::color(theme::text::DEFAULT))
                            .child(label)
                            .into_any_element(),
                    })
                    .child(git_line(git.as_ref(), selected))
                    .child(
                        div()
                            .w_full()
                            .font(mono_font())
                            .text_size(px(theme::font_size::SM))
                            .text_color(theme::color(theme::text::DIM))
                            .child(full_path),
                    )
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this, event: &gpui::MouseDownEvent, _, cx| {
                            if event.click_count >= 2 {
                                this.begin_rename(ix, cx);
                            } else {
                                this.select_workspace(ix, cx);
                            }
                        }),
                    )
                    .on_mouse_down(
                        MouseButton::Right,
                        cx.listener(move |this, _, _, cx| {
                            this.select_workspace(ix, cx);
                            #[cfg(target_os = "macos")]
                            if show_workspace_context_menu() {
                                this.begin_rename(ix, cx);
                            }
                            #[cfg(not(target_os = "macos"))]
                            this.begin_rename(ix, cx);
                        }),
                    )
            })
            .collect::<Vec<_>>();

        div()
            .flex()
            .flex_col()
            .w(px(self.workspace_sidebar_width))
            .h_full()
            .bg(theme::color(theme::surface::BASE))
            .border_r_1()
            .border_color(theme::color(theme::border::DEFAULT))
            .child(
                div()
                    .id("workspace-list")
                    .flex_1()
                    .min_h(px(0.0))
                    .overflow_y_scroll()
                    .flex()
                    .flex_col()
                    .gap(px(theme::space::XXS))
                    .px(px(theme::space::MD))
                    .py(px(theme::space::MD))
                    .children(items),
            )
            .child(self.render_sidebar_footer(cx))
    }

    fn render_sidebar_footer(&self, cx: &mut Context<Self>) -> AnyElement {
        let identity: AnyElement = match &self.github_state {
            GitHubState::Connected(account) => div()
                .flex()
                .flex_row()
                .items_center()
                .min_w(px(0.0))
                .gap(px(theme::space::SM))
                .child(render_avatar(
                    self.github_avatar.as_ref(),
                    &account.login,
                    20.0,
                ))
                .child(
                    div()
                        .min_w(px(0.0))
                        .truncate()
                        .text_size(px(theme::font_size::SM))
                        .text_color(theme::color(theme::status::DONE))
                        .child(format!("@{}", account.login)),
                )
                .into_any_element(),
            GitHubState::Checking => sidebar_status_text("Checking GitHub…", theme::text::DIM),
            GitHubState::SignedOut => {
                sidebar_status_text("Sign in with GitHub", theme::text::MUTED)
            }
            GitHubState::SigningIn => {
                sidebar_status_text("Starting sign-in…", theme::status::RUNNING)
            }
            GitHubState::AwaitingDevice(_) => {
                sidebar_status_text("Enter the one-time code…", theme::status::RUNNING)
            }
            GitHubState::Failed(_) => {
                sidebar_status_text("GitHub connection failed", theme::status::ERROR)
            }
        };

        let account = div()
            .id("github-account")
            .flex()
            .items_center()
            .min_w(px(0.0))
            .h(px(30.0))
            .px(px(theme::space::MD))
            .rounded(px(theme::radius::SM))
            .cursor_pointer()
            .hover(|style| style.bg(theme::color(theme::surface::HOVER)))
            .child(identity)
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, _, cx| this.set_active_view(ViewMode::Profile, cx)),
            );

        let update = match &self.update_state {
            UpdateState::Available(_) => Some(
                div()
                    .id("install-update")
                    .flex()
                    .items_center()
                    .h(px(30.0))
                    .px(px(theme::space::MD))
                    .rounded(px(theme::radius::SM))
                    .cursor_pointer()
                    .text_size(px(theme::font_size::SM))
                    .text_color(theme::color(theme::status::DONE))
                    .hover(|style| style.bg(theme::color(theme::surface::HOVER)))
                    .child("Update available — restart")
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|this, _, _, cx| this.install_update(cx)),
                    )
                    .into_any_element(),
            ),
            UpdateState::HomebrewUpdateAvailable(release) => Some(
                div()
                    .id("homebrew-update-notice")
                    .flex()
                    .items_center()
                    .min_w(px(0.0))
                    .h(px(30.0))
                    .px(px(theme::space::MD))
                    .rounded(px(theme::radius::SM))
                    .cursor_pointer()
                    .truncate()
                    .text_size(px(theme::font_size::SM))
                    .text_color(theme::color(theme::status::DONE))
                    .hover(|style| style.bg(theme::color(theme::surface::HOVER)))
                    .child(format!(
                        "Update available ({}) — click to copy `brew upgrade`",
                        &release.revision[..release.revision.len().min(7)]
                    ))
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|_, _, _, cx| {
                            cx.write_to_clipboard(ClipboardItem::new_string(
                                "brew upgrade --cask forge-app".to_string(),
                            ));
                        }),
                    )
                    .into_any_element(),
            ),
            UpdateState::Installing => Some(
                div()
                    .h(px(30.0))
                    .px(px(theme::space::MD))
                    .flex()
                    .items_center()
                    .text_size(px(theme::font_size::SM))
                    .text_color(theme::color(theme::status::RUNNING))
                    .child("Installing update…")
                    .into_any_element(),
            ),
            UpdateState::Failed(message) => Some(
                div()
                    .id("retry-update")
                    .h(px(30.0))
                    .px(px(theme::space::MD))
                    .flex()
                    .items_center()
                    .min_w(px(0.0))
                    .truncate()
                    .rounded(px(theme::radius::SM))
                    .cursor_pointer()
                    .text_size(px(theme::font_size::SM))
                    .text_color(theme::color(theme::status::ERROR))
                    .hover(|style| style.bg(theme::color(theme::surface::HOVER)))
                    .child(format!("Update failed — {message}"))
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|this, _, _, cx| this.check_for_updates(cx)),
                    )
                    .into_any_element(),
            ),
            UpdateState::Disabled | UpdateState::Checking | UpdateState::Current => None,
        };

        div()
            .flex()
            .flex_col()
            .flex_shrink_0()
            .gap(px(theme::space::XXS))
            .p(px(theme::space::MD))
            .border_t_1()
            .border_color(theme::color(theme::border::SUBTLE))
            .child(account)
            .children(update)
            .into_any_element()
    }

    /// Resize every pane in the active workspace to match its equal share of
    /// the window's current content area, measured in terminal cells.
    /// Approximates a monospace char cell from a single shaped glyph; a full
    /// per-frame text-system measurement is cheap enough at this scale.
    /// Draggable, unequal split sizing is a fast-follow.
    fn sync_terminal_size(&mut self, window: &Window) {
        let char_width = *self
            .char_width
            .get_or_insert_with(|| measure_char_width(window));
        let viewport = window.viewport_size();
        let sidebar_width = if self.show_workspace_sidebar {
            self.workspace_sidebar_width
        } else {
            0.0
        };
        let info_panel_width = if self.show_info_panel {
            self.info_panel_width
        } else {
            0.0
        };
        let available_width =
            (f32::from(viewport.width) - sidebar_width - info_panel_width - PADDING * 2.0).max(0.0);
        let available_height =
            (f32::from(viewport.height) - TOP_BAR_HEIGHT - PADDING * 2.0).max(0.0);

        if let Some(ws) = self.workspaces.active_workspace_mut() {
            let n = (ws.panes.len().max(1)) as f32;
            let (pane_width, pane_height) = match ws.layout {
                Layout::Row => (available_width / n, available_height),
                Layout::Column => (available_width, available_height / n),
            };
            let cols = ((pane_width / char_width).floor() as u16).max(MIN_COLS);
            let rows = ((pane_height / LINE_HEIGHT).floor() as u16).max(MIN_ROWS);

            for pane in ws.panes.iter_mut() {
                if pane.terminal.size() != (rows, cols) {
                    let _ = pane.terminal.resize(rows, cols);
                }
            }
        }
    }

    fn render_terminal(&self) -> impl IntoElement {
        let Some(ws) = self.workspaces.active_workspace() else {
            return div()
                .flex_1()
                .p(px(16.0))
                .text_color(theme::color(theme::text::DIM))
                .child("No workspace open")
                .into_any_element();
        };

        let base_font = mono_font();
        let is_row = ws.layout == Layout::Row;
        let show_focus = ws.panes.len() > 1;
        let panes = ws
            .panes
            .iter()
            .enumerate()
            .map(|(ix, pane)| render_pane(pane, ix == ws.focused, show_focus, &base_font));

        div()
            .flex_1()
            .h_full()
            .flex()
            .when(is_row, |d| d.flex_row())
            .when(!is_row, |d| d.flex_col())
            .gap(px(if show_focus { 1.0 } else { 0.0 }))
            .bg(theme::color(theme::border::DEFAULT))
            .children(panes)
            .into_any_element()
    }

    fn render_git_diff(&self, diff: &GitDiffView, cx: &mut Context<Self>) -> AnyElement {
        let line_count = diff.lines.len().max(1);
        let this = cx.entity();
        let body = uniform_list("git-diff-lines", line_count, move |range, _, cx| {
            let forge = this.read(cx);
            let lines = forge.git_diff.as_ref().map(|diff| &diff.lines);
            range
                .map(|line| {
                    let text = lines
                        .and_then(|lines| lines.get(line))
                        .cloned()
                        .unwrap_or_else(|| "No textual changes".into());
                    let added = text.starts_with('+') && !text.starts_with("+++");
                    let removed = text.starts_with('-') && !text.starts_with("---");
                    let hunk = text.starts_with("@@");
                    div()
                        .flex()
                        .flex_row()
                        .h(px(LINE_HEIGHT))
                        .when(added, |d| d.bg(rgba(0x244d342e)))
                        .when(removed, |d| d.bg(rgba(0x5a292e2e)))
                        .child(
                            div()
                                .flex()
                                .items_center()
                                .justify_end()
                                .w(px(50.0))
                                .h_full()
                                .pr(px(theme::space::ML))
                                .flex_shrink_0()
                                .bg(theme::color(theme::surface::INSET))
                                .border_r_1()
                                .border_color(theme::color(theme::border::SUBTLE))
                                .text_color(theme::color(theme::text::DIM))
                                .child((line + 1).to_string()),
                        )
                        .child(
                            div()
                                .flex_1()
                                .min_w(px(0.0))
                                .pl(px(theme::space::LG))
                                .text_color(theme::color(if added {
                                    theme::status::DONE
                                } else if removed {
                                    theme::status::ERROR
                                } else if hunk {
                                    theme::ACCENT
                                } else {
                                    theme::text::DEFAULT
                                }))
                                .child(if text.is_empty() { " ".into() } else { text }),
                        )
                })
                .collect()
        });

        div()
            .flex_1()
            .h_full()
            .flex()
            .flex_col()
            .bg(rgb(TERMINAL_BG))
            .child(
                body.flex_1()
                    .track_scroll(self.editor_scroll.clone())
                    .font(mono_font())
                    .text_size(px(FONT_SIZE))
                    .py(px(PADDING)),
            )
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .h(px(30.0))
                    .px(px(theme::space::MD))
                    .gap(px(theme::space::MD))
                    .bg(theme::color(theme::surface::RAISED))
                    .border_t_1()
                    .border_color(theme::color(theme::border::DEFAULT))
                    .child(
                        div()
                            .h(px(20.0))
                            .px(px(theme::space::MD))
                            .rounded(px(theme::radius::SM))
                            .bg(theme::color(theme::surface::ACTIVE))
                            .border_l_1()
                            .border_color(theme::color(theme::ACCENT))
                            .text_size(px(theme::font_size::XS))
                            .font_weight(FontWeight::MEDIUM)
                            .text_color(theme::color(theme::ACCENT))
                            .child("DIFF"),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(0.0))
                            .truncate()
                            .text_size(px(theme::font_size::SM))
                            .text_color(theme::color(theme::text::MUTED))
                            .child(diff.title.clone()),
                    )
                    .child(
                        div()
                            .flex_shrink_0()
                            .text_size(px(theme::font_size::XS))
                            .text_color(theme::color(theme::text::DIM))
                            .child("READ ONLY"),
                    ),
            )
            .into_any_element()
    }

    fn render_editor(&self, cx: &mut Context<Self>) -> impl IntoElement {
        if let Some(diff) = &self.git_diff {
            return self.render_git_diff(diff, cx);
        }

        let Some(editor) = &self.editor else {
            return div()
                .flex_1()
                .flex()
                .items_center()
                .justify_center()
                .bg(theme::color(theme::surface::BASE))
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .items_center()
                        .gap(px(theme::space::MD))
                        .child(
                            svg()
                                .path("icons/editor.svg")
                                .size(px(28.0))
                                .text_color(theme::color(theme::text::DIM)),
                        )
                        .child(
                            div()
                                .text_size(px(theme::font_size::LG))
                                .font_weight(FontWeight::MEDIUM)
                                .text_color(theme::color(theme::text::MUTED))
                                .child("No file open"),
                        )
                        .child(
                            div()
                                .text_size(px(theme::font_size::SM))
                                .text_color(theme::color(theme::text::DIM))
                                .child("Choose a file to start editing"),
                        )
                        .child(
                            div()
                                .id("browse-editor-files")
                                .flex()
                                .items_center()
                                .h(px(28.0))
                                .mt(px(theme::space::XS))
                                .px(px(theme::space::LG))
                                .rounded(px(theme::radius::MD))
                                .bg(theme::color(theme::surface::ACTIVE))
                                .cursor_pointer()
                                .text_size(px(theme::font_size::SM))
                                .text_color(theme::color(theme::text::DEFAULT))
                                .hover(|s| s.bg(theme::color(theme::surface::HOVER)))
                                .child("Browse files")
                                .on_mouse_down(
                                    MouseButton::Left,
                                    cx.listener(|this, _, _, cx| {
                                        this.select_info_tab(InfoTab::Files, cx)
                                    }),
                                ),
                        ),
                )
                .into_any_element();
        };

        // Virtualized: only lines scrolled into view are built. Rendering every
        // line of the rope made large files unusable.
        let line_count = editor.rope.len_lines();
        let this = cx.entity();
        let body = uniform_list("editor-lines", line_count, move |range, _window, cx| {
            // NOTE: reads back through the entity rather than capturing state,
            // so only the visible range is ever materialized.
            let forge = this.read(cx);
            let Some(editor) = &forge.editor else {
                return Vec::new();
            };
            let (cursor_line, cursor_col) = editor.cursor_line_col();
            range
                .map(|line| {
                    let text: String = editor
                        .rope
                        .line(line)
                        .chars()
                        .filter(|c| *c != '\n')
                        .collect();
                    let is_cursor_line =
                        editor.mode != forge_editor::Mode::Command && line == cursor_line;
                    let content: AnyElement = if is_cursor_line {
                        render_editor_line_with_cursor(&text, cursor_col)
                    } else {
                        div()
                            .flex_1()
                            .text_color(rgb(DEFAULT_FG))
                            .child(if text.is_empty() {
                                " ".to_string()
                            } else {
                                text
                            })
                            .into_any_element()
                    };
                    div()
                        .flex()
                        .flex_row()
                        .h(px(LINE_HEIGHT))
                        .when(is_cursor_line, |d| {
                            d.bg(theme::color(theme::surface::OVERLAY))
                        })
                        .child(
                            div()
                                .flex()
                                .items_center()
                                .justify_end()
                                .w(px(50.0))
                                .h_full()
                                .pr(px(theme::space::ML))
                                .flex_shrink_0()
                                .bg(theme::color(theme::surface::INSET))
                                .border_r_1()
                                .border_color(theme::color(theme::border::SUBTLE))
                                .text_color(theme::color(if is_cursor_line {
                                    theme::text::MUTED
                                } else {
                                    theme::text::DIM
                                }))
                                .child((line + 1).to_string()),
                        )
                        .child(
                            div()
                                .flex_1()
                                .min_w(px(0.0))
                                .pl(px(theme::space::LG))
                                .child(content),
                        )
                })
                .collect()
        });

        let mode_label = match editor.mode {
            forge_editor::Mode::Normal => "NORMAL",
            forge_editor::Mode::Insert => "INSERT",
            forge_editor::Mode::Command => "COMMAND",
        };
        let mode_color = match editor.mode {
            forge_editor::Mode::Normal => theme::ACCENT,
            forge_editor::Mode::Insert => theme::status::DONE,
            forge_editor::Mode::Command => theme::status::ATTENTION,
        };
        let (cursor_line, cursor_col) = editor.cursor_line_col();
        let display_path = self
            .workspaces
            .active_workspace()
            .and_then(|ws| editor.path.strip_prefix(&ws.path).ok())
            .unwrap_or(&editor.path)
            .display()
            .to_string();
        let footer_text = if editor.mode == forge_editor::Mode::Command {
            format!(":{}", editor.command_line)
        } else {
            editor.status.clone().unwrap_or(display_path)
        };

        div()
            .flex_1()
            .h_full()
            .flex()
            .flex_col()
            .bg(rgb(TERMINAL_BG))
            .child(
                body.flex_1()
                    .track_scroll(self.editor_scroll.clone())
                    .font(mono_font())
                    .text_size(px(FONT_SIZE))
                    .py(px(PADDING)),
            )
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .h(px(30.0))
                    .px(px(theme::space::MD))
                    .gap(px(theme::space::MD))
                    .bg(theme::color(theme::surface::RAISED))
                    .border_t_1()
                    .border_color(theme::color(theme::border::DEFAULT))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .h(px(20.0))
                            .px(px(theme::space::MD))
                            .rounded(px(theme::radius::SM))
                            .bg(theme::color(theme::surface::ACTIVE))
                            .border_l_1()
                            .border_color(theme::color(mode_color))
                            .text_size(px(theme::font_size::XS))
                            .font_weight(FontWeight::MEDIUM)
                            .text_color(theme::color(mode_color))
                            .child(mode_label),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(0.0))
                            .truncate()
                            .text_size(px(theme::font_size::SM))
                            .text_color(theme::color(theme::text::MUTED))
                            .child(footer_text),
                    )
                    .when(editor.dirty, |d| {
                        d.child(
                            div()
                                .text_size(px(theme::font_size::SM))
                                .text_color(theme::color(theme::status::ATTENTION))
                                .child("Modified"),
                        )
                    })
                    .child(
                        div()
                            .flex_shrink_0()
                            .text_size(px(theme::font_size::SM))
                            .text_color(theme::color(theme::text::DIM))
                            .child(format!("Ln {}, Col {}", cursor_line + 1, cursor_col + 1)),
                    ),
            )
            .into_any_element()
    }

    fn render_agents(&self) -> AnyElement {
        empty_state("No agents running yet — launch a CLI agent in a terminal pane.")
    }

    fn render_top_bar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let editor_label = self
            .git_diff
            .as_ref()
            .map(|diff| diff.title.clone())
            .or_else(|| {
                self.editor.as_ref().map(|editor| {
                    let name = editor
                        .path
                        .file_name()
                        .map(|name| name.to_string_lossy().to_string())
                        .unwrap_or_else(|| "Editor".into());
                    if editor.dirty {
                        format!("{name}  \u{2022}")
                    } else {
                        name
                    }
                })
            })
            .unwrap_or_else(|| "Editor".into());

        let view_tab = |id: &'static str,
                        icon: &'static str,
                        label: String,
                        view: ViewMode,
                        active: bool,
                        cx: &mut Context<Self>| {
            div()
                .id(id)
                .flex()
                .items_center()
                .justify_center()
                .w(px(116.0))
                .flex_shrink_0()
                .h(px(26.0))
                .px(px(theme::space::LG))
                .rounded(px(theme::radius::SM))
                .cursor_pointer()
                .text_size(px(theme::font_size::SM))
                .when(active, |d| {
                    d.bg(theme::color(theme::surface::ACTIVE))
                        .text_color(theme::color(theme::text::DEFAULT))
                        .font_weight(FontWeight::MEDIUM)
                })
                .when(!active, |d| {
                    d.text_color(theme::color(theme::text::DIM)).hover(|s| {
                        s.bg(theme::color(theme::surface::HOVER))
                            .text_color(theme::color(theme::text::MUTED))
                    })
                })
                .child(
                    div()
                        .flex()
                        .flex_row()
                        .items_center()
                        .min_w(px(0.0))
                        .gap(px(theme::space::SM))
                        .child(svg().path(icon).size(px(13.0)).text_color(theme::color(
                            if active {
                                theme::text::DEFAULT
                            } else {
                                theme::text::DIM
                            },
                        )))
                        .child(div().min_w(px(0.0)).truncate().child(label)),
                )
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |this, _, _, cx| this.set_active_view(view, cx)),
                )
                .into_any_element()
        };

        let views = vec![
            view_tab(
                "view-terminal",
                "icons/terminal.svg",
                "Terminal".into(),
                ViewMode::Terminal,
                self.active_view == ViewMode::Terminal,
                cx,
            ),
            view_tab(
                "view-editor",
                "icons/editor.svg",
                editor_label,
                ViewMode::Editor,
                self.active_view == ViewMode::Editor,
                cx,
            ),
            view_tab(
                "view-agents",
                "icons/agents.svg",
                "Agents".into(),
                ViewMode::Agents,
                self.active_view == ViewMode::Agents,
                cx,
            ),
            view_tab(
                "view-profile",
                "icons/user.svg",
                "Profile".into(),
                ViewMode::Profile,
                self.active_view == ViewMode::Profile,
                cx,
            ),
        ];

        div()
            .flex()
            .flex_row()
            .items_center()
            .flex_shrink_0()
            .h(px(TOP_BAR_HEIGHT))
            .bg(theme::color(theme::surface::BASE))
            .border_b_1()
            .border_color(theme::color(theme::border::DEFAULT))
            // Dedicated titlebar zones line up with the columns below instead
            // of letting controls float over the workspace sidebar.
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(theme::space::XS))
                    .w(px(self.workspace_sidebar_width))
                    .h_full()
                    .pl(px(TRAFFIC_LIGHT_INSET))
                    .pr(px(theme::space::MD))
                    .border_r_1()
                    .border_color(theme::color(theme::border::DEFAULT))
                    .child(
                        div()
                            .id("add-workspace")
                            .flex()
                            .items_center()
                            .justify_center()
                            .size(px(28.0))
                            .flex_shrink_0()
                            .rounded(px(theme::radius::MD))
                            .cursor_pointer()
                            .hover(|s| s.bg(theme::color(theme::surface::HOVER)))
                            .active(|s| s.bg(theme::color(theme::surface::ACTIVE)))
                            .child(
                                svg()
                                    .path("icons/plus.svg")
                                    .size(px(15.0))
                                    .text_color(theme::color(theme::text::MUTED)),
                            )
                            .on_mouse_down(
                                MouseButton::Left,
                                cx.listener(|this, _, _, cx| this.add_workspace(cx)),
                            ),
                    )
                    .child(
                        div()
                            .id("toggle-workspace-sidebar")
                            .flex()
                            .items_center()
                            .justify_center()
                            .size(px(28.0))
                            .flex_shrink_0()
                            .rounded(px(theme::radius::MD))
                            .cursor_pointer()
                            .hover(|s| s.bg(theme::color(theme::surface::HOVER)))
                            .active(|s| s.bg(theme::color(theme::surface::ACTIVE)))
                            .child(
                                svg()
                                    .path("icons/sidebar.svg")
                                    .size(px(15.0))
                                    .text_color(theme::color(theme::text::MUTED)),
                            )
                            .on_mouse_down(
                                MouseButton::Left,
                                cx.listener(|this, _, _, cx| this.toggle_workspace_sidebar(cx)),
                            ),
                    ),
            )
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .flex_1()
                    .min_w(px(0.0))
                    .h_full()
                    .px(px(theme::space::MD))
                    .gap(px(theme::space::MD))
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap(px(theme::space::XXS))
                            .p(px(theme::space::XXS))
                            .rounded(px(theme::radius::MD))
                            .bg(theme::color(theme::surface::INSET))
                            .border_1()
                            .border_color(theme::color(theme::border::SUBTLE))
                            .children(views),
                    )
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .h(px(20.0))
                            .px(px(theme::space::SM))
                            .rounded(px(theme::radius::SM))
                            .border_1()
                            .border_color(theme::color(theme::border::SUBTLE))
                            .text_size(px(theme::font_size::XS))
                            .text_color(theme::color(theme::text::DIM))
                            .child("\u{2318}E"),
                    )
                    .child(div().flex_1())
                    .child(
                        div()
                            .id("toggle-info")
                            .flex()
                            .items_center()
                            .justify_center()
                            .size(px(28.0))
                            .flex_shrink_0()
                            .rounded(px(theme::radius::MD))
                            .cursor_pointer()
                            .hover(|s| s.bg(theme::color(theme::surface::HOVER)))
                            .active(|s| s.bg(theme::color(theme::surface::ACTIVE)))
                            .child(
                                svg()
                                    .path("icons/sidebar-right.svg")
                                    .size(px(15.0))
                                    .text_color(theme::color(theme::text::MUTED)),
                            )
                            .on_mouse_down(
                                MouseButton::Left,
                                cx.listener(|this, _, _, cx| this.toggle_info_panel(cx)),
                            ),
                    ),
            )
    }

    fn render_info_panel(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let tabs = [InfoTab::Files, InfoTab::Git, InfoTab::Processes]
            .into_iter()
            .map(|tab| {
                let active = self.info_tab == tab;
                div()
                    .id(tab.label())
                    .flex_1()
                    .flex()
                    .flex_row()
                    .items_center()
                    .justify_center()
                    .h(px(28.0))
                    .gap(px(theme::space::SM))
                    .rounded(px(theme::radius::MD))
                    .cursor_pointer()
                    .text_size(px(theme::font_size::SM))
                    .when(active, |d| {
                        d.bg(theme::color(theme::surface::ACTIVE))
                            .text_color(theme::color(theme::text::DEFAULT))
                            .font_weight(FontWeight::MEDIUM)
                    })
                    .when(!active, |d| {
                        d.text_color(theme::color(theme::text::DIM))
                            .hover(|s| s.text_color(theme::color(theme::text::MUTED)))
                    })
                    .child(
                        svg()
                            .path(tab.icon())
                            .size(px(12.0))
                            .text_color(theme::color(if active {
                                theme::text::DEFAULT
                            } else {
                                theme::text::DIM
                            })),
                    )
                    .child(tab.label())
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this, _, _, cx| this.select_info_tab(tab, cx)),
                    )
            })
            .collect::<Vec<_>>();

        let body = match self.info_tab {
            InfoTab::Files => self.render_files_tab(cx),
            InfoTab::Git => self.render_git_tab(cx),
            InfoTab::Processes => self.render_processes_tab(),
        };

        div()
            .flex()
            .flex_col()
            .w(px(self.info_panel_width))
            .h_full()
            .bg(theme::color(theme::surface::BASE))
            .border_l_1()
            .border_color(theme::color(theme::border::DEFAULT))
            .child(
                div()
                    .flex()
                    .flex_row()
                    .gap(px(theme::space::XS))
                    .px(px(theme::space::MD))
                    .pt(px(theme::space::LG))
                    .pb(px(theme::space::XS))
                    .children(tabs),
            )
            .child(body)
    }

    fn render_files_tab(&self, cx: &mut Context<Self>) -> AnyElement {
        let mut rows: Vec<AnyElement> = Vec::new();
        if let Some(root) = &self.file_tree {
            for child in &root.children {
                push_file_rows(
                    child,
                    0,
                    &self.expanded_dirs,
                    &self.selected_file,
                    &mut rows,
                    cx,
                );
            }
        }

        if rows.is_empty() {
            return empty_state("No files");
        }

        div()
            .id("file-tree")
            .flex_1()
            .min_h(px(0.0))
            .overflow_y_scroll()
            .flex()
            .flex_col()
            .py(px(theme::space::XS))
            .children(rows)
            .into_any_element()
    }

    fn render_git_tab(&self, cx: &mut Context<Self>) -> AnyElement {
        let Some(status) = &self.git_status else {
            return render_git_loading();
        };
        if !status.is_repo {
            let target = self
                .git_repository()
                .map(|path| forge_workspace::full_display_path(&path, 80))
                .unwrap_or_else(|| "the current directory".into());
            return render_not_repository(target, cx);
        }

        let root = status
            .root
            .as_deref()
            .map(|path| forge_workspace::full_display_path(path, 80))
            .unwrap_or_default();
        let width = self.info_panel_width;
        let query = self.git_filter.trim().to_lowercase();
        let matches = |entry: &&forge_git::Entry| {
            query.is_empty() || entry.path.to_lowercase().contains(&query)
        };
        let merge = status
            .entries
            .iter()
            .filter(|entry| entry.is_conflicted())
            .filter(matches)
            .collect::<Vec<_>>();
        let staged = status
            .entries
            .iter()
            .filter(|entry| !entry.is_conflicted() && entry.staged.is_some())
            .filter(matches)
            .collect::<Vec<_>>();
        let changed = status
            .entries
            .iter()
            .filter(|entry| !entry.is_conflicted() && entry.unstaged.is_some())
            .filter(matches)
            .collect::<Vec<_>>();
        let graph_lanes = self
            .git_history
            .iter()
            .flat_map(|commit| {
                std::iter::once(commit.lane)
                    .chain(commit.joins.iter().flat_map(|join| [join.lane, join.other]))
            })
            .max()
            .map_or(1, |lane| (lane + 1).min(theme::graph::MAX_LANES));

        let mut rows = Vec::new();
        if status.entries.is_empty() {
            rows.push(render_git_clean_state(status.ahead, status.behind, cx));
        } else if merge.is_empty() && staged.is_empty() && changed.is_empty() {
            rows.push(render_git_filter_empty(
                &self.git_filter,
                status.entries.len(),
            ));
        }

        if !merge.is_empty() {
            let mut section_rows = vec![render_kero_git_section_header(
                "MERGE CONFLICTS",
                merge.len(),
                self.git_merge_collapsed,
                GitSection::Merge,
                Vec::new(),
                self.git_operation_in_flight,
                cx,
            )];
            if !self.git_merge_collapsed {
                section_rows.extend(merge.into_iter().map(|entry| {
                    render_kero_git_entry(
                        entry,
                        forge_git::Change::Conflicted,
                        false,
                        self.git_operation_in_flight,
                        width,
                        true,
                        cx,
                    )
                }));
            }
            rows.push(render_kero_git_card(section_rows, true));
        }

        if !staged.is_empty() {
            let mut section_rows = vec![render_kero_git_section_header(
                "STAGED",
                staged.len(),
                self.git_staged_collapsed,
                GitSection::Staged,
                vec![("unstage all", GitMutation::UnstageAll)],
                self.git_operation_in_flight,
                cx,
            )];
            if !self.git_staged_collapsed {
                section_rows.extend(staged.into_iter().map(|entry| {
                    render_kero_git_entry(
                        entry,
                        entry.staged.unwrap_or(forge_git::Change::Modified),
                        true,
                        self.git_operation_in_flight,
                        width,
                        true,
                        cx,
                    )
                }));
            }
            rows.push(render_kero_git_card(section_rows, false));
        }

        if !changed.is_empty() {
            let discard_targets = changed
                .iter()
                .map(|entry| {
                    git_discard_target(entry, entry.unstaged.unwrap_or(forge_git::Change::Modified))
                })
                .collect();
            rows.push(render_kero_git_section_header(
                "CHANGES",
                changed.len(),
                self.git_changes_collapsed,
                GitSection::Changes,
                vec![
                    ("discard all", GitMutation::DiscardAll(discard_targets)),
                    ("stage all", GitMutation::StageAll),
                ],
                self.git_operation_in_flight,
                cx,
            ));
            if !self.git_changes_collapsed {
                rows.extend(changed.into_iter().map(|entry| {
                    render_kero_git_entry(
                        entry,
                        entry.unstaged.unwrap_or(forge_git::Change::Modified),
                        false,
                        self.git_operation_in_flight,
                        width,
                        false,
                        cx,
                    )
                }));
            }
        }

        if query.is_empty() && !self.git_history.is_empty() {
            let mut history_rows = vec![render_kero_git_section_header(
                "HISTORY",
                self.git_history.len(),
                self.git_history_collapsed,
                GitSection::History,
                Vec::new(),
                self.git_operation_in_flight,
                cx,
            )];
            if !self.git_history_collapsed {
                for commit in &self.git_history {
                    let selected =
                        self.git_selection == Some(GitSelection::Commit(commit.id.clone()));
                    history_rows.push(render_kero_commit_row(
                        commit,
                        selected,
                        graph_lanes,
                        width,
                        cx,
                    ));
                    if selected {
                        if self.git_detail_loading {
                            history_rows.push(render_kero_commit_detail_state(
                                "Loading changed files…",
                                commit.lane,
                                graph_lanes,
                            ));
                        } else if self.git_changes.is_empty() {
                            history_rows.push(render_kero_commit_detail_state(
                                "No changed files",
                                commit.lane,
                                graph_lanes,
                            ));
                        } else {
                            history_rows.extend(self.git_changes.iter().enumerate().map(
                                |(index, change)| {
                                    render_kero_commit_file_row(
                                        index,
                                        change,
                                        commit.lane,
                                        graph_lanes,
                                        cx,
                                    )
                                },
                            ));
                        }
                    }
                }
            }
            rows.push(render_kero_history_section(history_rows));
        }

        let input = if self.git_filter_visible {
            render_kero_filter_bar(
                &self.git_filter,
                self.git_input == Some(GitInput::Filter),
                cx,
            )
        } else {
            render_kero_commit_box(
                &self.git_commit_message,
                self.git_input == Some(GitInput::CommitMessage),
                status.staged_count(),
                self.git_operation_in_flight,
                cx,
            )
        };

        div()
            .flex()
            .flex_col()
            .flex_1()
            .min_h(px(0.0))
            .child(render_kero_git_header(
                status,
                root,
                width,
                self.git_filter_visible,
                cx,
            ))
            .child(input)
            .children(
                self.git_operation_error
                    .as_ref()
                    .map(|error| render_git_error_banner(error, cx)),
            )
            .children(self.git_operation_in_flight.then(|| {
                div()
                    .h(px(2.0))
                    .w_2_5()
                    .bg(theme::color(theme::ACCENT))
                    .into_any_element()
            }))
            .child(
                div()
                    .id("git-change-list")
                    .flex()
                    .flex_col()
                    .flex_1()
                    .min_h(px(0.0))
                    .overflow_y_scroll()
                    .pb(px(theme::space::MD))
                    .opacity(if self.git_operation_in_flight {
                        0.4
                    } else {
                        1.0
                    })
                    .children(rows),
            )
            .into_any_element()
    }

    fn render_processes_tab(&self) -> AnyElement {
        if self.processes.is_empty() {
            return empty_state("No processes");
        }

        let rows = self.processes.iter().map(|p| {
            div()
                .flex()
                .flex_row()
                .items_center()
                .h(px(theme::row::HEIGHT))
                .px(px(theme::space::LG))
                .gap(px(theme::space::MD))
                .hover(|s| s.bg(theme::color(theme::surface::HOVER)))
                .child(
                    div()
                        .flex_1()
                        .min_w(px(0.0))
                        .truncate()
                        .pl(px(p.depth as f32 * 10.0))
                        .text_size(px(theme::font_size::MD))
                        .text_color(theme::color(if p.depth == 0 {
                            theme::text::DEFAULT
                        } else {
                            theme::text::MUTED
                        }))
                        .child(p.name.clone()),
                )
                .child(
                    div()
                        .w(px(40.0))
                        .flex_shrink_0()
                        .text_size(px(theme::font_size::SM))
                        .text_color(theme::color(if p.cpu >= 10.0 {
                            theme::status::ATTENTION
                        } else {
                            theme::text::DIM
                        }))
                        .child(format!("{:.0}%", p.cpu)),
                )
                .child(
                    div()
                        .w(px(44.0))
                        .flex_shrink_0()
                        .text_size(px(theme::font_size::SM))
                        .text_color(theme::color(theme::text::DIM))
                        .child(forge_proc::format_bytes(p.memory)),
                )
        });

        div()
            .flex()
            .flex_col()
            .flex_1()
            .min_h(px(0.0))
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .h(px(theme::row::HEIGHT))
                    .px(px(theme::space::LG))
                    .gap(px(theme::space::MD))
                    .border_b_1()
                    .border_color(theme::color(theme::border::SUBTLE))
                    .text_size(px(theme::font_size::XS))
                    .text_color(theme::color(theme::text::DIM))
                    .child(div().flex_1().child("PROCESS"))
                    .child(div().w(px(40.0)).flex_shrink_0().child("CPU"))
                    .child(div().w(px(44.0)).flex_shrink_0().child("MEM")),
            )
            .child(
                div()
                    .id("process-list")
                    .flex_1()
                    .min_h(px(0.0))
                    .overflow_y_scroll()
                    .flex()
                    .flex_col()
                    .py(px(theme::space::XS))
                    .children(rows),
            )
            .into_any_element()
    }

    fn render_palette(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let indices = self.filtered_palette_indices();
        let selected = self.palette_selected.min(indices.len().saturating_sub(1));
        let rows = indices.into_iter().enumerate().filter_map(|(row, ix)| {
            let item = self.palette_cache.get(ix)?;
            let is_selected = row == selected;
            Some(
                div()
                    .id(("palette-item", row))
                    .flex()
                    .flex_row()
                    .items_center()
                    .justify_between()
                    .h(px(28.0))
                    .mx(px(theme::space::XS))
                    .px(px(theme::space::MD))
                    .rounded(px(theme::radius::SM))
                    .when(is_selected, |d| d.bg(theme::color(theme::surface::ACTIVE)))
                    .child(
                        div()
                            .text_size(px(theme::font_size::MD))
                            .text_color(theme::color(if is_selected {
                                theme::text::DEFAULT
                            } else {
                                theme::text::MUTED
                            }))
                            .child(item.label.clone()),
                    )
                    .child(
                        div()
                            .text_size(px(theme::font_size::XS))
                            .text_color(theme::color(theme::text::DIM))
                            .child(item.kind),
                    ),
            )
        });

        div()
            .id("palette-overlay")
            .absolute()
            .top(px(0.0))
            .left(px(0.0))
            .w_full()
            .h_full()
            .flex()
            .flex_col()
            .items_center()
            .bg(rgba(0x0a0b0ccc))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, _, cx| this.close_palette(cx)),
            )
            .child(
                div()
                    .mt(px(96.0))
                    .w(px(520.0))
                    .flex()
                    .flex_col()
                    .bg(theme::color(theme::surface::OVERLAY))
                    .border_1()
                    .border_color(theme::color(theme::border::DEFAULT))
                    .rounded(px(theme::radius::LG))
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .items_center()
                            .h(px(46.0))
                            .px(px(theme::space::LG))
                            .gap(px(theme::space::MD))
                            .border_b_1()
                            .border_color(theme::color(theme::border::SUBTLE))
                            .text_size(px(theme::font_size::LG))
                            .child(
                                div()
                                    .text_color(theme::color(theme::text::DIM))
                                    .child("\u{2315}"),
                            )
                            .child(
                                div()
                                    .flex_1()
                                    .min_w(px(0.0))
                                    .truncate()
                                    .text_color(theme::color(if self.palette_query.is_empty() {
                                        theme::text::DIM
                                    } else {
                                        theme::text::DEFAULT
                                    }))
                                    .child(if self.palette_query.is_empty() {
                                        "Search files and workspaces\u{258f}".to_string()
                                    } else {
                                        format!("{}\u{258f}", self.palette_query)
                                    }),
                            )
                            .child(
                                div()
                                    .flex()
                                    .items_center()
                                    .h(px(20.0))
                                    .px(px(theme::space::SM))
                                    .rounded(px(theme::radius::SM))
                                    .border_1()
                                    .border_color(theme::color(theme::border::DEFAULT))
                                    .text_size(px(theme::font_size::XS))
                                    .text_color(theme::color(theme::text::DIM))
                                    .child("esc"),
                            ),
                    )
                    .child(div().flex().flex_col().py(px(4.0)).children(rows)),
            )
    }

    fn render_profile(&self, cx: &mut Context<Self>) -> AnyElement {
        let content = match &self.github_state {
            GitHubState::Connected(account) => {
                let account = account.clone();
                self.render_profile_connected(&account, cx)
            }
            GitHubState::AwaitingDevice(device) => {
                let device = device.clone();
                self.render_profile_device_code(&device, cx)
            }
            _ => self.render_profile_sign_in(cx),
        };

        div()
            .id("profile-view")
            .flex()
            .flex_1()
            .flex_col()
            .items_center()
            .justify_center()
            .size_full()
            .overflow_y_scroll()
            .p(px(32.0))
            .child(
                div()
                    .w(px(480.0))
                    .flex()
                    .flex_col()
                    .gap(px(theme::space::XL))
                    .child(content),
            )
            .into_any_element()
    }

    fn render_profile_sign_in(&self, cx: &mut Context<Self>) -> AnyElement {
        let (primary_label, primary_enabled): (&str, bool) = match &self.github_state {
            GitHubState::Checking => ("Checking…", false),
            GitHubState::SigningIn => ("Starting sign-in…", false),
            _ => ("Sign in with GitHub", true),
        };
        let error = match &self.github_state {
            GitHubState::Failed(message) => Some(message.clone()),
            _ => None,
        };

        let primary = div()
            .id("profile-sign-in")
            .flex()
            .items_center()
            .justify_center()
            .h(px(36.0))
            .px(px(theme::space::XL))
            .rounded(px(theme::radius::MD))
            .bg(theme::color(if primary_enabled {
                theme::ACCENT
            } else {
                theme::surface::ACTIVE
            }))
            .text_size(px(theme::font_size::MD))
            .text_color(theme::color(if primary_enabled {
                theme::text::ON_ACCENT
            } else {
                theme::text::DIM
            }))
            .when(primary_enabled, |button| {
                button
                    .cursor_pointer()
                    .hover(|style| style.bg(theme::color(theme::accent::HOVER)))
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|this, _, _, cx| this.begin_github_sign_in(cx)),
                    )
            })
            .child(primary_label);

        div()
            .flex()
            .flex_col()
            .gap(px(theme::space::XL))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(theme::space::SM))
                    .child(
                        div()
                            .text_size(px(22.0))
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(theme::color(theme::text::DEFAULT))
                            .child("Sign in to GitHub"),
                    )
                    .child(
                        div()
                            .text_size(px(theme::font_size::MD))
                            .line_height(px(21.0))
                            .text_color(theme::color(theme::text::MUTED))
                            .child(
                                "Connect a GitHub account to link Git credentials for \
                                 github.com and show your identity across Forge. New to \
                                 GitHub? You can create an account during sign-in.",
                            ),
                    ),
            )
            .children(error.map(|message| {
                div()
                    .p(px(theme::space::MD))
                    .rounded(px(theme::radius::MD))
                    .bg(theme::color(theme::danger::SURFACE))
                    .border_1()
                    .border_color(theme::color(theme::danger::BORDER))
                    .text_size(px(theme::font_size::SM))
                    .text_color(theme::color(theme::danger::TEXT))
                    .child(message)
            }))
            .child(primary)
            .into_any_element()
    }

    fn render_profile_device_code(
        &self,
        device: &github::DeviceAuthorization,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let cancel = div()
            .id("profile-cancel-sign-in")
            .flex()
            .items_center()
            .justify_center()
            .h(px(36.0))
            .px(px(theme::space::LG))
            .rounded(px(theme::radius::MD))
            .cursor_pointer()
            .text_size(px(theme::font_size::SM))
            .text_color(theme::color(theme::text::MUTED))
            .hover(|style| style.bg(theme::color(theme::surface::HOVER)))
            .child("Cancel")
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, _, cx| this.cancel_github_sign_in(cx)),
            );

        div()
            .flex()
            .flex_col()
            .gap(px(theme::space::XL))
            .child(
                div()
                    .text_size(px(22.0))
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(theme::color(theme::text::DEFAULT))
                    .child("Finish in your browser"),
            )
            .child(
                div()
                    .text_size(px(theme::font_size::MD))
                    .line_height(px(21.0))
                    .text_color(theme::color(theme::text::MUTED))
                    .child(format!(
                        "Forge opened {} — enter the code below to connect your account.",
                        device.verification_uri
                    )),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_center()
                    .py(px(theme::space::XL))
                    .rounded(px(theme::radius::LG))
                    .bg(theme::color(theme::surface::INSET))
                    .border_1()
                    .border_color(theme::color(theme::border::DEFAULT))
                    .font(mono_font())
                    .text_size(px(28.0))
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(theme::color(theme::text::DEFAULT))
                    .child(device.user_code.clone()),
            )
            .child(
                div()
                    .text_size(px(theme::font_size::SM))
                    .text_color(theme::color(theme::status::RUNNING))
                    .child("Waiting for you to authorize Forge on GitHub…"),
            )
            .child(cancel)
            .into_any_element()
    }

    fn render_profile_connected(
        &self,
        account: &github::Account,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let display_name = account
            .name
            .clone()
            .unwrap_or_else(|| account.login.clone());

        let sign_out = div()
            .id("profile-sign-out")
            .flex()
            .items_center()
            .justify_center()
            .h(px(36.0))
            .px(px(theme::space::LG))
            .rounded(px(theme::radius::MD))
            .cursor_pointer()
            .border_1()
            .border_color(theme::color(theme::border::DEFAULT))
            .text_size(px(theme::font_size::SM))
            .text_color(theme::color(theme::text::MUTED))
            .hover(|style| {
                style
                    .bg(theme::color(theme::danger::SURFACE))
                    .text_color(theme::color(theme::status::ERROR))
            })
            .child("Sign out")
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, _, cx| this.sign_out_github(cx)),
            );

        div()
            .flex()
            .flex_col()
            .gap(px(theme::space::XL))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(theme::space::LG))
                    .child(render_avatar(
                        self.github_avatar.as_ref(),
                        &account.login,
                        64.0,
                    ))
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap(px(theme::space::XXS))
                            .child(
                                div()
                                    .text_size(px(20.0))
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .text_color(theme::color(theme::text::DEFAULT))
                                    .child(display_name),
                            )
                            .child(
                                div()
                                    .text_size(px(theme::font_size::MD))
                                    .text_color(theme::color(theme::text::MUTED))
                                    .child(format!("@{}", account.login)),
                            ),
                    ),
            )
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(theme::space::SM))
                    .p(px(theme::space::LG))
                    .rounded(px(theme::radius::MD))
                    .bg(theme::color(theme::surface::INSET))
                    .border_1()
                    .border_color(theme::color(theme::border::SUBTLE))
                    .text_size(px(theme::font_size::SM))
                    .text_color(theme::color(theme::text::MUTED))
                    .child("Connected with GitHub OAuth. The token lives only in your macOS Keychain.")
                    .child("Git operations over HTTPS for github.com authenticate through Forge's credential helper."),
            )
            .child(sign_out)
            .into_any_element()
    }
}

/// The monospace font used by the terminal grid and editor, including its
/// fallback cascade. Override the primary family with `FORGE_FONT`.
fn mono_font() -> Font {
    let primary = std::env::var("FORGE_FONT").unwrap_or_else(|_| FONT_STACK[0].to_string());
    let mut f = font(primary);
    f.fallbacks = Some(FontFallbacks::from_fonts(
        FONT_STACK.iter().map(|s| s.to_string()).collect(),
    ));
    f
}

fn measure_char_width(window: &Window) -> f32 {
    window
        .text_system()
        .shape_line(
            "0".into(),
            px(FONT_SIZE),
            &[TextRun {
                len: 1,
                font: mono_font(),
                color: white(),
                background_color: None,
                underline: None,
                strikethrough: None,
            }],
            None,
        )
        .width
        .into()
}

fn render_pane(
    pane: &forge_workspace::Pane,
    focused: bool,
    show_focus: bool,
    base_font: &Font,
) -> AnyElement {
    let screen = pane.terminal.screen_snapshot();
    let screen_text = render_screen_text(&screen, base_font);

    div()
        .flex_1()
        .h_full()
        .font(mono_font())
        .text_size(px(FONT_SIZE))
        .line_height(px(LINE_HEIGHT))
        .p(px(PADDING))
        .bg(rgb(TERMINAL_BG))
        // A blue box around the only pane made the whole content area look
        // selected. Pane focus is only relevant once there is somewhere else
        // to move, and then a quiet top edge plus the split hairline is enough.
        .when(show_focus && focused, |d| {
            d.border_t_1().border_color(theme::color(theme::ACCENT))
        })
        .overflow_hidden()
        .child(screen_text)
        .into_any_element()
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct CellStyle {
    fg: u32,
    bg: Option<u32>,
    bold: bool,
}

/// Render a whole terminal grid as one multiline text element.
///
/// A prior implementation made one text element per row and padded every row
/// to the PTY width. At 160x45 that shaped 7,200 characters and laid out 45
/// elements on every repaint even when a command printed ten characters per
/// line. One element plus semantic right-trimming keeps the same grid while
/// removing most of that fixed work.
fn render_screen_text(screen: &vt100::Screen, base_font: &Font) -> StyledText {
    let (rows, cols) = screen.size();
    let (cursor_row, cursor_col) = screen.cursor_position();
    let cursor_visible = !screen.hide_cursor();
    let mut text = String::with_capacity((rows as usize) * 32);
    let mut runs: Vec<TextRun> = Vec::new();
    let mut pending: Option<(CellStyle, usize)> = None;

    for row in 0..rows {
        let has_cursor = cursor_visible && row == cursor_row;
        let end_col = (0..cols)
            .rev()
            .find(|&col| {
                if has_cursor && col == cursor_col {
                    return true;
                }
                screen.cell(row, col).is_some_and(|cell| {
                    cell.has_contents() || cell.bgcolor() != vt100::Color::Default || cell.inverse()
                })
            })
            .map_or(0, |col| col + 1);

        for col in 0..end_col {
            let cell = screen.cell(row, col);

            // A double-width glyph (CJK, emoji, many Nerd Font icons) occupies
            // this cell *and* the next. vt100 marks the second cell as a
            // continuation carrying no content; emitting anything for it would
            // add a character the terminal never wrote and shift every following
            // column right, desynchronizing our grid from the PTY's.
            if cell.map(|c| c.is_wide_continuation()).unwrap_or(false) {
                continue;
            }

            let is_cursor = has_cursor && col == cursor_col;

            let (mut fg, mut bg, bold, inverse) = match cell {
                Some(c) => (
                    resolve_fg(c.fgcolor()),
                    resolve_bg(c.bgcolor()),
                    c.bold(),
                    c.inverse(),
                ),
                None => (DEFAULT_FG, None, false, false),
            };
            if inverse ^ is_cursor {
                let prev_fg = fg;
                fg = bg.unwrap_or(TERMINAL_BG);
                bg = Some(prev_fg);
            }
            let style = CellStyle { fg, bg, bold };

            // `Cell::contents()` allocates a String, so skip it for blank cells —
            // which is most of a typical screen.
            let start = text.len();
            match cell {
                Some(c) if c.has_contents() => text.push_str(&c.contents()),
                _ => text.push(' '),
            }
            let byte_len = text.len() - start;

            append_run(&mut pending, &mut runs, style, byte_len, base_font);
        }

        if row + 1 < rows {
            text.push('\n');
            append_run(
                &mut pending,
                &mut runs,
                CellStyle {
                    fg: DEFAULT_FG,
                    bg: None,
                    bold: false,
                },
                1,
                base_font,
            );
        }
    }
    if let Some((style, len)) = pending {
        runs.push(text_run(style, len, base_font));
    }
    StyledText::new(text).with_runs(runs)
}

fn append_run(
    pending: &mut Option<(CellStyle, usize)>,
    runs: &mut Vec<TextRun>,
    style: CellStyle,
    byte_len: usize,
    base_font: &Font,
) {
    *pending = match pending.take() {
        Some((previous, len)) if previous == style => Some((previous, len + byte_len)),
        Some((previous, len)) => {
            runs.push(text_run(previous, len, base_font));
            Some((style, byte_len))
        }
        None => Some((style, byte_len)),
    };
}

fn text_run(style: CellStyle, len: usize, base_font: &Font) -> TextRun {
    TextRun {
        len,
        font: if style.bold {
            base_font.clone().bold()
        } else {
            base_font.clone()
        },
        color: Hsla::from(rgb(style.fg)),
        background_color: style.bg.map(|c| Hsla::from(rgb(c))),
        underline: None,
        strikethrough: None,
    }
}

fn resolve_fg(color: vt100::Color) -> u32 {
    match color {
        vt100::Color::Default => DEFAULT_FG,
        vt100::Color::Idx(i) => ansi_256(i),
        vt100::Color::Rgb(r, g, b) => rgb_u32(r, g, b),
    }
}

fn resolve_bg(color: vt100::Color) -> Option<u32> {
    match color {
        vt100::Color::Default => None,
        vt100::Color::Idx(i) => Some(ansi_256(i)),
        vt100::Color::Rgb(r, g, b) => Some(rgb_u32(r, g, b)),
    }
}

fn rgb_u32(r: u8, g: u8, b: u8) -> u32 {
    ((r as u32) << 16) | ((g as u32) << 8) | (b as u32)
}

/// Standard xterm 256-color palette lookup (16 base colors, 6x6x6 cube,
/// 24-step grayscale ramp).
fn ansi_256(idx: u8) -> u32 {
    const BASE16: [u32; 16] = [
        0x000000, 0xcd3131, 0x0dbc79, 0xe5e510, 0x2472c8, 0xbc3fbc, 0x11a8cd, 0xe5e5e5, 0x666666,
        0xf14c4c, 0x23d18b, 0xf5f543, 0x3b8eea, 0xd670d6, 0x29b8db, 0xffffff,
    ];
    match idx {
        0..=15 => BASE16[idx as usize],
        16..=231 => {
            let i = idx - 16;
            let r = i / 36;
            let g = (i / 6) % 6;
            let b = i % 6;
            let level = |v: u8| if v == 0 { 0u32 } else { 55 + v as u32 * 40 };
            (level(r) << 16) | (level(g) << 8) | level(b)
        }
        232..=255 => {
            let level = 8 + (idx as u32 - 232) * 10;
            (level << 16) | (level << 8) | level
        }
    }
}

/// Branch identity, repository context, tracking state, and panel actions.
fn render_kero_git_header(
    status: &forge_git::Status,
    root: String,
    width: f32,
    filter_open: bool,
    cx: &mut Context<Forge>,
) -> AnyElement {
    let branch = status
        .branch
        .clone()
        .unwrap_or_else(|| "Detached HEAD".into());
    let upstream = status.upstream.clone().unwrap_or_else(|| {
        status
            .branch
            .as_ref()
            .map(|_| "Unpublished branch".to_string())
            .unwrap_or_else(|| "Detached HEAD".to_string())
    });
    let compact_upstream = upstream.split('/').next().unwrap_or(&upstream);
    let mut tracking = if width > 360.0 {
        if root.is_empty() {
            upstream
        } else {
            format!("{upstream} · {root}")
        }
    } else {
        compact_upstream.to_string()
    };
    if status.ahead > 0 {
        tracking.push_str(&format!(" · ↑{}", status.ahead));
    }
    if status.behind > 0 {
        tracking.push_str(&format!(" · ↓{}", status.behind));
    }

    div()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(theme::space::MD))
        .px(px(theme::space::LG))
        .py(px(theme::space::ML))
        .child(
            svg()
                .path("icons/git-branch.svg")
                .size(px(14.0))
                .flex_shrink_0()
                .text_color(theme::color(theme::text::MUTED)),
        )
        .child(
            div()
                .flex()
                .flex_col()
                .flex_1()
                .min_w(px(0.0))
                .gap(px(1.0))
                .cursor_pointer()
                .child(
                    div()
                        .truncate()
                        .text_size(px(theme::font_size::LG))
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(theme::color(theme::text::DEFAULT))
                        .child(branch),
                )
                .child(
                    div()
                        .truncate()
                        .text_size(px(theme::font_size::SM))
                        .text_color(theme::color(theme::text::DIM))
                        .child(tracking),
                )
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|this, _, _, cx| this.show_git_branch_menu(cx)),
                ),
        )
        .child(
            div()
                .id("git-filter-toggle")
                .flex()
                .items_center()
                .justify_center()
                .size(px(26.0))
                .rounded(px(theme::radius::SM))
                .cursor_pointer()
                .when(filter_open, |element| {
                    element.bg(theme::color(theme::surface::HOVER))
                })
                .hover(|style| style.bg(theme::color(theme::surface::HOVER)))
                .active(|style| style.bg(theme::color(theme::surface::ACTIVE)))
                .child(
                    svg()
                        .path("icons/filter.svg")
                        .size(px(13.0))
                        .text_color(theme::color(if filter_open {
                            theme::text::DEFAULT
                        } else {
                            theme::text::DIM
                        })),
                )
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|this, _, _, cx| this.toggle_git_filter(cx)),
                ),
        )
        .child(
            div()
                .id("git-refresh")
                .flex()
                .items_center()
                .justify_center()
                .size(px(26.0))
                .rounded(px(theme::radius::SM))
                .cursor_pointer()
                .hover(|style| style.bg(theme::color(theme::surface::HOVER)))
                .active(|style| style.bg(theme::color(theme::surface::ACTIVE)))
                .child(
                    svg()
                        .path("icons/refresh.svg")
                        .size(px(13.0))
                        .text_color(theme::color(theme::text::DIM)),
                )
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|this, _, _, cx| this.refresh_git_status(cx)),
                ),
        )
        .child(
            div()
                .id("git-more")
                .flex()
                .items_center()
                .justify_center()
                .size(px(26.0))
                .rounded(px(theme::radius::SM))
                .cursor_pointer()
                .hover(|style| style.bg(theme::color(theme::surface::HOVER)))
                .active(|style| style.bg(theme::color(theme::surface::ACTIVE)))
                .child(
                    svg()
                        .path("icons/more.svg")
                        .size(px(13.0))
                        .text_color(theme::color(theme::text::DIM)),
                )
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|this, _, _, cx| this.show_git_more_menu(cx)),
                ),
        )
        .into_any_element()
}

fn render_git_error_banner(error: &str, cx: &mut Context<Forge>) -> AnyElement {
    let output = error.to_string();
    div()
        .flex()
        .flex_col()
        .mx(px(theme::space::LG))
        .mb(px(theme::space::ML))
        .px(px(theme::space::MD))
        .py(px(theme::space::SM))
        .gap(px(theme::space::XS))
        .rounded(px(theme::radius::MD))
        .border_l_2()
        .border_color(theme::color(theme::git::DELETED))
        .bg(theme::color(theme::danger::SURFACE))
        .child(
            div()
                .flex()
                .items_center()
                .child(
                    div()
                        .flex_1()
                        .text_size(px(11.0))
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(theme::color(theme::danger::TEXT))
                        .child("Git operation failed"),
                )
                .child(
                    div()
                        .id("dismiss-git-error")
                        .flex()
                        .items_center()
                        .justify_center()
                        .size(px(18.0))
                        .rounded(px(theme::radius::SM))
                        .cursor_pointer()
                        .text_color(theme::color(theme::text::DIM))
                        .hover(|style| style.bg(theme::color(theme::surface::HOVER)))
                        .child(
                            svg()
                                .path("icons/x.svg")
                                .size(px(12.0))
                                .text_color(theme::color(theme::text::DIM)),
                        )
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(|this, _, _, cx| this.dismiss_git_error(cx)),
                        ),
                ),
        )
        .child(
            div()
                .line_clamp(2)
                .text_size(px(10.5))
                .text_color(theme::color(theme::text::MUTED))
                .child(error.to_string()),
        )
        .child(
            div()
                .flex()
                .gap(px(theme::space::MD))
                .text_size(px(10.0))
                .font_weight(FontWeight::MEDIUM)
                .child(
                    div()
                        .id("retry-git-operation")
                        .cursor_pointer()
                        .text_color(theme::color(theme::ACCENT))
                        .child("Retry")
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(|this, _, _, cx| this.retry_git_operation(cx)),
                        ),
                )
                .child(
                    div()
                        .id("copy-git-error")
                        .cursor_pointer()
                        .text_color(theme::color(theme::text::DIM))
                        .child("Copy output")
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(move |_, _, _, cx| {
                                cx.write_to_clipboard(ClipboardItem::new_string(output.clone()));
                            }),
                        ),
                ),
        )
        .into_any_element()
}

fn render_kero_commit_box(
    message: &str,
    focused: bool,
    staged_count: usize,
    busy: bool,
    cx: &mut Context<Forge>,
) -> AnyElement {
    let can_commit = !message.trim().is_empty() && staged_count > 0 && !busy;
    let field_text = if message.is_empty() {
        "Commit message".to_string()
    } else if focused {
        format!("{message}▏")
    } else {
        message.to_string()
    };
    let button_text = if busy {
        "Committing…".to_string()
    } else if staged_count > 0 {
        format!("Commit · {staged_count}")
    } else {
        "Commit".to_string()
    };
    let button_color = if can_commit {
        theme::ACCENT
    } else {
        theme::accent::MUTED
    };

    div()
        .flex()
        .flex_col()
        .gap(px(theme::space::SM))
        .px(px(theme::space::LG))
        .pt(px(theme::space::XS))
        .pb(px(theme::space::ML))
        .border_b_1()
        .border_color(theme::color(theme::border::SUBTLE))
        .child(
            div()
                .id("git-commit-message")
                .flex()
                .items_start()
                .w_full()
                .h(px(48.0))
                .overflow_hidden()
                .line_clamp(2)
                .px(px(theme::space::MD))
                .py(px(theme::space::SM))
                .rounded(px(theme::radius::MD))
                .bg(theme::color(theme::surface::INSET))
                .border_1()
                .border_color(theme::color(if focused {
                    theme::ACCENT
                } else {
                    theme::border::DEFAULT
                }))
                .cursor_text()
                .text_size(px(theme::font_size::SM))
                .text_color(theme::color(if message.is_empty() {
                    theme::text::DIM
                } else {
                    theme::text::DEFAULT
                }))
                .child(field_text)
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|this, _, _, cx| this.focus_git_input(GitInput::CommitMessage, cx)),
                ),
        )
        .child(
            div()
                .id("git-commit-staged")
                .flex()
                .items_center()
                .justify_center()
                .w_full()
                .h(px(30.0))
                .gap(px(theme::space::XS))
                .rounded(px(theme::radius::MD))
                .bg(theme::color(button_color))
                .text_size(px(theme::font_size::SM))
                .font_weight(FontWeight::MEDIUM)
                .text_color(theme::color(if can_commit {
                    theme::text::ON_ACCENT
                } else {
                    theme::text::FAINT
                }))
                .when(can_commit, |element| {
                    element
                        .cursor_pointer()
                        .hover(|style| style.bg(theme::color(theme::accent::HOVER)))
                        .active(|style| style.bg(theme::color(theme::accent::PRESSED)))
                })
                .child(
                    svg()
                        .path("icons/check.svg")
                        .size(px(12.0))
                        .text_color(theme::color(if can_commit {
                            theme::text::ON_ACCENT
                        } else {
                            theme::text::FAINT
                        })),
                )
                .child(button_text)
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|this, _, _, cx| this.commit_git_message(cx)),
                ),
        )
        .into_any_element()
}

fn render_kero_filter_bar(filter: &str, focused: bool, cx: &mut Context<Forge>) -> AnyElement {
    let text = if filter.is_empty() {
        "Filter changed files".to_string()
    } else if focused {
        format!("{filter}▏")
    } else {
        filter.to_string()
    };
    div()
        .px(px(theme::space::LG))
        .pb(px(theme::space::ML))
        .border_b_1()
        .border_color(theme::color(theme::border::SUBTLE))
        .child(
            div()
                .id("git-filter-field")
                .flex()
                .flex_row()
                .items_center()
                .h(px(28.0))
                .px(px(theme::space::MD))
                .gap(px(theme::space::SM))
                .rounded(px(theme::radius::MD))
                .bg(theme::color(theme::surface::INSET))
                .border_1()
                .border_color(theme::color(if focused {
                    theme::ACCENT
                } else {
                    theme::border::SUBTLE
                }))
                .cursor_text()
                .child(
                    svg()
                        .path("icons/filter.svg")
                        .size(px(11.0))
                        .text_color(theme::color(theme::text::DIM)),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w(px(0.0))
                        .truncate()
                        .text_size(px(11.0))
                        .text_color(theme::color(if filter.is_empty() {
                            theme::text::DIM
                        } else {
                            theme::text::DEFAULT
                        }))
                        .child(text),
                )
                .child(
                    div()
                        .flex_shrink_0()
                        .text_size(px(11.0))
                        .text_color(theme::color(theme::text::DIM))
                        .child("esc"),
                )
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|this, _, _, cx| this.focus_git_input(GitInput::Filter, cx)),
                ),
        )
        .into_any_element()
}

fn render_kero_git_section_header(
    title: &'static str,
    count: usize,
    collapsed: bool,
    section: GitSection,
    actions: Vec<(&'static str, GitMutation)>,
    busy: bool,
    cx: &mut Context<Forge>,
) -> AnyElement {
    let card = matches!(section, GitSection::Merge | GitSection::Staged);
    let label_color = if collapsed {
        theme::text::DIM
    } else if card {
        theme::text::STRONG
    } else {
        theme::text::MUTED
    };
    div()
        .flex()
        .flex_row()
        .items_center()
        .h(px(28.0))
        .when(!card && title != "HISTORY", |element| element.mt(px(14.0)))
        .px(px(theme::space::SM))
        .gap(px(theme::space::SM))
        .when(card && !collapsed, |element| {
            element
                .border_b_1()
                .border_color(theme::color(theme::border::SUBTLE))
        })
        .text_size(px(theme::font_size::XS))
        .font_weight(FontWeight::SEMIBOLD)
        .child(
            div()
                .id(ElementId::Name(format!("git-section-{title}").into()))
                .flex()
                .flex_row()
                .items_center()
                .gap(px(theme::space::XS))
                .cursor_pointer()
                .text_color(theme::color(label_color))
                .hover(|style| style.text_color(theme::color(theme::text::STRONG)))
                .child(
                    svg()
                        .path(if collapsed {
                            "icons/chevron-right.svg"
                        } else {
                            "icons/chevron-down.svg"
                        })
                        .size(px(12.0))
                        .flex_shrink_0()
                        .text_color(theme::color(label_color)),
                )
                .child(title)
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |this, _, _, cx| this.toggle_git_section(section, cx)),
                ),
        )
        .child(div().flex_1())
        .children(actions.into_iter().map(|(label, mutation)| {
            div()
                .id(ElementId::Name(
                    format!("git-section-action-{title}-{label}").into(),
                ))
                .cursor_pointer()
                .opacity(if busy { 0.3 } else { 1.0 })
                .text_size(px(10.0))
                .font_weight(FontWeight::NORMAL)
                .text_color(theme::color(theme::text::DIM))
                .hover(|style| style.text_color(theme::color(theme::text::MUTED)))
                .child(label)
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |this, _, _, cx| this.run_git_mutation(mutation.clone(), cx)),
                )
        }))
        .child(
            div()
                .min_w(px(18.0))
                .text_align(TextAlign::Right)
                .text_size(px(10.0))
                .font_weight(FontWeight::NORMAL)
                .text_color(theme::color(theme::text::MUTED))
                .child(count.to_string()),
        )
        .into_any_element()
}

fn render_kero_git_card(rows: Vec<AnyElement>, danger: bool) -> AnyElement {
    div()
        .flex()
        .flex_col()
        .mx(px(theme::space::MD))
        .mt(px(14.0))
        .pb(px(theme::space::XS))
        .overflow_hidden()
        .rounded(px(theme::radius::MD))
        .border_1()
        .border_color(theme::color(if danger {
            theme::danger::BORDER
        } else {
            theme::border::SECTION
        }))
        .bg(theme::color(if danger {
            theme::danger::SURFACE
        } else {
            theme::surface::SECTION
        }))
        .children(rows)
        .into_any_element()
}

fn render_kero_history_section(rows: Vec<AnyElement>) -> AnyElement {
    div()
        .flex()
        .flex_col()
        .mt(px(theme::space::XL))
        .pt(px(14.0))
        .border_t_1()
        .border_color(theme::color(theme::border::SUBTLE))
        .children(rows)
        .into_any_element()
}

fn git_discard_target(entry: &forge_git::Entry, change: forge_git::Change) -> GitDiscardTarget {
    GitDiscardTarget {
        path: entry.path.clone(),
        previous_path: entry.previous_path.clone(),
        untracked: change == forge_git::Change::Untracked,
    }
}

fn render_kero_git_entry(
    entry: &forge_git::Entry,
    change: forge_git::Change,
    staged: bool,
    busy: bool,
    width: f32,
    card: bool,
    cx: &mut Context<Forge>,
) -> AnyElement {
    let path = entry.path.clone();
    let click_path = path.clone();
    let action_path = path.clone();
    let previous_path = entry.previous_path.clone();
    let name = Path::new(&path)
        .file_name()
        .map(|name| name.to_string_lossy().to_string())
        .unwrap_or_else(|| path.clone());
    let directory = Path::new(&path)
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .map(|parent| left_truncate_path(&parent.display().to_string(), 34));
    let stat = if staged {
        entry.staged_stat
    } else {
        entry.unstaged_stat
    };
    let action = if staged {
        GitMutation::Unstage {
            path: action_path,
            previous_path,
        }
    } else {
        GitMutation::Stage {
            path: action_path,
            previous_path,
        }
    };
    let can_discard = !staged && change != forge_git::Change::Conflicted;
    let discard = git_discard_target(entry, change);

    div()
        .id(ElementId::Name(
            format!("git-{}-{path}", if staged { "staged" } else { "changed" }).into(),
        ))
        .flex()
        .flex_row()
        .items_center()
        .h(px(26.0))
        .mx(px(if card {
            theme::space::XS
        } else {
            theme::space::MD
        }))
        .px(px(theme::space::SM))
        .gap(px(theme::space::MD))
        .rounded(px(theme::radius::SM))
        .cursor_pointer()
        .hover(|style| style.bg(theme::color(theme::surface::HOVER)))
        .active(|style| style.bg(theme::color(theme::surface::ACTIVE)))
        .child(
            div()
                .w(px(11.0))
                .flex_shrink_0()
                .font(mono_font())
                .text_size(px(theme::font_size::SM))
                .font_weight(FontWeight::BOLD)
                .text_color(theme::color(git_change_color(change)))
                .child(change.code()),
        )
        .child(
            div()
                .when(width < 220.0, |element| element.flex_1().min_w(px(0.0)))
                .when(width >= 220.0, |element| {
                    element.flex_shrink_0().max_w(px(150.0))
                })
                .truncate()
                .text_size(px(theme::font_size::MD))
                .font_weight(FontWeight::MEDIUM)
                .text_color(theme::color(theme::text::DEFAULT))
                .child(name),
        )
        .children((width >= 220.0).then(|| {
            div()
                .flex_1()
                .min_w(px(0.0))
                .truncate()
                .text_size(px(theme::font_size::SM))
                .text_color(theme::color(theme::text::FAINT))
                .child(directory.unwrap_or_default())
        }))
        .children((width > 360.0).then(|| {
            let stat = stat.unwrap_or_default();
            div()
                .flex()
                .flex_shrink_0()
                .gap(px(theme::space::XS))
                .font(mono_font())
                .text_size(px(10.0))
                .children((stat.additions > 0).then(|| {
                    div()
                        .text_color(theme::color(theme::git::ADDED))
                        .child(format!("+{}", stat.additions))
                }))
                .children((stat.deletions > 0).then(|| {
                    div()
                        .text_color(theme::color(theme::git::DELETED))
                        .child(format!("−{}", stat.deletions))
                }))
        }))
        .children(can_discard.then(|| {
            div()
                .id(ElementId::Name(format!("git-discard-{path}").into()))
                .flex()
                .items_center()
                .justify_center()
                .size(px(22.0))
                .rounded(px(theme::radius::SM))
                .opacity(if busy { 0.3 } else { 0.55 })
                .text_color(theme::color(theme::text::DIM))
                .hover(|style| style.bg(theme::color(theme::surface::HOVER)).opacity(1.0))
                .child(
                    svg()
                        .path("icons/undo.svg")
                        .size(px(13.0))
                        .text_color(theme::color(theme::text::DIM)),
                )
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |this, _, _, cx| {
                        cx.stop_propagation();
                        this.run_git_mutation(GitMutation::Discard(discard.clone()), cx);
                    }),
                )
        }))
        .child(
            div()
                .id(ElementId::Name(
                    format!("git-action-{staged}-{path}").into(),
                ))
                .flex()
                .items_center()
                .justify_center()
                .size(px(22.0))
                .rounded(px(theme::radius::SM))
                .opacity(if busy { 0.3 } else { 0.55 })
                .text_color(theme::color(theme::text::DIM))
                .hover(|style| style.bg(theme::color(theme::surface::HOVER)).opacity(1.0))
                .child(
                    svg()
                        .path(if staged {
                            "icons/minus.svg"
                        } else {
                            "icons/plus.svg"
                        })
                        .size(px(13.0))
                        .text_color(theme::color(theme::text::DIM)),
                )
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |this, _, _, cx| {
                        cx.stop_propagation();
                        this.run_git_mutation(action.clone(), cx);
                    }),
                ),
        )
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(move |this, _, _, cx| {
                this.open_working_git_diff(click_path.clone(), staged, cx)
            }),
        )
        .into_any_element()
}

fn render_kero_commit_row(
    commit: &forge_git::Commit,
    selected: bool,
    lanes: usize,
    width: f32,
    cx: &mut Context<Forge>,
) -> AnyElement {
    let id = commit.id.clone();
    let reference = git_commit_branch(commit);
    let lane_color = theme::graph::LANES[commit.lane % theme::graph::LANES.len()];
    let metadata = format!(
        "{} · {} · {}",
        commit.short_id,
        commit.relative_date,
        author_initials(&commit.author)
    );
    div()
        .id(ElementId::Name(format!("git-commit-{id}").into()))
        .flex()
        .flex_row()
        .items_center()
        .h(px(theme::graph::ROW))
        .mx(px(theme::space::MD))
        .rounded(px(theme::radius::SM))
        .cursor_pointer()
        .when(selected, |element| {
            element.bg(theme::color_a(theme::accent::WASH))
        })
        .hover(|style| style.bg(theme::color(theme::surface::HOVER)))
        .active(|style| style.bg(theme::color(theme::surface::ACTIVE)))
        .child(render_kero_commit_rail(commit, selected, lanes))
        .child(
            div()
                .flex_1()
                .min_w(px(0.0))
                .flex()
                .flex_row()
                .items_center()
                .gap(px(theme::space::SM))
                .pr(px(theme::space::SM))
                .child(
                    div()
                        .flex_1()
                        .min_w(px(0.0))
                        .truncate()
                        .text_size(px(theme::font_size::MD))
                        .font_weight(if selected {
                            FontWeight::MEDIUM
                        } else {
                            FontWeight::NORMAL
                        })
                        .text_color(theme::color(if selected {
                            theme::text::DEFAULT
                        } else {
                            theme::text::MUTED
                        }))
                        .child(commit.subject.clone()),
                )
                .children(reference.map(|reference| {
                    div()
                        .flex_shrink_0()
                        .h(px(18.0))
                        .max_w(px(112.0))
                        .truncate()
                        .flex()
                        .items_center()
                        .px(px(theme::space::SM))
                        .rounded(px(theme::radius::SM))
                        .border_1()
                        .border_color(theme::color(lane_color))
                        .bg(theme::color_a((lane_color << 8) | 0x1f))
                        .text_size(px(theme::font_size::XS))
                        .font_weight(FontWeight::MEDIUM)
                        .text_color(theme::color(lane_color))
                        .child(reference)
                }))
                .children((width > 360.0).then(|| {
                    div()
                        .flex_shrink_0()
                        .max_w(px(150.0))
                        .truncate()
                        .font(mono_font())
                        .text_size(px(10.0))
                        .text_color(theme::color(theme::text::FAINT))
                        .child(metadata)
                })),
        )
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(move |this, _, _, cx| this.select_git_commit(id.clone(), cx)),
        )
        .into_any_element()
}

fn render_kero_commit_rail(commit: &forge_git::Commit, selected: bool, lanes: usize) -> AnyElement {
    let gutter = theme::graph::PITCH * lanes as f32 + theme::graph::PAD;
    let mut rail = div().relative().w(px(gutter)).h_full().flex_shrink_0();
    for segment in &commit.pass_through {
        rail = rail.child(render_graph_line(segment.lane, segment.style));
    }
    rail = rail.child(render_graph_line(commit.lane, commit.line_style));
    for join in &commit.joins {
        let bounds = graph_join_bounds(join.lane, join.other);
        let color = theme::graph::LANES[join.other % theme::graph::LANES.len()];
        let join_box = div()
            .absolute()
            .left(px(bounds.left))
            .w(px(bounds.width))
            .h(px(13.75))
            .when(bounds.vertical_on_right, |element| {
                element.border_r(px(theme::graph::LINE))
            })
            .when(!bounds.vertical_on_right, |element| {
                element.border_l(px(theme::graph::LINE))
            })
            .border_color(theme::color(color));
        rail = rail.child(match (join.kind, bounds.vertical_on_right) {
            (forge_git::JoinKind::Merge, true) => join_box
                .top(px(12.25))
                .border_t(px(theme::graph::LINE))
                .rounded_tr(px(theme::graph::JOIN_RADIUS)),
            (forge_git::JoinKind::Merge, false) => join_box
                .top(px(12.25))
                .border_t(px(theme::graph::LINE))
                .rounded_tl(px(theme::graph::JOIN_RADIUS)),
            (forge_git::JoinKind::BranchOut, true) => join_box
                .top(px(0.0))
                .border_b(px(theme::graph::LINE))
                .rounded_br(px(theme::graph::JOIN_RADIUS)),
            (forge_git::JoinKind::BranchOut, false) => join_box
                .top(px(0.0))
                .border_b(px(theme::graph::LINE))
                .rounded_bl(px(theme::graph::JOIN_RADIUS)),
        });
    }

    let x = graph_lane_x(commit.lane);
    let lane_color = theme::graph::LANES[commit.lane % theme::graph::LANES.len()];
    if commit.is_head {
        let row_bg = if selected {
            // Opaque result of the 10% accent wash over surface::BASE.
            (0x202836 << 8) | 0xff
        } else {
            (theme::surface::BASE << 8) | 0xff
        };
        rail.child(
            div()
                .absolute()
                .left(px(x - 4.5))
                .top(px(8.5))
                .size(px(9.0))
                .rounded(px(5.0))
                .border_2()
                .border_color(theme::color(lane_color))
                .bg(theme::color_a(row_bg)),
        )
        .into_any_element()
    } else {
        // Draw the dot directly over the rail. The previous 2px background
        // halo cut every vertical line into visibly disconnected segments.
        rail.child(
            div()
                .absolute()
                .left(px(x - theme::graph::DOT / 2.0))
                .top(px((theme::graph::ROW - theme::graph::DOT) / 2.0))
                .size(px(theme::graph::DOT))
                .rounded(px(6.0))
                .bg(theme::color(lane_color)),
        )
        .into_any_element()
    }
}

fn render_graph_line(lane: usize, style: forge_git::LineStyle) -> AnyElement {
    let (top, height) = match style {
        forge_git::LineStyle::Full => (0.0, theme::graph::ROW),
        forge_git::LineStyle::Newest => (theme::graph::ROW / 2.0, theme::graph::ROW / 2.0),
        forge_git::LineStyle::Oldest => (0.0, theme::graph::ROW / 2.0),
        forge_git::LineStyle::Isolated => (0.0, 0.0),
    };
    div()
        .absolute()
        .left(px(graph_lane_x(lane) - theme::graph::LINE / 2.0))
        .top(px(top))
        .w(px(theme::graph::LINE))
        .h(px(height))
        .bg(theme::color(
            theme::graph::LANES[lane % theme::graph::LANES.len()],
        ))
        .into_any_element()
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct GraphJoinBounds {
    left: f32,
    width: f32,
    vertical_on_right: bool,
}

fn graph_join_bounds(lane: usize, other: usize) -> GraphJoinBounds {
    let own_x = graph_lane_x(lane);
    let other_x = graph_lane_x(other);
    let half_line = theme::graph::LINE / 2.0;
    if other_x > own_x {
        GraphJoinBounds {
            left: own_x,
            // GPUI draws borders inside the box. Extending by half the border
            // width centers the right edge exactly on the other lane.
            width: other_x - own_x + half_line,
            vertical_on_right: true,
        }
    } else {
        GraphJoinBounds {
            // Offset the left edge by half the border width so its center,
            // rather than its outer edge, lands exactly on the other lane.
            left: other_x - half_line,
            width: own_x - other_x + half_line,
            vertical_on_right: false,
        }
    }
}

fn graph_lane_x(lane: usize) -> f32 {
    theme::graph::ORIGIN + theme::graph::PITCH * lane.min(theme::graph::MAX_LANES - 1) as f32
}

fn render_kero_commit_detail_state(message: &'static str, lane: usize, lanes: usize) -> AnyElement {
    let gutter = theme::graph::PITCH * lanes as f32 + theme::graph::PAD;
    div()
        .flex()
        .items_center()
        .h(px(22.0))
        .mx(px(theme::space::MD))
        .child(
            div()
                .relative()
                .w(px(gutter))
                .h_full()
                .flex_shrink_0()
                .child(render_graph_child_line(lane)),
        )
        .text_size(px(10.0))
        .text_color(theme::color(theme::text::FAINT))
        .child(message)
        .into_any_element()
}

fn render_kero_commit_file_row(
    index: usize,
    change: &forge_git::FileChange,
    lane: usize,
    lanes: usize,
    cx: &mut Context<Forge>,
) -> AnyElement {
    let path = change.path.clone();
    let click_path = path.clone();
    let name = Path::new(&path)
        .file_name()
        .map(|name| name.to_string_lossy().to_string())
        .unwrap_or_else(|| path.clone());
    let directory = Path::new(&path)
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .map(|parent| left_truncate_path(&parent.display().to_string(), 34));
    let gutter = theme::graph::PITCH * lanes as f32 + theme::graph::PAD;
    div()
        .id(ElementId::Name(
            format!("git-commit-file-{index}-{path}").into(),
        ))
        .flex()
        .flex_row()
        .items_center()
        .h(px(22.0))
        .mx(px(theme::space::MD))
        .pr(px(theme::space::SM))
        .gap(px(theme::space::MD))
        .rounded(px(theme::radius::SM))
        .cursor_pointer()
        .hover(|style| style.bg(theme::color(theme::surface::HOVER)))
        .active(|style| style.bg(theme::color(theme::surface::ACTIVE)))
        .child(
            div()
                .relative()
                .w(px(gutter))
                .h_full()
                .flex_shrink_0()
                .child(render_graph_child_line(lane)),
        )
        .child(
            div()
                .w(px(11.0))
                .flex_shrink_0()
                .font(mono_font())
                .text_size(px(11.0))
                .font_weight(FontWeight::BOLD)
                .text_color(theme::color(git_change_color(change.change)))
                .child(change.change.code()),
        )
        .child(
            div()
                .flex_shrink_0()
                .max_w(px(150.0))
                .truncate()
                .text_size(px(11.5))
                .font_weight(FontWeight::MEDIUM)
                .text_color(theme::color(theme::text::STRONG))
                .child(name),
        )
        .children(directory.map(|directory| {
            div()
                .flex_1()
                .min_w(px(0.0))
                .truncate()
                .text_size(px(10.0))
                .text_color(theme::color(theme::text::FAINT))
                .child(directory)
        }))
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(move |this, _, _, cx| this.open_git_diff(click_path.clone(), cx)),
        )
        .into_any_element()
}

fn render_graph_child_line(lane: usize) -> AnyElement {
    div()
        .absolute()
        .left(px(graph_lane_x(lane) - theme::graph::LINE / 2.0))
        .top(px(0.0))
        .w(px(theme::graph::LINE))
        .h_full()
        .bg(theme::color(
            theme::graph::LANES[lane % theme::graph::LANES.len()],
        ))
        .into_any_element()
}

fn render_git_clean_state(ahead: u32, behind: u32, cx: &mut Context<Forge>) -> AnyElement {
    let action = if ahead > 0 {
        Some((
            format!("Push {ahead} commit{}", if ahead == 1 { "" } else { "s" }),
            GitMutation::Push,
        ))
    } else if behind > 0 {
        Some((
            format!("Pull {behind} commit{}", if behind == 1 { "" } else { "s" }),
            GitMutation::Pull,
        ))
    } else {
        None
    };
    div()
        .flex()
        .flex_col()
        .items_center()
        .justify_center()
        .gap(px(theme::space::MD))
        .py(px(18.0))
        .px(px(theme::space::LG))
        .child(
            div()
                .text_size(px(11.0))
                .font_weight(FontWeight::MEDIUM)
                .text_color(theme::color(theme::text::MUTED))
                .child("Working tree clean"),
        )
        .children(action.map(|(label, mutation)| {
            div()
                .id("git-clean-next-action")
                .h(px(26.0))
                .px(px(theme::space::LG))
                .flex()
                .items_center()
                .justify_center()
                .rounded(px(theme::radius::MD))
                .border_1()
                .border_color(theme::color(theme::border::DEFAULT))
                .cursor_pointer()
                .text_size(px(10.5))
                .font_weight(FontWeight::MEDIUM)
                .text_color(theme::color(theme::ACCENT))
                .hover(|style| style.bg(theme::color(theme::surface::HOVER)))
                .child(label)
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |this, _, _, cx| this.run_git_mutation(mutation.clone(), cx)),
                )
        }))
        .into_any_element()
}

fn render_git_filter_empty(filter: &str, hidden: usize) -> AnyElement {
    render_kero_inline_state(
        &format!("No files match “{filter}”"),
        Some(format!(
            "{hidden} file{} hidden by filter",
            if hidden == 1 { "" } else { "s" }
        )),
    )
}

fn render_kero_inline_state(message: &str, detail: Option<String>) -> AnyElement {
    div()
        .flex()
        .flex_col()
        .items_center()
        .justify_center()
        .gap(px(theme::space::XS))
        .py(px(18.0))
        .px(px(theme::space::LG))
        .child(
            div()
                .text_size(px(11.0))
                .font_weight(FontWeight::MEDIUM)
                .text_color(theme::color(theme::text::MUTED))
                .child(message.to_string()),
        )
        .children(detail.map(|detail| {
            div()
                .text_size(px(10.0))
                .text_color(theme::color(theme::text::FAINT))
                .child(detail)
        }))
        .into_any_element()
}

fn render_git_loading() -> AnyElement {
    div()
        .flex()
        .flex_col()
        .flex_1()
        .opacity(0.4)
        .child(
            div()
                .flex()
                .items_center()
                .gap(px(theme::space::MD))
                .px(px(theme::space::LG))
                .py(px(theme::space::ML))
                .child(
                    div()
                        .size(px(14.0))
                        .rounded(px(7.0))
                        .bg(theme::color(theme::surface::HOVER)),
                )
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .gap(px(theme::space::XS))
                        .child(
                            div()
                                .w(px(118.0))
                                .h(px(10.0))
                                .rounded(px(theme::radius::SM))
                                .bg(theme::color(theme::surface::HOVER)),
                        )
                        .child(
                            div()
                                .w(px(82.0))
                                .h(px(8.0))
                                .rounded(px(theme::radius::SM))
                                .bg(theme::color(theme::surface::HOVER)),
                        ),
                ),
        )
        .into_any_element()
}

fn render_not_repository(target: String, cx: &mut Context<Forge>) -> AnyElement {
    div()
        .flex()
        .flex_col()
        .flex_1()
        .items_center()
        .justify_center()
        .gap(px(theme::space::MD))
        .px(px(18.0))
        .child(
            svg()
                .path("icons/git-branch.svg")
                .size(px(24.0))
                .text_color(theme::color(theme::text::DIM)),
        )
        .child(
            div()
                .text_size(px(12.0))
                .font_weight(FontWeight::MEDIUM)
                .text_color(theme::color(theme::text::DEFAULT))
                .child("No Git repository"),
        )
        .child(
            div()
                .max_w(px(280.0))
                .text_align(TextAlign::Center)
                .text_size(px(10.5))
                .text_color(theme::color(theme::text::FAINT))
                .child(target),
        )
        .child(
            div()
                .id("git-initialize")
                .flex()
                .items_center()
                .justify_center()
                .h(px(28.0))
                .px(px(theme::space::LG))
                .rounded(px(theme::radius::MD))
                .bg(theme::color(theme::ACCENT))
                .cursor_pointer()
                .text_size(px(11.0))
                .font_weight(FontWeight::MEDIUM)
                .text_color(theme::color(theme::text::ON_ACCENT))
                .hover(|style| style.bg(theme::color(theme::accent::HOVER)))
                .active(|style| style.bg(theme::color(theme::accent::PRESSED)))
                .child("git init")
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|this, _, _, cx| {
                        this.run_git_mutation(GitMutation::Initialize, cx)
                    }),
                ),
        )
        .into_any_element()
}

fn left_truncate_path(path: &str, max_chars: usize) -> String {
    let count = path.chars().count();
    if count <= max_chars {
        return path.to_string();
    }
    let keep = max_chars.saturating_sub(1);
    let tail = path.chars().skip(count - keep).collect::<String>();
    format!("…{tail}")
}

fn author_initials(author: &str) -> String {
    author
        .split_whitespace()
        .filter_map(|part| part.chars().next())
        .take(2)
        .flat_map(char::to_uppercase)
        .collect()
}

/// A round avatar thumbnail, or an initials placeholder while the bitmap is
/// still loading (or failed to load) — never blocks on the network.
fn render_avatar(image: Option<&Arc<Image>>, login: &str, size: f32) -> AnyElement {
    let content = match image {
        Some(image) => img(image.clone())
            .size(px(size))
            .object_fit(ObjectFit::Cover)
            .into_any_element(),
        None => div()
            .size(px(size))
            .flex()
            .items_center()
            .justify_center()
            .bg(theme::color(theme::surface::ACTIVE))
            .text_size(px((size * 0.42).max(8.0)))
            .text_color(theme::color(theme::text::MUTED))
            .child(author_initials(login))
            .into_any_element(),
    };
    div()
        .size(px(size))
        .flex_shrink_0()
        .overflow_hidden()
        .rounded(px(size / 2.0))
        .child(content)
        .into_any_element()
}

fn sidebar_status_text(text: &'static str, color: u32) -> AnyElement {
    div()
        .min_w(px(0.0))
        .truncate()
        .text_size(px(theme::font_size::SM))
        .text_color(theme::color(color))
        .child(text)
        .into_any_element()
}

fn git_change_color(change: forge_git::Change) -> u32 {
    match change {
        forge_git::Change::Added => theme::git::ADDED,
        forge_git::Change::Modified => theme::git::MODIFIED,
        forge_git::Change::Deleted => theme::git::DELETED,
        forge_git::Change::Renamed | forge_git::Change::Copied => theme::git::RENAMED,
        forge_git::Change::Untracked => theme::git::UNTRACKED,
        forge_git::Change::Conflicted => theme::git::CONFLICTED,
    }
}

fn git_line(summary: Option<&forge_git::Summary>, _selected: bool) -> AnyElement {
    let Some(git) = summary.filter(|g| g.is_repo) else {
        // Reserve the metadata line so git and non-git workspace rows keep the
        // same vertical rhythm.
        return div().h(px(13.0)).into_any_element();
    };

    let base = theme::text::MUTED;
    let mut branch = git.branch.clone().unwrap_or_else(|| "detached".into());
    if git.is_dirty() {
        branch.push('*');
    }

    let mut markers: Vec<(String, u32)> = Vec::new();
    if git.ahead > 0 {
        markers.push((format!("\u{2191}{}", git.ahead), base));
    }
    if git.behind > 0 {
        markers.push((format!("\u{2193}{}", git.behind), base));
    }
    div()
        .flex()
        .flex_row()
        .items_center()
        .w_full()
        .h(px(13.0))
        .gap(px(theme::space::SM))
        .font(mono_font())
        .text_size(px(theme::font_size::SM))
        .child(
            div()
                .flex_1()
                .min_w(px(0.0))
                .truncate()
                .text_color(theme::color(base))
                .child(branch),
        )
        .children(markers.into_iter().map(|(text, color)| {
            div()
                .flex_shrink_0()
                .text_color(theme::color(color))
                .child(text)
        }))
        .into_any_element()
}

fn empty_state(message: &'static str) -> AnyElement {
    div()
        .flex()
        .flex_1()
        .min_h(px(0.0))
        .items_center()
        .justify_center()
        .p(px(theme::space::XL))
        .text_size(px(theme::font_size::MD))
        .text_color(theme::color(theme::text::DIM))
        .child(message)
        .into_any_element()
}

fn git_commit_branch(commit: &forge_git::Commit) -> Option<String> {
    // Only tag commits that carry their own ref decoration (a real branch
    // head or tag). Lane-inherited labels would tag every ancestor commit
    // on the mainline, burying the sidebar in redundant chips.
    commit
        .refs
        .iter()
        .map(|label| label.strip_prefix("HEAD -> ").unwrap_or(label))
        .map(|label| label.strip_prefix("tag: ").unwrap_or(label))
        .find(|label| !label.is_empty() && *label != "HEAD" && !label.ends_with("/HEAD"))
        .map(str::to_string)
}

fn push_file_rows(
    node: &FileNode,
    depth: usize,
    expanded: &HashSet<PathBuf>,
    selected: &Option<PathBuf>,
    rows: &mut Vec<AnyElement>,
    cx: &mut Context<Forge>,
) {
    let is_selected = selected.as_ref() == Some(&node.path);
    let is_expanded = expanded.contains(&node.path);
    // Geometric chevrons in a fixed-width slot. ASCII "v"/">" read as text and
    // misaligned against the filename column.
    let icon = match (node.is_dir, is_expanded) {
        (true, true) => "\u{25be}",
        (true, false) => "\u{25b8}",
        (false, _) => "",
    };
    let click_path = node.path.clone();
    let is_dir = node.is_dir;

    rows.push(
        div()
            .id(ElementId::Path(Arc::from(node.path.as_path())))
            .flex()
            .flex_row()
            .items_center()
            .flex_shrink_0()
            .h(px(theme::row::HEIGHT))
            .pl(px(theme::space::MD + depth as f32 * theme::space::LG))
            .pr(px(theme::space::MD))
            .cursor_pointer()
            .when(is_selected, |d| d.bg(theme::color(theme::surface::ACTIVE)))
            .hover(|s| s.bg(theme::color(theme::surface::HOVER)))
            .child(
                div()
                    .w(px(theme::row::ICON_SLOT))
                    .flex_shrink_0()
                    .text_size(px(theme::font_size::XS))
                    .text_color(theme::color(theme::text::DIM))
                    .child(icon),
            )
            .child(
                div()
                    .flex_1()
                    .min_w(px(0.0))
                    .truncate()
                    .text_size(px(theme::font_size::MD))
                    .text_color(theme::color(if is_selected {
                        theme::text::DEFAULT
                    } else if node.is_dir {
                        theme::text::MUTED
                    } else {
                        theme::text::DIM
                    }))
                    .child(node.name.clone()),
            )
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, _, _, cx| {
                    if is_dir {
                        this.toggle_dir(click_path.clone(), cx);
                    } else {
                        this.open_file(click_path.clone(), cx);
                    }
                }),
            )
            .into_any_element(),
    );

    if node.is_dir && is_expanded {
        for child in &node.children {
            push_file_rows(child, depth + 1, expanded, selected, rows, cx);
        }
    }
}

fn render_editor_line_with_cursor(text: &str, cursor_col: usize) -> AnyElement {
    let chars: Vec<char> = text.chars().collect();
    let idx = cursor_col.min(chars.len());

    let pre: String = chars[..idx].iter().collect();
    let cursor_ch = chars
        .get(idx)
        .map(|c| c.to_string())
        .unwrap_or_else(|| " ".to_string());
    let post: String = if idx < chars.len() {
        chars[idx + 1..].iter().collect()
    } else {
        String::new()
    };

    div()
        .flex_1()
        .flex()
        .flex_row()
        .text_color(rgb(DEFAULT_FG))
        .child(pre)
        .child(
            div()
                .bg(rgb(DEFAULT_FG))
                .text_color(rgb(TERMINAL_BG))
                .child(cursor_ch),
        )
        .child(post)
        .into_any_element()
}

fn collect_files(node: &FileNode, root: &std::path::Path, items: &mut Vec<PaletteItem>) {
    if !node.is_dir {
        let label = node
            .path
            .strip_prefix(root)
            .unwrap_or(&node.path)
            .to_string_lossy()
            .to_string();
        items.push(PaletteItem {
            label,
            kind: "FILE",
            action: PaletteAction::OpenFile(node.path.clone()),
        });
        return;
    }
    for child in &node.children {
        collect_files(child, root, items);
    }
}

fn next_primary_view(active: ViewMode) -> ViewMode {
    match active {
        ViewMode::Terminal => ViewMode::Editor,
        ViewMode::Editor => ViewMode::Agents,
        ViewMode::Agents => ViewMode::Terminal,
        ViewMode::Profile => ViewMode::Terminal,
    }
}

fn should_close_editor(active_view: ViewMode, key: &str) -> bool {
    active_view == ViewMode::Editor && key == "escape"
}

fn translate_keystroke(ks: &Keystroke) -> Option<Vec<u8>> {
    if ks.modifiers.control {
        if let Some(ch) = ks.key.chars().next() {
            if ch.is_ascii_alphabetic() {
                return Some(vec![(ch.to_ascii_uppercase() as u8) & 0x1f]);
            }
        }
    }

    if let Some(key_char) = ks.key_char.as_ref() {
        if !key_char.is_empty() {
            return Some(key_char.as_bytes().to_vec());
        }
    }

    match ks.key.as_str() {
        "enter" => Some(b"\r".to_vec()),
        "backspace" => Some(vec![0x7f]),
        "tab" => Some(b"\t".to_vec()),
        "escape" => Some(vec![0x1b]),
        "space" => Some(b" ".to_vec()),
        "up" => Some(b"\x1b[A".to_vec()),
        "down" => Some(b"\x1b[B".to_vec()),
        "right" => Some(b"\x1b[C".to_vec()),
        "left" => Some(b"\x1b[D".to_vec()),
        _ => None,
    }
}

impl Focusable for Forge {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for Forge {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let render_start = self.frame_stats.is_some().then(Instant::now);
        self.sync_terminal_size(window);
        let top_bar = self.render_top_bar(cx);
        let sidebar = self.show_workspace_sidebar.then(|| self.render_sidebar(cx));
        // Only the visible view is built: rendering both and discarding one
        // meant paying for a full editor layout on every terminal frame.
        let main_content = div()
            .id("main-content")
            .flex()
            .flex_1()
            .min_w(px(0.0))
            .h_full()
            .child(match self.active_view {
                ViewMode::Terminal => self.render_terminal().into_any_element(),
                ViewMode::Editor => self.render_editor(cx).into_any_element(),
                ViewMode::Agents => self.render_agents(),
                ViewMode::Profile => self.render_profile(cx),
            })
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, _, cx| this.blur_git_input(cx)),
            );
        let show_info_panel = self.show_info_panel;
        let info_panel = show_info_panel.then(|| self.render_info_panel(cx));
        let palette = self.palette_open.then(|| self.render_palette(cx));
        let workspace_resize_handle = self
            .show_workspace_sidebar
            .then(|| self.render_resize_handle(ResizeSide::Workspace, cx));
        let info_panel_resize_handle = self
            .show_info_panel
            .then(|| self.render_resize_handle(ResizeSide::InfoPanel, cx));

        let root = div()
            .id("forge-root")
            .key_context("Forge")
            .track_focus(&self.focus_handle)
            .on_key_down(cx.listener(Self::handle_key_down))
            .on_mouse_move(cx.listener(|this, event: &gpui::MouseMoveEvent, _, cx| {
                this.update_sidebar_resize(f32::from(event.position.x), cx);
            }))
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, _, _, cx| this.end_sidebar_resize(cx)),
            )
            .on_mouse_up_out(
                MouseButton::Left,
                cx.listener(|this, _, _, cx| this.end_sidebar_resize(cx)),
            )
            .relative()
            .flex()
            .flex_col()
            .size_full()
            .bg(theme::color(theme::surface::BASE))
            .child(top_bar)
            .child(
                div()
                    .flex()
                    .flex_row()
                    .flex_1()
                    .min_h(px(0.0))
                    .children(sidebar)
                    .children(workspace_resize_handle)
                    .child(main_content)
                    .children(info_panel_resize_handle)
                    .children(info_panel),
            )
            .children(palette);

        if let (Some(stats), Some(start)) = (self.frame_stats.as_mut(), render_start) {
            stats.record(start.elapsed());
        }
        root
    }
}

fn version_text() -> String {
    format!(
        "forge {} ({})",
        env!("CARGO_PKG_VERSION"),
        updater::BUILD_REVISION
    )
}

fn print_version_if_requested() -> bool {
    let requested = std::env::args_os().nth(1).is_some_and(|argument| {
        argument == std::ffi::OsStr::new("--version") || argument == std::ffi::OsStr::new("-V")
    });
    if requested {
        println!("{}", version_text());
    }
    requested
}

fn main() {
    if print_version_if_requested() {
        return;
    }
    if let Some(code) = github::run_git_credential_helper() {
        std::process::exit(code);
    }
    Application::new()
        .with_assets(assets::Assets)
        .run(|cx: &mut App| {
            let bounds = Bounds::centered(None, size(px(1280.0), px(800.0)), cx);
            let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));

            let window = cx
                .open_window(
                    WindowOptions {
                        window_bounds: Some(WindowBounds::Windowed(bounds)),
                        titlebar: Some(TitlebarOptions {
                            title: Some(SharedString::from("Forge")),
                            // Transparent titlebar lets our top bar occupy the full
                            // window width, with the traffic lights sitting inside
                            // it instead of in a separate system strip above.
                            appears_transparent: true,
                            traffic_light_position: Some(point(
                                px(13.0),
                                px((TOP_BAR_HEIGHT - 12.0) / 2.0),
                            )),
                        }),
                        ..Default::default()
                    },
                    move |_window, cx| cx.new(|cx| Forge::new(cx, vec![cwd.clone()])),
                )
                .unwrap();

            window
                .update(cx, |view, window, cx| {
                    window.focus(&view.focus_handle(cx));
                    cx.activate(true);
                })
                .unwrap();
        });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_includes_package_and_build_revision() {
        assert_eq!(
            version_text(),
            format!(
                "forge {} ({})",
                env!("CARGO_PKG_VERSION"),
                updater::BUILD_REVISION
            )
        );
    }

    #[test]
    fn primary_view_toggle_round_trips_even_without_an_open_file() {
        assert_eq!(next_primary_view(ViewMode::Terminal), ViewMode::Editor);
        assert_eq!(next_primary_view(ViewMode::Editor), ViewMode::Agents);
        assert_eq!(next_primary_view(ViewMode::Agents), ViewMode::Terminal);
        assert_eq!(next_primary_view(ViewMode::Profile), ViewMode::Terminal);
    }

    #[test]
    fn escape_closes_only_the_editor_surface() {
        assert!(should_close_editor(ViewMode::Editor, "escape"));
        assert!(!should_close_editor(ViewMode::Editor, "enter"));
        assert!(!should_close_editor(ViewMode::Terminal, "escape"));
        assert!(!should_close_editor(ViewMode::Agents, "escape"));
    }

    fn commit_fixture(refs: &[&str]) -> forge_git::Commit {
        forge_git::Commit {
            id: "abc123".into(),
            short_id: "abc123".into(),
            parents: Vec::new(),
            graph: "*".into(),
            connectors: Vec::new(),
            refs: refs.iter().map(|r| r.to_string()).collect(),
            is_head: false,
            merged_to_main: false,
            lane: 0,
            line_style: forge_git::LineStyle::Isolated,
            pass_through: Vec::new(),
            joins: Vec::new(),
            subject: "Test commit".into(),
            author: "Test".into(),
            relative_date: "now".into(),
        }
    }

    #[test]
    fn commit_branch_tag_only_shows_for_commits_with_their_own_ref() {
        // Plain history commits (no decoration on this exact commit) get no
        // tag; tagging every ancestor of `main` was the reported "main~1,
        // main~2, ..." clutter.
        assert_eq!(git_commit_branch(&commit_fixture(&[])), None);
        assert_eq!(
            git_commit_branch(&commit_fixture(&["HEAD -> main", "origin/main"])),
            Some("main".to_string())
        );
        assert_eq!(
            git_commit_branch(&commit_fixture(&["tag: v1.0"])),
            Some("v1.0".to_string())
        );
        // A remote's symbolic HEAD pointer isn't a real branch.
        assert_eq!(git_commit_branch(&commit_fixture(&["origin/HEAD"])), None);
    }

    #[test]
    fn join_bounds_center_vertical_borders_on_lanes_in_both_directions() {
        let right = graph_join_bounds(0, 2);
        assert!(right.vertical_on_right);
        assert_eq!(
            right.left + right.width - theme::graph::LINE / 2.0,
            graph_lane_x(2)
        );
        assert_eq!(right.left, graph_lane_x(0));

        let left = graph_join_bounds(2, 0);
        assert!(!left.vertical_on_right);
        assert_eq!(left.left + theme::graph::LINE / 2.0, graph_lane_x(0));
        assert_eq!(left.left + left.width, graph_lane_x(2));
    }
}
