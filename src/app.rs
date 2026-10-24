//! Application shell: state, menus, dialogs, toasts, file handling and the glue that turns
//! UI gestures into `Command`s.
//!
//! DESIGN NOTES (immediate mode + borrow checker)
//! * While a pane is being drawn it holds `&self.project`. Panes therefore collect *intents*
//!   (clicks, commits, drags) in locals and the shell applies them after the pane finished.
//!   Every document change goes through `App::exec` -> `CommandHistory::execute`
//!   (command pattern + scheduler validation + automatic rollback).
//! * UI-only state (selection, collapsed rows, zoom, scroll, the cell being edited) lives in
//!   `App`, never in the project, so undo/redo never disturbs it and never marks the file dirty.
//! * Every failure is a toast (non-blocking). No `unwrap()` touches user input.

use crate::commands::*;
use crate::export;
use crate::model::*;
use crate::parse::*;
use crate::persistence;
use crate::scheduler::{self, SchedError, Schedule};
use crate::view::*;
use chrono::{Local, NaiveDate};
use eframe::egui;
use egui::{Context, Key, Modifiers};
use std::collections::HashMap;
use std::path::PathBuf;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Col {
    Id,
    Name,
    Duration,
    Start,
    Finish,
    Preds,
    Resources,
}

pub struct CellEdit {
    pub task: TaskId,
    pub col: Col,
    pub buf: String,
    pub fresh: bool,
}

pub struct Toast {
    pub msg: String,
    pub error: bool,
    pub until: f64,
}

#[derive(Clone, Debug)]
pub enum Pending {
    New,
    Open(String),
    Sample,
    Stress,
}

pub struct ProjectDraft {
    pub meta: ProjectMeta,
    pub new_holiday: NaiveDate,
}

pub enum Dialog {
    Open(String),
    SaveAs { path: String, then_close: bool },
    ExportSvg(String),
    ConfirmClose,
    ConfirmDiscard(Pending),
    Project(ProjectDraft),
    LevelReport(String),
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum DetailTab {
    Task,
    Resources,
}

pub struct GanttDrag {
    pub task: TaskId,
    pub resize: bool,
    pub press_x: f32,
    pub dx_days: i64,
}

/// Persistent single-field text buffer used by the detail pane (commit on focus loss / Enter).
#[derive(Default)]
pub struct FieldEdit {
    pub key: Option<String>,
    pub buf: String,
}

pub struct App {
    pub project: Project,
    pub schedule: Schedule,
    pub history: CommandHistory,
    pub path: Option<PathBuf>,
    pub dirty: bool,
    pub allow_close: bool,
    pub close_requested_again: bool,

    // selection (order = click order; last = primary)
    pub selected: Vec<TaskId>,
    pub anchor: Option<TaskId>,
    pub selected_resource: Option<ResourceId>,

    // view
    pub opts: ViewOptions,
    pub rows: Vec<Row>,
    pub rows_dirty: bool,
    pub row_of: HashMap<TaskId, usize>,
    pub scroll_y: f32,
    pub view_x0: f64,
    pub ppd: f32,
    pub did_initial_view: bool,

    // interaction
    pub edit: Option<CellEdit>,
    pub gantt_drag: Option<GanttDrag>,
    pub field: FieldEdit,
    pub detail_tab: DetailTab,
    pub new_resource_name: String,
    pub pred_pick: Option<TaskId>,

    // shell
    pub dialog: Option<Dialog>,
    pub toasts: Vec<Toast>,
    pub last_title: String,
    pub frame_ms: f32,
}

impl App {
    pub fn new(project: Project, path: Option<PathBuf>) -> App {
        let mut project = project;
        let schedule = match scheduler::schedule(&project) {
            Ok(s) => {
                project.apply_schedule(&s);
                s
            }
            Err(_) => Schedule::default(),
        };
        let mut a = App {
            project,
            schedule,
            history: CommandHistory::default(),
            path,
            dirty: false,
            allow_close: false,
            close_requested_again: false,
            selected: vec![],
            anchor: None,
            selected_resource: None,
            opts: ViewOptions::default(),
            rows: vec![],
            rows_dirty: true,
            row_of: HashMap::new(),
            scroll_y: 0.0,
            view_x0: 0.0,
            ppd: 28.0,
            did_initial_view: false,
            edit: None,
            gantt_drag: None,
            field: FieldEdit::default(),
            detail_tab: DetailTab::Task,
            new_resource_name: String::new(),
            pred_pick: None,
            dialog: None,
            toasts: vec![],
            last_title: String::new(),
            frame_ms: 0.0,
        };
        a.rebuild_rows();
        a
    }

