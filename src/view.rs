//! View-model: which rows the sheet and the Gantt show (outline, filters, grouping).
//! Sheet and Gantt share ONE `Vec<Row>` so row N in the table is always row N in the chart.

use crate::model::*;
use crate::scheduler::Schedule;
use std::collections::{HashMap, HashSet};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum StatusFilter {
    All,
    Incomplete,
    NotStarted,
    InProgress,
    Complete,
    Critical,
}
impl StatusFilter {
    pub const ALL: [StatusFilter; 6] = [
        StatusFilter::All,
        StatusFilter::Incomplete,
        StatusFilter::NotStarted,
        StatusFilter::InProgress,
        StatusFilter::Complete,
        StatusFilter::Critical,
    ];
    pub fn label(self) -> &'static str {
        match self {
            StatusFilter::All => "All tasks",
            StatusFilter::Incomplete => "Incomplete tasks",
            StatusFilter::NotStarted => "Not started",
            StatusFilter::InProgress => "In progress",
            StatusFilter::Complete => "Completed",
            StatusFilter::Critical => "Critical tasks",
        }
    }
}

#[derive(Clone, Debug)]
pub struct ViewOptions {
    pub status: StatusFilter,
    pub resource: Option<ResourceId>,
    pub group_by_resource: bool,
    pub collapsed: HashSet<TaskId>,
}
impl Default for ViewOptions {
    fn default() -> Self {
        ViewOptions { status: StatusFilter::All, resource: None, group_by_resource: false, collapsed: HashSet::new() }
    }
}
impl ViewOptions {
    pub fn filtering(&self) -> bool {
        self.status != StatusFilter::All || self.resource.is_some()
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum RowKind {
    Task,
    Group,
}

#[derive(Clone, Debug)]
pub struct Row {
    pub kind: RowKind,
    pub task: Option<TaskId>,
    pub level: usize,
    pub has_children: bool,
    pub label: String,
}

fn task_matches(p: &Project, s: &Schedule, t: &Task, o: &ViewOptions) -> bool {
    let ts = s.tasks.get(&t.id);
    let prog = ts.map(|x| x.progress).unwrap_or(t.progress);
    let ok_status = match o.status {
        StatusFilter::All => true,
        StatusFilter::Incomplete => prog < 0.9999,
        StatusFilter::NotStarted => prog <= 0.0,
        StatusFilter::InProgress => prog > 0.0 && prog < 0.9999,
        StatusFilter::Complete => prog >= 0.9999,
        StatusFilter::Critical => ts.map_or(false, |x| x.critical),
    };
    let ok_res = match o.resource {
        None => true,
        Some(r) => p.assignments.iter().any(|a| a.task == t.id && a.resource == r),
    };
    ok_status && ok_res
}

pub fn build_rows(p: &Project, s: &Schedule, o: &ViewOptions) -> Vec<Row> {
    let cm = p.children_map();
    let by_id: HashMap<TaskId, &Task> = p.tasks.iter().map(|t| (t.id, t)).collect();

    if o.group_by_resource {
        return build_grouped(p, s, o);
    }

    // visible = tasks matching the filter + all their ancestors (keeps the outline intact)
    let visible: Option<HashSet<TaskId>> = if o.filtering() {
        let mut set = HashSet::new();
        for t in &p.tasks {
            let leaf = !cm.contains_key(&Some(t.id));
            if leaf && task_matches(p, s, t, o) {
                let mut cur = Some(t.id);
                let mut guard = 0;
                while let Some(id) = cur {
                    if !set.insert(id) || guard > 10_000 {
                        break;
                    }
                    guard += 1;
                    cur = by_id.get(&id).and_then(|x| x.parent);
                }
            }
        }
        Some(set)
    } else {
        None
    };

    let mut rows = Vec::with_capacity(p.tasks.len());
    let mut stack: Vec<(TaskId, usize)> = vec![];
    if let Some(roots) = cm.get(&None) {
        for r in roots.iter().rev() {
            stack.push((*r, 0));
        }
    }
    while let Some((id, level)) = stack.pop() {
        if let Some(v) = &visible {
            if !v.contains(&id) {
                continue;
            }
        }
        let kids = cm.get(&Some(id));
        let has_children = kids.map_or(false, |k| !k.is_empty());
        rows.push(Row { kind: RowKind::Task, task: Some(id), level, has_children, label: String::new() });
        if has_children && !o.collapsed.contains(&id) {
            if let Some(k) = kids {
                for c in k.iter().rev() {
                    stack.push((*c, level + 1));
                }
            }
        }
    }
    rows
}

/// Grouping by resource REORDERS the list: one header row per resource, then its tasks (flat).
fn build_grouped(p: &Project, s: &Schedule, o: &ViewOptions) -> Vec<Row> {
    let cm = p.children_map();
    let is_leaf = |id: TaskId| !cm.contains_key(&Some(id));
    let mut rows = vec![];
    let mut assigned: HashSet<TaskId> = HashSet::new();
    for r in &p.resources {
        if o.resource.map_or(false, |x| x != r.id) {
            continue;
        }
        let tasks: Vec<&Task> = p
            .tasks
            .iter()
            .filter(|t| is_leaf(t.id) && p.assignments.iter().any(|a| a.task == t.id && a.resource == r.id) && task_matches(p, s, t, o))
            .collect();
        if tasks.is_empty() {
            continue;
        }
        rows.push(Row { kind: RowKind::Group, task: None, level: 0, has_children: true, label: format!("{}  ({})", r.name, tasks.len()) });
        for t in tasks {
            assigned.insert(t.id);
            rows.push(Row { kind: RowKind::Task, task: Some(t.id), level: 1, has_children: false, label: String::new() });
        }
    }
    if o.resource.is_none() {
        let un: Vec<&Task> = p
            .tasks
            .iter()
            .filter(|t| is_leaf(t.id) && !p.assignments.iter().any(|a| a.task == t.id) && task_matches(p, s, t, o))
            .collect();
        if !un.is_empty() {
            rows.push(Row { kind: RowKind::Group, task: None, level: 0, has_children: true, label: format!("(Unassigned)  ({})", un.len()) });
            for t in un {
                rows.push(Row { kind: RowKind::Task, task: Some(t.id), level: 1, has_children: false, label: String::new() });
            }
        }
    }
    rows
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scheduler::schedule;

    #[test]
    fn outline_collapse_hides_children() {
        let p = Project::sample();
        let s = schedule(&p).unwrap();
        let all = build_rows(&p, &s, &ViewOptions::default());
        assert_eq!(all.len(), p.tasks.len());
        let mut o = ViewOptions::default();
        o.collapsed.insert(p.tasks[0].id);
        let rows = build_rows(&p, &s, &o);
        assert_eq!(rows.len(), p.tasks.len() - 3); // "Planning" has 3 children
    }

    #[test]
    fn status_filter_keeps_ancestors() {
        let p = Project::sample();
        let s = schedule(&p).unwrap();
        let o = ViewOptions { status: StatusFilter::Complete, ..Default::default() };
        let rows = build_rows(&p, &s, &o);
        // only "Kick-off & scope" is complete + its parent "Planning"
        assert_eq!(rows.len(), 2);
    }

    #[test]
    fn resource_filter_and_grouping() {
        let p = Project::sample();
        let s = schedule(&p).unwrap();
        let alice = p.resources[0].id;
        let o = ViewOptions { resource: Some(alice), ..Default::default() };
        let rows = build_rows(&p, &s, &o);
        assert!(rows.iter().filter_map(|r| r.task).all(|id| {
            p.assignments.iter().any(|a| a.task == id && a.resource == alice) || p.has_children(id)
        }));
        let g = ViewOptions { group_by_resource: true, ..Default::default() };
        let rows = build_rows(&p, &s, &g);
        assert!(rows.iter().any(|r| r.kind == RowKind::Group));
        // first row is a group header; grouping reorders the list
        assert_eq!(rows[0].kind, RowKind::Group);
    }
}
