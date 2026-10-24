//! The scheduling engine ("the brain").
//!
//! `schedule(&Project) -> Result<Schedule, SchedError>` is a PURE function: it never mutates the
//! project and knows nothing about the UI. The UI is just a view of its output.
//!
//! ALGORITHM (all arithmetic is on the integer working-minute axis, see calendar.rs)
//!   1. validate links, hierarchy; topologically sort leaf tasks (Kahn). A leftover set means a
//!      cycle -> `SchedError::Cycle(path)`; callers must refuse to keep/save that state.
//!   2. forward pass:  ES = max(requirement of every incoming link, project start) + leveling delay,
//!      then constraints are applied. A MUST constraint overrides the links ("constraint wins").
//!   3. backward pass: LF = min(bound of every outgoing link, project finish), constraints tighten it.
//!   4. total float = LS - ES, free float = min slack to successors, critical <=> total float <= 0.
//!   5. any link not satisfied by the FINAL dates is reported in `violated` (drawn red in the UI).
//!   6. summary tasks roll up their children (min start / max finish / weighted progress).
//!
//! Performance: the whole thing is O(tasks + links) with a handful of Vec passes; a 1,000-task /
//! 500-link project reschedules in well under a millisecond in release builds, so a full
//! recompute after each command is cheaper than maintaining an incremental structure.

use crate::calendar::{cal_of, Timeline};
use crate::model::*;
use chrono::NaiveDate;
use std::collections::{HashMap, HashSet, VecDeque};
use std::fmt;

#[derive(Debug, Clone, PartialEq)]
pub enum SchedError {
    /// Closed path of task ids, first == last: A -> B -> A
    Cycle(Vec<TaskId>),
    UnknownTask(TaskId),
    SelfLink(TaskId),
    SummaryLink(TaskId),
    BadHierarchy,
    BadCalendar,
}

impl SchedError {
    /// Human message; `name` resolves ids to task names.
    pub fn describe(&self, name: &dyn Fn(TaskId) -> String) -> String {
        match self {
            SchedError::Cycle(path) => {
                let p: Vec<String> = path.iter().map(|id| format!("{} (#{})", name(*id), id)).collect();
                format!("Circular dependency: {}", p.join(" \u{2192} "))
            }
            SchedError::UnknownTask(id) => format!("A link refers to unknown task #{}", id),
            SchedError::SelfLink(id) => format!("Task '{}' cannot depend on itself", name(*id)),
            SchedError::SummaryLink(id) => format!("'{}' is a summary task: link its sub-tasks instead", name(*id)),
            SchedError::BadHierarchy => "The task hierarchy contains a loop".into(),
            SchedError::BadCalendar => "The calendar has no working time".into(),
        }
    }
}
impl fmt::Display for SchedError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.describe(&|id| format!("task {}", id)))
    }
}
impl std::error::Error for SchedError {}

#[derive(Debug, Clone)]
pub struct TaskSched {
    pub start_pos: i64,
    pub finish_pos: i64,
    pub start_cal: f64,
    pub finish_cal: f64,
    pub start: NaiveDate,
    pub finish: NaiveDate,
    pub late_start: i64,
    pub late_finish: i64,
    pub total_float: i64,
    pub free_float: i64,
    pub critical: bool,
    pub is_summary: bool,
    pub progress: f64,
    /// A "no later than" constraint that the dates could not honour.
    pub constraint_violated: bool,
    pub duration: i64,
}

#[derive(Debug, Clone, Default)]
pub struct Schedule {
    pub tasks: HashMap<TaskId, TaskSched>,
    /// (pred, succ) links that the final dates do not satisfy.
    pub violated: HashSet<(TaskId, TaskId)>,
    /// Zero-slack links between two critical tasks.
    pub critical_links: HashSet<(TaskId, TaskId)>,
    pub mpd: i64,
    pub project_start_cal: f64,
    pub project_finish_cal: f64,
    pub project_finish_pos: i64,
}

fn link_requirement(kind: DepKind, lag: i64, pred_es: i64, pred_ef: i64, dur: i64) -> i64 {
    // minimum ES of the successor implied by this link
    match kind {
        DepKind::FS => pred_ef + lag,
        DepKind::SS => pred_es + lag,
        DepKind::FF => pred_ef + lag - dur,
        DepKind::SF => pred_es + lag - dur,
    }
}

fn link_slack(kind: DepKind, lag: i64, pred_es: i64, pred_ef: i64, succ_es: i64, succ_ef: i64) -> i64 {
    match kind {
        DepKind::FS => succ_es - (pred_ef + lag),
        DepKind::SS => succ_es - (pred_es + lag),
        DepKind::FF => succ_ef - (pred_ef + lag),
        DepKind::SF => succ_ef - (pred_es + lag),
    }
}

