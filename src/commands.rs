//! Undo/redo via the Command pattern.
//!
//! DESIGN
//! * Every user action is a `Command` value that knows how to `apply` itself to the project and
//!   how to produce its `inverse()` (usually: swap old/new). Nothing clones the whole `Project`,
//!   so history is cheap, and UI-only state (selection, scroll, zoom) is untouched by undo.
//! * `CommandHistory::execute` = apply -> run the scheduler -> if the scheduler rejects the result
//!   (e.g. a dependency cycle) the command is rolled back with its inverse and NOT recorded.
//!   The project therefore can never be left in an invalid state.
//! * Commands carry the data needed to be replayed deterministically: new task ids are allocated
//!   *before* the command is built, so undo -> redo restores the very same ids.

use crate::model::*;
use crate::scheduler::{self, SchedError, Schedule};
use std::collections::BTreeMap;

#[derive(Clone, Debug)]
pub enum Command {
    SetTaskName { id: TaskId, old: String, new: String },
    SetTaskDescription { id: TaskId, old: String, new: String },
    /// (is_milestone, duration_min) pairs so "type 0" <-> milestone round-trips exactly.
    SetDuration { id: TaskId, old: (bool, i64), new: (bool, i64) },
    SetProgress { id: TaskId, old: f64, new: f64 },
    SetConstraint { id: TaskId, old: Constraint, new: Constraint },
    SetParent { id: TaskId, old: Option<TaskId>, new: Option<TaskId> },
    /// Insert tasks (with their links/assignments) at exact Vec indices, ascending.
    InsertTasks { items: Vec<(usize, Task)>, deps: Vec<Dependency>, assigns: Vec<Assignment> },
    /// Inverse of InsertTasks.
    DeleteTasks { items: Vec<(usize, Task)>, deps: Vec<Dependency>, assigns: Vec<Assignment> },
    AddDependency(Dependency),
    RemoveDependency(Dependency),
    ChangeDependency { old: Dependency, new: Dependency },
    InsertResource { index: usize, resource: Resource },
    DeleteResource { index: usize, resource: Resource, assigns: Vec<Assignment> },
    SetResource { old: Resource, new: Resource },
    SetAssignment { task: TaskId, resource: ResourceId, old: Option<f64>, new: Option<f64> },
    SetLevelingDelays { old: BTreeMap<TaskId, u32>, new: BTreeMap<TaskId, u32> },
    SetBaseline { old: Option<Baseline>, new: Option<Baseline> },
    SetProjectMeta { old: ProjectMeta, new: ProjectMeta },
    Batch { label: String, cmds: Vec<Command> },
}

impl Command {
    pub fn label(&self) -> String {
        match self {
            Command::SetTaskName { .. } => "Rename task".into(),
            Command::SetTaskDescription { .. } => "Edit description".into(),
            Command::SetDuration { .. } => "Change duration".into(),
            Command::SetProgress { .. } => "Change progress".into(),
            Command::SetConstraint { .. } => "Change constraint / move task".into(),
            Command::SetParent { .. } => "Change outline level".into(),
            Command::InsertTasks { items, .. } => format!("Add {} task(s)", items.len()),
            Command::DeleteTasks { items, .. } => format!("Delete {} task(s)", items.len()),
            Command::AddDependency(_) => "Add link".into(),
            Command::RemoveDependency(_) => "Remove link".into(),
            Command::ChangeDependency { .. } => "Edit link".into(),
            Command::InsertResource { .. } => "Add resource".into(),
            Command::DeleteResource { .. } => "Delete resource".into(),
            Command::SetResource { .. } => "Edit resource".into(),
            Command::SetAssignment { .. } => "Edit assignment".into(),
            Command::SetLevelingDelays { .. } => "Level resources".into(),
            Command::SetBaseline { .. } => "Set baseline".into(),
            Command::SetProjectMeta { .. } => "Edit project / calendar".into(),
            Command::Batch { label, .. } => label.clone(),
        }
    }