    // ------------------------------------------------------------------ helpers

    pub fn mpd(&self) -> i64 {
        self.project.calendar.minutes_per_day()
    }

    pub fn rebuild_rows(&mut self) {
        self.rows = build_rows(&self.project, &self.schedule, &self.opts);
        self.row_of.clear();
        for (i, r) in self.rows.iter().enumerate() {
            if let Some(t) = r.task {
                self.row_of.insert(t, i);
            }
        }
        self.rows_dirty = false;
    }

    pub fn toast_err(&mut self, msg: impl Into<String>) {
        self.toasts.push(Toast { msg: msg.into(), error: true, until: f64::MAX });
    }
    pub fn toast_info(&mut self, msg: impl Into<String>) {
        self.toasts.push(Toast { msg: msg.into(), error: false, until: f64::MAX });
    }

    pub fn describe(&self, e: &SchedError) -> String {
        let p = &self.project;
        e.describe(&|id| p.task(id).map(|t| t.name.clone()).unwrap_or_else(|| format!("#{}", id)))
    }

    pub fn task_name(&self, id: TaskId) -> String {
        self.project.task(id).map(|t| t.name.clone()).unwrap_or_else(|| format!("#{}", id))
    }

    /// The single funnel for document changes. Returns false (and toasts) if rejected.
    pub fn exec(&mut self, cmd: Command) -> bool {
        match self.history.execute(cmd, &mut self.project) {
            Ok(s) => {
                self.schedule = s;
                self.dirty = true;
                self.rows_dirty = true;
                true
            }
            Err(e) => {
                let m = self.describe(&e);
                self.toast_err(m);
                false
            }
        }
    }

    fn after_history(&mut self, label: &str, r: Option<Result<Schedule, SchedError>>, verb: &str) {
        match r {
            Some(Ok(s)) => {
                self.schedule = s;
                self.dirty = true;
                self.rows_dirty = true;
                self.selected.retain(|id| self.project.task(*id).is_some());
                self.toast_info(format!("{} {}", verb, label));
            }
            Some(Err(e)) => {
                let m = self.describe(&e);
                self.toast_err(m);
            }
            None => {}
        }
    }

    pub fn undo(&mut self) {
        let label = self.history.undo_label().unwrap_or_default();
        let r = self.history.undo(&mut self.project);
        self.after_history(&label, r, "Undid:");
    }
    pub fn redo(&mut self) {
        let label = self.history.redo_label().unwrap_or_default();
        let r = self.history.redo(&mut self.project);
        self.after_history(&label, r, "Redid:");
    }

    // ------------------------------------------------------------------ selection

    pub fn primary(&self) -> Option<TaskId> {
        self.selected.last().copied()
    }

    pub fn select_click(&mut self, id: TaskId, mods: Modifiers) {
        if mods.shift {
            if let (Some(a), Some(&ra), Some(&rb)) = (self.anchor, self.anchor.and_then(|a| self.row_of.get(&a)), self.row_of.get(&id)) {
                let _ = a;
                let (lo, hi) = (ra.min(rb), ra.max(rb));
                self.selected = self.rows[lo..=hi].iter().filter_map(|r| r.task).collect();
                if ra > rb {
                    self.selected.reverse();
                }
                if let Some(pos) = self.selected.iter().position(|x| *x == id) {
                    let v = self.selected.remove(pos);
                    self.selected.push(v);
                }
                return;
            }
        }
        if mods.command || mods.ctrl {
            if let Some(pos) = self.selected.iter().position(|x| *x == id) {
                self.selected.remove(pos);
            } else {
                self.selected.push(id);
            }
        } else {
            self.selected = vec![id];
        }
        self.anchor = Some(id);
    }

    // ------------------------------------------------------------------ editing actions

