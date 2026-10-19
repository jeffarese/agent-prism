//! The session event loop. It owns the terminal, the child PTY, the output
//! filter, the shadow emulator, the dock painter and the open panel.
//!
//! Shape of the API (the implementer may add to it, but keep this shape so
//! that the `agp` binary can be written against it):
//!
//! ```ignore
//! let (mut runner, events_tx) = Runner::new(RunnerOptions { .. });
//! // other threads send ExternalEvent::Custom(..) through events_tx
//! let outcome = runner.run(&mut host, first_child_spec)?;
//! ```
//!
//! The runner calls the host:
//! - `dock_model()` whenever the dock must be drawn;
//! - `on_event()` for every external event, timer tick, menu-key press,
//!   panel action, dock click and child exit. The host reacts through
//!   [`Control`]: request a redraw, open or close a panel, set the dock
//!   mode, restart the child with a new spec (account switch), or quit.

use std::io::{self, Read, Write};
use std::marker::PhantomData;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use anyhow::Context as _;
use crossbeam_channel::{Receiver, Sender};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use crossterm::terminal;
use prism_core::model::{DockMode, DockModel};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use signal_hook::consts::signal::{SIGCONT, SIGHUP, SIGTERM, SIGWINCH};
use signal_hook::iterator::Signals;

use crate::dock::{DockPainter, dock_rows, render_dock};
use crate::filter::OutputFilter;
use crate::input::{InputItem, InputScanner, KeyParser, ParsedEvent};
use crate::layout::{dock_height, resolve_mode};
use crate::panel::{Panel, PanelOutcome};
use crate::pty::{ChildSpec, ChildStatus, PtyChild};

/// Events that the host can inject from other threads.
pub enum ExternalEvent<E> {
    Custom(E),
    /// Ask the runner to stop the child and exit.
    Shutdown,
}

/// Events the runner reports to the host.
pub enum HostEvent<E, A> {
    Custom(E),
    /// About 4 times per second.
    Tick,
    /// The menu key was pressed while no panel was open.
    MenuKey,
    /// A panel returned `PanelOutcome::Action`.
    PanelAction(A),
    /// A mouse click on a dock row (0-based row inside the dock).
    DockClick {
        row: u16,
        col: u16,
    },
    /// The child exited by itself (not because of `Control::restart_child`).
    ChildExited {
        code: i32,
    },
    /// The terminal was resized.
    Resized {
        rows: u16,
        cols: u16,
    },
}

pub trait Host<E, A> {
    fn dock_model(&mut self) -> DockModel;
    fn on_event(&mut self, event: HostEvent<E, A>, ctl: &mut Control<A>);
}

/// Requests from the host to the runner, applied after `on_event` returns.
pub struct Control<A> {
    pub(crate) redraw: bool,
    pub(crate) open_panel: Option<Box<dyn Panel<A>>>,
    pub(crate) close_panel: bool,
    pub(crate) dock_mode: Option<DockMode>,
    pub(crate) restart: Option<ChildSpec>,
    pub(crate) quit: Option<i32>,
    pub(crate) forward: Vec<u8>,
}

impl<A> Default for Control<A> {
    fn default() -> Self {
        Self {
            redraw: false,
            open_panel: None,
            close_panel: false,
            dock_mode: None,
            restart: None,
            quit: None,
            forward: Vec::new(),
        }
    }
}

impl<A> Control<A> {
    /// Redraw the dock (the host's model changed).
    pub fn redraw(&mut self) {
        self.redraw = true;
    }
    pub fn open_panel(&mut self, panel: Box<dyn Panel<A>>) {
        self.open_panel = Some(panel);
    }
    pub fn close_panel(&mut self) {
        self.close_panel = true;
    }
    pub fn set_dock_mode(&mut self, mode: DockMode) {
        self.dock_mode = Some(mode);
    }
    /// Stop the current child (SIGTERM, then SIGKILL after 3 s) and start
    /// this one in the same terminal area. Used for account switches.
    pub fn restart_child(&mut self, spec: ChildSpec) {
        self.restart = Some(spec);
    }
    /// Stop the child and leave `run` with this exit code.
    pub fn quit(&mut self, code: i32) {
        self.quit = Some(code);
    }
    /// Send bytes to the child as if typed.
    pub fn send_to_child(&mut self, bytes: &[u8]) {
        self.forward.extend_from_slice(bytes);
    }

    /// Test inspector: the requested restart, if any (taken).
    #[doc(hidden)]
    pub fn take_restart(&mut self) -> Option<ChildSpec> {
        self.restart.take()
    }

    /// Test inspector: the requested quit code, if any.
    #[doc(hidden)]
    pub fn quit_code(&self) -> Option<i32> {
        self.quit
    }

