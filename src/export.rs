//! SVG export of the Gantt view (raw SVG string, no extra dependencies).
//! Uses the same row list and the same elbow router as the on-screen painter so the printout
//! matches what the user sees (filters, grouping, collapsed summaries, baseline, critical path).

use crate::calendar::{cal_of, date_of_cal};
use crate::geometry::*;
use crate::model::*;
use crate::scheduler::Schedule;
use crate::view::{Row, RowKind};
use chrono::{Datelike, NaiveDate};
use std::collections::HashMap;
use std::fmt::Write;

fn esc(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

pub fn gantt_svg(p: &Project, s: &Schedule, rows: &[Row], today: NaiveDate) -> String {
    const LABEL_W: f32 = 320.0;
    const DAY_W: f32 = 16.0;
    let mut c0 = s.project_start_cal;
    let mut c1 = s.project_finish_cal;
    if let Some(b) = &p.baseline {
        for (a, f) in b.entries.values() {
            c0 = c0.min(*a);
            c1 = c1.max(*f);
        }
    }
    let c0 = c0.floor() - 2.0;
    let c1 = c1.ceil() + 3.0;
    let width = LABEL_W + (c1 - c0) as f32 * DAY_W + 20.0;
    let height = HEADER_H + 24.0 + rows.len() as f32 * ROW_H + 30.0;
    let top = HEADER_H + 24.0;
    let x = |c: f64| -> f32 { LABEL_W + ((c - c0) as f32) * DAY_W };
    let row_of: HashMap<TaskId, usize> = rows.iter().enumerate().filter_map(|(i, r)| r.task.map(|t| (t, i))).collect();
    let y_mid = |i: usize| top + i as f32 * ROW_H + ROW_H / 2.0;

    let mut o = String::new();
    let _ = writeln!(o, r##"<?xml version="1.0" encoding="UTF-8"?>"##);
    let _ = writeln!(o, r##"<svg xmlns="http://www.w3.org/2000/svg" width="{w}" height="{h}" viewBox="0 0 {w} {h}" font-family="Helvetica, Arial, sans-serif" font-size="11">"##, w = width, h = height);
    let _ = writeln!(o, r##"<defs><marker id="ah" markerWidth="7" markerHeight="7" refX="6" refY="3.5" orient="auto"><path d="M0,0 L7,3.5 L0,7 z" fill="context-stroke"/></marker></defs>"##);
    let _ = writeln!(o, r##"<rect width="100%" height="100%" fill="#ffffff"/>"##);
    let _ = writeln!(o, r##"<text x="8" y="18" font-size="15" font-weight="bold">{}</text>"##, esc(&p.name));

    // weekends / holidays + day header
    let mut d = date_of_cal(c0);
    let end = date_of_cal(c1);
    let mut last_month = (0, 0);
    while d <= end {
        let cx = x(cal_of(d));
        if !p.calendar.is_working(d) {
            let _ = writeln!(o, r##"<rect x="{cx}" y="{top}" width="{DAY_W}" height="{}" fill="#f1f3f6"/>"##, rows.len() as f32 * ROW_H);
        }
        let m = (d.year(), d.month());
        if m != last_month {
            let _ = writeln!(o, r##"<text x="{}" y="{}" font-weight="bold" fill="#333">{}</text>"##, cx + 2.0, HEADER_H - 2.0, d.format("%b %Y"));
            let _ = writeln!(o, r##"<line x1="{cx}" y1="{}" x2="{cx}" y2="{}" stroke="#999"/>"##, 24.0, top + rows.len() as f32 * ROW_H);
            last_month = m;
        }
        let _ = writeln!(o, r##"<text x="{}" y="{}" fill="#666" font-size="9" text-anchor="middle">{}</text>"##, cx + DAY_W / 2.0, top - 4.0, d.day());
        d = crate::calendar::next_day(d);
    }
    let _ = writeln!(o, r##"<line x1="0" y1="{top}" x2="{width}" y2="{top}" stroke="#666"/>"##);

    // rows: labels + stripes
    for (i, r) in rows.iter().enumerate() {
        let y = top + i as f32 * ROW_H;
        if i % 2 == 1 {
            let _ = writeln!(o, r##"<rect x="0" y="{y}" width="{LABEL_W}" height="{ROW_H}" fill="#fafafa"/>"##);
        }
        match (&r.kind, r.task.and_then(|id| p.task(id))) {
            (RowKind::Group, _) => {
                let _ = writeln!(o, r##"<rect x="0" y="{y}" width="{width}" height="{ROW_H}" fill="#e8eef7" opacity="0.7"/>"##);
                let _ = writeln!(o, r##"<text x="6" y="{}" font-weight="bold">{}</text>"##, y + 17.0, esc(&r.label));
            }
            (_, Some(t)) => {
                let is_sum = s.tasks.get(&t.id).map_or(false, |x| x.is_summary);
                let _ = writeln!(
                    o,
                    r##"<text x="{}" y="{}"{}>{} {}</text>"##,
                    6.0 + r.level as f32 * 12.0,
                    y + 17.0,
                    if is_sum { r#" font-weight="bold""# } else { "" },
                    esc(&t.wbs),
                    esc(&t.name)
                );
            }
            _ => {}
        }
    }

    // today
    let tx = x(cal_of(today) + 0.5);
    if tx > LABEL_W && tx < width {
        let _ = writeln!(o, r##"<line x1="{tx}" y1="{}" x2="{tx}" y2="{}" stroke="#e5484d" stroke-width="1.5" stroke-dasharray="4 3"/>"##, 24.0, top + rows.len() as f32 * ROW_H);
    }

    // dependency arrows
    for dep in &p.deps {
        let (Some(&ra), Some(&rb)) = (row_of.get(&dep.pred), row_of.get(&dep.succ)) else { continue };
        let (Some(a), Some(b)) = (s.tasks.get(&dep.pred), s.tasks.get(&dep.succ)) else { continue };
        let (ff, tf) = link_ends(dep.kind);
        let px = if ff { x(a.finish_cal) } else { x(a.start_cal) };
        let qx = if tf { x(b.finish_cal) } else { x(b.start_cal) };
        let pts = route((px, y_mid(ra)), ff, (qx, y_mid(rb)), tf);
        let key = (dep.pred, dep.succ);
        let (color, w) = if s.violated.contains(&key) {
            ("#ff0000", 2.2)
        } else if s.critical_links.contains(&key) {
            ("#c0392b", 1.4)
        } else {
            ("#6b6b6b", 1.0)
        };
        let path: Vec<String> = pts.iter().enumerate().map(|(i, (px, py))| format!("{}{},{}", if i == 0 { "M" } else { "L" }, px, py)).collect();
        let _ = writeln!(o, r##"<path d="{}" fill="none" stroke="{}" stroke-width="{}" marker-end="url(#ah)"/>"##, path.join(" "), color, w);
    }

    // bars
    for (i, r) in rows.iter().enumerate() {
        let Some(id) = r.task else { continue };
        let (Some(t), Some(ts)) = (p.task(id), s.tasks.get(&id)) else { continue };
        let y = top + i as f32 * ROW_H;
        let (x0, x1) = (x(ts.start_cal), x(ts.finish_cal));
        if let Some((bs, bf)) = p.baseline.as_ref().and_then(|b| b.entries.get(&id)) {
            let _ = writeln!(o, r##"<rect x="{}" y="{}" width="{}" height="3" fill="#9aa0a6"/>"##, x(*bs), y + 19.0, (x(*bf) - x(*bs)).max(1.0));
        }
        if t.is_milestone && !ts.is_summary {
            let (cx, cy) = (x0, y + 11.0);
            let fill = if ts.critical { "#c0392b" } else { "#111" };
            let _ = writeln!(o, r##"<path d="M{} {} L{} {} L{} {} L{} {} z" fill="{}"/>"##, cx - 6.0, cy, cx, cy - 6.0, cx + 6.0, cy, cx, cy + 6.0, fill);
        } else if ts.is_summary {
            let w = (x1 - x0).max(2.0);
            let _ = writeln!(o, r##"<rect x="{x0}" y="{}" width="{w}" height="6" fill="#333"/>"##, y + 6.0);
            let _ = writeln!(o, r##"<rect x="{x0}" y="{}" width="{}" height="6" fill="#000"/>"##, y + 6.0, w * ts.progress as f32);
            let _ = writeln!(o, r##"<path d="M{} {} l8 0 l-4 6 z M{} {} l-8 0 l4 6 z" fill="#333"/>"##, x0, y + 12.0, x1, y + 12.0);
        } else {
            let w = (x1 - x0).max(2.0);
            let (base, dark) = if ts.critical { ("#d64541", "#8f1d1d") } else { ("#4285cf", "#1c5496") };
            let _ = writeln!(o, r##"<rect x="{x0}" y="{}" width="{w}" height="12" rx="2" fill="{base}"/>"##, y + 5.0);
            let _ = writeln!(o, r##"<rect x="{x0}" y="{}" width="{}" height="12" rx="2" fill="{dark}"/>"##, y + 5.0, w * ts.progress as f32);
            let names = p.resource_names(id);
            if !names.is_empty() {
                let _ = writeln!(o, r##"<text x="{}" y="{}" fill="#555" font-size="10">{}</text>"##, x1 + 5.0, y + 15.0, esc(&names));
            }
        }
    }
    let _ = writeln!(o, r##"<text x="8" y="{}" fill="#777" font-size="9">Legend: blue = task, red = critical path, black bracket = summary, grey strip = baseline, red dashed line = today. Exported by ProjectCraft.</text>"##, height - 8.0);
    let _ = writeln!(o, "</svg>");
    o
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scheduler::schedule;
    use crate::view::{build_rows, ViewOptions};

    #[test]
    fn svg_is_wellformed_and_contains_tasks() {
        let mut p = Project::sample();
        p.tasks[1].name = "A & B <test>".into();
        let s = schedule(&p).unwrap();
        let rows = build_rows(&p, &s, &ViewOptions::default());
        let svg = gantt_svg(&p, &s, &rows, p.start_date);
        assert!(svg.starts_with("<?xml"));
        assert!(svg.trim_end().ends_with("</svg>"));
        assert!(svg.contains("A &amp; B &lt;test&gt;"));
        assert!(svg.contains("marker-end"));
        assert_eq!(svg.matches("<svg").count(), 1);
        // every opened rect/path/text is self-closed or closed: cheap balance check on text tags
        assert_eq!(svg.matches("<text").count(), svg.matches("</text>").count());
    }
}