    pub fn add_task(&mut self, as_child: bool) {
        let sel = self.primary();
        let mut t = self.project.new_task("New task");
        let mut cmds = vec![];
        let index;
        match (sel.and_then(|s| self.project.task(s).cloned()), as_child) {
            (Some(s), true) => {
                t.parent = Some(s.id);
                index = self.project.tasks.len();
                if !self.project.has_children(s.id) {
                    // a parent becomes a summary task: summaries cannot carry links
                    for d in self.project.deps.iter().filter(|d| d.pred == s.id || d.succ == s.id) {
                        cmds.push(Command::RemoveDependency(d.clone()));
                    }
                }
            }
            (Some(s), false) => {
                t.parent = s.parent;
                index = self.project.task_pos(s.id).map(|i| i + 1).unwrap_or(self.project.tasks.len());
            }
            (None, _) => index = self.project.tasks.len(),
        }
        let id = t.id;
        cmds.push(Command::InsertTasks { items: vec![(index, t)], deps: vec![], assigns: vec![] });
        if self.exec(Command::batch("Add task", cmds)) {
            self.selected = vec![id];
            self.anchor = Some(id);
            if let Some(p) = self.project.task(id).and_then(|t| t.parent) {
                self.opts.collapsed.remove(&p);
            }
            self.rebuild_rows();
            self.edit = Some(CellEdit { task: id, col: Col::Name, buf: "New task".into(), fresh: true });
        }
    }

    pub fn delete_selected(&mut self) {
        if self.selected.is_empty() {
            return;
        }
        if let Some(cmd) = build_delete(&self.project, &self.selected) {
            if self.exec(cmd) {
                self.selected.clear();
                self.anchor = None;
            }
        }
    }

    pub fn indent(&mut self, out: bool) {
        let mut sel: Vec<TaskId> = self.selected.clone();
        sel.sort_by_key(|id| self.project.task_pos(*id).unwrap_or(usize::MAX));
        let cm = self.project.children_map();
        let mut cmds = vec![];
        let mut new_summaries: Vec<TaskId> = vec![];
        let mut failed = 0;
        for id in &sel {
            let Some(t) = self.project.task(*id) else { continue };
            if out {
                match t.parent {
                    Some(p) => {
                        let gp = self.project.task(p).and_then(|x| x.parent);
                        cmds.push(Command::SetParent { id: *id, old: t.parent, new: gp });
                    }
                    None => failed += 1,
                }
            } else {
                let sibs = cm.get(&t.parent).cloned().unwrap_or_default();
                let pos = sibs.iter().position(|x| x == id).unwrap_or(0);
                // previous sibling that is not itself part of the selection
                let target = sibs[..pos].iter().rev().find(|s| !sel.contains(s)).copied();
                match target {
                    Some(np) => {
                        cmds.push(Command::SetParent { id: *id, old: t.parent, new: Some(np) });
                        if !self.project.has_children(np) && !new_summaries.contains(&np) {
                            new_summaries.push(np);
                        }
                    }
                    None => failed += 1,
                }
            }
        }
        let mut removed = 0;
        for np in &new_summaries {
            for d in self.project.deps.iter().filter(|d| d.pred == *np || d.succ == *np) {
                cmds.insert(0, Command::RemoveDependency(d.clone()));
                removed += 1;
            }
        }
        if cmds.is_empty() {
            self.toast_err(if out { "Cannot outdent: already at the top level" } else { "Cannot indent: no previous task at this level to nest under" });
            return;
        }
        if self.exec(Command::batch(if out { "Outdent" } else { "Indent" }, cmds)) {
            if removed > 0 {
                self.toast_info(format!("{} link(s) on the new summary task were removed (summaries roll up their sub-tasks)", removed));
            }
            if failed > 0 {
                self.toast_info(format!("{} task(s) could not be moved", failed));
            }
        }
    }

    pub fn link_selected(&mut self, kind: DepKind) {
        if self.selected.len() < 2 {
            self.toast_err("Select two or more tasks (Ctrl/Shift-click) to link them in the order selected");
            return;
        }
        let mut cmds = vec![];
        for w in self.selected.windows(2) {
            let d = Dependency { pred: w[0], succ: w[1], kind, lag_min: 0 };
            if self.project.deps.iter().any(|x| x.pred == d.pred && x.succ == d.succ) {
                cmds.push(Command::ChangeDependency { old: self.project.deps.iter().find(|x| x.pred == d.pred && x.succ == d.succ).cloned().unwrap_or(d.clone()), new: d });
            } else {
                cmds.push(Command::AddDependency(d));
            }
        }
        self.exec(Command::batch("Link tasks", cmds));
    }