    /// Test inspector: a panel open was requested.
    #[doc(hidden)]
    pub fn panel_requested(&self) -> bool {
        self.open_panel.is_some()
    }

    /// Test inspector: the requested dock mode, if any.
    #[doc(hidden)]
    pub fn dock_mode_requested(&self) -> Option<DockMode> {
        self.dock_mode
    }
}

/// Options for [`Runner::new`].
#[derive(Clone, Debug)]
pub struct RunnerOptions {
    /// Label of the menu key shown in the dock, e.g. "⌃\\".
    pub menu_key_label: String,
    /// Dock mode at start.
    pub initial_dock_mode: DockMode,
    /// How long a child gets between SIGTERM and SIGKILL on restart / quit.
    pub restart_grace: Duration,
}

impl Default for RunnerOptions {
    fn default() -> Self {
        Self {
            menu_key_label: "⌃\\".into(),
            initial_dock_mode: DockMode::Compact,
            restart_grace: Duration::from_secs(3),
        }
    }
}

/// The result of [`Runner::run`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RunOutcome {
    /// The child's exit code (or the code passed to [`Control::quit`]).
    pub exit_code: i32,
}

/// The session event loop. See the module docs.
pub struct Runner<E, A> {
    opts: RunnerOptions,
    ext_rx: Receiver<ExternalEvent<E>>,
    _a: PhantomData<fn() -> A>,
}

enum Ev {
    Stdin(Vec<u8>),
    Pty(u64, Vec<u8>),
    PtyEof(u64),
    Child(u64, ChildStatus),
    Signal(i32),
}

const TICK: Duration = Duration::from_millis(250);
const QUIET: Duration = Duration::from_millis(16);
const MIN_FRAME: Duration = Duration::from_millis(50);
const MAX_DEFER: Duration = Duration::from_millis(200);
const SYNC_MAX: Duration = Duration::from_millis(500);
const SYNC_ON: &[u8] = b"\x1b[?2026h";
const SYNC_OFF: &[u8] = b"\x1b[?2026l";

fn write_out(bytes: &[u8]) -> io::Result<()> {
    let mut o = io::stdout().lock();
    o.write_all(bytes)?;
    o.flush()
}

/// Leaves raw mode and resets the scroll region if `run` exits early.
struct TermGuard {
    armed: bool,
}

impl Drop for TermGuard {
    fn drop(&mut self) {
        if self.armed {
            let _ = write_out(b"\x1b[r\x1b[0m\x1b[?25h");
            let _ = terminal::disable_raw_mode();
        }
    }
}

fn is_tty(fd: i32) -> bool {
    // SAFETY: isatty on a standard fd.
    unsafe { libc::isatty(fd) == 1 }
}

/// Wait up to `timeout` for stdin to be readable.
fn poll_stdin(timeout: Duration) -> bool {
    let mut p = libc::pollfd {
        fd: 0,
        events: libc::POLLIN,
        revents: 0,
    };
    let ms = timeout.as_millis().min(i32::MAX as u128) as i32;
    // SAFETY: one valid pollfd.
    let r = unsafe { libc::poll(&mut p, 1, ms) };
    r > 0 && (p.revents & (libc::POLLIN | libc::POLLHUP)) != 0
}

fn read_stdin(buf: &mut [u8]) -> io::Result<usize> {
    // SAFETY: reading into a valid buffer.
    let n = unsafe { libc::read(0, buf.as_mut_ptr().cast(), buf.len()) };
    if n < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(n as usize)
    }
}

/// Ask the terminal for the cursor position (1-based row). Bytes read that
/// are not the reply are returned for normal input processing.
fn query_cursor_row() -> (Option<u16>, Vec<u8>) {
    if write_out(b"\x1b[6n").is_err() {
        return (None, Vec::new());
    }
    let deadline = Instant::now() + Duration::from_millis(200);
    let mut got = Vec::new();
    let mut buf = [0u8; 256];
    loop {
        if let Some((row, start, end)) = find_cpr(&got) {
            got.drain(start..end);
            return (Some(row), got);
        }
        let now = Instant::now();
        if now >= deadline || !poll_stdin(deadline - now) {
            return (None, got);
        }
        match read_stdin(&mut buf) {
            Ok(0) | Err(_) => return (None, got),
            Ok(n) => got.extend_from_slice(&buf[..n]),
        }
    }
}

/// Find `ESC [ row ; col R` in `b`: (row, start, end).
fn find_cpr(b: &[u8]) -> Option<(u16, usize, usize)> {
    let mut i = 0;
    while i + 1 < b.len() {
        if b[i] == 0x1b && b[i + 1] == b'[' {
            let mut j = i + 2;
            while j < b.len() && (b[j].is_ascii_digit() || b[j] == b';') {
                j += 1;
            }
            if j < b.len() && b[j] == b'R' {
                let body = std::str::from_utf8(&b[i + 2..j]).ok()?;
                let mut it = body.split(';');
                if let (Some(r), Some(_c)) = (it.next(), it.next())
                    && let Ok(r) = r.parse::<u16>()
                {
                    return Some((r, i, j + 1));
                }
            }
        }
        i += 1;
    }
    None
}