    pub fn batch(label: &str, cmds: Vec<Command>) -> Command {
        if cmds.len() == 1 {
            // unwrap-free: pop the only element
            let mut cmds = cmds;
            if let Some(c) = cmds.pop() {
                return c;
            }
            return Command::Batch { label: label.into(), cmds: vec![] };
        }
        Command::Batch { label: label.into(), cmds }
    }

    pub fn inverse(&self) -> Command {
        use Command::*;
        match self.clone() {
            SetTaskName { id, old, new } => SetTaskName { id, old: new, new: old },
            SetTaskDescription { id, old, new } => SetTaskDescription { id, old: new, new: old },
            SetDuration { id, old, new } => SetDuration { id, old: new, new: old },
            SetProgress { id, old, new } => SetProgress { id, old: new, new: old },
            SetConstraint { id, old, new } => SetConstraint { id, old: new, new: old },
            SetParent { id, old, new } => SetParent { id, old: new, new: old },
            InsertTasks { items, deps, assigns } => DeleteTasks { items, deps, assigns },
            DeleteTasks { items, deps, assigns } => InsertTasks { items, deps, assigns },
            AddDependency(d) => RemoveDependency(d),
            RemoveDependency(d) => AddDependency(d),
            ChangeDependency { old, new } => ChangeDependency { old: new, new: old },
            InsertResource { index, resource } => DeleteResource { index, resource, assigns: vec![] },
            DeleteResource { index, resource, assigns } => {
                // re-inserting must also restore the assignments: express as a batch
                Batch {
                    label: "Restore resource".into(),
                    cmds: std::iter::once(InsertResource { index, resource })
                        .chain(assigns.into_iter().map(|a| SetAssignment { task: a.task, resource: a.resource, old: None, new: Some(a.units) }))
                        .collect(),
                }
            }
            SetResource { old, new } => SetResource { old: new, new: old },
            SetAssignment { task, resource, old, new } => SetAssignment { task, resource, old: new, new: old },
            SetLevelingDelays { old, new } => SetLevelingDelays { old: new, new: old },
            SetBaseline { old, new } => SetBaseline { old: new, new: old },
            SetProjectMeta { old, new } => SetProjectMeta { old: new, new: old },
            Batch { label, cmds } => Batch { label, cmds: cmds.iter().rev().map(|c| c.inverse()).collect() },
        }
    }