    pub fn unlink_selected(&mut self) {
        let sel = self.selected.clone();
        let mut cmds = vec![];
        for d in &self.project.deps {
            let hit = if sel.len() == 1 { d.succ == sel[0] } else { sel.contains(&d.pred) && sel.contains(&d.succ) };
            if hit {
                cmds.push(Command::RemoveDependency(d.clone()));
            }
        }
        if cmds.is_empty() {
            self.toast_info("No links to remove between the selected tasks");
            return;
        }
        self.exec(Command::batch("Remove links", cmds));
    }

    pub fn set_baseline(&mut self) {
        let entries = self.schedule.tasks.iter().map(|(id, ts)| (*id, (ts.start_cal, ts.finish_cal))).collect();
        let b = Baseline { saved_at: Local::now().format("%Y-%m-%d %H:%M").to_string(), entries };
        let old = self.project.baseline.clone();
        if self.exec(Command::SetBaseline { old, new: Some(b) }) {
            self.toast_info("Baseline saved (grey strips under the bars)");
        }
    }

    pub fn clear_baseline(&mut self) {
        let old = self.project.baseline.clone();
        if old.is_some() {
            self.exec(Command::SetBaseline { old, new: None });
        }
    }

    pub fn level_resources(&mut self) {
        match scheduler::level(&self.project) {
            Ok(res) => {
                let old: std::collections::BTreeMap<TaskId, u32> = self.project.tasks.iter().filter(|t| t.leveling_delay_days > 0).map(|t| (t.id, t.leveling_delay_days)).collect();
                let moved = res.delays.len();
                let unresolved = res.unresolved.len();
                if old == res.delays {
                    self.toast_info("Nothing to level: no resource is overallocated (or no slack is available)");
                } else if self.exec(Command::SetLevelingDelays { old, new: res.delays.clone() }) {
                    let mut msg = format!("Leveling delayed {} task(s) using their float; the finish date did not move.", moved);
                    if unresolved > 0 {
                        msg.push_str(&format!("\n{} overallocated day(s) remain: those tasks are critical and cannot move without extending the project.", unresolved));
                    }
                    self.dialog = Some(Dialog::LevelReport(msg));
                }
                if old_is_empty_and_no_change(&res) {
                    // nothing else
                }
            }
            Err(e) => {
                let m = self.describe(&e);
                self.toast_err(m);
            }
        }
    }

    pub fn clear_leveling(&mut self) {
        let old: std::collections::BTreeMap<TaskId, u32> = self.project.tasks.iter().filter(|t| t.leveling_delay_days > 0).map(|t| (t.id, t.leveling_delay_days)).collect();
        if !old.is_empty() {
            self.exec(Command::SetLevelingDelays { old, new: Default::default() });
        }
    }

    // ------------------------------------------------------------------ cell text / commit

    pub fn preds_text(&self, id: TaskId) -> String {
        let mpd = self.mpd();
        self.project.deps.iter().filter(|d| d.succ == id).map(|d| format_link(d.pred, d.kind, d.lag_min, mpd)).collect::<Vec<_>>().join(", ")
    }

    pub fn cell_text(&self, id: TaskId, col: Col) -> String {
        let mpd = self.mpd();
        let Some(t) = self.project.task(id) else { return String::new() };
        let ts = self.schedule.tasks.get(&id);
        match col {
            Col::Id => t.id.to_string(),
            Col::Name => t.name.clone(),
            Col::Duration => {
                if t.is_milestone {
                    "0d".into()
                } else if let Some(ts) = ts.filter(|x| x.is_summary) {
                    format_duration(ts.duration, mpd)
                } else {
                    format_duration(t.duration_min, mpd)
                }
            }
            Col::Start => format_date(ts.map(|x| x.start).unwrap_or(t.start)),
            Col::Finish => format_date(ts.map(|x| x.finish).unwrap_or(t.finish)),
            Col::Preds => self.preds_text(id),
            Col::Resources => self.project.resource_names(id),
        }
    }

    pub fn is_editable(&self, id: TaskId, col: Col) -> bool {
        let summary = self.schedule.tasks.get(&id).map_or(false, |t| t.is_summary);
        match col {
            Col::Id => false,
            Col::Name => true,
            _ => !summary,
        }
    }

