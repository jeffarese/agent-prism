//! The adopt flow (DESIGN §4.3): show the plan, confirm, back up, apply.

use std::sync::Arc;
use std::sync::mpsc::{Receiver, channel};

use crossterm::event::{KeyCode, KeyEvent};
use prism_core::backup::{BackupTarget, backup_profile};
use prism_core::config::Config;
use prism_core::procs::{profile_in_use, running};
use prism_core::profile::Profile;
use prism_core::share::{Action, AdoptPlan, apply_adopt, plan_adopt};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::{Line, Span};

use super::theme as t;
use crate::app::AppCtx;

/// What the parent should do after a key.
#[derive(Debug, PartialEq, Eq)]
pub enum FlowOutcome {
    /// Stay in the flow.
    Continue,
    /// Leave the flow. `true` if something changed on disk.
    Done(bool),
}

/// Result of a successful apply.
#[derive(Clone, Debug, PartialEq)]
pub struct Applied {
    pub backup: Option<String>,
    pub journal: String,
    pub changed: usize,
    /// Memory conflict copies to reconcile, in `conflicts_dir`.
    pub memory_conflicts: usize,
    pub conflicts_dir: String,
}

enum Stage {
    Plan,
    Confirm,
    Running(Receiver<Result<Applied, String>>),
    Done(Applied),
    Error(String),
}

/// The adopt flow for one profile.
pub struct AdoptView {
    ctx: Arc<AppCtx>,
    profile: Profile,
    plan: Result<AdoptPlan, String>,
    in_use: bool,
    /// Backup target (local path or `ssh://host/path`); empty = no backup.
    pub backup_target: String,
    stage: Stage,
    tick: usize,
    scroll: usize,
}

/// Counts of a plan, for summaries.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PlanCounts {
    pub link: usize,
    pub merge: usize,
    pub conflicts: usize,
    pub localize: usize,
    pub unmanaged: usize,
}

/// Count the actions of a plan.
pub fn plan_counts(plan: &AdoptPlan) -> PlanCounts {
    let mut c = PlanCounts::default();
    for a in &plan.actions {
        match a {
            Action::Link { .. } | Action::Relink { .. } => c.link += 1,
            Action::Merge { preview, .. } => {
                c.merge += 1;
                c.conflicts += preview.conflicts.len();
            }
            Action::Localize { .. } => c.localize += 1,
            Action::Skip { reason, .. } if reason != "ok" => c.unmanaged += 1,
            Action::Skip { .. } => {}
        }
    }
    c
}

/// One-line summary of a plan: `3 to link · 2 to merge (1 conflict) · 1 to localize`.
pub fn plan_summary(plan: &AdoptPlan) -> String {
    if plan.is_noop() {
        return "already shared".into();
    }
    let c = plan_counts(plan);
    let mut parts = Vec::new();
    if c.link > 0 {
        parts.push(format!("{} to link", c.link));
    }
    if c.merge > 0 {
        let mut s = format!("{} to merge", c.merge);
        if c.conflicts > 0 {
            s.push_str(&format!(
                " ({} conflict{})",
                c.conflicts,
                plural(c.conflicts)
            ));
        }
        parts.push(s);
    }
    if c.localize > 0 {
        parts.push(format!("{} to make local", c.localize));
    }
    parts.join(" · ")
}

fn plural(n: usize) -> &'static str {
    if n == 1 { "" } else { "s" }
}

/// Plan adopt for `profile` with the config's share settings.
pub fn plan_for(ctx: &AppCtx, config: &Config, profile: &Profile) -> Result<AdoptPlan, String> {
    plan_adopt(
        &profile.id,
        &profile.dir,
        &profile.common_home,
        &config.share_rules(profile.tool),
        &ctx.paths.profile_overlay(&profile.id),
    )
    .map_err(|e| format!("{e:#}"))
}

impl AdoptView {
    /// Plan the flow for `profile`. Read-only until the user confirms.
    pub fn new(ctx: Arc<AppCtx>, config: &Config, profile: Profile) -> Self {
        let plan = plan_for(&ctx, config, &profile);
        let in_use = profile_in_use(&profile, &running(profile.tool));
        Self {
            ctx,
            profile,
            plan,
            in_use,
            backup_target: config.backup.target.clone(),
            stage: Stage::Plan,
            tick: 0,
            scroll: 0,
        }
    }

    /// True while the background apply runs.
    #[cfg(test)]
    pub fn is_running(&self) -> bool {
        matches!(self.stage, Stage::Running(_))
    }

