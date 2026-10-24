//! Right pane: the Gantt chart, custom-painted with `egui::Painter`.
//!
//! * Time axis: Day / Week / Month header chosen from the current zoom (`ppd` = pixels per
//!   calendar day), pan with drag / wheel, zoom with Ctrl+wheel or the toolbar.
//! * Rows: the very same `Vec<Row>` as the sheet; only visible rows are painted.
//! * Direct manipulation: drag a bar to move it, drag its right edge to change duration.
//!   While dragging we only draw a ghost; on release ONE command is executed (so one undo step)
//!   and the scheduler cascades the change to successors.
//!   Moving sets a "Start No Earlier Than" constraint, exactly like MS Project.

use crate::app::*;
use crate::commands::Command;
use crate::calendar::{cal_of, date_of_cal, next_day};
use crate::geometry::*;
use crate::model::*;
use crate::view::RowKind;
use chrono::{Datelike, Duration, NaiveDate};
use eframe::egui;
use egui::{Align2, Color32, CursorIcon, FontId, Pos2, Rect, Sense, Shape, Stroke, Vec2};

struct Palette {
    bg: Color32,
    stripe: Color32,
    weekend: Color32,
    grid: Color32,
    text: Color32,
    header_bg: Color32,
    group: Color32,
}
fn palette(dark: bool) -> Palette {
    if dark {
        Palette {
            bg: Color32::from_rgb(27, 27, 30),
            stripe: Color32::from_rgba_unmultiplied(255, 255, 255, 6),
            weekend: Color32::from_rgba_unmultiplied(255, 255, 255, 12),
            grid: Color32::from_rgb(55, 55, 60),
            text: Color32::from_rgb(210, 210, 215),
            header_bg: Color32::from_rgb(40, 41, 46),
            group: Color32::from_rgba_unmultiplied(90, 130, 200, 40),
        }
    } else {
        Palette {
            bg: Color32::from_rgb(255, 255, 255),
            stripe: Color32::from_rgba_unmultiplied(0, 0, 0, 8),
            weekend: Color32::from_rgba_unmultiplied(40, 60, 100, 18),
            grid: Color32::from_rgb(222, 224, 228),
            text: Color32::from_rgb(40, 40, 45),
            header_bg: Color32::from_rgb(236, 238, 242),
            group: Color32::from_rgba_unmultiplied(70, 110, 170, 40),
        }
    }
}

const NORMAL: Color32 = Color32::from_rgb(66, 133, 207);
const NORMAL_DARK: Color32 = Color32::from_rgb(28, 84, 150);
const CRIT: Color32 = Color32::from_rgb(214, 69, 65);
const CRIT_DARK: Color32 = Color32::from_rgb(143, 29, 29);
const LINK: Color32 = Color32::from_rgb(120, 120, 128);
const LINK_CRIT: Color32 = Color32::from_rgb(200, 40, 40);
const LINK_BAD: Color32 = Color32::from_rgb(255, 0, 0);

#[derive(Clone, Copy, PartialEq)]
enum Mode {
    Day,
    Week,
    Month,
}

fn month_start(d: NaiveDate) -> NaiveDate {
    NaiveDate::from_ymd_opt(d.year(), d.month(), 1).unwrap_or(d)
}
fn next_month(d: NaiveDate) -> NaiveDate {
    let (y, m) = if d.month() == 12 { (d.year() + 1, 1) } else { (d.year(), d.month() + 1) };
    NaiveDate::from_ymd_opt(y, m, 1).unwrap_or(d)
}

impl App {
    /// (task, resize?) of the bar under `pos`, if any.
    fn gantt_hit(&self, rect: Rect, pos: Pos2) -> Option<(TaskId, bool)> {
        let row = self.row_at(rect, pos)?;
        let id = self.rows.get(row)?.task?;
        let ts = self.schedule.tasks.get(&id)?;
        if ts.is_summary {
            return None;
        }
        let x = |c: f64| rect.left() + ((c - self.view_x0) * self.ppd as f64) as f32;
        let top = rect.top() + HEADER_H + row as f32 * ROW_H - self.scroll_y;
        let t = self.project.task(id)?;
        if t.is_milestone {
            return if (pos.x - x(ts.start_cal)).abs() <= 8.0 { Some((id, false)) } else { None };
        }
        let (x0, x1) = (x(ts.start_cal), x(ts.finish_cal).max(x(ts.start_cal) + 3.0));
        if pos.y < top + 3.0 || pos.y > top + ROW_H - 3.0 {
            return None;
        }
        if pos.x >= x1 - 6.0 && pos.x <= x1 + 3.0 {
            Some((id, true))
        } else if pos.x >= x0 && pos.x < x1 - 6.0 {
            Some((id, false))
        } else {
            None
        }
    }