fn find_cycle(preds: &[Vec<(usize, DepKind, i64)>], remaining: &[bool], ids: &[TaskId]) -> Vec<TaskId> {
    // Every node left after Kahn's algorithm has at least one predecessor that is also left, so
    // walking predecessors must eventually revisit a node: that closes a cycle.
    let start = match remaining.iter().position(|r| *r) {
        Some(s) => s,
        None => return vec![],
    };
    let mut seen_at: HashMap<usize, usize> = HashMap::new();
    let mut path = vec![];
    let mut cur = start;
    loop {
        if let Some(&at) = seen_at.get(&cur) {
            let mut cyc: Vec<usize> = path[at..].to_vec(); // pred-direction
            cyc.reverse(); // succ-direction
            let mut out: Vec<TaskId> = cyc.iter().map(|i| ids[*i]).collect();
            if let Some(first) = out.first().copied() {
                out.push(first);
            }
            return out;
        }
        seen_at.insert(cur, path.len());
        path.push(cur);
        match preds[cur].iter().find(|(p, _, _)| remaining[*p]) {
            Some((p, _, _)) => cur = *p,
            None => return vec![ids[cur]],
        }
    }
}

pub fn schedule(p: &Project) -> Result<Schedule, SchedError> {
    if !p.calendar.is_valid() {
        return Err(SchedError::BadCalendar);
    }
    let mpd = p.calendar.minutes_per_day();
    let n = p.tasks.len();
    let ids: Vec<TaskId> = p.tasks.iter().map(|t| t.id).collect();
    let idx: HashMap<TaskId, usize> = ids.iter().enumerate().map(|(i, id)| (*id, i)).collect();

    // ---- hierarchy ----
    let mut parent_ix: Vec<Option<usize>> = vec![None; n];
    let mut children: Vec<Vec<usize>> = vec![vec![]; n];
    for (i, t) in p.tasks.iter().enumerate() {
        if let Some(pi) = t.parent.and_then(|pid| idx.get(&pid).copied()) {
            parent_ix[i] = Some(pi);
            children[pi].push(i);
        }
    }
    let is_summary: Vec<bool> = children.iter().map(|c| !c.is_empty()).collect();

    // ---- links ----
    let mut preds: Vec<Vec<(usize, DepKind, i64)>> = vec![vec![]; n];
    let mut succs: Vec<Vec<(usize, DepKind, i64)>> = vec![vec![]; n];
    for d in &p.deps {
        let (a, b) = match (idx.get(&d.pred), idx.get(&d.succ)) {
            (Some(a), Some(b)) => (*a, *b),
            (None, _) => return Err(SchedError::UnknownTask(d.pred)),
            (_, None) => return Err(SchedError::UnknownTask(d.succ)),
        };
        if a == b {
            return Err(SchedError::SelfLink(d.pred));
        }
        if is_summary[a] {
            return Err(SchedError::SummaryLink(d.pred));
        }
        if is_summary[b] {
            return Err(SchedError::SummaryLink(d.succ));
        }
        preds[b].push((a, d.kind, d.lag_min));
        succs[a].push((b, d.kind, d.lag_min));
    }

    // ---- topological order (Kahn) ----
    let mut indeg: Vec<usize> = preds.iter().map(|v| v.len()).collect();
    let mut queue: VecDeque<usize> = (0..n).filter(|i| indeg[*i] == 0).collect();
    let mut order = Vec::with_capacity(n);
    while let Some(i) = queue.pop_front() {
        order.push(i);
        for (s, _, _) in &succs[i] {
            indeg[*s] -= 1;
            if indeg[*s] == 0 {
                queue.push_back(*s);
            }
        }
    }
    if order.len() < n {
        let remaining: Vec<bool> = (0..n).map(|i| indeg[i] > 0).collect();
        return Err(SchedError::Cycle(find_cycle(&preds, &remaining, &ids)));
    }

    // hierarchy loop check: every task must be reachable from a root
    {
        let mut visited = 0usize;
        let mut stack: Vec<usize> = (0..n).filter(|i| parent_ix[*i].is_none()).collect();
        while let Some(i) = stack.pop() {
            visited += 1;
            stack.extend(children[i].iter().copied());
        }
        if visited != n {
            return Err(SchedError::BadHierarchy);
        }
    }

    // ---- timeline ----
    let mut origin = p.start_date;
    for t in &p.tasks {
        if let Some(d) = t.constraint.date {
            if d < origin {
                origin = d;
            }
        }
    }
    let mut tl = Timeline::new(&p.calendar, origin);
    let base = tl.day_index(p.start_date) * mpd;

    // constraint positions: (start-of-day pos, end-of-day pos)
    let cpos: Vec<Option<(i64, i64)>> = p
        .tasks
        .iter()
        .map(|t| {
            if t.constraint.kind == ConstraintKind::Asap {
                return None;
            }
            t.constraint.date.map(|d| {
                let di = tl.day_index(d);
                (di * mpd, (di + 1) * mpd)
            })
        })
        .collect();

    let mut dur: Vec<i64> = p.tasks.iter().map(|t| t.effective_duration()).collect();
    let mut es = vec![0i64; n];
    let mut ef = vec![0i64; n];

    // ---- forward pass ----
    for &i in &order {
        if is_summary[i] {
            continue;
        }
        let t = &p.tasks[i];
        let d = dur[i];
        let mut s = base;
        for &(pi, kind, lag) in &preds[i] {
            s = s.max(link_requirement(kind, lag, es[pi], ef[pi], d));
        }
        s += t.leveling_delay_days as i64 * mpd;
        if let Some((c_start, c_end)) = cpos[i] {
            let fin = if d > 0 { c_end } else { c_start };
            match t.constraint.kind {
                ConstraintKind::StartNoEarlierThan => s = s.max(c_start),
                ConstraintKind::MustStartOn => s = c_start, // constraint wins over links
                ConstraintKind::MustFinishOn => s = fin - d,
                ConstraintKind::FinishNoEarlierThan => s = s.max(fin - d),
                _ => {}
            }
        }
        s = s.max(0);
        es[i] = s;
        ef[i] = s + d;
    }

    let mut project_finish = base;
    for i in 0..n {
        if !is_summary[i] {
            project_finish = project_finish.max(ef[i]);
        }
    }

    // ---- backward pass ----
    let mut lf = vec![project_finish; n];
    let mut ls = vec![0i64; n];
    for &i in order.iter().rev() {
        if is_summary[i] {
            continue;
        }
        let d = dur[i];
        let mut f = project_finish;
        for &(si, kind, lag) in &succs[i] {
            let bound = match kind {
                DepKind::FS => ls[si] - lag,
                DepKind::SS => ls[si] - lag + d,
                DepKind::FF => lf[si] - lag,
                DepKind::SF => lf[si] - lag + d,
            };
            f = f.min(bound);
        }
        if let Some((c_start, c_end)) = cpos[i] {
            let fin = if d > 0 { c_end } else { c_start };
            match p.tasks[i].constraint.kind {
                ConstraintKind::MustStartOn | ConstraintKind::MustFinishOn => f = f.min(ef[i]),
                ConstraintKind::FinishNoLaterThan => f = f.min(fin),
                ConstraintKind::StartNoLaterThan => f = f.min(c_start + d),
                _ => {}
            }
        }
        lf[i] = f;
        ls[i] = f - d;
    }

    // ---- floats / critical ----
    let mut total_float = vec![0i64; n];
    let mut free_float = vec![0i64; n];
    let mut critical = vec![false; n];
    for i in 0..n {
        if is_summary[i] {
            continue;
        }
        total_float[i] = ls[i] - es[i];
        let mut ff = project_finish - ef[i];
        for &(si, kind, lag) in &succs[i] {
            ff = ff.min(link_slack(kind, lag, es[i], ef[i], es[si], ef[si]));
        }
        free_float[i] = ff.max(0);
        critical[i] = total_float[i] <= 0;
    }

    // ---- violated links / critical links ----
    let mut violated = HashSet::new();
    let mut critical_links = HashSet::new();
    for dep in &p.deps {
        let (a, b) = (idx[&dep.pred], idx[&dep.succ]);
        let slack = link_slack(dep.kind, dep.lag_min, es[a], ef[a], es[b], ef[b]);
        if slack < 0 {
            violated.insert((dep.pred, dep.succ));
        } else if slack == 0 && critical[a] && critical[b] {
            critical_links.insert((dep.pred, dep.succ));
        }
    }

    // ---- summary roll-up (post-order over the hierarchy) ----
    let mut prog = vec![0.0f64; n];
    let mut weight = vec![0i64; n];
    for i in 0..n {
        if !is_summary[i] {
            prog[i] = p.tasks[i].progress.clamp(0.0, 1.0);
            weight[i] = dur[i];
        }
    }
    // iterative post-order
    let mut post = Vec::with_capacity(n);
    {
        let mut stack: Vec<(usize, bool)> = (0..n).filter(|i| parent_ix[*i].is_none()).map(|i| (i, false)).collect();
        while let Some((i, done)) = stack.pop() {
            if done {
                post.push(i);
            } else {
                stack.push((i, true));
                for c in &children[i] {
                    stack.push((*c, false));
                }
            }
        }
    }
    let mut min_ls = vec![i64::MAX; n];
    let mut max_lf = vec![i64::MIN; n];
    let mut min_tf = vec![i64::MAX; n];
    for &i in &post {
        if is_summary[i] {
            let (mut s, mut f) = (i64::MAX, i64::MIN);
            let (mut w, mut wp) = (0i64, 0.0f64);
            let mut avg = 0.0;
            let mut crit = false;
            let (mut mls, mut mlf, mut mtf) = (i64::MAX, i64::MIN, i64::MAX);
            for &c in &children[i] {
                s = s.min(es[c]);
                f = f.max(ef[c]);
                w += weight[c];
                wp += weight[c] as f64 * prog[c];
                avg += prog[c];
                crit |= critical[c];
                let (cls, clf, ctf) = if is_summary[c] { (min_ls[c], max_lf[c], min_tf[c]) } else { (ls[c], lf[c], total_float[c]) };
                mls = mls.min(cls);
                mlf = mlf.max(clf);
                mtf = mtf.min(ctf);
            }
            es[i] = s;
            ef[i] = f;
            dur[i] = f - s;
            weight[i] = w;
            prog[i] = if w > 0 { wp / w as f64 } else { avg / children[i].len().max(1) as f64 };
            critical[i] = crit;
            min_ls[i] = mls;
            max_lf[i] = mlf;
            min_tf[i] = mtf;
            ls[i] = mls;
            lf[i] = mlf;
            total_float[i] = mtf;
        }
    }

    // ---- positions -> dates / Gantt coordinates ----
    let mut out = Schedule { mpd, project_finish_pos: project_finish, ..Default::default() };
    let mut pmin = f64::MAX;
    let mut pmax = f64::MIN;
    for i in 0..n {
        let t = &p.tasks[i];
        let zero = ef[i] == es[i];
        let (sd, sc) = pos_start(&mut tl, es[i], mpd);
        let (fd, fc) = if zero { (sd, sc) } else { pos_end(&mut tl, ef[i], mpd) };
        pmin = pmin.min(sc);
        pmax = pmax.max(fc);
        let constraint_violated = match (cpos[i], t.constraint.kind) {
            (Some((c_start, _)), ConstraintKind::StartNoLaterThan) => es[i] > c_start,
            (Some((_, c_end)), ConstraintKind::FinishNoLaterThan) => ef[i] > if dur[i] > 0 { c_end } else { c_end - mpd },
            _ => false,
        };
        out.tasks.insert(
            t.id,
            TaskSched {
                start_pos: es[i],
                finish_pos: ef[i],
                start_cal: sc,
                finish_cal: fc,
                start: sd,
                finish: fd,
                late_start: ls[i],
                late_finish: lf[i],
                total_float: total_float[i],
                free_float: free_float[i],
                critical: critical[i],
                is_summary: is_summary[i],
                progress: prog[i],
                constraint_violated,
                duration: dur[i],
            },
        );
    }
    out.violated = violated;
    out.critical_links = critical_links;
    if n == 0 {
        pmin = cal_of(p.start_date);
        pmax = pmin + 1.0;
    }
    out.project_start_cal = pmin;
    out.project_finish_cal = pmax;
    Ok(out)
}