    pub fn apply(&self, p: &mut Project) {
        use Command::*;
        match self {
            SetTaskName { id, new, .. } => {
                if let Some(t) = p.task_mut(*id) {
                    t.name = new.clone();
                }
            }
            SetTaskDescription { id, new, .. } => {
                if let Some(t) = p.task_mut(*id) {
                    t.description = new.clone();
                }
            }
            SetDuration { id, new, .. } => {
                if let Some(t) = p.task_mut(*id) {
                    t.is_milestone = new.0;
                    t.duration_min = new.1;
                }
            }
            SetProgress { id, new, .. } => {
                if let Some(t) = p.task_mut(*id) {
                    t.progress = new.clamp(0.0, 1.0);
                }
            }
            SetConstraint { id, new, .. } => {
                if let Some(t) = p.task_mut(*id) {
                    t.constraint = *new;
                }
            }
            SetParent { id, new, .. } => {
                if let Some(t) = p.task_mut(*id) {
                    t.parent = *new;
                }
            }
            InsertTasks { items, deps, assigns } => {
                for (i, t) in items {
                    let at = (*i).min(p.tasks.len());
                    p.tasks.insert(at, t.clone());
                    if t.id >= p.next_task_id {
                        p.next_task_id = t.id + 1;
                    }
                }
                p.deps.extend(deps.iter().cloned());
                p.assignments.extend(assigns.iter().cloned());
            }
            DeleteTasks { items, deps, assigns } => {
                let ids: std::collections::HashSet<TaskId> = items.iter().map(|(_, t)| t.id).collect();
                p.tasks.retain(|t| !ids.contains(&t.id));
                // remove every link / assignment touching a deleted task (not just the recorded ones)
                p.deps.retain(|d| !ids.contains(&d.pred) && !ids.contains(&d.succ));
                p.assignments.retain(|a| !ids.contains(&a.task));
                let _ = (deps, assigns);
            }
            AddDependency(d) => {
                p.deps.retain(|x| !(x.pred == d.pred && x.succ == d.succ));
                p.deps.push(d.clone());
            }
            RemoveDependency(d) => p.deps.retain(|x| !(x.pred == d.pred && x.succ == d.succ)),
            ChangeDependency { old, new } => {
                p.deps.retain(|x| !(x.pred == old.pred && x.succ == old.succ));
                p.deps.retain(|x| !(x.pred == new.pred && x.succ == new.succ));
                p.deps.push(new.clone());
            }
            InsertResource { index, resource } => {
                let at = (*index).min(p.resources.len());
                p.resources.insert(at, resource.clone());
                if resource.id >= p.next_resource_id {
                    p.next_resource_id = resource.id + 1;
                }
            }
            DeleteResource { resource, .. } => {
                p.resources.retain(|r| r.id != resource.id);
                p.assignments.retain(|a| a.resource != resource.id);
            }
            SetResource { new, .. } => {
                if let Some(r) = p.resources.iter_mut().find(|r| r.id == new.id) {
                    *r = new.clone();
                }
            }
            SetAssignment { task, resource, new, .. } => {
                p.assignments.retain(|a| !(a.task == *task && a.resource == *resource));
                if let Some(u) = new {
                    p.assignments.push(Assignment { task: *task, resource: *resource, units: *u });
                }
            }
            SetLevelingDelays { new, .. } => {
                for t in &mut p.tasks {
                    t.leveling_delay_days = new.get(&t.id).copied().unwrap_or(0);
                }
            }
            SetBaseline { new, .. } => p.baseline = new.clone(),
            SetProjectMeta { new, .. } => p.set_meta(new),
            Batch { cmds, .. } => {
                for c in cmds {
                    c.apply(p);
                }
            }
        }
    }
}

struct Entry {
    forward: Command,
    inverse: Command,
}

pub struct CommandHistory {
    undo: Vec<Entry>,
    redo: Vec<Entry>,
    limit: usize,
}

impl Default for CommandHistory {
    fn default() -> Self {
        CommandHistory { undo: vec![], redo: vec![], limit: 1000 }
    }
}

impl CommandHistory {
    pub fn clear(&mut self) {
        self.undo.clear();
        self.redo.clear();
    }
    pub fn can_undo(&self) -> bool {
        !self.undo.is_empty()
    }
    pub fn can_redo(&self) -> bool {
        !self.redo.is_empty()
    }
    pub fn undo_label(&self) -> Option<String> {
        self.undo.last().map(|e| e.forward.label())
    }
    pub fn redo_label(&self) -> Option<String> {
        self.redo.last().map(|e| e.forward.label())
    }

    /// Apply `cmd`, re-schedule, and record it. On a scheduler error the project is restored.
    pub fn execute(&mut self, cmd: Command, p: &mut Project) -> Result<Schedule, SchedError> {
        let inverse = cmd.inverse();
        cmd.apply(p);
        match scheduler::schedule(p) {
            Ok(s) => {
                p.apply_schedule(&s);
                self.undo.push(Entry { forward: cmd, inverse });
                if self.undo.len() > self.limit {
                    self.undo.remove(0);
                }
                self.redo.clear();
                Ok(s)
            }
            Err(e) => {
                inverse.apply(p);
                Err(e)
            }
        }
    }