    fn start(&mut self) {
        let Ok(plan) = self.plan.clone() else {
            return;
        };
        // The tool may have started on this profile since the plan was shown.
        self.in_use = profile_in_use(&self.profile, &running(self.profile.tool));
        if self.in_use {
            self.stage = Stage::Plan;
            return;
        }
        let (tx, rx) = channel();
        let ctx = self.ctx.clone();
        let target = self.backup_target.clone();
        let profile = self.profile.clone();
        std::thread::spawn(move || {
            let _ = tx.send(run_apply(&ctx, &plan, &profile, &target));
        });
        self.stage = Stage::Running(rx);
    }

    /// Poll the background job and advance the spinner.
    pub fn tick(&mut self) {
        self.tick += 1;
        if let Stage::Running(rx) = &self.stage
            && let Ok(r) = rx.try_recv()
        {
            self.stage = match r {
                Ok(a) => Stage::Done(a),
                Err(e) => Stage::Error(e),
            };
        }
    }

    /// Handle a key.
    pub fn handle_key(&mut self, key: KeyEvent) -> FlowOutcome {
        match &self.stage {
            Stage::Plan => match key.code {
                KeyCode::Esc | KeyCode::Char('q') => return FlowOutcome::Done(false),
                KeyCode::Enter | KeyCode::Char('a') => {
                    if matches!(&self.plan, Ok(p) if !p.is_noop()) && !self.in_use {
                        self.stage = Stage::Confirm;
                    } else if matches!(&self.plan, Ok(p) if p.is_noop()) {
                        return FlowOutcome::Done(false);
                    }
                }
                KeyCode::Char('r') => {
                    self.in_use = profile_in_use(&self.profile, &running(self.profile.tool));
                }
                KeyCode::Up | KeyCode::Char('k') => self.scroll = self.scroll.saturating_sub(1),
                KeyCode::Down | KeyCode::Char('j') => self.scroll += 1,
                _ => {}
            },
            Stage::Confirm => match key.code {
                KeyCode::Char('y') => self.start(),
                KeyCode::Char('n') | KeyCode::Esc => self.stage = Stage::Plan,
                _ => {}
            },
            Stage::Running(_) => {}
            Stage::Done(_) => {
                if matches!(key.code, KeyCode::Enter | KeyCode::Esc | KeyCode::Char(' ')) {
                    return FlowOutcome::Done(true);
                }
            }
            Stage::Error(_) => match key.code {
                KeyCode::Esc | KeyCode::Enter => return FlowOutcome::Done(true),
                KeyCode::Char('r') => self.stage = Stage::Plan,
                _ => {}
            },
        }
        FlowOutcome::Continue
    }

    /// Key hints for the current stage.
    pub fn hints(&self) -> Vec<(&'static str, &'static str)> {
        match &self.stage {
            Stage::Plan => match &self.plan {
                Ok(p) if p.is_noop() => vec![("⏎", "done"), ("esc", "back")],
                Ok(_) if self.in_use => vec![("r", "check again"), ("esc", "back")],
                Ok(_) => vec![("⏎", "apply…"), ("↑↓", "scroll"), ("esc", "back")],
                Err(_) => vec![("esc", "back")],
            },
            Stage::Confirm => vec![("y", "back up and apply"), ("n", "cancel")],
            Stage::Running(_) => vec![],
            Stage::Done(_) => vec![("⏎", "done")],
            Stage::Error(_) => vec![("r", "review again"), ("esc", "back")],
        }
    }

    /// Draw the flow (no frame; the parent draws it).
    pub fn render(&mut self, area: Rect, buf: &mut Buffer) {
        let head = Line::from(vec![
            Span::styled(" Share sessions and settings · ", t::bold()),
            Span::styled(self.profile.label.clone(), t::title()),
            Span::styled(
                format!("  {}", tilde(&self.ctx, &self.profile.dir)),
                t::dim(),
            ),
        ]);
        let mut lines = vec![head, Line::from("")];
        match &self.stage {
            Stage::Running(_) => {
                lines.push(Line::from(vec![
                    Span::styled(format!(" {} ", t::spinner(self.tick)), t::title()),
                    Span::raw("Backing up, then applying… do not close this window."),
                ]));
            }
            Stage::Done(a) => {
                lines.push(Line::from(Span::styled(
                    format!(
                        " ✓ Done — {} change{} applied.",
                        a.changed,
                        plural(a.changed)
                    ),
                    t::ok(),
                )));
                lines.push(Line::from(""));
                if let Some(b) = &a.backup {
                    lines.push(kv("Backup", b));
                }
                lines.push(kv("Journal", &a.journal));
                if a.memory_conflicts > 0 {
                    lines.push(Line::from(""));
                    lines.push(Line::from(Span::styled(
                        format!(
                            " {} memory conflict{} to reconcile in {}. Shared memory was not edited.",
                            a.memory_conflicts,
                            plural(a.memory_conflicts),
                            a.conflicts_dir
                        ),
                        t::warn(),
                    )));
                }
            }
            Stage::Error(e) => {
                lines.push(Line::from(Span::styled(" ✕ Adopt stopped.", t::error())));
                lines.push(Line::from(""));
                lines.push(Line::from(Span::styled(format!(" {e}"), t::error())));
            }
            Stage::Plan | Stage::Confirm => self.plan_lines(&mut lines),
        }
        let skip = if matches!(self.stage, Stage::Plan) {
            self.scroll
                .min(lines.len().saturating_sub(area.height as usize))
        } else {
            0
        };
        self.scroll = skip;
        let mut lines: Vec<Line> = lines.into_iter().skip(skip).collect();
        if matches!(self.stage, Stage::Confirm) {
            let n = area.height as usize;
            let confirm = vec![Line::from(Span::styled(
                " Apply these changes? A backup is made first.",
                t::warn(),
            ))];
            lines.truncate(n.saturating_sub(confirm.len() + 1));
            lines.push(Line::from(""));
            lines.extend(confirm);
        }
        t::draw_lines(buf, area, lines);
    }