/// Start position -> (date, calendar-day coordinate).
fn pos_start(tl: &mut Timeline, pos: i64, mpd: i64) -> (NaiveDate, f64) {
    let d = pos.div_euclid(mpd);
    let o = pos.rem_euclid(mpd);
    let date = tl.date_of(d);
    (date, cal_of(date) + o as f64 / mpd as f64)
}

/// Finish position -> (date, coordinate). A finish exactly on a day boundary belongs to the
/// END of the previous working day ("1 day starting Friday finishes Friday, not Saturday").
fn pos_end(tl: &mut Timeline, pos: i64, mpd: i64) -> (NaiveDate, f64) {
    let d = pos.div_euclid(mpd);
    let o = pos.rem_euclid(mpd);
    if o == 0 && d > 0 {
        let date = tl.date_of(d - 1);
        (date, cal_of(date) + 1.0)
    } else {
        let date = tl.date_of(d);
        (date, cal_of(date) + o as f64 / mpd as f64)
    }
}

// ======================================================================================
// Resource leveling
// ======================================================================================

#[derive(Debug, Clone, Default)]
pub struct LevelResult {
    /// task id -> days of leveling delay (only non-zero entries)
    pub delays: std::collections::BTreeMap<TaskId, u32>,
    /// (resource, working-day index) still overallocated after leveling (no slack left to use)
    pub unresolved: Vec<(ResourceId, i64)>,
    pub iterations: usize,
}

