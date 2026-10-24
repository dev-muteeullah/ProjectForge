//! SQLite persistence.
//!
//! DESIGN
//! * The working file IS a SQLite database (extension `.pcraft`), versioned with
//!   `PRAGMA user_version`. Foreign keys are ON: dependency.pred/succ and assignment.task/resource
//!   reference their parents with ON DELETE CASCADE, so the file cannot contain dangling links
//!   even if edited by another tool.
//! * `save` runs ONE transaction (replace-all). SQLite makes that atomic and crash-safe: a crash
//!   mid-save leaves the previous version intact (rollback journal).
//! * `save` validates the project with the scheduler first and refuses to write an invalid
//!   state (e.g. a dependency cycle).
//! * Dates are ISO-8601 TEXT, enums are short TEXT codes, durations are INTEGER working minutes.

use crate::calendar::Calendar;
use crate::model::*;
use crate::scheduler::{self, SchedError};
use chrono::NaiveDate;
use rusqlite::{params, Connection, OptionalExtension};
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::path::Path;

#[derive(Debug)]
pub enum PersistError {
    Db(rusqlite::Error),
    Invalid(SchedError),
    Format(String),
    NotFound(String),
}
impl fmt::Display for PersistError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PersistError::Db(e) => write!(f, "Database error: {}", e),
            PersistError::Invalid(e) => write!(f, "Project is invalid and was not saved: {}", e),
            PersistError::Format(m) => write!(f, "Unreadable project file: {}", m),
            PersistError::NotFound(p) => write!(f, "File not found: {}", p),
        }
    }
}
impl std::error::Error for PersistError {}
impl From<rusqlite::Error> for PersistError {
    fn from(e: rusqlite::Error) -> Self {
        PersistError::Db(e)
    }
}

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS meta (
    key   TEXT PRIMARY KEY,
    value TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS holiday (
    date TEXT PRIMARY KEY
);
CREATE TABLE IF NOT EXISTS task (
    id                  INTEGER PRIMARY KEY,
    seq                 INTEGER NOT NULL,              -- sibling order (Vec order)
    parent_id           INTEGER REFERENCES task(id) ON DELETE CASCADE,
    name                TEXT NOT NULL,
    description         TEXT NOT NULL DEFAULT '',
    duration_min        INTEGER NOT NULL CHECK (duration_min >= 0),
    is_milestone        INTEGER NOT NULL CHECK (is_milestone IN (0,1)),
    progress            REAL NOT NULL CHECK (progress >= 0.0 AND progress <= 1.0),
    constraint_kind     TEXT NOT NULL,
    constraint_date     TEXT,
    leveling_delay_days INTEGER NOT NULL DEFAULT 0,
    start_date          TEXT NOT NULL,                 -- cached scheduler output
    finish_date         TEXT NOT NULL,
    wbs                 TEXT NOT NULL DEFAULT ''
);
CREATE TABLE IF NOT EXISTS dependency (
    pred_id  INTEGER NOT NULL REFERENCES task(id) ON DELETE CASCADE,
    succ_id  INTEGER NOT NULL REFERENCES task(id) ON DELETE CASCADE,
    kind     TEXT NOT NULL CHECK (kind IN ('FS','SS','FF','SF')),
    lag_min  INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (pred_id, succ_id),
    CHECK (pred_id <> succ_id)
);
CREATE TABLE IF NOT EXISTS resource (
    id        INTEGER PRIMARY KEY,
    seq       INTEGER NOT NULL,
    name      TEXT NOT NULL UNIQUE,
    kind      TEXT NOT NULL CHECK (kind IN ('Work','Material','Cost')),
    max_units REAL NOT NULL CHECK (max_units >= 0),
    std_rate  REAL NOT NULL
);
CREATE TABLE IF NOT EXISTS assignment (
    task_id     INTEGER NOT NULL REFERENCES task(id) ON DELETE CASCADE,
    resource_id INTEGER NOT NULL REFERENCES resource(id) ON DELETE CASCADE,
    units       REAL NOT NULL CHECK (units > 0),
    PRIMARY KEY (task_id, resource_id)
);
CREATE TABLE IF NOT EXISTS baseline_task (
    task_id    INTEGER PRIMARY KEY REFERENCES task(id) ON DELETE CASCADE,
    start_cal  REAL NOT NULL,
    finish_cal REAL NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_task_parent ON task(parent_id);
CREATE INDEX IF NOT EXISTS idx_dep_succ ON dependency(succ_id);
CREATE INDEX IF NOT EXISTS idx_assign_res ON assignment(resource_id);
";

fn open(path: &Path) -> Result<Connection, PersistError> {
    let conn = Connection::open(path)?;
    conn.execute_batch("PRAGMA foreign_keys = ON;")?;
    Ok(conn)
}

fn set_meta(tx: &rusqlite::Transaction, k: &str, v: &str) -> rusqlite::Result<()> {
    tx.execute("INSERT OR REPLACE INTO meta(key,value) VALUES (?1,?2)", params![k, v])?;
    Ok(())
}

pub fn save(path: &Path, p: &Project) -> Result<(), PersistError> {
    // refuse to persist an invalid project
    let sched = scheduler::schedule(p).map_err(PersistError::Invalid)?;
    let mut p = p.clone();
    p.apply_schedule(&sched);

    let mut conn = open(path)?;
    conn.execute_batch(SCHEMA)?;
    let tx = conn.transaction()?;
    tx.execute_batch(
        "DELETE FROM baseline_task; DELETE FROM assignment; DELETE FROM dependency; \
         DELETE FROM task; DELETE FROM resource; DELETE FROM holiday; DELETE FROM meta;",
    )?;
    // parents may be inserted after children in Vec order -> check FKs at COMMIT instead
    tx.execute_batch("PRAGMA defer_foreign_keys = ON;")?;

    set_meta(&tx, "file_version", &FILE_VERSION.to_string())?;
    set_meta(&tx, "name", &p.name)?;
    set_meta(&tx, "start_date", &p.start_date.format("%Y-%m-%d").to_string())?;
    set_meta(&tx, "cal_name", &p.calendar.name)?;
    set_meta(&tx, "cal_hours", &p.calendar.hours_per_day.to_string())?;
    let mask: String = p.calendar.work_week.iter().map(|b| if *b { '1' } else { '0' }).collect();
    set_meta(&tx, "cal_week", &mask)?;
    set_meta(&tx, "next_task_id", &p.next_task_id.to_string())?;
    set_meta(&tx, "next_resource_id", &p.next_resource_id.to_string())?;
    if let Some(b) = &p.baseline {
        set_meta(&tx, "baseline_saved_at", &b.saved_at)?;
    }
    for h in &p.calendar.holidays {
        tx.execute("INSERT INTO holiday(date) VALUES (?1)", params![h.format("%Y-%m-%d").to_string()])?;
    }
    {
        let mut st = tx.prepare(
            "INSERT INTO task(id,seq,parent_id,name,description,duration_min,is_milestone,progress,\
             constraint_kind,constraint_date,leveling_delay_days,start_date,finish_date,wbs) \
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14)",
        )?;
        let known: std::collections::HashSet<TaskId> = p.tasks.iter().map(|t| t.id).collect();
        for (seq, t) in p.tasks.iter().enumerate() {
            st.execute(params![
                t.id,
                seq as i64,
                t.parent.filter(|x| known.contains(x)),
                t.name,
                t.description,
                t.duration_min.max(0),
                t.is_milestone as i64,
                t.progress.clamp(0.0, 1.0),
                t.constraint.kind.as_str(),
                t.constraint.date.map(|d| d.format("%Y-%m-%d").to_string()),
                t.leveling_delay_days as i64,
                t.start.format("%Y-%m-%d").to_string(),
                t.finish.format("%Y-%m-%d").to_string(),
                t.wbs,
            ])?;
        }
    }
    {
        let mut st = tx.prepare("INSERT INTO dependency(pred_id,succ_id,kind,lag_min) VALUES (?1,?2,?3,?4)")?;
        for d in &p.deps {
            st.execute(params![d.pred, d.succ, d.kind.as_str(), d.lag_min])?;
        }
        let mut st = tx.prepare("INSERT INTO resource(id,seq,name,kind,max_units,std_rate) VALUES (?1,?2,?3,?4,?5,?6)")?;
        for (i, r) in p.resources.iter().enumerate() {
            st.execute(params![r.id, i as i64, r.name, r.kind.as_str(), r.max_units, r.std_rate])?;
        }
        let mut st = tx.prepare("INSERT INTO assignment(task_id,resource_id,units) VALUES (?1,?2,?3)")?;
        for a in &p.assignments {
            st.execute(params![a.task, a.resource, a.units])?;
        }
        if let Some(b) = p.pruned_baseline() {
            let mut st = tx.prepare("INSERT INTO baseline_task(task_id,start_cal,finish_cal) VALUES (?1,?2,?3)")?;
            for (id, (s, f)) in &b.entries {
                st.execute(params![id, s, f])?;
            }
        }
    }
    tx.pragma_update(None, "user_version", FILE_VERSION)?;
    tx.commit()?;
    Ok(())
}

fn parse_d(s: &str) -> Result<NaiveDate, PersistError> {
    NaiveDate::parse_from_str(s, "%Y-%m-%d").map_err(|_| PersistError::Format(format!("bad date '{}'", s)))
}

pub fn load(path: &Path) -> Result<Project, PersistError> {
    if !path.exists() {
        return Err(PersistError::NotFound(path.display().to_string()));
    }
    let conn = open(path)?;
    let ver: u32 = conn.pragma_query_value(None, "user_version", |r| r.get(0))?;
    if ver == 0 {
        return Err(PersistError::Format("not a ProjectCraft file".into()));
    }
    if ver > FILE_VERSION {
        return Err(PersistError::Format(format!("file version {} is newer than this app ({})", ver, FILE_VERSION)));
    }
    let meta: BTreeMap<String, String> = {
        let mut st = conn.prepare("SELECT key,value FROM meta")?;
        let rows = st.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?;
        rows.collect::<Result<_, _>>()?
    };
    let get = |k: &str| meta.get(k).cloned().ok_or_else(|| PersistError::Format(format!("missing meta '{}'", k)));
    let start_date = parse_d(&get("start_date")?)?;
    let mut cal = Calendar::default();
    cal.name = get("cal_name")?;
    cal.hours_per_day = get("cal_hours")?.parse().map_err(|_| PersistError::Format("bad cal_hours".into()))?;
    let mask = get("cal_week")?;
    if mask.len() != 7 {
        return Err(PersistError::Format("bad cal_week".into()));
    }
    for (i, c) in mask.chars().enumerate() {
        cal.work_week[i] = c == '1';
    }
    let mut hol = BTreeSet::new();
    {
        let mut st = conn.prepare("SELECT date FROM holiday ORDER BY date")?;
        let rows = st.query_map([], |r| r.get::<_, String>(0))?;
        for r in rows {
            hol.insert(parse_d(&r?)?);
        }
    }
    cal.holidays = hol;
    let mut p = Project::empty(&get("name")?, start_date);
    p.calendar = cal;
    p.next_task_id = get("next_task_id")?.parse().unwrap_or(1);
    p.next_resource_id = get("next_resource_id")?.parse().unwrap_or(1);

    {
        let mut st = conn.prepare(
            "SELECT id,parent_id,name,description,duration_min,is_milestone,progress,constraint_kind,constraint_date,\
             leveling_delay_days,start_date,finish_date,wbs FROM task ORDER BY seq",
        )?;
        let mut rows = st.query([])?;
        while let Some(r) = rows.next()? {
            let kind_s: String = r.get(7)?;
            let kind = ConstraintKind::parse(&kind_s).ok_or_else(|| PersistError::Format(format!("bad constraint '{}'", kind_s)))?;
            let cdate: Option<String> = r.get(8)?;
            let cdate = match cdate {
                Some(s) => Some(parse_d(&s)?),
                None => None,
            };
            let ms: i64 = r.get(5)?;
            let delay: i64 = r.get(9)?;
            let sd: String = r.get(10)?;
            let fd: String = r.get(11)?;
            p.tasks.push(Task {
                id: r.get(0)?,
                parent: r.get(1)?,
                name: r.get(2)?,
                description: r.get(3)?,
                duration_min: r.get(4)?,
                is_milestone: ms != 0,
                progress: r.get(6)?,
                constraint: Constraint { kind, date: cdate },
                leveling_delay_days: delay.max(0) as u32,
                start: parse_d(&sd)?,
                finish: parse_d(&fd)?,
                wbs: r.get(12)?,
            });
        }
    }
    {
        let mut st = conn.prepare("SELECT pred_id,succ_id,kind,lag_min FROM dependency")?;
        let mut rows = st.query([])?;
        while let Some(r) = rows.next()? {
            let k: String = r.get(2)?;
            p.deps.push(Dependency {
                pred: r.get(0)?,
                succ: r.get(1)?,
                kind: DepKind::parse(&k).ok_or_else(|| PersistError::Format(format!("bad link kind '{}'", k)))?,
                lag_min: r.get(3)?,
            });
        }
        let mut st = conn.prepare("SELECT id,name,kind,max_units,std_rate FROM resource ORDER BY seq")?;
        let mut rows = st.query([])?;
        while let Some(r) = rows.next()? {
            let k: String = r.get(2)?;
            p.resources.push(Resource {
                id: r.get(0)?,
                name: r.get(1)?,
                kind: ResourceKind::parse(&k).ok_or_else(|| PersistError::Format(format!("bad resource kind '{}'", k)))?,
                max_units: r.get(3)?,
                std_rate: r.get(4)?,
            });
        }
        let mut st = conn.prepare("SELECT task_id,resource_id,units FROM assignment")?;
        let mut rows = st.query([])?;
        while let Some(r) = rows.next()? {
            p.assignments.push(Assignment { task: r.get(0)?, resource: r.get(1)?, units: r.get(2)? });
        }
        let saved: Option<String> = conn.query_row("SELECT value FROM meta WHERE key='baseline_saved_at'", [], |r| r.get(0)).optional()?;
        if let Some(saved_at) = saved {
            let mut entries = BTreeMap::new();
            let mut st = conn.prepare("SELECT task_id,start_cal,finish_cal FROM baseline_task")?;
            let mut rows = st.query([])?;
            while let Some(r) = rows.next()? {
                entries.insert(r.get::<_, i64>(0)?, (r.get::<_, f64>(1)?, r.get::<_, f64>(2)?));
            }
            p.baseline = Some(Baseline { saved_at, entries });
        }
    }
    // a loaded file must also be schedulable
    let s = scheduler::schedule(&p).map_err(PersistError::Invalid)?;
    p.apply_schedule(&s);
    Ok(p)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> std::path::PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!("projectcraft_test_{}_{}.pcraft", name, std::process::id()));
        let _ = std::fs::remove_file(&p);
        p
    }

    #[test]
    fn roundtrip_sample() {
        let path = tmp("rt");
        let mut p = Project::sample();
        p.baseline = Some(Baseline { saved_at: "now".into(), entries: [(p.tasks[1].id, (1.0, 2.0))].into_iter().collect() });
        p.calendar.holidays.insert(NaiveDate::from_ymd_opt(2026, 12, 25).unwrap());
        p.tasks[2].constraint = Constraint { kind: ConstraintKind::StartNoEarlierThan, date: NaiveDate::from_ymd_opt(2027, 1, 4) };
        p.tasks[2].description = "multi\nline \"quoted\" text — ünïcode".into();
        save(&path, &p).unwrap();
        let q = load(&path).unwrap();
        assert_eq!(q.name, p.name);
        assert_eq!(q.start_date, p.start_date);
        assert_eq!(q.calendar, p.calendar);
        assert_eq!(q.tasks.len(), p.tasks.len());
        for (a, b) in p.tasks.iter().zip(q.tasks.iter()) {
            assert_eq!((a.id, a.parent, &a.name, &a.description, a.duration_min, a.is_milestone, a.constraint), (b.id, b.parent, &b.name, &b.description, b.duration_min, b.is_milestone, b.constraint));
            assert!((a.progress - b.progress).abs() < 1e-12);
        }
        assert_eq!(q.deps, p.deps);
        assert_eq!(q.resources, p.resources);
        assert_eq!(q.assignments.len(), p.assignments.len());
        assert_eq!(q.baseline, p.baseline);
        assert_eq!(q.next_task_id, p.next_task_id);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn save_twice_overwrites_cleanly() {
        let path = tmp("twice");
        let mut p = Project::sample();
        save(&path, &p).unwrap();
        p.tasks.truncate(3);
        p.deps.clear();
        p.assignments.clear();
        save(&path, &p).unwrap();
        let q = load(&path).unwrap();
        assert_eq!(q.tasks.len(), 3);
        assert!(q.deps.is_empty());
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn refuses_to_save_a_cycle() {
        let path = tmp("cycle");
        let mut p = Project::sample();
        let (a, b) = (p.tasks[1].id, p.tasks[2].id); // existing a->b link; add b->a
        p.deps.push(Dependency { pred: b, succ: a, kind: DepKind::FS, lag_min: 0 });
        let err = save(&path, &p).unwrap_err();
        assert!(matches!(err, PersistError::Invalid(SchedError::Cycle(_))));
        assert!(!path.exists() || load(&path).is_err(), "nothing valid may be written");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn foreign_keys_are_enforced_in_the_file() {
        let path = tmp("fk");
        save(&path, &Project::sample()).unwrap();
        let conn = open(&path).unwrap();
        let bad = conn.execute("INSERT INTO dependency(pred_id,succ_id,kind,lag_min) VALUES (9999,1,'FS',0)", []);
        assert!(bad.is_err(), "dangling dependency must be rejected by FK constraint");
        let bad = conn.execute("INSERT INTO dependency(pred_id,succ_id,kind,lag_min) VALUES (1,1,'FS',0)", []);
        assert!(bad.is_err(), "self link must be rejected by CHECK");
        // cascade: deleting a task removes its links
        let before: i64 = conn.query_row("SELECT COUNT(*) FROM dependency", [], |r| r.get(0)).unwrap();
        conn.execute("DELETE FROM task WHERE id=(SELECT pred_id FROM dependency LIMIT 1)", []).unwrap();
        let after: i64 = conn.query_row("SELECT COUNT(*) FROM dependency", [], |r| r.get(0)).unwrap();
        assert!(after < before);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn load_errors_are_graceful() {
        assert!(matches!(load(Path::new("/definitely/not/here.pcraft")), Err(PersistError::NotFound(_))));
        let path = tmp("junk");
        std::fs::write(&path, b"this is not sqlite").unwrap();
        assert!(load(&path).is_err());
        let _ = std::fs::remove_file(&path);
        // an empty-but-valid sqlite db has user_version 0
        let path = tmp("empty");
        Connection::open(&path).unwrap().execute_batch("CREATE TABLE x(a);").unwrap();
        assert!(matches!(load(&path), Err(PersistError::Format(_))));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn large_project_roundtrip() {
        let path = tmp("big");
        let p = Project::stress(1000);
        save(&path, &p).unwrap();
        let q = load(&path).unwrap();
        assert_eq!(q.tasks.len(), p.tasks.len());
        assert_eq!(q.deps.len(), p.deps.len());
        let _ = std::fs::remove_file(&path);
    }
}