    pub fn commit_cell(&mut self, id: TaskId, col: Col, text: &str) -> Result<(), String> {
        let mpd = self.mpd();
        let t = self.project.task(id).cloned().ok_or("That task no longer exists")?;
        match col {
            Col::Id => Ok(()),
            Col::Name => {
                let name = text.trim();
                if name.is_empty() {
                    return Err("Task name cannot be empty".into());
                }
                if name == t.name {
                    return Ok(());
                }
                self.exec(Command::SetTaskName { id, old: t.name, new: name.to_string() });
                Ok(())
            }
            Col::Duration => {
                let min = parse_duration(text, mpd)?;
                let new = if min == 0 { (true, t.duration_min) } else { (false, min) };
                if new == (t.is_milestone, t.duration_min) || (new.0 && t.is_milestone) {
                    return Ok(());
                }
                self.exec(Command::SetDuration { id, old: (t.is_milestone, t.duration_min), new });
                Ok(())
            }
            Col::Start => {
                let d = parse_date(text)?;
                let old = t.constraint;
                let kind = if old.kind == ConstraintKind::MustStartOn { ConstraintKind::MustStartOn } else { ConstraintKind::StartNoEarlierThan };
                let new = Constraint { kind, date: Some(d) };
                if new != old {
                    self.exec(Command::SetConstraint { id, old, new });
                }
                Ok(())
            }
            Col::Finish => {
                let d = parse_date(text)?;
                let old = t.constraint;
                let kind = if old.kind == ConstraintKind::MustFinishOn { ConstraintKind::MustFinishOn } else { ConstraintKind::FinishNoEarlierThan };
                let new = Constraint { kind, date: Some(d) };
                if new != old {
                    self.exec(Command::SetConstraint { id, old, new });
                }
                Ok(())
            }
            Col::Preds => {
                let wanted = parse_predecessors(text, mpd)?;
                self.set_predecessors(id, wanted)
            }
            Col::Resources => {
                let wanted = parse_resources(text)?;
                self.set_assignments(id, wanted);
                Ok(())
            }
        }
    }

    pub fn set_predecessors(&mut self, id: TaskId, wanted: Vec<(TaskId, DepKind, i64)>) -> Result<(), String> {
        for (p, _, _) in &wanted {
            if *p == id {
                return Err("A task cannot be its own predecessor".into());
            }
            if self.project.task(*p).is_none() {
                return Err(format!("There is no task with ID {}", p));
            }
        }
        let existing: Vec<Dependency> = self.project.deps.iter().filter(|d| d.succ == id).cloned().collect();
        let mut cmds = vec![];
        for e in &existing {
            if !wanted.iter().any(|(p, _, _)| *p == e.pred) {
                cmds.push(Command::RemoveDependency(e.clone()));
            }
        }
        for (p, kind, lag) in &wanted {
            let new = Dependency { pred: *p, succ: id, kind: *kind, lag_min: *lag };
            match existing.iter().find(|e| e.pred == *p) {
                Some(old) if *old == new => {}
                Some(old) => cmds.push(Command::ChangeDependency { old: old.clone(), new }),
                None => cmds.push(Command::AddDependency(new)),
            }
        }
        if !cmds.is_empty() {
            self.exec(Command::batch("Edit predecessors", cmds));
        }
        Ok(())
    }

    pub fn set_assignments(&mut self, id: TaskId, wanted: Vec<(String, f64)>) {
        let mut cmds = vec![];
        let mut keep: Vec<ResourceId> = vec![];
        let mut created: Vec<(String, ResourceId)> = vec![];
        for (name, units) in &wanted {
            let rid = match self.project.resource_by_name(name).map(|r| r.id).or_else(|| created.iter().find(|(n, _)| n.eq_ignore_ascii_case(name)).map(|(_, i)| *i)) {
                Some(r) => r,
                None => {
                    let rid = self.project.alloc_resource_id();
                    cmds.push(Command::InsertResource {
                        index: self.project.resources.len() + created.len(),
                        resource: Resource { id: rid, name: name.clone(), kind: ResourceKind::Work, max_units: 1.0, std_rate: 0.0 },
                    });
                    created.push((name.clone(), rid));
                    rid
                }
            };
            keep.push(rid);
            let old = self.project.assignments.iter().find(|a| a.task == id && a.resource == rid).map(|a| a.units);
            if old != Some(*units) {
                cmds.push(Command::SetAssignment { task: id, resource: rid, old, new: Some(*units) });
            }
        }
        for a in self.project.assignments.iter().filter(|a| a.task == id) {
            if !keep.contains(&a.resource) {
                cmds.push(Command::SetAssignment { task: id, resource: a.resource, old: Some(a.units), new: None });
            }
        }
        if !cmds.is_empty() {
            self.exec(Command::batch("Edit resources", cmds));
        }
    }