struct ChildSlot {
    pty: PtyChild,
    gen_id: u64,
}

struct Session<A> {
    opts: RunnerOptions,
    tx: Sender<Ev>,
    rows: u16,
    cols: u16,
    dock_h: u16,
    dock_mode: DockMode,
    shown_accounts: usize,
    filter: OutputFilter,
    shadow: vt100::Parser,
    painter: DockPainter,
    scanner: InputScanner,
    keys: KeyParser,
    panel: Option<Box<dyn Panel<A>>>,
    child: Option<ChildSlot>,
    next_gen: u64,
    /// A restart waiting for the old child to exit, and its SIGKILL time.
    restart: Option<(ChildSpec, Instant)>,
    /// The current child exited with this code; waiting for its EOF.
    exited: Option<(i32, Instant)>,
    eof_seen: bool,
    quit: Option<i32>,
    out: Vec<u8>,
    dirty: bool,
    need_region: bool,
    last_draw: Instant,
    last_output: Instant,
    dirty_since: Option<Instant>,
    sync_since: Option<Instant>,
    stdin_stop: Arc<AtomicBool>,
}

impl<E: Send + 'static, A: 'static> Runner<E, A> {
    /// Create a runner and the sender for [`ExternalEvent`]s.
    pub fn new(opts: RunnerOptions) -> (Self, Sender<ExternalEvent<E>>) {
        let (tx, rx) = crossbeam_channel::unbounded();
        (
            Self {
                opts,
                ext_rx: rx,
                _a: PhantomData,
            },
            tx,
        )
    }

    /// Run `spec` in the wrapped terminal until it exits or the host quits.
    pub fn run(
        &mut self,
        host: &mut dyn Host<E, A>,
        spec: ChildSpec,
    ) -> anyhow::Result<RunOutcome> {
        if !is_tty(0) || !is_tty(1) {
            anyhow::bail!("agp needs a terminal on stdin and stdout");
        }
        let (cols, rows) = terminal::size().context("terminal size")?;
        terminal::enable_raw_mode().context("raw mode")?;
        let mut guard = TermGuard { armed: true };

        let (tx, rx) = crossbeam_channel::unbounded::<Ev>();
        let model = host.dock_model();
        let dock_mode = self.opts.initial_dock_mode;
        let shown = dock_rows(&model);
        let dock_h = dock_height(dock_mode, rows, shown, None);
        let n = rows - dock_h;
        let now = Instant::now();
        let mut s: Session<A> = Session {
            opts: self.opts.clone(),
            tx: tx.clone(),
            rows,
            cols,
            dock_h,
            dock_mode,
            shown_accounts: shown,
            filter: OutputFilter::new(n),
            shadow: vt100::Parser::new(n.max(1), cols.max(1), 0),
            painter: DockPainter::new(),
            scanner: InputScanner::new(),
            keys: KeyParser::new(),
            panel: None,
            child: None,
            next_gen: 0,
            restart: None,
            exited: None,
            eof_seen: false,
            quit: None,
            out: Vec::with_capacity(64 * 1024),
            dirty: true,
            need_region: false,
            last_draw: now - MIN_FRAME,
            last_output: now,
            dirty_since: Some(now),
            sync_since: None,
            stdin_stop: Arc::new(AtomicBool::new(false)),
        };

        // Make room for the dock below the shell output (main screen).
        let (row, leftover) = query_cursor_row();
        let mut start = Vec::new();
        let cur_row = if dock_h > 0 {
            for _ in 0..dock_h {
                start.extend_from_slice(b"\r\n");
            }
            let _ = write!(start, "\x1b[{dock_h}A");
            row.map(|r| r.min(n)).unwrap_or(n)
        } else {
            row.unwrap_or(1).min(n)
        };
        s.shadow.process(format!("\x1b[{cur_row};1H").as_bytes());
        s.region_bytes(&mut start);
        write_out(&start)?;

        // Threads.
        let stop = s.stdin_stop.clone();
        let stx = tx.clone();
        std::thread::Builder::new()
            .name("agp-stdin".into())
            .spawn(move || {
                let mut buf = vec![0u8; 16 * 1024];
                while !stop.load(Ordering::SeqCst) {
                    if !poll_stdin(Duration::from_millis(50)) {
                        continue;
                    }
                    if stop.load(Ordering::SeqCst) {
                        break;
                    }
                    match read_stdin(&mut buf) {
                        Ok(0) => break,
                        Ok(n) => {
                            if stx.send(Ev::Stdin(buf[..n].to_vec())).is_err() {
                                break;
                            }
                        }
                        Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                        Err(e) if e.kind() == io::ErrorKind::WouldBlock => {}
                        Err(_) => break,
                    }
                }
            })?;
        let mut signals = Signals::new([SIGWINCH, SIGTERM, SIGHUP, SIGCONT]).context("signals")?;
        let sig_handle = signals.handle();
        let sigtx = tx.clone();
        std::thread::Builder::new()
            .name("agp-signals".into())
            .spawn(move || {
                for sig in signals.forever() {
                    if sigtx.send(Ev::Signal(sig)).is_err() {
                        break;
                    }
                }
            })?;

        let result = (|| -> anyhow::Result<i32> {
            s.spawn_child(&spec)?;
            if !leftover.is_empty() {
                s.on_stdin(host, &leftover)?;
            }
            s.event_loop(host, &rx, &self.ext_rx)
        })();

        s.stdin_stop.store(true, Ordering::SeqCst);
        sig_handle.close();
        if let Some(c) = &s.child
            && !c.pty.has_exited()
        {
            c.pty.terminate(s.opts.restart_grace);
        }
        let mut fin = Vec::new();
        s.cleanup_bytes(&mut fin);
        let _ = write_out(&fin);
        guard.armed = false;
        let _ = terminal::disable_raw_mode();
        Ok(RunOutcome { exit_code: result? })
    }
}