/// Basic leveling: repeatedly find the first overallocated (resource, day) and push back ONE
/// non-critical task active on that day by one working day. Only tasks with at least a day of
/// total float are moved, so the project finish date never slips. Critical tasks are never moved.
pub fn level(project: &Project) -> Result<LevelResult, SchedError> {
    let mut p = project.clone();
    for t in &mut p.tasks {
        t.leveling_delay_days = 0;
    }
    let work: HashMap<ResourceId, f64> = p.resources.iter().filter(|r| r.kind == ResourceKind::Work).map(|r| (r.id, r.max_units)).collect();
    let mut skip: HashSet<(ResourceId, i64)> = HashSet::new();
    let mut result = LevelResult::default();
    let limit = 200 + p.tasks.len() * 20;

    for iter in 0..limit {
        result.iterations = iter;
        let s = schedule(&p)?;
        let mpd = s.mpd;
        // demand[(resource)] -> map day -> (units, tasks)
        let mut demand: HashMap<(ResourceId, i64), (f64, Vec<TaskId>)> = HashMap::new();
        for a in &p.assignments {
            if !work.contains_key(&a.resource) {
                continue;
            }
            if let Some(ts) = s.tasks.get(&a.task) {
                if ts.is_summary || ts.duration == 0 {
                    continue;
                }
                let first = ts.start_pos.div_euclid(mpd);
                let last = (ts.finish_pos - 1).div_euclid(mpd);
                for day in first..=last {
                    let e = demand.entry((a.resource, day)).or_insert((0.0, vec![]));
                    e.0 += a.units;
                    e.1.push(a.task);
                }
            }
        }
        let mut over: Vec<(i64, ResourceId)> = demand
            .iter()
            .filter(|((r, day), (u, _))| *u > work[r] + 1e-9 && !skip.contains(&(*r, *day)))
            .map(|((r, day), _)| (*day, *r))
            .collect();
        if over.is_empty() {
            break;
        }
        over.sort();
        let (day, res) = over[0];
        let tasks = &demand[&(res, day)].1;
        let mut best: Option<(i64, i64, TaskId)> = None; // (start, float, id): delay the LATEST-starting task, ties -> most float
        for tid in tasks {
            let ts = &s.tasks[tid];
            let task = match p.task(*tid) {
                Some(t) => t,
                None => continue,
            };
            let pinned = matches!(task.constraint.kind, ConstraintKind::MustStartOn | ConstraintKind::MustFinishOn);
            if pinned || ts.critical || ts.total_float < mpd {
                continue;
            }
            let key = (ts.start_pos, ts.total_float, *tid);
            if best.map_or(true, |b| key > b) {
                best = Some(key);
            }
        }
        match best {
            Some((_, _, tid)) => {
                if let Some(t) = p.task_mut(tid) {
                    t.leveling_delay_days += 1;
                }
            }
            None => {
                skip.insert((res, day));
                result.unresolved.push((res, day));
            }
        }
    }
    for t in &p.tasks {
        if t.leveling_delay_days > 0 {
            result.delays.insert(t.id, t.leveling_delay_days);
        }
    }
    Ok(result)
}