    fn plan_lines(&self, lines: &mut Vec<Line<'static>>) {
        let plan = match &self.plan {
            Ok(p) => p,
            Err(e) => {
                lines.push(Line::from(Span::styled(
                    format!(" Could not plan: {e}"),
                    t::error(),
                )));
                return;
            }
        };
        if self.in_use {
            lines.push(Line::from(Span::styled(
                format!(
                    " ⚠ {} is running on this account. Quit it first, then press r.",
                    self.profile.tool.name()
                ),
                t::warn(),
            )));
            lines.push(Line::from(""));
        }
        if plan.is_noop() {
            lines.push(Line::from(Span::styled(
                " ✓ Nothing to do — this account already shares everything.",
                t::ok(),
            )));
            return;
        }
        let mut link = Vec::new();
        let mut merge = Vec::new();
        let mut local = Vec::new();
        let mut unmanaged = Vec::new();
        for a in &plan.actions {
            match a {
                Action::Link { name } => link.push(name.clone()),
                Action::Relink { name, .. } => link.push(format!("{name} (relink)")),
                Action::Merge { name, preview } => {
                    let mut s = format!(
                        "{name}: {} moved, {} same",
                        preview.move_files, preview.identical
                    );
                    if !preview.memory_conflicts.is_empty() {
                        s.push_str(&format!(
                            ", {} memory conflict{} kept aside",
                            preview.memory_conflicts.len(),
                            plural(preview.memory_conflicts.len())
                        ));
                    }
                    if !preview.conflicts.is_empty() {
                        s.push_str(&format!(
                            ", {} conflict{} kept aside",
                            preview.conflicts.len(),
                            plural(preview.conflicts.len())
                        ));
                    }
                    if !preview.overlay_keys.is_empty() {
                        s.push_str(&format!(
                            ", own settings: {}",
                            preview.overlay_keys.join(" ")
                        ));
                    }
                    merge.push(s);
                }
                Action::Localize { name, .. } => local.push(name.clone()),
                Action::Skip { name, reason } if reason != "ok" => unmanaged.push(name.clone()),
                Action::Skip { .. } => {}
            }
        }
        let group = |lines: &mut Vec<Line<'static>>,
                     title: &str,
                     why: &str,
                     items: &[String],
                     list: bool| {
            if items.is_empty() {
                return;
            }
            lines.push(Line::from(vec![
                Span::styled(format!(" {title}"), t::title()),
                Span::styled(format!("  {why}"), t::dim()),
            ]));
            if list {
                for i in items {
                    lines.push(Line::from(format!("   · {i}")));
                }
            } else {
                lines.push(Line::from(format!("   {}", items.join(", "))));
            }
        };
        let common = tilde(&self.ctx, &self.profile.common_home);
        let shared = format!("shared with {common}");
        let merged = format!("copied into {common}, then linked");
        group(lines, "Will link", &shared, &link, false);
        group(lines, "Will merge", &merged, &merge, true);
        group(lines, "Will make local", "kept per account", &local, false);
        group(
            lines,
            "Left alone",
            "agent-prism does not touch these",
            &unmanaged,
            false,
        );
        lines.push(Line::from(""));
        let target = if self.backup_target.trim().is_empty() {
            "none".to_string()
        } else {
            self.backup_target.clone()
        };
        lines.push(Line::from(vec![
            Span::styled(" Backup first → ", t::dim()),
            Span::raw(target),
        ]));
    }
}