    // ------------------------------------------------------------------ files

    pub fn replace_project(&mut self, p: Project, path: Option<PathBuf>) {
        *self = {
            let mut n = App::new(p, path);
            n.toasts = std::mem::take(&mut self.toasts);
            n.ppd = self.ppd;
            n
        };
    }

    pub fn do_pending(&mut self, what: Pending) {
        match what {
            Pending::New => {
                let start = next_monday_from(Local::now().date_naive());
                self.replace_project(Project::empty("New project", start), None);
            }
            Pending::Sample => self.replace_project(Project::sample(), None),
            Pending::Stress => self.replace_project(Project::stress(1000), None),
            Pending::Open(path) => match persistence::load(std::path::Path::new(&path)) {
                Ok(p) => {
                    self.replace_project(p, Some(PathBuf::from(&path)));
                    self.toast_info(format!("Opened {}", path));
                }
                Err(e) => self.toast_err(e.to_string()),
            },
        }
    }

    /// Ask first when there are unsaved changes (data-integrity requirement).
    pub fn request(&mut self, what: Pending) {
        if self.dirty {
            self.dialog = Some(Dialog::ConfirmDiscard(what));
        } else {
            self.do_pending(what);
        }
    }

    /// Returns true when saved.
    pub fn save(&mut self) -> bool {
        match self.path.clone() {
            Some(p) => self.save_to(p),
            None => {
                self.dialog = Some(Dialog::SaveAs { path: default_name(&self.project.name), then_close: false });
                false
            }
        }
    }

    pub fn save_to(&mut self, path: PathBuf) -> bool {
        match persistence::save(&path, &self.project) {
            Ok(()) => {
                self.dirty = false;
                self.toast_info(format!("Saved {}", path.display()));
                self.path = Some(path);
                true
            }
            Err(e) => {
                self.toast_err(e.to_string());
                false
            }
        }
    }

    pub fn export_svg(&mut self, path: &str) {
        let svg = export::gantt_svg(&self.project, &self.schedule, &self.rows, Local::now().date_naive());
        match std::fs::write(path, svg) {
            Ok(()) => self.toast_info(format!("Exported Gantt chart to {}", path)),
            Err(e) => self.toast_err(format!("Could not write {}: {}", path, e)),
        }
    }

    // ------------------------------------------------------------------ frame

    fn handle_close(&mut self, ctx: &Context) {
        if ctx.input(|i| i.viewport().close_requested()) && self.dirty && !self.allow_close {
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            self.dialog = Some(Dialog::ConfirmClose);
        }
    }

    fn handle_shortcuts(&mut self, ctx: &Context) {
        let typing = ctx.egui_wants_keyboard_input();
        let (undo, redo, save, del) = ctx.input_mut(|i| {
            let redo = i.consume_key(Modifiers::COMMAND | Modifiers::SHIFT, Key::Z) || i.consume_key(Modifiers::COMMAND, Key::Y);
            let undo = !redo && i.consume_key(Modifiers::COMMAND, Key::Z);
            let save = i.consume_key(Modifiers::COMMAND, Key::S);
            let del = i.key_pressed(Key::Delete);
            (undo, redo, save, del)
        });
        if save {
            self.save();
        }
        if !typing {
            if undo {
                self.undo();
            }
            if redo {
                self.redo();
            }
            if del && self.dialog.is_none() {
                self.delete_selected();
            }
        }
    }

