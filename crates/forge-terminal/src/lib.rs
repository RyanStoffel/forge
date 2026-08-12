//! Terminal engine: spawns a PTY and maintains a parsed terminal grid.
//!
//! Uses `vt100` for VT/ANSI parsing today. This is deliberately isolated
//! behind this crate's API so it can be swapped for `libghostty-vt` later
//! once its Rust bindings stabilize (see docs/engineering-plan.md, risk
//! table) without touching any UI code.

use std::io::{Read, Write};
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::thread;

/// Invoked on the reader thread whenever new output has been parsed, so the
/// UI can wake and repaint instead of polling on a timer. Implementations must
/// be cheap and non-blocking; coalescing repeated signals is the caller's job.
pub type OutputNotifier = Arc<dyn Fn() + Send + Sync>;

use anyhow::{Context, Result};
use portable_pty::{native_pty_system, Child, CommandBuilder, MasterPty, PtySize};

pub use vt100::{Cell, Screen};

pub struct TerminalPane {
    parser: Arc<Mutex<vt100::Parser>>,
    writer: Box<dyn Write + Send>,
    master: Box<dyn MasterPty + Send>,
    child: Box<dyn Child + Send + Sync>,
    generation: Arc<Mutex<u64>>,
    /// Last complete screen available to render without waiting for the parser
    /// thread. `Arc` makes the fallback an atomic-sized clone rather than a
    /// full grid copy.
    screen_cache: Mutex<Arc<vt100::Screen>>,
    size: Mutex<(u16, u16)>,
}

impl TerminalPane {
    /// Spawn the user's shell (`$SHELL`, falling back to `/bin/zsh`) in a new
    /// PTY, rooted at `cwd`, with an initial grid size of `rows` x `cols`.
    ///
    /// `on_output` is called from the reader thread each time a chunk has been
    /// parsed.
    pub fn spawn(
        cwd: &Path,
        rows: u16,
        cols: u16,
        on_output: Option<OutputNotifier>,
    ) -> Result<Self> {
        let pty_system = native_pty_system();
        let pair = pty_system
            .openpty(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .context("failed to open pty")?;

        let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/zsh".to_string());
        let mut cmd = CommandBuilder::new(shell);
        cmd.cwd(cwd);
        cmd.env("TERM", "xterm-256color");

        let child = pair
            .slave
            .spawn_command(cmd)
            .context("failed to spawn shell in pty")?;
        drop(pair.slave);

        let reader = pair
            .master
            .try_clone_reader()
            .context("failed to clone pty reader")?;
        let writer = pair
            .master
            .take_writer()
            .context("failed to take pty writer")?;

        let parser_value = vt100::Parser::new(rows, cols, 10_000);
        let screen_cache = Mutex::new(Arc::new(parser_value.screen().clone()));
        let parser = Arc::new(Mutex::new(parser_value));
        let generation = Arc::new(Mutex::new(0u64));

        spawn_reader_thread(reader, parser.clone(), generation.clone(), on_output);

        Ok(Self {
            parser,
            writer,
            master: pair.master,
            child,
            generation,
            screen_cache,
            size: Mutex::new((rows, cols)),
        })
    }

    /// Write raw bytes (typically keyboard input) to the shell's stdin.
    pub fn write_input(&mut self, bytes: &[u8]) -> Result<()> {
        self.writer.write_all(bytes).context("pty write failed")?;
        self.writer.flush().ok();
        Ok(())
    }

    /// Resize the underlying PTY and the parser's grid to match.
    pub fn resize(&mut self, rows: u16, cols: u16) -> Result<()> {
        self.master
            .resize(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .context("pty resize failed")?;
        self.parser.lock().unwrap().set_size(rows, cols);
        *self.size.lock().unwrap() = (rows, cols);
        Ok(())
    }

    /// Run `f` against the current terminal screen contents.
    pub fn with_screen<R>(&self, f: impl FnOnce(&vt100::Screen) -> R) -> R {
        let parser = self.parser.lock().unwrap();
        f(parser.screen())
    }

    /// Obtain a renderable screen without ever waiting for PTY parsing.
    ///
    /// During sustained output the parser may own its mutex continuously. A
    /// blocking renderer then misses frame deadlines behind terminal I/O. If
    /// the parser is busy, return the previous completed snapshot; the output
    /// notifier has already queued another repaint where we'll catch up.
    pub fn screen_snapshot(&self) -> Arc<vt100::Screen> {
        if let Ok(parser) = self.parser.try_lock() {
            let snapshot = Arc::new(parser.screen().clone());
            if let Ok(mut cache) = self.screen_cache.lock() {
                *cache = Arc::clone(&snapshot);
            }
            snapshot
        } else {
            self.screen_cache
                .lock()
                .map(|screen| Arc::clone(&screen))
                .unwrap_or_else(|_| Arc::new(vt100::Parser::new(1, 1, 0).screen().clone()))
        }
    }

    /// Current grid size as (rows, cols).
    pub fn size(&self) -> (u16, u16) {
        *self.size.lock().unwrap()
    }

    /// Monotonically increasing counter bumped every time new output has
    /// been parsed. UI code can poll this cheaply to decide whether a
    /// repaint is needed instead of diffing the whole grid.
    pub fn generation(&self) -> u64 {
        *self.generation.lock().unwrap()
    }

    /// Whether the shell process is still running.
    pub fn is_alive(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(None))
    }

    /// PID of the shell driving this pane, used to scope process inspection to
    /// what's actually running in the pane.
    pub fn pid(&self) -> Option<u32> {
        self.child.process_id()
    }

    /// PID of the pane's *foreground* process group leader — the program the
    /// user is actually interacting with, which is the shell when idle and the
    /// running command otherwise.
    ///
    /// This is the same signal terminals use to title their tabs, and is far
    /// cheaper than walking the process tree.
    pub fn foreground_pid(&self) -> Option<u32> {
        let fd = self.master.as_raw_fd()?;
        // SAFETY: `as_raw_fd()` is the live PTY master owned by `self`; the fd
        // remains valid for this call and `tcgetpgrp` does not retain it.
        let pid = unsafe { libc::tcgetpgrp(fd) };
        (pid > 0).then_some(pid as u32)
    }
}

fn spawn_reader_thread(
    mut reader: Box<dyn Read + Send>,
    parser: Arc<Mutex<vt100::Parser>>,
    generation: Arc<Mutex<u64>>,
    on_output: Option<OutputNotifier>,
) {
    thread::spawn(move || {
        let mut buf = [0u8; 8192];
        loop {
            match reader.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    parser.lock().unwrap().process(&buf[..n]);
                    *generation.lock().unwrap() += 1;
                    if let Some(notify) = &on_output {
                        notify();
                    }
                    // Give a non-blocking screen snapshot a chance between
                    // chunks during an unbounded output flood.
                    thread::yield_now();
                }
                Err(_) => break,
            }
        }
    });
}