impl<A: 'static> Session<A> {
    fn child_rows(&self) -> u16 {
        self.rows - self.dock_h
    }

    fn region_bytes(&self, out: &mut Vec<u8>) {
        let _ = write!(out, "\x1b[1;{}r", self.child_rows().max(1));
        self.restore_cursor(out);
    }

    fn restore_cursor(&self, out: &mut Vec<u8>) {
        let sc = self.shadow.screen();
        let (r, c) = sc.cursor_position();
        let _ = write!(out, "\x1b[{};{}H", r + 1, c + 1);
        out.extend_from_slice(&sc.attributes_formatted());
        out.extend_from_slice(if sc.hide_cursor() {
            b"\x1b[?25l"
        } else {
            b"\x1b[?25h"
        });
    }

    /// Turn off modes the child left on (mouse, bracketed paste, alt screen).
    fn child_modes_off(&self, out: &mut Vec<u8>) {
        let sc = self.shadow.screen();
        if sc.mouse_protocol_mode() != vt100::MouseProtocolMode::None {
            out.extend_from_slice(b"\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006l");
        }
        if sc.bracketed_paste() {
            out.extend_from_slice(b"\x1b[?2004l");
        }
        if sc.alternate_screen() {
            out.extend_from_slice(b"\x1b[?1049l");
        }
    }

    fn clear_rows(&self, from: u16, to: u16, out: &mut Vec<u8>) {
        for r in from..to {
            let _ = write!(out, "\x1b[{};1H\x1b[0m\x1b[2K", r + 1);
        }
    }

    fn cleanup_bytes(&mut self, out: &mut Vec<u8>) {
        out.extend_from_slice(SYNC_OFF);
        if self.panel.is_some() {
            out.extend_from_slice(b"\x1b[<u");
        }
        out.extend_from_slice(b"\x1b[r");
        self.clear_rows(self.child_rows(), self.rows, out);
        self.child_modes_off(out);
        let (r, c) = self.shadow.screen().cursor_position();
        let _ = write!(out, "\x1b[{};{}H\x1b[0m\x1b[?25h", r + 1, c + 1);
    }

    fn spawn_child(&mut self, spec: &ChildSpec) -> anyhow::Result<()> {
        self.next_gen += 1;
        let gen_id = self.next_gen;
        let tx = self.tx.clone();
        let n = self.child_rows();
        let pty = PtyChild::spawn(spec, n.max(1), self.cols.max(1), move |st| {
            let _ = tx.send(Ev::Child(gen_id, st));
        })
        .with_context(|| format!("starting {}", spec.program.display()))?;
        let mut reader = pty.reader()?;
        let tx = self.tx.clone();
        std::thread::Builder::new()
            .name("agp-pty".into())
            .spawn(move || {
                let mut buf = vec![0u8; 64 * 1024];
                loop {
                    match reader.read(&mut buf) {
                        Ok(0) => break,
                        Ok(n) => {
                            if tx.send(Ev::Pty(gen_id, buf[..n].to_vec())).is_err() {
                                return;
                            }
                        }
                        Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                        Err(_) => break,
                    }
                }
                let _ = tx.send(Ev::PtyEof(gen_id));
            })?;
        self.child = Some(ChildSlot { pty, gen_id });
        self.exited = None;
        self.eof_seen = false;
        Ok(())
    }

    fn current_gen(&self) -> u64 {
        self.child.as_ref().map(|c| c.gen_id).unwrap_or(0)
    }

    fn event_loop<E>(
        &mut self,
        host: &mut dyn Host<E, A>,
        rx: &Receiver<Ev>,
        ext: &Receiver<ExternalEvent<E>>,
    ) -> anyhow::Result<i32> {
        let mut next_tick = Instant::now() + TICK;
        loop {
            if let Some(code) = self.quit {
                return Ok(code);
            }
            // Child exited: wait for EOF (bounded), then tell the host.
            if let Some((code, at)) = self.exited
                && (self.eof_seen || at.elapsed() >= Duration::from_millis(200))
            {
                self.exited = None;
                if let Some((spec, _)) = self.restart.take() {
                    self.do_restart(&spec)?;
                } else {
                    self.child = None;
                    let mut ctl = Control::default();
                    host.on_event(HostEvent::ChildExited { code }, &mut ctl);
                    self.apply(host, ctl)?;
                    if let Some((spec, _)) = self.restart.take() {
                        self.do_restart(&spec)?;
                    } else if self.quit.is_none() {
                        return Ok(code);
                    }
                }
                continue;
            }

            self.maybe_draw(host)?;

            let now = Instant::now();
            let mut deadline = next_tick;
            if let Some(d) = self.scanner.pending_deadline() {
                deadline = deadline.min(d);
            }
            if self.dirty {
                deadline = deadline.min((self.last_output + QUIET).max(self.last_draw + MIN_FRAME));
            }
            if let Some((_, at)) = self.exited {
                deadline = deadline.min(at + Duration::from_millis(200));
            }
            let timeout = deadline.saturating_duration_since(now);

            crossbeam_channel::select! {
                recv(rx) -> ev => {
                    let Ok(ev) = ev else { anyhow::bail!("internal channel closed") };
                    self.on_ev(host, ev)?;
                    // Drain what is queued to batch output.
                    while let Ok(ev) = rx.try_recv() {
                        self.on_ev(host, ev)?;
                        if self.quit.is_some() { break; }
                    }
                }
                recv(ext) -> ev => {
                    match ev {
                        Ok(ExternalEvent::Custom(e)) => {
                            let mut ctl = Control::default();
                            host.on_event(HostEvent::Custom(e), &mut ctl);
                            self.apply(host, ctl)?;
                        }
                        Ok(ExternalEvent::Shutdown) => self.quit = Some(0),
                        Err(_) => {}
                    }
                }
                default(timeout) => {}
            }

            let now = Instant::now();
            if let Some(d) = self.scanner.pending_deadline()
                && now >= d
                && let Some(item) = self.scanner.flush_pending()
            {
                self.on_item(host, item)?;
            }
            if now >= next_tick {
                next_tick = now + TICK;
                self.on_tick(host)?;
            }
        }
    }

    fn on_tick<E>(&mut self, host: &mut dyn Host<E, A>) -> anyhow::Result<()> {
        if let Some((_, kill_at)) = &self.restart
            && Instant::now() >= *kill_at
            && let Some(c) = &self.child
        {
            let _ = c.pty.signal_group(libc::SIGKILL);
        }
        if let Some(p) = self.panel.as_mut() {
            p.tick();
            self.mark_dirty();
        }
        let mut ctl = Control::default();
        host.on_event(HostEvent::Tick, &mut ctl);
        self.apply(host, ctl)
    }

    fn mark_dirty(&mut self) {
        if !self.dirty {
            self.dirty = true;
            self.dirty_since = Some(Instant::now());
        }
    }

    fn on_ev<E>(&mut self, host: &mut dyn Host<E, A>, ev: Ev) -> anyhow::Result<()> {
        match ev {
            Ev::Stdin(b) => self.on_stdin(host, &b)?,
            Ev::Pty(g, data) => {
                if g == self.current_gen() {
                    self.on_output(&data)?;
                }
            }
            Ev::PtyEof(g) => {
                if g == self.current_gen() {
                    self.eof_seen = true;
                }
            }
            Ev::Child(g, st) => {
                if g == self.current_gen() {
                    match st {
                        ChildStatus::Exited(code) => {
                            self.exited = Some((code, Instant::now()));
                        }
                        ChildStatus::Stopped => self.suspend(host)?,
                        ChildStatus::Continued => {}
                    }
                }
            }
            Ev::Signal(sig) => match sig {
                SIGWINCH => self.on_resize(host, false)?,
                SIGCONT => self.resume(host)?,
                SIGTERM | SIGHUP => {
                    if let Some(c) = &self.child {
                        let _ = c.pty.signal_group(sig);
                        c.pty.terminate(self.opts.restart_grace);
                    }
                    self.quit = Some(128 + sig);
                }
                _ => {}
            },
        }
        Ok(())
    }

    fn on_output(&mut self, data: &[u8]) -> anyhow::Result<()> {
        let mut out = std::mem::take(&mut self.out);
        out.clear();
        let rep = self.filter.process(data, &mut out);
        self.shadow.process(&out);
        if rep.dock_damaged {
            self.painter.invalidate();
            self.mark_dirty();
        }
        if rep.needs_region {
            self.need_region = true;
        }
        if self.filter.in_sync() {
            if self.sync_since.is_none() {
                self.sync_since = Some(Instant::now());
            }
        } else {
            self.sync_since = None;
        }
        if self.need_region && self.filter.in_ground() {
            self.need_region = false;
            self.region_bytes(&mut out);
        }
        self.last_output = Instant::now();
        let r = write_out(&out);
        self.out = out;
        r.context("writing to the terminal")
    }

    fn can_draw(&self) -> bool {
        self.filter.in_ground()
            && !self.need_region
            && (!self.filter.in_sync() || self.sync_since.is_some_and(|t| t.elapsed() >= SYNC_MAX))
    }

    fn maybe_draw<E>(&mut self, host: &mut dyn Host<E, A>) -> anyhow::Result<()> {
        if !self.dirty || !self.can_draw() {
            return Ok(());
        }
        let now = Instant::now();
        if now < self.last_draw + MIN_FRAME {
            return Ok(());
        }
        let quiet = now >= self.last_output + QUIET;
        let waited = self.dirty_since.is_some_and(|t| now >= t + MAX_DEFER);
        if !quiet && !waited {
            return Ok(());
        }
        self.draw(host)
    }

    fn draw<E>(&mut self, host: &mut dyn Host<E, A>) -> anyhow::Result<()> {
        let mut model = host.dock_model();
        model.menu_key_label.clone_from(&self.opts.menu_key_label);
        model.mode = self.dock_mode;
        if dock_rows(&model) != self.shown_accounts {
            self.shown_accounts = dock_rows(&model);
            self.relayout(host)?;
        }
        self.dirty = false;
        self.dirty_since = None;
        self.last_draw = Instant::now();
        if self.dock_h == 0 {
            return Ok(());
        }
        let area = Rect::new(0, 0, self.cols, self.dock_h);
        let mut buf = Buffer::empty(area);
        match self.panel.as_mut() {
            Some(p) => p.render(area, &mut buf),
            None => {
                let mut m = model;
                m.mode = resolve_mode(self.dock_mode, self.rows, self.shown_accounts);
                render_dock(&m, area, &mut buf);
            }
        }
        let mut out = Vec::new();
        out.extend_from_slice(SYNC_ON);
        if self.painter.paint(&buf, self.child_rows(), &mut out) {
            self.restore_cursor(&mut out);
            out.extend_from_slice(SYNC_OFF);
            write_out(&out).context("writing to the terminal")?;
        }
        Ok(())
    }

    fn wanted_dock_h(&self) -> u16 {
        let ph = self.panel.as_ref().map(|p| p.preferred_height(self.rows));
        dock_height(self.dock_mode, self.rows, self.shown_accounts, ph)
    }

    /// Apply a new dock height / size: resize child, shadow, region; clear
    /// stale rows and invalidate the dock.
    fn relayout<E>(&mut self, host: &mut dyn Host<E, A>) -> anyhow::Result<()> {
        let _ = host;
        let new_h = self.wanted_dock_h();
        if new_h == self.dock_h {
            return Ok(());
        }
        let old_n = self.child_rows();
        let new_n = self.rows - new_h;
        let mut out = Vec::new();
        out.extend_from_slice(SYNC_ON);
        if new_n < old_n && !self.shadow.screen().alternate_screen() {
            // Main screen: scroll the child's text up so the cursor line
            // stays visible above the dock.
            let (cr, cc) = self.shadow.screen().cursor_position();
            if cr >= new_n {
                let k = cr - new_n + 1;
                let mut mv = Vec::new();
                let _ = write!(mv, "\x1b[{old_n};1H");
                mv.extend(std::iter::repeat_n(b'\n', usize::from(k)));
                let _ = write!(mv, "\x1b[{};{}H", cr + 1 - k, cc + 1);
                self.shadow.process(&mv);
                out.extend_from_slice(&mv);
            }
        }
        if new_n > old_n {
            self.clear_rows(old_n, new_n, &mut out);
        }
        self.dock_h = new_h;
        self.apply_child_size(&mut out)?;
        out.extend_from_slice(SYNC_OFF);
        write_out(&out)?;
        self.painter.invalidate();
        self.mark_dirty();
        self.dirty_since = Some(Instant::now() - MAX_DEFER);
        Ok(())
    }

    fn apply_child_size(&mut self, out: &mut Vec<u8>) -> anyhow::Result<()> {
        let n = self.child_rows().max(1);
        self.filter.set_child_rows(n);
        self.shadow.screen_mut().set_size(n, self.cols.max(1));
        if let Some(c) = &self.child {
            let _ = c.pty.resize(n, self.cols.max(1));
        }
        if self.filter.in_ground() {
            self.region_bytes(out);
        } else {
            self.need_region = true;
        }
        Ok(())
    }

    fn on_resize<E>(&mut self, host: &mut dyn Host<E, A>, force: bool) -> anyhow::Result<()> {
        let (cols, rows) = terminal::size().context("terminal size")?;
        if !force && (cols, rows) == (self.cols, self.rows) {
            return Ok(());
        }
        self.rows = rows;
        self.cols = cols;
        self.dock_h = self.wanted_dock_h();
        let mut out = Vec::new();
        self.apply_child_size(&mut out)?;
        // Clear the dock rows; the dock is redrawn in full.
        let n = self.child_rows();
        self.clear_rows(n, rows, &mut out);
        self.restore_cursor(&mut out);
        write_out(&out)?;
        self.painter.invalidate();
        self.mark_dirty();
        let mut ctl = Control::default();
        host.on_event(HostEvent::Resized { rows, cols }, &mut ctl);
        self.apply(host, ctl)
    }

    fn suspend<E>(&mut self, host: &mut dyn Host<E, A>) -> anyhow::Result<()> {
        let mut out = Vec::new();
        self.cleanup_bytes(&mut out);
        write_out(&out)?;
        let _ = terminal::disable_raw_mode();
        // SAFETY: raise on our own process; returns after SIGCONT.
        unsafe {
            libc::raise(libc::SIGTSTP);
        }
        self.resume(host)
    }

    fn resume<E>(&mut self, host: &mut dyn Host<E, A>) -> anyhow::Result<()> {
        terminal::enable_raw_mode().context("raw mode")?;
        let mut out = Vec::new();
        if self.panel.is_some() {
            out.extend_from_slice(b"\x1b[>0u");
        }
        write_out(&out)?;
        if let Some(c) = &self.child {
            let _ = c.pty.signal_group(libc::SIGCONT);
        }
        self.on_resize(host, true)
    }

    fn on_stdin<E>(&mut self, host: &mut dyn Host<E, A>, bytes: &[u8]) -> anyhow::Result<()> {
        let items = self.scanner.feed(bytes, self.child_rows(), Instant::now());
        for item in items {
            self.on_item(host, item)?;
        }
        Ok(())
    }

    fn write_child(&mut self, bytes: &[u8]) {
        if self.restart.is_some() {
            return;
        }
        if let Some(c) = self.child.as_mut()
            && let Err(e) = c.pty.write_all(bytes)
        {
            log::warn!("write to child: {e}");
        }
    }

    fn on_item<E>(&mut self, host: &mut dyn Host<E, A>, item: InputItem) -> anyhow::Result<()> {
        let mut ctl = Control::default();
        match item {
            InputItem::Forward(b) => {
                if self.panel.is_some() {
                    let mut evs = self.keys.feed(&b);
                    evs.extend(self.keys.flush());
                    for ev in evs {
                        let out = match (self.panel.as_mut(), ev) {
                            (Some(p), ParsedEvent::Key(k)) => p.handle_key(k),
                            (Some(p), ParsedEvent::Paste(t)) => p.handle_paste(&t),
                            // Mouse events above the panel are ignored.
                            _ => continue,
                        };
                        self.on_panel_outcome(host, out)?;
                    }
                    return Ok(());
                }
                self.write_child(&b);
                return Ok(());
            }
            InputItem::MenuKey => {
                if let Some(p) = self.panel.as_mut() {
                    let out =
                        p.handle_key(KeyEvent::new(KeyCode::Char('\\'), KeyModifiers::CONTROL));
                    return self.on_panel_outcome(host, out);
                }
                host.on_event(HostEvent::MenuKey, &mut ctl);
            }
            InputItem::DockMouse {
                button,
                col,
                row,
                pressed,
            } => {
                if let Some(p) = self.panel.as_mut() {
                    let mut kp = KeyParser::new();
                    let seq = format!(
                        "\x1b[<{button};{};{}{}",
                        col + 1,
                        row + 1,
                        if pressed { 'M' } else { 'm' }
                    );
                    for ev in kp.feed(seq.as_bytes()) {
                        if let ParsedEvent::Mouse(m) = ev {
                            let out = p.handle_mouse(m);
                            return self.on_panel_outcome(host, out);
                        }
                    }
                    return Ok(());
                }
                if pressed && button & (32 | 64) == 0 {
                    host.on_event(HostEvent::DockClick { row, col }, &mut ctl);
                }
            }
            InputItem::Paste(t) => {
                if let Some(p) = self.panel.as_mut() {
                    let out = p.handle_paste(&t);
                    return self.on_panel_outcome(host, out);
                }
                // Panel closed meanwhile: give it to the child.
                let mut b = b"\x1b[200~".to_vec();
                b.extend_from_slice(t.as_bytes());
                b.extend_from_slice(b"\x1b[201~");
                self.write_child(&b);
                return Ok(());
            }
        }
        self.apply(host, ctl)
    }

    fn on_panel_outcome<E>(
        &mut self,
        host: &mut dyn Host<E, A>,
        out: PanelOutcome<A>,
    ) -> anyhow::Result<()> {
        self.mark_dirty();
        match out {
            PanelOutcome::Continue => Ok(()),
            PanelOutcome::Close => self.close_panel(host),
            PanelOutcome::CloseAndForward(b) => {
                self.close_panel(host)?;
                self.write_child(&b);
                Ok(())
            }
            PanelOutcome::Action(a) => {
                let mut ctl = Control::default();
                host.on_event(HostEvent::PanelAction(a), &mut ctl);
                self.apply(host, ctl)
            }
            PanelOutcome::Replace(p) => {
                self.panel = Some(p);
                self.painter.invalidate();
                self.relayout(host)
            }
        }
    }

    fn open_panel<E>(
        &mut self,
        host: &mut dyn Host<E, A>,
        p: Box<dyn Panel<A>>,
    ) -> anyhow::Result<()> {
        if self.panel.is_none() {
            write_out(b"\x1b[>0u")?;
        }
        self.panel = Some(p);
        self.scanner.set_panel_focus(true);
        self.keys = KeyParser::new();
        self.painter.invalidate();
        self.mark_dirty();
        self.relayout(host)
    }

    fn close_panel<E>(&mut self, host: &mut dyn Host<E, A>) -> anyhow::Result<()> {
        if self.panel.take().is_none() {
            return Ok(());
        }
        write_out(b"\x1b[<u")?;
        self.scanner.set_panel_focus(false);
        self.painter.invalidate();
        self.mark_dirty();
        self.relayout(host)
    }

    fn apply<E>(&mut self, host: &mut dyn Host<E, A>, ctl: Control<A>) -> anyhow::Result<()> {
        let Control {
            redraw,
            open_panel,
            close_panel,
            dock_mode,
            restart,
            quit,
            forward,
        } = ctl;
        if !forward.is_empty() {
            self.write_child(&forward);
        }
        if let Some(m) = dock_mode {
            self.dock_mode = m;
            self.painter.invalidate();
            self.mark_dirty();
            self.relayout(host)?;
        }
        if close_panel {
            self.close_panel(host)?;
        }
        if let Some(p) = open_panel {
            self.open_panel(host, p)?;
        }
        if let Some(spec) = restart {
            self.request_restart(spec)?;
        }
        if let Some(code) = quit {
            if let Some(c) = &self.child {
                c.pty.terminate(self.opts.restart_grace);
            }
            self.quit = Some(code);
        }
        if redraw {
            self.mark_dirty();
        }
        Ok(())
    }

    fn request_restart(&mut self, spec: ChildSpec) -> anyhow::Result<()> {
        match &self.child {
            Some(c) if !c.pty.has_exited() => {
                if self.restart.is_none() {
                    let _ = c.pty.signal_group(libc::SIGHUP);
                    let _ = c.pty.signal_group(libc::SIGTERM);
                }
                self.restart = Some((spec, Instant::now() + self.opts.restart_grace));
                Ok(())
            }
            Some(_) => {
                // Exited, waiting for EOF: restart when that completes.
                self.restart = Some((spec, Instant::now()));
                Ok(())
            }
            None => self.do_restart(&spec),
        }
    }

    fn do_restart(&mut self, spec: &ChildSpec) -> anyhow::Result<()> {
        let mut out = Vec::new();
        self.child_modes_off(&mut out);
        out.extend_from_slice(b"\x1b[0m\x1b[?25h");
        let n = self.child_rows().max(1);
        self.filter = OutputFilter::new(n);
        self.shadow = vt100::Parser::new(n, self.cols.max(1), 0);
        out.extend_from_slice(b"\x1b[H\x1b[2J");
        self.region_bytes(&mut out);
        write_out(&out)?;
        self.need_region = false;
        self.painter.invalidate();
        self.mark_dirty();
        self.child = None;
        self.spawn_child(spec)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cpr() {
        assert_eq!(find_cpr(b"ab\x1b[12;40Rc"), Some((12, 2, 10)));
        assert_eq!(find_cpr(b"\x1b[12;40"), None);
        assert_eq!(find_cpr(b"\x1b[?1;2c"), None);
    }
}
