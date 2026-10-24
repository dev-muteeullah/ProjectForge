//! Domain model. This is the root aggregate that is serialized (serde) and persisted (SQLite).
//!
//! DESIGN NOTES
//! * Tasks live in one `Vec<Task>`; the Vec order defines *sibling* order. Hierarchy is expressed
//!   with `parent`. A task is a "summary" simply when some other task names it as parent - that
//!   is derived, never stored, so it can't go out of sync.
//! * `start`/`finish`/`wbs` on Task are CACHES written by the scheduler (`apply_schedule`).
//!   They are never an input to scheduling.
//! * Durations are integer *working minutes* (1 day = calendar.hours_per_day * 60). Integers keep
//!   the CPM arithmetic exact.

use crate::calendar::Calendar;
use crate::scheduler::Schedule;
use chrono::{Datelike, NaiveDate, Weekday};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};

pub type TaskId = i64;
pub type ResourceId = i64;

/// Bump when the on-disk schema changes (also stored in `PRAGMA user_version`).
pub const FILE_VERSION: u32 = 1;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum DepKind {
    FS,
    SS,
    FF,
    SF,
}
impl DepKind {
    pub const ALL: [DepKind; 4] = [DepKind::FS, DepKind::SS, DepKind::FF, DepKind::SF];
    pub fn as_str(self) -> &'static str {
        match self {
            DepKind::FS => "FS",
            DepKind::SS => "SS",
            DepKind::FF => "FF",
            DepKind::SF => "SF",
        }
    }
    pub fn parse(s: &str) -> Option<DepKind> {
        match s.trim().to_ascii_uppercase().as_str() {
            "FS" => Some(DepKind::FS),
            "SS" => Some(DepKind::SS),
            "FF" => Some(DepKind::FF),
            "SF" => Some(DepKind::SF),
            _ => None,
        }
    }
}