/// Daily demand for one resource, as (calendar-day coordinate, units) for each working day in
/// [first_day, last_day] (working-day indices relative to the schedule's timeline).
/// Used by the workload histogram. Returns (date, demand) pairs.
pub fn resource_histogram(p: &Project, s: &Schedule, resource: ResourceId) -> Vec<(NaiveDate, f64)> {
    let mpd = s.mpd;
    let mut per_day: std::collections::BTreeMap<NaiveDate, f64> = Default::default();
    let cal = &p.calendar;
    for a in p.assignments.iter().filter(|a| a.resource == resource) {
        if let Some(ts) = s.tasks.get(&a.task) {
            if ts.is_summary || ts.duration == 0 {
                continue;
            }
            // walk real dates from start to finish, counting working days only
            let mut d = ts.start;
            let end = ts.finish;
            let mut guard = 0;
            while d <= end && guard < 100_000 {
                if cal.is_working(d) {
                    *per_day.entry(d).or_insert(0.0) += a.units;
                }
                d = crate::calendar::next_day(d);
                guard += 1;
            }
        }
    }
    let _ = mpd;
    per_day.into_iter().collect()
}

// ======================================================================================
// Tests
// ======================================================================================
#[cfg(test)]
mod tests {
    use super::*;

    const MPD: i64 = 480;

    fn d(y: i32, m: u32, day: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(y, m, day).unwrap()
    }

    /// Project starting Monday 2026-10-26.
    fn proj() -> Project {
        Project::empty("t", d(2026, 10, 26))
    }

    fn add(p: &mut Project, name: &str, days: i64) -> TaskId {
        let mut t = p.new_task(name);
        t.duration_min = days * MPD;
        let id = t.id;
        p.tasks.push(t);
        id
    }
    fn link(p: &mut Project, a: TaskId, b: TaskId, kind: DepKind, lag_days: i64) {
        p.deps.push(Dependency { pred: a, succ: b, kind, lag_min: lag_days * MPD });
    }