fn kv(k: &str, v: &str) -> Line<'static> {
    Line::from(vec![
        Span::styled(format!(" {k:<9}"), t::dim()),
        Span::styled(v.to_string(), Style::default()),
    ])
}

/// `~/.claude-x` for a dir under home.
pub fn tilde(ctx: &AppCtx, dir: &std::path::Path) -> String {
    match dir.strip_prefix(&ctx.paths.home) {
        Ok(rest) => format!("~/{}", rest.display()),
        Err(_) => dir.display().to_string(),
    }
}

fn run_apply(
    ctx: &AppCtx,
    plan: &AdoptPlan,
    profile: &Profile,
    target: &str,
) -> Result<Applied, String> {
    let backup = if target.trim().is_empty() {
        None
    } else {
        let bt = BackupTarget::parse(target, &ctx.paths).map_err(|e| format!("{e:#}"))?;
        let history = plan.common.join("history.jsonl");
        Some(
            backup_profile(&bt, &profile.id, &profile.dir, &history)
                .map_err(|e| format!("Backup failed, nothing changed: {e:#}"))?,
        )
    };
    let journal = apply_adopt(plan, &ctx.paths.adopt_dir()).map_err(|e| format!("{e:#}"))?;
    Ok(Applied {
        backup,
        journal: journal.path.display().to_string(),
        changed: journal.entries.len(),
        memory_conflicts: journal.memory_conflicts().count(),
        conflicts_dir: plan
            .common
            .join(".agent-prism-conflicts")
            .join(&profile.id)
            .display()
            .to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::testutil::*;

    fn view() -> (Fixture, AdoptView) {
        let f = Fixture::new();
        let cfg = config();
        let p = cfg.profiles(f.paths())[0].clone();
        std::fs::write(p.dir.join("CLAUDE.md"), "# other").unwrap();
        std::fs::create_dir_all(p.dir.join("projects/x")).unwrap();
        std::fs::write(p.dir.join("projects/x/s.jsonl"), "{}").unwrap();
        let mut v = AdoptView::new(f.ctx.clone(), &cfg, p);
        v.backup_target = f.paths().state_dir.join("backups").display().to_string();
        (f, v)
    }

    fn draw(v: &mut AdoptView, w: u16, h: u16) -> String {
        let area = Rect::new(0, 0, w, h);
        let mut buf = Buffer::empty(area);
        v.render(area, &mut buf);
        text(&buf)
    }

    #[test]
    fn plan_confirm_apply_done() {
        let (f, mut v) = view();
        for (w, h) in SIZES {
            let s = draw(&mut v, w, h);
            assert!(s.contains("Share sessions"), "{s}");
        }
        let s = draw(&mut v, 120, 40);
        assert!(s.contains("Will merge") && s.contains("projects"), "{s}");
        assert!(s.contains("Backup first"), "{s}");
        assert_eq!(v.handle_key(key(KeyCode::Enter)), FlowOutcome::Continue);
        assert!(draw(&mut v, 80, 24).contains("Apply these changes?"));
        // Anything but y does not apply.
        v.handle_key(ch('x'));
        assert!(!v.is_running());
        v.handle_key(ch('y'));
        assert!(v.is_running());
        assert!(draw(&mut v, 80, 24).contains("Backing up"));
        for _ in 0..500 {
            v.tick();
            if !v.is_running() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        let s = draw(&mut v, 120, 40);
        assert!(s.contains("Done"), "{s}");
        assert!(s.contains("Journal"), "{s}");
        let dir = f.paths().home.join(".claude-a/projects");
        assert!(
            std::fs::symlink_metadata(dir)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_eq!(v.handle_key(key(KeyCode::Enter)), FlowOutcome::Done(true));
    }

    #[test]
    fn error_state_on_bad_backup() {
        let (_f, mut v) = view();
        v.backup_target = "ssh://".into();
        v.handle_key(key(KeyCode::Enter));
        v.handle_key(ch('y'));
        for _ in 0..500 {
            v.tick();
            if !v.is_running() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        for (w, h) in SIZES {
            assert!(draw(&mut v, w, h).contains("Adopt stopped"));
        }
        assert_eq!(v.handle_key(key(KeyCode::Esc)), FlowOutcome::Done(true));
    }

    #[test]
    fn esc_backs_out_without_changes() {
        let (f, mut v) = view();
        assert_eq!(v.handle_key(key(KeyCode::Esc)), FlowOutcome::Done(false));
        let dir = f.paths().home.join(".claude-a/projects");
        assert!(
            !std::fs::symlink_metadata(dir)
                .unwrap()
                .file_type()
                .is_symlink()
        );
    }
}