/// A link "pred -> succ". `lag_min` > 0 is lag, < 0 is lead.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Dependency {
    pub pred: TaskId,
    pub succ: TaskId,
    pub kind: DepKind,
    pub lag_min: i64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ConstraintKind {
    Asap,
    MustStartOn,
    MustFinishOn,
    StartNoEarlierThan,
    StartNoLaterThan,
    FinishNoEarlierThan,
    FinishNoLaterThan,
}
impl ConstraintKind {
    pub const ALL: [ConstraintKind; 7] = [
        ConstraintKind::Asap,
        ConstraintKind::MustStartOn,
        ConstraintKind::MustFinishOn,
        ConstraintKind::StartNoEarlierThan,
        ConstraintKind::StartNoLaterThan,
        ConstraintKind::FinishNoEarlierThan,
        ConstraintKind::FinishNoLaterThan,
    ];
    pub fn as_str(self) -> &'static str {
        match self {
            ConstraintKind::Asap => "ASAP",
            ConstraintKind::MustStartOn => "MSO",
            ConstraintKind::MustFinishOn => "MFO",
            ConstraintKind::StartNoEarlierThan => "SNET",
            ConstraintKind::StartNoLaterThan => "SNLT",
            ConstraintKind::FinishNoEarlierThan => "FNET",
            ConstraintKind::FinishNoLaterThan => "FNLT",
        }
    }
    pub fn parse(s: &str) -> Option<ConstraintKind> {
        ConstraintKind::ALL.into_iter().find(|k| k.as_str() == s)
    }
    pub fn label(self) -> &'static str {
        match self {
            ConstraintKind::Asap => "As Soon As Possible",
            ConstraintKind::MustStartOn => "Must Start On",
            ConstraintKind::MustFinishOn => "Must Finish On",
            ConstraintKind::StartNoEarlierThan => "Start No Earlier Than",
            ConstraintKind::StartNoLaterThan => "Start No Later Than",
            ConstraintKind::FinishNoEarlierThan => "Finish No Earlier Than",
            ConstraintKind::FinishNoLaterThan => "Finish No Later Than",
        }
    }
    pub fn needs_date(self) -> bool {
        self != ConstraintKind::Asap
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct Constraint {
    pub kind: ConstraintKind,
    pub date: Option<NaiveDate>,
}
impl Default for Constraint {
    fn default() -> Self {
        Constraint { kind: ConstraintKind::Asap, date: None }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Task {
    pub id: TaskId,
    pub name: String,
    pub description: String,
    pub parent: Option<TaskId>,
    /// Working minutes. Ignored (treated as 0) when `is_milestone`.
    pub duration_min: i64,
    pub is_milestone: bool,
    /// 0.0 ..= 1.0
    pub progress: f64,
    pub constraint: Constraint,
    /// Added by resource leveling: extra working days of delay on top of dependency logic.
    pub leveling_delay_days: u32,
    // ---- caches written by the scheduler ----
    pub start: NaiveDate,
    pub finish: NaiveDate,
    pub wbs: String,
}
impl Task {
    pub fn effective_duration(&self) -> i64 {
        if self.is_milestone {
            0
        } else {
            self.duration_min.max(0)
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ResourceKind {
    Work,
    Material,
    Cost,
}
impl ResourceKind {
    pub const ALL: [ResourceKind; 3] = [ResourceKind::Work, ResourceKind::Material, ResourceKind::Cost];
    pub fn as_str(self) -> &'static str {
        match self {
            ResourceKind::Work => "Work",
            ResourceKind::Material => "Material",
            ResourceKind::Cost => "Cost",
        }
    }
    pub fn parse(s: &str) -> Option<ResourceKind> {
        ResourceKind::ALL.into_iter().find(|k| k.as_str() == s)
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Resource {
    pub id: ResourceId,
    pub name: String,
    pub kind: ResourceKind,
    /// 1.0 == 100% (one full-time person).
    pub max_units: f64,
    /// Cost per working hour (Work), per unit (Material) or fixed (Cost).
    pub std_rate: f64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Assignment {
    pub task: TaskId,
    pub resource: ResourceId,
    /// 0.5 == 50% allocation.
    pub units: f64,
}

/// Snapshot of the schedule, stored in Gantt calendar-day coordinates so it renders with no recompute.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Baseline {
    pub saved_at: String,
    /// task id -> (start_cal, finish_cal)
    pub entries: BTreeMap<TaskId, (f64, f64)>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ProjectMeta {
    pub name: String,
    pub start_date: NaiveDate,
    pub calendar: Calendar,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Project {
    pub version: u32,
    pub name: String,
    pub start_date: NaiveDate,
    pub calendar: Calendar,
    pub tasks: Vec<Task>,
    pub deps: Vec<Dependency>,
    pub resources: Vec<Resource>,
    pub assignments: Vec<Assignment>,
    pub baseline: Option<Baseline>,
    pub next_task_id: TaskId,
    pub next_resource_id: ResourceId,
}

fn next_monday(from: NaiveDate) -> NaiveDate {
    let mut d = from;
    while d.weekday() != Weekday::Mon {
        d = d.succ_opt().unwrap_or(d);
        if d == NaiveDate::MAX {
            break;
        }
    }
    d
}

impl Project {
    pub fn empty(name: &str, start: NaiveDate) -> Project {
        Project {
            version: FILE_VERSION,
            name: name.into(),
            start_date: start,
            calendar: Calendar::default(),
            tasks: vec![],
            deps: vec![],
            resources: vec![],
            assignments: vec![],
            baseline: None,
            next_task_id: 1,
            next_resource_id: 1,
        }
    }

    pub fn meta(&self) -> ProjectMeta {
        ProjectMeta { name: self.name.clone(), start_date: self.start_date, calendar: self.calendar.clone() }
    }
    pub fn set_meta(&mut self, m: &ProjectMeta) {
        self.name = m.name.clone();
        self.start_date = m.start_date;
        self.calendar = m.calendar.clone();
    }

    pub fn alloc_task_id(&mut self) -> TaskId {
        let id = self.next_task_id;
        self.next_task_id += 1;
        id
    }
    pub fn alloc_resource_id(&mut self) -> ResourceId {
        let id = self.next_resource_id;
        self.next_resource_id += 1;
        id
    }

    pub fn new_task(&mut self, name: &str) -> Task {
        let id = self.alloc_task_id();
        Task {
            id,
            name: name.into(),
            description: String::new(),
            parent: None,
            duration_min: self.calendar.minutes_per_day(),
            is_milestone: false,
            progress: 0.0,
            constraint: Constraint::default(),
            leveling_delay_days: 0,
            start: self.start_date,
            finish: self.start_date,
            wbs: String::new(),
        }
    }

    pub fn task(&self, id: TaskId) -> Option<&Task> {
        self.tasks.iter().find(|t| t.id == id)
    }
    pub fn task_mut(&mut self, id: TaskId) -> Option<&mut Task> {
        self.tasks.iter_mut().find(|t| t.id == id)
    }
    pub fn task_pos(&self, id: TaskId) -> Option<usize> {
        self.tasks.iter().position(|t| t.id == id)
    }
    pub fn resource(&self, id: ResourceId) -> Option<&Resource> {
        self.resources.iter().find(|r| r.id == id)
    }
    pub fn resource_by_name(&self, name: &str) -> Option<&Resource> {
        self.resources.iter().find(|r| r.name.eq_ignore_ascii_case(name.trim()))
    }

    /// parent -> ordered children (Vec order). Orphans (dangling parent) are treated as roots.
    pub fn children_map(&self) -> HashMap<Option<TaskId>, Vec<TaskId>> {
        let known: std::collections::HashSet<TaskId> = self.tasks.iter().map(|t| t.id).collect();
        let mut m: HashMap<Option<TaskId>, Vec<TaskId>> = HashMap::new();
        for t in &self.tasks {
            let p = t.parent.filter(|p| known.contains(p));
            m.entry(p).or_default().push(t.id);
        }
        m
    }

    pub fn has_children(&self, id: TaskId) -> bool {
        self.tasks.iter().any(|t| t.parent == Some(id))
    }

    /// `id` plus all descendants.
    pub fn subtree(&self, id: TaskId) -> Vec<TaskId> {
        let cm = self.children_map();
        let mut out = vec![];
        let mut stack = vec![id];
        while let Some(x) = stack.pop() {
            out.push(x);
            if let Some(ch) = cm.get(&Some(x)) {
                for c in ch.iter().rev() {
                    stack.push(*c);
                }
            }
        }
        out
    }

    pub fn assignments_of(&self, task: TaskId) -> impl Iterator<Item = &Assignment> {
        self.assignments.iter().filter(move |a| a.task == task)
    }

    pub fn resource_names(&self, task: TaskId) -> String {
        let mut parts = vec![];
        for a in self.assignments_of(task) {
            if let Some(r) = self.resource(a.resource) {
                if (a.units - 1.0).abs() < 1e-9 {
                    parts.push(r.name.clone());
                } else {
                    parts.push(format!("{}[{}%]", r.name, (a.units * 100.0).round() as i64));
                }
            }
        }
        parts.join(", ")
    }

    /// WBS codes ("1", "1.2", "1.2.3") derived from the tree + sibling order.
    pub fn wbs_codes(&self) -> HashMap<TaskId, String> {
        let cm = self.children_map();
        let mut out = HashMap::new();
        let mut stack: Vec<(TaskId, String)> = vec![];
        if let Some(roots) = cm.get(&None) {
            for (i, r) in roots.iter().enumerate().rev() {
                stack.push((*r, format!("{}", i + 1)));
            }
        }
        while let Some((id, code)) = stack.pop() {
            if let Some(ch) = cm.get(&Some(id)) {
                for (i, c) in ch.iter().enumerate().rev() {
                    stack.push((*c, format!("{}.{}", code, i + 1)));
                }
            }
            out.insert(id, code);
        }
        out
    }

    /// Write scheduler output back into the cached fields.
    pub fn apply_schedule(&mut self, s: &Schedule) {
        let wbs = self.wbs_codes();
        for t in &mut self.tasks {
            if let Some(ts) = s.tasks.get(&t.id) {
                t.start = ts.start;
                t.finish = ts.finish;
            }
            if let Some(w) = wbs.get(&t.id) {
                t.wbs = w.clone();
            }
        }
    }

    /// Remove baseline entries whose tasks no longer exist (called before persisting).
    pub fn pruned_baseline(&self) -> Option<Baseline> {
        self.baseline.as_ref().map(|b| Baseline {
            saved_at: b.saved_at.clone(),
            entries: b
                .entries
                .iter()
                .filter(|(id, _)| self.tasks.iter().any(|t| t.id == **id))
                .map(|(k, v)| (*k, *v))
                .collect(),
        })
    }

    // ------------------------------------------------------------------ sample data

    pub fn sample() -> Project {
        let today = chrono::Local::now().date_naive();
        let mut p = Project::empty("Website Relaunch (sample)", next_monday(today));
        let mpd = p.calendar.minutes_per_day();

        let alice = p.alloc_resource_id();
        let bob = p.alloc_resource_id();
        let carol = p.alloc_resource_id();
        p.resources.push(Resource { id: alice, name: "Alice".into(), kind: ResourceKind::Work, max_units: 1.0, std_rate: 60.0 });
        p.resources.push(Resource { id: bob, name: "Bob".into(), kind: ResourceKind::Work, max_units: 1.0, std_rate: 50.0 });
        p.resources.push(Resource { id: carol, name: "Carol".into(), kind: ResourceKind::Work, max_units: 1.0, std_rate: 45.0 });

        // (name, parent index, days, milestone)
        let spec: &[(&str, Option<usize>, i64, bool)] = &[
            /*0*/ ("Planning", None, 0, false),
            /*1*/ ("Kick-off & scope", Some(0), 2, false),
            /*2*/ ("Requirements", Some(0), 5, false),
            /*3*/ ("Plan approved", Some(0), 0, true),
            /*4*/ ("Design", None, 0, false),
            /*5*/ ("Wireframes", Some(4), 4, false),
            /*6*/ ("Visual design", Some(4), 6, false),
            /*7*/ ("Design sign-off", Some(4), 0, true),
            /*8*/ ("Build", None, 0, false),
            /*9*/ ("Backend API", Some(8), 10, false),
            /*10*/ ("Frontend pages", Some(8), 8, false),
            /*11*/ ("Content migration", Some(8), 5, false),
            /*12*/ ("Integration", Some(8), 3, false),
            /*13*/ ("Launch", None, 0, false),
            /*14*/ ("QA & fixes", Some(13), 5, false),
            /*15*/ ("Go live", Some(13), 0, true),
        ];
        let mut ids = vec![];
        for (name, parent, days, ms) in spec {
            let mut t = p.new_task(name);
            t.parent = parent.map(|i| ids[i] as TaskId);
            t.duration_min = days * mpd;
            t.is_milestone = *ms;
            ids.push(t.id);
            p.tasks.push(t);
        }
        let link = |p: &mut Project, a: usize, b: usize, kind: DepKind, lag_days: i64| {
            p.deps.push(Dependency { pred: ids[a], succ: ids[b], kind, lag_min: lag_days * mpd });
        };
        link(&mut p, 1, 2, DepKind::FS, 0);
        link(&mut p, 2, 3, DepKind::FS, 0);
        link(&mut p, 3, 5, DepKind::FS, 0);
        link(&mut p, 5, 6, DepKind::SS, 2);
        link(&mut p, 6, 7, DepKind::FS, 0);
        link(&mut p, 7, 9, DepKind::FS, 0);
        link(&mut p, 7, 10, DepKind::FS, 0);
        link(&mut p, 7, 11, DepKind::FS, 1);
        link(&mut p, 9, 12, DepKind::FS, 0);
        link(&mut p, 10, 12, DepKind::FF, 0);
        link(&mut p, 12, 14, DepKind::FS, 0);
        link(&mut p, 11, 14, DepKind::FS, 0);
        link(&mut p, 14, 15, DepKind::FS, 0);

        let assign = |p: &mut Project, t: usize, r: ResourceId, u: f64| p.assignments.push(Assignment { task: ids[t], resource: r, units: u });
        assign(&mut p, 1, alice, 1.0);
        assign(&mut p, 2, alice, 1.0);
        assign(&mut p, 5, bob, 1.0);
        assign(&mut p, 6, bob, 1.0);
        assign(&mut p, 9, alice, 1.0);
        assign(&mut p, 10, bob, 1.0);
        assign(&mut p, 11, carol, 0.5);
        assign(&mut p, 12, alice, 0.5);
        assign(&mut p, 12, bob, 0.5);
        assign(&mut p, 14, carol, 1.0);
        if let Some(t) = p.task_mut(ids[1]) {
            t.progress = 1.0;
        }
        if let Some(t) = p.task_mut(ids[2]) {
            t.progress = 0.4;
        }
        p.next_task_id = ids.len() as i64 + 1;
        p
    }

    /// Deterministic large project for the performance requirement (n tasks, ~n/2 dependencies).
    pub fn stress(n: usize) -> Project {
        let mut p = Project::empty(&format!("Stress test ({} tasks)", n), next_monday(chrono::Local::now().date_naive()));
        let mpd = p.calendar.minutes_per_day();
        let mut seed: u64 = 0x9E3779B97F4A7C15;
        let mut rnd = move || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        };
        let phases = (n / 50).max(1);
        let mut ids: Vec<TaskId> = vec![];
        for ph in 0..phases {
            let mut s = p.new_task(&format!("Phase {}", ph + 1));
            s.duration_min = 0;
            let sid = s.id;
            p.tasks.push(s);
            for k in 0..(n / phases) {
                let mut t = p.new_task(&format!("Task {}.{}", ph + 1, k + 1));
                t.parent = Some(sid);
                t.duration_min = (1 + (rnd() % 8) as i64) * mpd;
                ids.push(t.id);
                p.tasks.push(t);
            }
        }
        let leaves = ids.len();
        for i in 1..leaves {
            if i % 2 == 0 {
                let j = (rnd() as usize) % i; // pred always earlier => acyclic
                p.deps.push(Dependency { pred: ids[j], succ: ids[i], kind: DepKind::FS, lag_min: 0 });
            }
        }
        p
    }
}