    #[test]
    fn one_day_task_starting_friday_finishes_friday() {
        let mut p = Project::empty("t", d(2026, 10, 23)); // Friday
        let a = add(&mut p, "A", 1);
        let s = schedule(&p).unwrap();
        assert_eq!(s.tasks[&a].start, d(2026, 10, 23));
        assert_eq!(s.tasks[&a].finish, d(2026, 10, 23)); // NOT Saturday
    }

    #[test]
    fn five_day_task_from_monday_finishes_friday_and_successor_starts_monday() {
        let mut p = proj();
        let a = add(&mut p, "A", 5);
        let b = add(&mut p, "B", 2);
        link(&mut p, a, b, DepKind::FS, 0);
        let s = schedule(&p).unwrap();
        assert_eq!(s.tasks[&a].finish, d(2026, 10, 30));
        assert_eq!(s.tasks[&b].start, d(2026, 11, 2));
        assert_eq!(s.tasks[&b].finish, d(2026, 11, 3));
    }

    #[test]
    fn task_spanning_weekend() {
        let mut p = proj();
        let a = add(&mut p, "A", 3);
        let b = add(&mut p, "B", 3); // B starts after A: Thu..Mon? A=Mon-Wed, B=Thu-Mon
        link(&mut p, a, b, DepKind::FS, 0);
        let s = schedule(&p).unwrap();
        assert_eq!(s.tasks[&b].start, d(2026, 10, 29));
        assert_eq!(s.tasks[&b].finish, d(2026, 11, 2)); // Thu, Fri, Mon
    }

    #[test]
    fn holiday_extends_task() {
        let mut p = proj();
        p.calendar.holidays.insert(d(2026, 10, 28));
        let a = add(&mut p, "A", 3);
        let s = schedule(&p).unwrap();
        assert_eq!(s.tasks[&a].finish, d(2026, 10, 29)); // Mon, Tue, (Wed holiday) Thu
    }

    #[test]
    fn ss_ff_sf_links_and_lag() {
        let mut p = proj();
        let a = add(&mut p, "A", 4);
        let ss = add(&mut p, "SS", 2);
        let ff = add(&mut p, "FF", 2);
        let sf = add(&mut p, "SF", 1);
        link(&mut p, a, ss, DepKind::SS, 2); // starts 2 days after A starts -> Wed
        link(&mut p, a, ff, DepKind::FF, 0); // ends when A ends -> Thu, so starts Wed
        link(&mut p, a, sf, DepKind::SF, 2); // sf finishes 2 days after A starts
        let s = schedule(&p).unwrap();
        assert_eq!(s.tasks[&ss].start, d(2026, 10, 28));
        assert_eq!(s.tasks[&ff].finish, d(2026, 10, 29));
        assert_eq!(s.tasks[&ff].start, d(2026, 10, 28));
        // SF: EF(sf) >= ES(a)+2d => EF = Wed start => task of 1 day starts Tue, ends Tue
        assert_eq!(s.tasks[&sf].finish, d(2026, 10, 27));
    }

    #[test]
    fn lead_time_overlaps() {
        let mut p = proj();
        let a = add(&mut p, "A", 5);
        let b = add(&mut p, "B", 2);
        link(&mut p, a, b, DepKind::FS, -2); // B starts 2 days before A finishes
        let s = schedule(&p).unwrap();
        assert_eq!(s.tasks[&b].start, d(2026, 10, 29)); // Thu
    }

    #[test]
    fn critical_path_and_floats() {
        // A(3) -> C(2)   and   B(1) -> C ; B has 2 days float
        let mut p = proj();
        let a = add(&mut p, "A", 3);
        let b = add(&mut p, "B", 1);
        let c = add(&mut p, "C", 2);
        link(&mut p, a, c, DepKind::FS, 0);
        link(&mut p, b, c, DepKind::FS, 0);
        let s = schedule(&p).unwrap();
        assert!(s.tasks[&a].critical);
        assert!(s.tasks[&c].critical);
        assert!(!s.tasks[&b].critical);
        assert_eq!(s.tasks[&b].total_float, 2 * MPD);
        assert_eq!(s.tasks[&b].free_float, 2 * MPD);
        assert_eq!(s.tasks[&a].total_float, 0);
        assert!(s.critical_links.contains(&(a, c)));
        assert!(!s.critical_links.contains(&(b, c)));
    }