    fn row_at(&self, rect: Rect, pos: Pos2) -> Option<usize> {
        let y = pos.y - rect.top() - HEADER_H + self.scroll_y;
        if y < 0.0 {
            return None;
        }
        let i = (y / ROW_H).floor() as usize;
        (i < self.rows.len()).then_some(i)
    }

    fn finish_drag(&mut self, d: GanttDrag) {
        if d.dx_days == 0 {
            return;
        }
        let (Some(t), Some(ts)) = (self.project.task(d.task).cloned(), self.schedule.tasks.get(&d.task).cloned()) else { return };
        let mpd = self.mpd();
        let cal = self.project.calendar.clone();
        if !d.resize {
            let raw = ts.start + Duration::days(d.dx_days);
            let new = if d.dx_days > 0 { cal.next_working_on_or_after(raw) } else { cal.prev_working_on_or_before(raw) };
            if new == ts.start {
                return;
            }
            let old = t.constraint;
            let kind = if old.kind == ConstraintKind::MustStartOn { ConstraintKind::MustStartOn } else { ConstraintKind::StartNoEarlierThan };
            let newc = Constraint { kind, date: Some(new) };
            if self.exec(Command::SetConstraint { id: d.task, old, new: newc }) {
                let actual = self.schedule.tasks.get(&d.task).map(|x| x.start);
                if actual.map_or(false, |a| a != new) {
                    self.toast_info("The start is held by predecessor links - drag the predecessor, or change the link / constraint.");
                }
            }
        } else {
            if t.is_milestone {
                return;
            }
            let raw = ts.finish + Duration::days(d.dx_days);
            let nf = if d.dx_days > 0 { cal.next_working_on_or_after(raw) } else { cal.prev_working_on_or_before(raw) };
            let delta = cal.working_days_between(ts.finish, nf);
            let new_dur = (t.duration_min + delta * mpd).max(mpd);
            if new_dur != t.duration_min {
                self.exec(Command::SetDuration { id: d.task, old: (false, t.duration_min), new: (false, new_dur) });
            }
        }
    }