#[cfg(test)]
mod tests {
    /// Documents the cell layout the grid renderer depends on: a double-width
    /// glyph occupies two columns, with the content in the first and a
    /// contentless continuation in the second.
    ///
    /// The renderer must skip continuation cells. If it emitted a character
    /// for them, every following column would shift right and the drawn grid
    /// would desynchronize from the PTY's. This test fails loudly if vt100
    /// ever changes that model.
    #[test]
    fn wide_glyphs_occupy_two_cells_with_a_continuation() {
        let mut parser = vt100::Parser::new(4, 20, 0);
        parser.process("日本".as_bytes());
        let screen = parser.screen();

        let first = screen.cell(0, 0).expect("cell 0");
        assert!(first.is_wide(), "CJK glyph should be wide");
        assert_eq!(first.contents(), "日");

        let continuation = screen.cell(0, 1).expect("cell 1");
        assert!(continuation.is_wide_continuation());
        assert_eq!(
            continuation.contents(),
            "",
            "continuation holds no codepoints"
        );

        // Subtle, and the reason the renderer must branch on
        // `is_wide_continuation()` rather than `has_contents()`: a
        // continuation cell reports *true* here despite `contents()` being
        // empty, because vt100 packs the continuation flag into the same
        // length field `has_contents()` tests. Treating it as "has content"
        // and emitting `contents()` yields an empty run; treating it as blank
        // and emitting a space would shift every following column right.
        assert!(
            continuation.has_contents(),
            "has_contents() is true for continuations - do not use it to \
             detect them"
        );

        // The next glyph therefore begins two columns along, not one.
        assert_eq!(screen.cell(0, 2).expect("cell 2").contents(), "本");
    }

    #[test]
    fn ascii_is_single_width_with_no_continuation() {
        let mut parser = vt100::Parser::new(4, 20, 0);
        parser.process(b"ab");
        let screen = parser.screen();

        let a = screen.cell(0, 0).expect("cell 0");
        assert!(!a.is_wide());
        assert_eq!(a.contents(), "a");

        let b = screen.cell(0, 1).expect("cell 1");
        assert!(!b.is_wide_continuation());
        assert_eq!(b.contents(), "b");
    }
}