    #[test]
    fn cycle_is_detected_with_path() {
        let mut p = proj();
        let a = add(&mut p, "A", 1);
        let b = add(&mut p, "B", 1);
        let c = add(&mut p, "C", 1);
        link(&mut p, a, b, DepKind::FS, 0);
        link(&mut p, b, c, DepKind::FS, 0);
        link(&mut p, c, a, DepKind::FS, 0);
        match schedule(&p) {
            Err(SchedError::Cycle(path)) => {
                assert_eq!(path.first(), path.last());
                assert_eq!(path.len(), 4);
                for id in [a, b, c] {
                    assert!(path.contains(&id));
                }
                // path must follow real links
                for w in path.windows(2) {
                    assert!(p.deps.iter().any(|d| d.pred == w[0] && d.succ == w[1]), "{:?}", path);
                }
            }
            other => panic!("expected cycle, got {:?}", other.map(|_| ())),
        }
    }

    #[test]
    fn two_node_cycle_and_self_link() {
        let mut p = proj();
        let a = add(&mut p, "A", 1);
        let b = add(&mut p, "B", 1);
        link(&mut p, a, b, DepKind::FS, 0);
        link(&mut p, b, a, DepKind::SS, 0);
        assert!(matches!(schedule(&p), Err(SchedError::Cycle(_))));
        let mut p = proj();
        let a = add(&mut p, "A", 1);
        link(&mut p, a, a, DepKind::FS, 0);
        assert_eq!(schedule(&p).err(), Some(SchedError::SelfLink(a)));
    }

    #[test]
    fn must_start_on_wins_and_marks_link_violated() {
        let mut p = proj();
        let a = add(&mut p, "A", 5); // Mon..Fri
        let b = add(&mut p, "B", 2);
        link(&mut p, a, b, DepKind::FS, 0); // would start Mon 2 Nov
        p.task_mut(b).unwrap().constraint = Constraint { kind: ConstraintKind::MustStartOn, date: Some(d(2026, 10, 28)) };
        let s = schedule(&p).unwrap();
        assert_eq!(s.tasks[&b].start, d(2026, 10, 28)); // constraint wins
        assert!(s.violated.contains(&(a, b)));
        // a satisfied constraint doesn't flag the link
        p.task_mut(b).unwrap().constraint = Constraint { kind: ConstraintKind::MustStartOn, date: Some(d(2026, 11, 4)) };
        let s = schedule(&p).unwrap();
        assert_eq!(s.tasks[&b].start, d(2026, 11, 4));
        assert!(s.violated.is_empty());
    }

    #[test]
    fn snet_and_fnet_delay_but_never_advance() {
        let mut p = proj();
        let a = add(&mut p, "A", 1);
        p.task_mut(a).unwrap().constraint = Constraint { kind: ConstraintKind::StartNoEarlierThan, date: Some(d(2026, 11, 2)) };
        assert_eq!(schedule(&p).unwrap().tasks[&a].start, d(2026, 11, 2));
        p.task_mut(a).unwrap().constraint = Constraint { kind: ConstraintKind::StartNoEarlierThan, date: Some(d(2026, 10, 1)) };
        assert_eq!(schedule(&p).unwrap().tasks[&a].start, d(2026, 10, 26));
        p.task_mut(a).unwrap().constraint = Constraint { kind: ConstraintKind::FinishNoEarlierThan, date: Some(d(2026, 10, 30)) };
        assert_eq!(schedule(&p).unwrap().tasks[&a].finish, d(2026, 10, 30));
    }

    #[test]
    fn must_finish_on() {
        let mut p = proj();
        let a = add(&mut p, "A", 2);
        p.task_mut(a).unwrap().constraint = Constraint { kind: ConstraintKind::MustFinishOn, date: Some(d(2026, 11, 3)) };
        let s = schedule(&p).unwrap();
        assert_eq!(s.tasks[&a].finish, d(2026, 11, 3));
        assert_eq!(s.tasks[&a].start, d(2026, 11, 2));
    }

    #[test]
    fn snlt_violation_flagged() {
        let mut p = proj();
        let a = add(&mut p, "A", 2);
        let b = add(&mut p, "B", 2);
        link(&mut p, a, b, DepKind::FS, 0); // B starts Wed
        p.task_mut(b).unwrap().constraint = Constraint { kind: ConstraintKind::StartNoLaterThan, date: Some(d(2026, 10, 27)) };
        let s = schedule(&p).unwrap();
        assert!(s.tasks[&b].constraint_violated);
    }

    #[test]
    fn milestones_have_zero_duration() {
        let mut p = proj();
        let a = add(&mut p, "A", 2);
        let m = add(&mut p, "M", 0);
        p.task_mut(m).unwrap().is_milestone = true;
        link(&mut p, a, m, DepKind::FS, 0);
        let s = schedule(&p).unwrap();
        assert_eq!(s.tasks[&m].start_pos, s.tasks[&m].finish_pos);
        assert_eq!(s.tasks[&m].start, s.tasks[&m].finish);
    }