    fn update_title(&mut self, ctx: &Context) {
        let name = self.path.as_ref().and_then(|p| p.file_name()).map(|s| s.to_string_lossy().to_string()).unwrap_or_else(|| "untitled".into());
        let t = format!("{}{} - {} - ProjectCraft", if self.dirty { "\u{25CF} " } else { "" }, name, self.project.name);
        if t != self.last_title {
            ctx.send_viewport_cmd(egui::ViewportCommand::Title(t.clone()));
            self.last_title = t;
        }
    }
}

fn old_is_empty_and_no_change(_r: &scheduler::LevelResult) -> bool {
    false
}

pub fn next_monday_from(d: NaiveDate) -> NaiveDate {
    use chrono::Datelike;
    let mut d = d;
    while d.weekday() != chrono::Weekday::Mon {
        d = d.succ_opt().unwrap_or(d);
    }
    d
}

fn default_name(name: &str) -> String {
    let clean: String = name.chars().map(|c| if c.is_alphanumeric() { c } else { '_' }).collect();
    format!("{}.pcraft", clean.trim_matches('_'))
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        self.frame_ms = ctx.input(|i| i.unstable_dt) * 1000.0;
        if self.close_requested_again {
            self.close_requested_again = false;
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }
        self.handle_close(&ctx);
        self.handle_shortcuts(&ctx);
        if self.rows_dirty {
            self.rebuild_rows();
        }
        self.update_title(&ctx);

        egui::Panel::top("menu").show(ui, |ui| self.ui_menu(ui));
        egui::Panel::top("toolbar").show(ui, |ui| self.ui_toolbar(ui));
        egui::Panel::bottom("status").show(ui, |ui| self.ui_status(ui));
        egui::Panel::bottom("detail").resizable(true).default_size(230.0).size_range(120.0..=600.0).show(ui, |ui| self.ui_detail(ui));
        egui::Panel::left("sheet").frame(egui::Frame::new()).resizable(true).default_size(660.0).size_range(300.0..=1600.0).show(ui, |ui| self.ui_sheet(ui));
        egui::CentralPanel::default().frame(egui::Frame::new()).show(ui, |ui| self.ui_gantt(ui));

        self.ui_dialogs(&ctx);
        self.ui_toasts(&ctx);
    }
}

#[cfg(test)]
mod tests {
    //! Headless tests of the UI glue: everything the buttons/cells call, minus the painting.
    use super::*;

    fn app() -> App {
        App::new(Project::sample(), None)
    }
    fn id_of(a: &App, name: &str) -> TaskId {
        a.project.tasks.iter().find(|t| t.name == name).map(|t| t.id).expect("task")
    }

    #[test]
    fn cell_edits_go_through_commands_and_undo() {
        let mut a = app();
        let t = id_of(&a, "Wireframes");
        let fin_before = a.schedule.tasks[&t].finish;
        a.commit_cell(t, Col::Name, "Wires").unwrap();
        a.commit_cell(t, Col::Duration, "6d").unwrap();
        assert_eq!(a.project.task(t).unwrap().name, "Wires");
        let fin_after = a.schedule.tasks[&t].finish;
        assert_ne!(fin_after, fin_before);
        a.undo();
        assert_eq!(a.project.task(t).unwrap().duration_min, 4 * 480);
        assert_eq!(a.schedule.tasks[&t].finish, fin_before);
        a.redo();
        assert_eq!(a.schedule.tasks[&t].finish, fin_after);
        assert!(a.dirty);
    }

    #[test]
    fn bad_input_returns_errors_not_panics() {
        let mut a = app();
        let t = id_of(&a, "Wireframes");
        for (col, txt) in [(Col::Duration, "abc"), (Col::Start, "31/31/2026"), (Col::Preds, "zz"), (Col::Preds, "9999"), (Col::Resources, "Bob[x]"), (Col::Name, "  ")] {
            assert!(a.commit_cell(t, col, txt).is_err(), "{:?} {}", col, txt);
        }
        assert!(!a.dirty, "rejected input must not dirty the project");
    }

    #[test]
    fn predecessor_cycle_is_rejected_and_rolled_back() {
        let mut a = app();
        let (w, v) = (id_of(&a, "Wireframes"), id_of(&a, "Visual design"));
        // Visual design already depends on Wireframes (SS+2d); make Wireframes depend on it
        let before = a.project.deps.len();
        a.commit_cell(w, Col::Preds, &format!("{}", v)).unwrap();
        assert_eq!(a.project.deps.len(), before, "cycle must be rolled back");
        assert!(a.toasts.iter().any(|t| t.error && t.msg.contains("Circular")), "{:?}", a.toasts.iter().map(|t| &t.msg).collect::<Vec<_>>());
    }