    pub fn ui_gantt(&mut self, ui: &mut egui::Ui) {
        let avail = ui.available_size();
        let (rect, resp) = ui.allocate_exact_size(avail, Sense::click_and_drag());
        let dark = ui.visuals().dark_mode;
        let pal = palette(dark);
        if !self.did_initial_view {
            self.view_x0 = self.schedule.project_start_cal - 2.0;
            self.did_initial_view = true;
        }

        // ------------------------------------------------------------ input
        let body_h = (rect.height() - HEADER_H).max(0.0);
        let max_scroll = (self.rows.len() as f32 * ROW_H - body_h).max(0.0);
        if resp.hovered() || self.gantt_drag.is_some() {
            let (scroll, zoom, hover) = ui.input(|i| (i.smooth_scroll_delta, i.zoom_delta(), i.pointer.hover_pos()));
            if (zoom - 1.0).abs() > 1e-4 {
                let anchor_px = hover.map_or(rect.center().x, |p| p.x) - rect.left();
                let c_at = self.view_x0 + anchor_px as f64 / self.ppd as f64;
                self.ppd = (self.ppd * zoom).clamp(1.5, 90.0);
                self.view_x0 = c_at - anchor_px as f64 / self.ppd as f64;
            } else {
                self.view_x0 -= scroll.x as f64 / self.ppd as f64;
                self.scroll_y = (self.scroll_y - scroll.y).clamp(0.0, max_scroll);
            }
        }
        self.scroll_y = self.scroll_y.clamp(0.0, max_scroll);

        let press = ui.input(|i| i.pointer.press_origin());
        let hover_pos = resp.hover_pos();
        if resp.drag_started() {
            if let Some(pp) = press {
                if rect.contains(pp) {
                    if let Some((task, resize)) = self.gantt_hit(rect, pp) {
                        if !self.selected.contains(&task) {
                            self.selected = vec![task];
                            self.anchor = Some(task);
                        }
                        self.gantt_drag = Some(GanttDrag { task, resize, press_x: pp.x, dx_days: 0 });
                    }
                }
            }
        }
        if resp.dragged() {
            let ppd = self.ppd;
            if let Some(d) = self.gantt_drag.as_mut() {
                if let Some(p) = ui.input(|i| i.pointer.latest_pos()) {
                    d.dx_days = ((p.x - d.press_x) / ppd).round() as i64;
                }
            } else {
                let dd = resp.drag_delta();
                self.view_x0 -= dd.x as f64 / self.ppd as f64;
                self.scroll_y = (self.scroll_y - dd.y).clamp(0.0, max_scroll);
            }
        }
        if resp.drag_stopped() {
            if let Some(d) = self.gantt_drag.take() {
                self.finish_drag(d);
            }
        }
        if resp.clicked() {
            if let Some(p) = resp.interact_pointer_pos() {
                let mods = ui.input(|i| i.modifiers);
                match self.row_at(rect, p).and_then(|r| self.rows[r].task) {
                    Some(id) => self.select_click(id, mods),
                    None => {
                        if !(mods.shift || mods.command || mods.ctrl) {
                            self.selected.clear();
                        }
                    }
                }
            }
        }
        if let Some(h) = hover_pos {
            if self.gantt_drag.is_some() {
                ui.ctx().set_cursor_icon(if self.gantt_drag.as_ref().map_or(false, |d| d.resize) { CursorIcon::ResizeHorizontal } else { CursorIcon::Grabbing });
            } else if let Some((_, resize)) = self.gantt_hit(rect, h) {
                ui.ctx().set_cursor_icon(if resize { CursorIcon::ResizeHorizontal } else { CursorIcon::Grab });
            }
        }

        // ------------------------------------------------------------ paint (read-only)
        let painter = ui.painter_at(rect);
        painter.rect_filled(rect, 0.0, pal.bg);
        let ppd = self.ppd as f64;
        let x0c = self.view_x0;
        let x1c = x0c + rect.width() as f64 / ppd;
        let x = |c: f64| rect.left() + ((c - x0c) * ppd) as f32;
        let body_top = rect.top() + HEADER_H;
        let mode = if self.ppd >= 18.0 {
            Mode::Day
        } else if self.ppd >= 5.0 {
            Mode::Week
        } else {
            Mode::Month
        };
        let font = FontId::proportional(11.0);
        let small = FontId::proportional(9.5);

        let first_row = (self.scroll_y / ROW_H).floor() as usize;
        let last_row = (((self.scroll_y + body_h) / ROW_H).ceil() as usize).min(self.rows.len());
        let row_top = |i: usize| body_top + i as f32 * ROW_H - self.scroll_y;

        // row stripes / groups / selection (behind everything)
        let body_clip = Rect::from_min_max(Pos2::new(rect.left(), body_top), rect.right_bottom());
        let bp = painter.with_clip_rect(body_clip);
        for i in first_row..last_row {
            let r = Rect::from_min_size(Pos2::new(rect.left(), row_top(i)), Vec2::new(rect.width(), ROW_H));
            let row = &self.rows[i];
            if row.kind == RowKind::Group {
                bp.rect_filled(r, 0.0, pal.group);
                bp.text(Pos2::new(rect.left() + 8.0, r.center().y), Align2::LEFT_CENTER, &row.label, font.clone(), pal.text);
                continue;
            }
            if i % 2 == 1 {
                bp.rect_filled(r, 0.0, pal.stripe);
            }
            if row.task.map_or(false, |t| self.selected.contains(&t)) {
                bp.rect_filled(r, 0.0, Color32::from_rgba_unmultiplied(80, 140, 230, 38));
            }
        }

        // ---- header + grid + weekends ----
        let first = date_of_cal(x0c.floor() - 1.0);
        let last = date_of_cal(x1c.ceil() + 1.0);
        let hdr = Rect::from_min_size(rect.min, Vec2::new(rect.width(), HEADER_H));
        painter.rect_filled(hdr, 0.0, pal.header_bg);
        let half = HEADER_H / 2.0;
        let seg = |painter: &egui::Painter, c0: f64, c1: f64, y: f32, h: f32, label: &str, font: &FontId| {
            let r = Rect::from_min_max(Pos2::new(x(c0), y), Pos2::new(x(c1), y + h));
            let p = painter.with_clip_rect(r.intersect(hdr));
            p.text(Pos2::new(r.left() + 4.0, r.center().y), Align2::LEFT_CENTER, label, font.clone(), pal.text);
            painter.line_segment([Pos2::new(r.left(), y), Pos2::new(r.left(), y + h)], Stroke::new(1.0, pal.grid));
        };
        // top row
        match mode {
            Mode::Day | Mode::Week => {
                let mut m = month_start(first);
                let mut guard = 0;
                while m <= last && guard < 400 {
                    let n = next_month(m);
                    seg(&painter, cal_of(m), cal_of(n), rect.top(), half, &m.format("%B %Y").to_string(), &font);
                    if n == m {
                        break;
                    }
                    m = n;
                    guard += 1;
                }
            }
            Mode::Month => {
                let mut y = first.year();
                while y <= last.year() {
                    if let (Some(a), Some(b)) = (NaiveDate::from_ymd_opt(y, 1, 1), NaiveDate::from_ymd_opt(y + 1, 1, 1)) {
                        seg(&painter, cal_of(a), cal_of(b), rect.top(), half, &y.to_string(), &font);
                    }
                    y += 1;
                }
            }
        }
        // bottom row + body grid
        let grid_top = body_top;
        let grid_bottom = rect.bottom();
        let vline = |c: f64, strong: bool| {
            let xx = x(c);
            bp.line_segment([Pos2::new(xx, grid_top), Pos2::new(xx, grid_bottom)], Stroke::new(1.0, if strong { pal.grid } else { pal.grid.gamma_multiply(0.5) }));
        };
        match mode {
            Mode::Day => {
                let mut d = first;
                let mut guard = 0;
                while d <= last && guard < 800 {
                    let c = cal_of(d);
                    if !self.project.calendar.is_working(d) {
                        bp.rect_filled(Rect::from_min_max(Pos2::new(x(c), grid_top), Pos2::new(x(c + 1.0), grid_bottom)), 0.0, pal.weekend);
                    }
                    let lbl = if self.ppd >= 26.0 { format!("{} {}", &d.format("%a").to_string()[..1], d.day()) } else { d.day().to_string() };
                    seg(&painter, c, c + 1.0, rect.top() + half, half, &lbl, &small);
                    vline(c, d.weekday() == chrono::Weekday::Mon);
                    d = next_day(d);
                    guard += 1;
                }
            }
            Mode::Week => {
                let mut d = first;
                let mut guard = 0;
                while d <= last && guard < 800 {
                    let c = cal_of(d);
                    if !self.project.calendar.is_working(d) {
                        bp.rect_filled(Rect::from_min_max(Pos2::new(x(c), grid_top), Pos2::new(x(c + 1.0), grid_bottom)), 0.0, pal.weekend);
                    }
                    if d.weekday() == chrono::Weekday::Mon {
                        seg(&painter, c, c + 7.0, rect.top() + half, half, &d.format("%d").to_string(), &small);
                        vline(c, true);
                    }
                    d = next_day(d);
                    guard += 1;
                }
            }
            Mode::Month => {
                let mut m = month_start(first);
                let mut guard = 0;
                while m <= last && guard < 400 {
                    let n = next_month(m);
                    seg(&painter, cal_of(m), cal_of(n), rect.top() + half, half, &m.format("%b").to_string(), &small);
                    vline(cal_of(m), true);
                    if n == m {
                        break;
                    }
                    m = n;
                    guard += 1;
                }
            }
        }
        painter.line_segment([Pos2::new(rect.left(), rect.top() + half), Pos2::new(rect.right(), rect.top() + half)], Stroke::new(1.0, pal.grid));
        painter.line_segment([Pos2::new(rect.left(), body_top), Pos2::new(rect.right(), body_top)], Stroke::new(1.0, pal.grid.gamma_multiply(1.6)));

        // ---- dependency arrows (under the bars) ----
        let bar_mid = |i: usize| row_top(i) + 5.0 + 6.0;
        let edge_x = |ts: &crate::scheduler::TaskSched, finish: bool, milestone: bool| -> f32 {
            if milestone {
                // diamond half-width 6px
                if finish {
                    x(ts.start_cal) + 6.0
                } else {
                    x(ts.start_cal) - 6.0
                }
            } else if finish {
                x(ts.finish_cal).max(x(ts.start_cal) + 3.0)
            } else {
                x(ts.start_cal)
            }
        };
        for dep in &self.project.deps {
            let (Some(&ra), Some(&rb)) = (self.row_of.get(&dep.pred), self.row_of.get(&dep.succ)) else { continue };
            if ra.max(rb) < first_row || ra.min(rb) >= last_row {
                continue;
            }
            let (Some(a), Some(b)) = (self.schedule.tasks.get(&dep.pred), self.schedule.tasks.get(&dep.succ)) else { continue };
            let ma = self.project.task(dep.pred).map_or(false, |t| t.is_milestone);
            let mb = self.project.task(dep.succ).map_or(false, |t| t.is_milestone);
            let (from_f, to_f) = link_ends(dep.kind);
            let p = (edge_x(a, from_f, ma), bar_mid(ra));
            let q = (edge_x(b, to_f, mb), bar_mid(rb));
            let pts = route(p, from_f, q, to_f);
            let key = (dep.pred, dep.succ);
            let (color, w) = if self.schedule.violated.contains(&key) {
                (LINK_BAD, 2.4)
            } else if self.schedule.critical_links.contains(&key) {
                (LINK_CRIT, 1.5)
            } else {
                (LINK, 1.1)
            };
            let sel = self.selected.contains(&dep.pred) || self.selected.contains(&dep.succ);
            let stroke = Stroke::new(if sel { w + 0.6 } else { w }, color);
            let line: Vec<Pos2> = pts.iter().map(|(px, py)| Pos2::new(*px, *py)).collect();
            bp.add(Shape::line(line, stroke));
            // arrowhead
            let dir = arrow_dir(to_f);
            bp.add(Shape::convex_polygon(
                vec![Pos2::new(q.0, q.1), Pos2::new(q.0 - 6.0 * dir, q.1 - 3.5), Pos2::new(q.0 - 6.0 * dir, q.1 + 3.5)],
                color,
                Stroke::NONE,
            ));
        }

        // ---- bars ----
        for i in first_row..last_row {
            let Some(id) = self.rows[i].task else { continue };
            let (Some(t), Some(ts)) = (self.project.task(id), self.schedule.tasks.get(&id)) else { continue };
            let top = row_top(i);
            let (x0, x1) = (x(ts.start_cal), x(ts.finish_cal));
            if x1 < rect.left() - 300.0 || x0 > rect.right() + 10.0 {
                continue;
            }
            // baseline: thin grey strip under the bar
            if let Some((bs, bf)) = self.project.baseline.as_ref().and_then(|b| b.entries.get(&id)) {
                let r = Rect::from_min_max(Pos2::new(x(*bs), top + ROW_H - 8.0), Pos2::new(x(*bf).max(x(*bs) + 2.0), top + ROW_H - 5.0));
                bp.rect_filled(r, 1.0, Color32::from_rgb(140, 142, 148));
            }
            let (base, dark_c) = if ts.critical { (CRIT, CRIT_DARK) } else { (NORMAL, NORMAL_DARK) };
            if ts.is_summary {
                let w = (x1 - x0).max(3.0);
                let r = Rect::from_min_size(Pos2::new(x0, top + 6.0), Vec2::new(w, 6.0));
                let col = if dark { Color32::from_rgb(200, 200, 205) } else { Color32::from_rgb(40, 40, 45) };
                bp.rect_filled(r, 1.0, col.gamma_multiply(0.55));
                bp.rect_filled(Rect::from_min_size(r.min, Vec2::new(w * ts.progress as f32, 6.0)), 1.0, col);
                for (ex, s) in [(x0, 1.0f32), (x0 + w, -1.0)] {
                    bp.add(Shape::convex_polygon(vec![Pos2::new(ex, top + 6.0), Pos2::new(ex + 8.0 * s, top + 6.0), Pos2::new(ex, top + 15.0)], col, Stroke::NONE));
                }
            } else if t.is_milestone {
                let c = Pos2::new(x0, top + 11.0);
                let col = if ts.critical { CRIT } else if dark { Color32::from_rgb(235, 235, 240) } else { Color32::from_rgb(25, 25, 30) };
                bp.add(Shape::convex_polygon(vec![Pos2::new(c.x - 7.0, c.y), Pos2::new(c.x, c.y - 7.0), Pos2::new(c.x + 7.0, c.y), Pos2::new(c.x, c.y + 7.0)], col, Stroke::NONE));
                bp.text(Pos2::new(c.x + 11.0, c.y), Align2::LEFT_CENTER, &t.name, font.clone(), pal.text);
            } else {
                let w = (x1 - x0).max(3.0);
                let r = Rect::from_min_size(Pos2::new(x0, top + 5.0), Vec2::new(w, 12.0));
                bp.rect_filled(r, 2.0, base);
                // progress fill: a darker rectangle over the left portion
                let pw = (w * ts.progress as f32).clamp(0.0, w);
                if pw > 0.5 {
                    bp.rect_filled(Rect::from_min_size(r.min, Vec2::new(pw, 12.0)), 2.0, dark_c);
                }
                let names = self.project.resource_names(id);
                let label = if names.is_empty() { String::new() } else { names };
                if !label.is_empty() {
                    bp.text(Pos2::new(r.right() + 6.0, r.center().y), Align2::LEFT_CENTER, label, small.clone(), pal.text.gamma_multiply(0.85));
                }
                if ts.constraint_violated {
                    bp.text(Pos2::new(r.left() - 4.0, r.center().y), Align2::RIGHT_CENTER, "!", font.clone(), Color32::RED);
                }
            }
        }

        // ---- drag ghost ----
        if let Some(d) = &self.gantt_drag {
            if let (Some(ts), Some(&ri), Some(t)) = (self.schedule.tasks.get(&d.task), self.row_of.get(&d.task), self.project.task(d.task)) {
                let top = row_top(ri);
                let off = (d.dx_days as f64 * ppd) as f32;
                let (gx0, gx1) = if d.resize { (x(ts.start_cal), (x(ts.finish_cal) + off).max(x(ts.start_cal) + 4.0)) } else { (x(ts.start_cal) + off, x(ts.finish_cal) + off) };
                let r = Rect::from_min_max(Pos2::new(gx0, top + 4.0), Pos2::new(gx1.max(gx0 + 4.0), top + ROW_H - 4.0));
                bp.rect_filled(r, 2.0, Color32::from_rgba_unmultiplied(255, 200, 60, 70));
                bp.rect_stroke(r, 2.0, Stroke::new(1.5, Color32::from_rgb(255, 190, 40)), egui::StrokeKind::Outside);
                let msg = if d.resize {
                    let nf = ts.finish + Duration::days(d.dx_days);
                    format!("finish \u{2192} {}", nf.format("%a %d %b"))
                } else {
                    let ns = ts.start + Duration::days(d.dx_days);
                    format!("{}: start \u{2192} {}", t.name, ns.format("%a %d %b"))
                };
                bp.text(Pos2::new(gx0, top - 2.0), Align2::LEFT_BOTTOM, msg, font.clone(), pal.text);
            }
        }

        // ---- today marker ----
        let today = chrono::Local::now().date_naive();
        let tx = x(cal_of(today) + 0.5);
        if tx >= rect.left() && tx <= rect.right() {
            let col = Color32::from_rgb(229, 72, 77);
            painter.line_segment([Pos2::new(tx, rect.top() + half), Pos2::new(tx, rect.bottom())], Stroke::new(1.5, col));
            painter.add(Shape::convex_polygon(vec![Pos2::new(tx - 5.0, body_top - 6.0), Pos2::new(tx + 5.0, body_top - 6.0), Pos2::new(tx, body_top)], col, Stroke::NONE));
        }
        painter.rect_stroke(rect, 0.0, Stroke::new(1.0, pal.grid), egui::StrokeKind::Inside);
    }
}