    #[test]
    fn summary_rolls_up_children() {
        let mut p = proj();
        let s = add(&mut p, "Sum", 0);
        let a = add(&mut p, "A", 2);
        let b = add(&mut p, "B", 3);
        p.task_mut(a).unwrap().parent = Some(s);
        p.task_mut(b).unwrap().parent = Some(s);
        p.task_mut(a).unwrap().progress = 1.0;
        link(&mut p, a, b, DepKind::FS, 0);
        let r = schedule(&p).unwrap();
        assert!(r.tasks[&s].is_summary);
        assert_eq!(r.tasks[&s].start, d(2026, 10, 26));
        assert_eq!(r.tasks[&s].finish, d(2026, 10, 30));
        assert_eq!(r.tasks[&s].duration, 5 * MPD);
        assert!((r.tasks[&s].progress - 0.4).abs() < 1e-9); // (2*1 + 3*0)/5
        assert!(r.tasks[&s].critical);
    }

    #[test]
    fn links_on_summary_are_rejected() {
        let mut p = proj();
        let s = add(&mut p, "Sum", 0);
        let a = add(&mut p, "A", 2);
        let o = add(&mut p, "O", 2);
        p.task_mut(a).unwrap().parent = Some(s);
        link(&mut p, o, s, DepKind::FS, 0);
        assert_eq!(schedule(&p).err(), Some(SchedError::SummaryLink(s)));
    }

    #[test]
    fn unknown_task_link_rejected() {
        let mut p = proj();
        let a = add(&mut p, "A", 1);
        link(&mut p, a, 999, DepKind::FS, 0);
        assert_eq!(schedule(&p).err(), Some(SchedError::UnknownTask(999)));
    }

    #[test]
    fn leveling_delays_noncritical_task_only() {
        // X(10d, no resource) is the critical path. A(5d) and B(2d) both need Al at 100% in parallel;
        // both have float, so one can be pushed back without moving the project finish.
        let mut p = proj();
        let r = p.alloc_resource_id();
        p.resources.push(Resource { id: r, name: "Al".into(), kind: ResourceKind::Work, max_units: 1.0, std_rate: 0.0 });
        let x = add(&mut p, "X", 10);
        let a = add(&mut p, "A", 5);
        let b = add(&mut p, "B", 2);
        p.assignments.push(Assignment { task: a, resource: r, units: 1.0 });
        p.assignments.push(Assignment { task: b, resource: r, units: 1.0 });
        let res = level(&p).unwrap();
        assert!(!res.delays.is_empty(), "{:?}", res);
        assert!(!res.delays.contains_key(&x), "critical task must never move");
        assert!(res.unresolved.is_empty());
        // After applying delays there is no overlap and project length unchanged (10d)
        for (id, dd) in &res.delays {
            p.task_mut(*id).unwrap().leveling_delay_days = *dd;
        }
        let s = schedule(&p).unwrap();
        assert_eq!(s.project_finish_pos, 10 * MPD);
        let hist = resource_histogram(&p, &s, r);
        assert!(hist.iter().all(|(_, u)| *u <= 1.0 + 1e-9), "{:?}", hist);
    }

    #[test]
    fn leveling_reports_unresolvable() {
        // two 5-day tasks, same resource, no float: nothing may move
        let mut p = proj();
        let r = p.alloc_resource_id();
        p.resources.push(Resource { id: r, name: "Al".into(), kind: ResourceKind::Work, max_units: 1.0, std_rate: 0.0 });
        let a = add(&mut p, "A", 5);
        let b = add(&mut p, "B", 5);
        p.assignments.push(Assignment { task: a, resource: r, units: 1.0 });
        p.assignments.push(Assignment { task: b, resource: r, units: 1.0 });
        let res = level(&p).unwrap();
        assert!(res.delays.is_empty());
        assert!(!res.unresolved.is_empty());
    }

    #[test]
    fn sample_project_schedules() {
        let p = Project::sample();
        let s = schedule(&p).unwrap();
        assert_eq!(s.tasks.len(), p.tasks.len());
        assert!(s.tasks.values().any(|t| t.critical));
    }

    #[test]
    fn perf_1000_tasks_500_deps() {
        let p = Project::stress(1000);
        assert!(p.tasks.len() >= 1000);
        assert!(p.deps.len() >= 400);
        let t0 = std::time::Instant::now();
        let runs = 20;
        for _ in 0..runs {
            schedule(&p).unwrap();
        }
        let per = t0.elapsed() / runs;
        println!("schedule(1000 tasks, {} deps) = {:?} per run", p.deps.len(), per);
        // generous bound that even a debug build meets; release is ~100x faster
        assert!(per.as_millis() < 100, "too slow: {:?}", per);
    }
}