    pub fn undo(&mut self, p: &mut Project) -> Option<Result<Schedule, SchedError>> {
        let e = self.undo.pop()?;
        e.inverse.apply(p);
        let r = scheduler::schedule(p);
        if let Ok(s) = &r {
            p.apply_schedule(s);
        }
        self.redo.push(e);
        Some(r)
    }

    pub fn redo(&mut self, p: &mut Project) -> Option<Result<Schedule, SchedError>> {
        let e = self.redo.pop()?;
        e.forward.apply(p);
        let r = scheduler::schedule(p);
        if let Ok(s) = &r {
            p.apply_schedule(s);
        }
        self.undo.push(e);
        Some(r)
    }
}

// ----------------------------------------------------------------------- command builders

/// Build a DeleteTasks command (with subtree, links, assignments) for the given tasks.
pub fn build_delete(p: &Project, ids: &[TaskId]) -> Option<Command> {
    let mut all: Vec<TaskId> = vec![];
    for id in ids {
        for s in p.subtree(*id) {
            if !all.contains(&s) {
                all.push(s);
            }
        }
    }
    if all.is_empty() {
        return None;
    }
    let mut items: Vec<(usize, Task)> = p.tasks.iter().enumerate().filter(|(_, t)| all.contains(&t.id)).map(|(i, t)| (i, t.clone())).collect();
    items.sort_by_key(|(i, _)| *i);
    let deps = p.deps.iter().filter(|d| all.contains(&d.pred) || all.contains(&d.succ)).cloned().collect();
    let assigns = p.assignments.iter().filter(|a| all.contains(&a.task)).cloned().collect();
    Some(Command::DeleteTasks { items, deps, assigns })
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::NaiveDate;

    fn proj() -> Project {
        Project::empty("t", NaiveDate::from_ymd_opt(2026, 10, 26).unwrap())
    }
    fn add_task(h: &mut CommandHistory, p: &mut Project, name: &str) -> TaskId {
        let t = p.new_task(name);
        let id = t.id;
        let at = p.tasks.len();
        h.execute(Command::InsertTasks { items: vec![(at, t)], deps: vec![], assigns: vec![] }, p).unwrap();
        id
    }

    #[test]
    fn execute_undo_redo_roundtrip() {
        let mut p = proj();
        let mut h = CommandHistory::default();
        let a = add_task(&mut h, &mut p, "A");
        assert_eq!(p.tasks.len(), 1);
        h.execute(Command::SetTaskName { id: a, old: "A".into(), new: "Alpha".into() }, &mut p).unwrap();
        assert_eq!(p.task(a).unwrap().name, "Alpha");
        h.undo(&mut p).unwrap().unwrap();
        assert_eq!(p.task(a).unwrap().name, "A");
        h.undo(&mut p).unwrap().unwrap();
        assert!(p.tasks.is_empty());
        h.redo(&mut p).unwrap().unwrap();
        assert_eq!(p.tasks.len(), 1);
        assert_eq!(p.tasks[0].id, a, "redo must restore the same id");
        h.redo(&mut p).unwrap().unwrap();
        assert_eq!(p.task(a).unwrap().name, "Alpha");
        assert!(!h.can_redo());
    }

    #[test]
    fn new_command_clears_redo() {
        let mut p = proj();
        let mut h = CommandHistory::default();
        let a = add_task(&mut h, &mut p, "A");
        h.undo(&mut p);
        assert!(h.can_redo());
        let _ = add_task(&mut h, &mut p, "B");
        assert!(!h.can_redo());
        let _ = a;
    }

    #[test]
    fn cycle_is_rolled_back_and_not_recorded() {
        let mut p = proj();
        let mut h = CommandHistory::default();
        let a = add_task(&mut h, &mut p, "A");
        let b = add_task(&mut h, &mut p, "B");
        let ab = Dependency { pred: a, succ: b, kind: DepKind::FS, lag_min: 0 };
        let ba = Dependency { pred: b, succ: a, kind: DepKind::FS, lag_min: 0 };
        h.execute(Command::AddDependency(ab), &mut p).unwrap();
        let depth = h.undo.len();
        let err = h.execute(Command::AddDependency(ba), &mut p).unwrap_err();
        assert!(matches!(err, SchedError::Cycle(_)));
        assert_eq!(p.deps.len(), 1, "invalid link must be rolled back");
        assert_eq!(h.undo.len(), depth, "failed command must not enter history");
    }

    #[test]
    fn delete_and_undo_restores_order_links_and_assignments() {
        let mut p = proj();
        let mut h = CommandHistory::default();
        let a = add_task(&mut h, &mut p, "A");
        let b = add_task(&mut h, &mut p, "B");
        let c = add_task(&mut h, &mut p, "C");
        let rid = p.alloc_resource_id();
        h.execute(Command::InsertResource { index: 0, resource: Resource { id: rid, name: "R".into(), kind: ResourceKind::Work, max_units: 1.0, std_rate: 1.0 } }, &mut p).unwrap();
        h.execute(Command::SetAssignment { task: b, resource: rid, old: None, new: Some(0.5) }, &mut p).unwrap();
        h.execute(Command::AddDependency(Dependency { pred: a, succ: b, kind: DepKind::FS, lag_min: 0 }), &mut p).unwrap();
        h.execute(Command::AddDependency(Dependency { pred: b, succ: c, kind: DepKind::SS, lag_min: 480 }), &mut p).unwrap();
        let cmd = build_delete(&p, &[b]).unwrap();
        h.execute(cmd, &mut p).unwrap();
        assert_eq!(p.tasks.iter().map(|t| t.id).collect::<Vec<_>>(), vec![a, c]);
        assert!(p.deps.is_empty());
        assert!(p.assignments.is_empty());
        h.undo(&mut p).unwrap().unwrap();
        assert_eq!(p.tasks.iter().map(|t| t.id).collect::<Vec<_>>(), vec![a, b, c]);
        assert_eq!(p.deps.len(), 2);
        assert_eq!(p.assignments.len(), 1);
    }

    #[test]
    fn delete_resource_undo_restores_assignments() {
        let mut p = proj();
        let mut h = CommandHistory::default();
        let a = add_task(&mut h, &mut p, "A");
        let rid = p.alloc_resource_id();
        let res = Resource { id: rid, name: "R".into(), kind: ResourceKind::Work, max_units: 1.0, std_rate: 1.0 };
        h.execute(Command::InsertResource { index: 0, resource: res.clone() }, &mut p).unwrap();
        h.execute(Command::SetAssignment { task: a, resource: rid, old: None, new: Some(1.0) }, &mut p).unwrap();
        let assigns = p.assignments.clone();
        h.execute(Command::DeleteResource { index: 0, resource: res, assigns }, &mut p).unwrap();
        assert!(p.resources.is_empty() && p.assignments.is_empty());
        h.undo(&mut p).unwrap().unwrap();
        assert_eq!(p.resources.len(), 1);
        assert_eq!(p.assignments.len(), 1);
    }

    #[test]
    fn batch_undo_is_atomic_and_reversed() {
        let mut p = proj();
        let mut h = CommandHistory::default();
        let a = add_task(&mut h, &mut p, "A");
        let cmd = Command::batch(
            "two edits",
            vec![
                Command::SetTaskName { id: a, old: "A".into(), new: "B".into() },
                Command::SetTaskName { id: a, old: "B".into(), new: "C".into() },
            ],
        );
        h.execute(cmd, &mut p).unwrap();
        assert_eq!(p.task(a).unwrap().name, "C");
        h.undo(&mut p).unwrap().unwrap();
        assert_eq!(p.task(a).unwrap().name, "A");
    }

    #[test]
    fn undo_does_not_clone_project_history_is_small() {
        // history entries hold only the command payloads
        assert!(std::mem::size_of::<Entry>() < 2048);
    }
}