    #[test]
    fn predecessor_text_syntax_roundtrips() {
        let mut a = app();
        let (w, v) = (id_of(&a, "Wireframes"), id_of(&a, "Visual design"));
        assert_eq!(a.preds_text(v), format!("{}SS+2d", w));
        a.commit_cell(v, Col::Preds, &format!("{}FF-1d", w)).unwrap();
        assert_eq!(a.preds_text(v), format!("{}FF-1d", w));
    }

    #[test]
    fn add_indent_outdent_delete() {
        let mut a = app();
        let n0 = a.project.tasks.len();
        let w = id_of(&a, "Wireframes");
        a.selected = vec![w];
        a.add_task(false);
        assert_eq!(a.project.tasks.len(), n0 + 1);
        let new = a.primary().unwrap();
        assert_eq!(a.project.task(new).unwrap().parent, a.project.task(w).unwrap().parent);
        // indent under Wireframes: Wireframes becomes a summary and loses its link
        assert!(a.project.deps.iter().any(|d| d.succ == w || d.pred == w));
        a.indent(false);
        assert_eq!(a.project.task(new).unwrap().parent, Some(w));
        assert!(a.schedule.tasks[&w].is_summary);
        assert!(!a.project.deps.iter().any(|d| d.succ == w || d.pred == w));
        a.indent(true);
        assert_ne!(a.project.task(new).unwrap().parent, Some(w));
        a.delete_selected();
        assert_eq!(a.project.tasks.len(), n0);
        a.undo();
        assert_eq!(a.project.tasks.len(), n0 + 1);
    }

    #[test]
    fn link_and_unlink_selection() {
        let mut a = app();
        let (x, y) = (id_of(&a, "Kick-off & scope"), id_of(&a, "Content migration"));
        a.selected = vec![x, y];
        a.link_selected(DepKind::SS);
        assert!(a.project.deps.iter().any(|d| d.pred == x && d.succ == y && d.kind == DepKind::SS));
        a.unlink_selected();
        assert!(!a.project.deps.iter().any(|d| d.pred == x && d.succ == y));
    }

    #[test]
    fn resources_text_creates_and_assigns() {
        let mut a = app();
        let t = id_of(&a, "Wireframes");
        a.commit_cell(t, Col::Resources, "Alice[50%], Dave").unwrap();
        assert!(a.project.resource_by_name("Dave").is_some());
        assert_eq!(a.project.resource_names(t), "Alice[50%], Dave");
        a.undo();
        assert_eq!(a.project.resource_names(t), "Bob");
        assert!(a.project.resource_by_name("Dave").is_none());
    }

    #[test]
    fn baseline_and_leveling_commands() {
        let mut a = app();
        a.set_baseline();
        assert_eq!(a.project.baseline.as_ref().unwrap().entries.len(), a.project.tasks.len());
        a.undo();
        assert!(a.project.baseline.is_none());
        a.level_resources();
        a.undo(); // leveling (if any) is one undo step; never panics
    }

    #[test]
    fn save_load_roundtrip_clears_dirty() {
        let mut a = app();
        let t = id_of(&a, "Wireframes");
        a.commit_cell(t, Col::Name, "Saved name").unwrap();
        assert!(a.dirty);
        let mut p = std::env::temp_dir();
        p.push(format!("pc_app_test_{}.pcraft", std::process::id()));
        assert!(a.save_to(p.clone()));
        assert!(!a.dirty);
        let q = persistence::load(&p).unwrap();
        assert_eq!(q.task(t).unwrap().name, "Saved name");
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn stress_project_edit_is_fast() {
        let mut a = App::new(Project::stress(1000), None);
        let t = a.project.tasks[5].id;
        let t0 = std::time::Instant::now();
        for i in 0..10 {
            a.commit_cell(t, Col::Duration, &format!("{}d", 2 + i)).unwrap();
        }
        a.rebuild_rows();
        let per = t0.elapsed() / 10;
        println!("edit+reschedule+rows (1000 tasks) = {:?}", per);
        assert!(per.as_millis() < 150, "{:?}", per);
    }
}
