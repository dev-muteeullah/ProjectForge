//! Bottom pane: context-sensitive details.
//! * Task tab: edit the selected task, its links (type + lag), constraint and assignments.
//! * Resources tab: manage resources; selecting one shows its workload histogram
//!   (red bars = demand above Max Units, i.e. overallocation).

use crate::app::*;
use crate::commands::Command;
use crate::model::*;
use crate::parse::*;
use crate::scheduler::resource_histogram;
use eframe::egui;
use egui::{Align2, Color32, FontId, Pos2, Rect, RichText, Sense, Stroke, Vec2};

impl App {
    /// Text field that keeps its own buffer while focused and returns the new text when committed.
    fn text_field(&mut self, ui: &mut egui::Ui, key: &str, current: &str, width: f32, multiline: bool) -> Option<String> {
        let editing = self.field.key.as_deref() == Some(key);
        let mut text = if editing { self.field.buf.clone() } else { current.to_string() };
        let r = if multiline {
            ui.add(egui::TextEdit::multiline(&mut text).desired_width(width).desired_rows(2))
        } else {
            ui.add(egui::TextEdit::singleline(&mut text).desired_width(width))
        };
        if r.has_focus() {
            self.field.key = Some(key.to_string());
            self.field.buf = text;
            return None;
        }
        if r.lost_focus() && editing {
            self.field.key = None;
            let esc = ui.input(|i| i.key_pressed(egui::Key::Escape));
            if !esc && text != current {
                return Some(self.field.buf.clone());
            }
        }
        None
    }

    pub fn ui_detail(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.selectable_value(&mut self.detail_tab, DetailTab::Task, "Task details");
            ui.selectable_value(&mut self.detail_tab, DetailTab::Resources, "Resources & workload");
        });
        ui.separator();
        match self.detail_tab {
            DetailTab::Task => self.ui_task_detail(ui),
            DetailTab::Resources => self.ui_resources(ui),
        }
    }

    fn ui_task_detail(&mut self, ui: &mut egui::Ui) {
        let Some(id) = self.primary() else {
            ui.label("Select a task in the sheet or chart to see its details.  Tip: Ctrl/Shift-click selects several tasks to link them.");
            return;
        };
        let Some(t) = self.project.task(id).cloned() else { return };
        let ts = self.schedule.tasks.get(&id).cloned();
        let mpd = self.mpd();
        let is_summary = ts.as_ref().map_or(false, |x| x.is_summary);

        egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
            ui.horizontal_top(|ui| {
                // ---- column 1: basics
                ui.vertical(|ui| {
                    ui.set_width(330.0);
                    ui.label(RichText::new(format!("#{}  WBS {}", t.id, t.wbs)).weak());
                    ui.horizontal(|ui| {
                        ui.label("Name");
                        if let Some(v) = self.text_field(ui, &format!("name{}", id), &t.name, 250.0, false) {
                            if let Err(e) = self.commit_cell(id, Col::Name, &v) {
                                self.toast_err(e);
                            }
                        }
                    });
                    ui.label("Notes");
                    if let Some(v) = self.text_field(ui, &format!("desc{}", id), &t.description, 320.0, true) {
                        self.exec(Command::SetTaskDescription { id, old: t.description.clone(), new: v });
                    }
                    ui.horizontal(|ui| {
                        ui.label("Duration");
                        ui.add_enabled_ui(!is_summary, |ui| {
                            let cur = self.cell_text(id, Col::Duration);
                            if let Some(v) = self.text_field(ui, &format!("dur{}", id), &cur, 70.0, false) {
                                if let Err(e) = self.commit_cell(id, Col::Duration, &v) {
                                    self.toast_err(e);
                                }
                            }
                            let mut ms = t.is_milestone;
                            if ui.checkbox(&mut ms, "Milestone").changed() {
                                self.exec(Command::SetDuration { id, old: (t.is_milestone, t.duration_min), new: (ms, if ms { t.duration_min } else { t.duration_min.max(mpd) }) });
                            }
                        });
                    });
                    ui.horizontal(|ui| {
                        ui.label("Progress");
                        let mut pct = ((ts.as_ref().map_or(t.progress, |x| x.progress)) * 100.0).round() as f32;
                        ui.add_enabled_ui(!is_summary, |ui| {
                            let r = ui.add(egui::Slider::new(&mut pct, 0.0..=100.0).suffix("%").step_by(5.0));
                            if r.drag_stopped() || (r.changed() && !r.dragged()) {
                                self.exec(Command::SetProgress { id, old: t.progress, new: (pct / 100.0) as f64 });
                            }
                        });
                    });
                });
                ui.separator();
                // ---- column 2: schedule + constraint
                ui.vertical(|ui| {
                    ui.set_width(330.0);
                    if let Some(s) = &ts {
                        ui.label(format!("Start {}   Finish {}", s.start.format("%a %Y-%m-%d"), s.finish.format("%a %Y-%m-%d")));
                        ui.label(format!("Total float {}   Free float {}", format_duration(s.total_float.max(0), mpd), format_duration(s.free_float, mpd)));
                        if s.critical {
                            ui.label(RichText::new("On the critical path").color(Color32::from_rgb(200, 60, 60)));
                        }
                        if s.constraint_violated {
                            ui.colored_label(Color32::RED, "Constraint cannot be met by the schedule");
                        }
                        if t.leveling_delay_days > 0 {
                            ui.label(format!("Leveling delay: {} day(s)", t.leveling_delay_days));
                        }
                    }
                    ui.add_enabled_ui(!is_summary, |ui| {
                        let mut kind = t.constraint.kind;
                        let mut date = t.constraint.date.unwrap_or(t.start);
                        let mut changed = false;
                        ui.horizontal(|ui| {
                            ui.label("Constraint");
                            egui::ComboBox::from_id_salt("cons_kind").selected_text(kind.label()).show_ui(ui, |ui| {
                                for k in ConstraintKind::ALL {
                                    if ui.selectable_value(&mut kind, k, k.label()).changed() {
                                        changed = true;
                                    }
                                }
                            });
                        });
                        if kind.needs_date() {
                            ui.horizontal(|ui| {
                                ui.label("Date");
                                if crate::ui_shell::date_picker(ui, "cons_date", &mut date) {
                                    changed = true;
                                }
                            });
                        }
                        if changed {
                            let new = Constraint { kind, date: if kind.needs_date() { Some(date) } else { None } };
                            if new != t.constraint {
                                self.exec(Command::SetConstraint { id, old: t.constraint, new });
                            }
                        }
                    });
                });
                ui.separator();
                // ---- column 3: links
                ui.vertical(|ui| {
                    ui.set_width(380.0);
                    ui.label(RichText::new("Predecessors").strong());
                    let preds: Vec<Dependency> = self.project.deps.iter().filter(|d| d.succ == id).cloned().collect();
                    let mut change: Option<(Dependency, Dependency)> = None;
                    let mut remove: Option<Dependency> = None;
                    for d in &preds {
                        ui.horizontal(|ui| {
                            let bad = self.schedule.violated.contains(&(d.pred, d.succ));
                            let name = self.task_name(d.pred);
                            let mut l = RichText::new(format!("#{} {}", d.pred, name));
                            if bad {
                                l = l.color(Color32::RED);
                            }
                            ui.label(l);
                            let mut kind = d.kind;
                            egui::ComboBox::from_id_salt(("pk", d.pred, d.succ)).width(54.0).selected_text(kind.as_str()).show_ui(ui, |ui| {
                                for k in DepKind::ALL {
                                    ui.selectable_value(&mut kind, k, k.as_str());
                                }
                            });
                            let cur = format_lag(d.lag_min, mpd);
                            if let Some(v) = self.text_field(ui, &format!("lag{}_{}", d.pred, d.succ), &cur, 54.0, false) {
                                match parse_lag(&v, mpd) {
                                    Ok(lag) => change = Some((d.clone(), Dependency { lag_min: lag, ..d.clone() })),
                                    Err(e) => self.toast_err(e),
                                }
                            }
                            if kind != d.kind {
                                change = Some((d.clone(), Dependency { kind, ..d.clone() }));
                            }
                            if ui.small_button("\u{2715}").on_hover_text("Remove link").clicked() {
                                remove = Some(d.clone());
                            }
                            if bad {
                                ui.label(RichText::new("violated by a constraint").color(Color32::RED).small());
                            }
                        });
                    }
                    if let Some((old, new)) = change {
                        self.exec(Command::ChangeDependency { old, new });
                    }
                    if let Some(d) = remove {
                        self.exec(Command::RemoveDependency(d));
                    }
                    ui.add_enabled_ui(!is_summary, |ui| {
                        let mut pick = self.pred_pick;
                        egui::ComboBox::from_id_salt("add_pred").selected_text("+ add predecessor").show_ui(ui, |ui| {
                            for o in &self.project.tasks {
                                if o.id != id && !self.project.has_children(o.id) {
                                    ui.selectable_value(&mut pick, Some(o.id), format!("#{} {}", o.id, o.name));
                                }
                            }
                        });
                        self.pred_pick = None;
                        if let Some(p) = pick {
                            self.exec(Command::AddDependency(Dependency { pred: p, succ: id, kind: DepKind::FS, lag_min: 0 }));
                        }
                    });
                    let succs: Vec<String> = self.project.deps.iter().filter(|d| d.pred == id).map(|d| format!("#{} {} ({}{})", d.succ, self.task_name(d.succ), d.kind.as_str(), format_lag(d.lag_min, mpd))).collect();
                    if !succs.is_empty() {
                        ui.label(RichText::new("Successors").strong());
                        for s in succs {
                            ui.label(s);
                        }
                    }
                });
                ui.separator();
                // ---- column 4: assignments
                ui.vertical(|ui| {
                    ui.set_width(300.0);
                    ui.label(RichText::new("Resource assignments").strong());
                    let assigns: Vec<Assignment> = self.project.assignments_of(id).cloned().collect();
                    let mut remove = None;
                    let mut set = None;
                    for a in &assigns {
                        ui.horizontal(|ui| {
                            let name = self.project.resource(a.resource).map(|r| r.name.clone()).unwrap_or_default();
                            ui.label(name);
                            let mut pct = (a.units * 100.0).round();
                            let r = ui.add(egui::DragValue::new(&mut pct).range(1.0..=1000.0).suffix("%"));
                            if r.drag_stopped() || (r.changed() && !r.dragged()) {
                                set = Some((a.clone(), pct / 100.0));
                            }
                            if ui.small_button("\u{2715}").clicked() {
                                remove = Some(a.clone());
                            }
                        });
                    }
                    if let Some((a, u)) = set {
                        self.exec(Command::SetAssignment { task: a.task, resource: a.resource, old: Some(a.units), new: Some(u) });
                    }
                    if let Some(a) = remove {
                        self.exec(Command::SetAssignment { task: a.task, resource: a.resource, old: Some(a.units), new: None });
                    }
                    let mut pick: Option<ResourceId> = None;
                    egui::ComboBox::from_id_salt("add_res").selected_text("+ assign resource").show_ui(ui, |ui| {
                        for r in &self.project.resources {
                            if !assigns.iter().any(|a| a.resource == r.id) {
                                ui.selectable_value(&mut pick, Some(r.id), r.name.clone());
                            }
                        }
                    });
                    if let Some(r) = pick {
                        self.exec(Command::SetAssignment { task: id, resource: r, old: None, new: Some(1.0) });
                    }
                    ui.label(RichText::new("Or type names in the sheet, e.g.  Alice[50%], Bob").weak().small());
                });
            });
        });
    }

    fn ui_resources(&mut self, ui: &mut egui::Ui) {
        ui.horizontal_top(|ui| {
            ui.vertical(|ui| {
                ui.set_width(520.0);
                let resources = self.project.resources.clone();
                let mut remove: Option<usize> = None;
                let mut edit: Option<(Resource, Resource)> = None;
                egui::ScrollArea::vertical().id_salt("res_list").max_height(ui.available_height() - 30.0).show(ui, |ui| {
                    for (i, r) in resources.iter().enumerate() {
                        ui.horizontal(|ui| {
                            let sel = self.selected_resource == Some(r.id);
                            if ui.selectable_label(sel, "\u{1F4CA}").on_hover_text("Show workload").clicked() {
                                self.selected_resource = Some(r.id);
                            }
                            if let Some(v) = self.text_field(ui, &format!("rname{}", r.id), &r.name, 110.0, false) {
                                if v.trim().is_empty() {
                                    self.toast_err("Resource name cannot be empty");
                                } else if self.project.resources.iter().any(|o| o.id != r.id && o.name.eq_ignore_ascii_case(v.trim())) {
                                    self.toast_err("A resource with that name already exists");
                                } else {
                                    edit = Some((r.clone(), Resource { name: v.trim().to_string(), ..r.clone() }));
                                }
                            }
                            let mut kind = r.kind;
                            egui::ComboBox::from_id_salt(("rk", r.id)).width(76.0).selected_text(kind.as_str()).show_ui(ui, |ui| {
                                for k in ResourceKind::ALL {
                                    ui.selectable_value(&mut kind, k, k.as_str());
                                }
                            });
                            if kind != r.kind {
                                edit = Some((r.clone(), Resource { kind, ..r.clone() }));
                            }
                            let mut mu = (r.max_units * 100.0).round();
                            let resp = ui.add(egui::DragValue::new(&mut mu).range(0.0..=10000.0).suffix("% max"));
                            if resp.drag_stopped() || (resp.changed() && !resp.dragged()) {
                                edit = Some((r.clone(), Resource { max_units: mu / 100.0, ..r.clone() }));
                            }
                            let mut rate = r.std_rate;
                            let resp = ui.add(egui::DragValue::new(&mut rate).range(0.0..=1.0e6).speed(0.5).prefix("rate "));
                            if resp.drag_stopped() || (resp.changed() && !resp.dragged()) {
                                edit = Some((r.clone(), Resource { std_rate: rate, ..r.clone() }));
                            }
                            if ui.small_button("\u{2715}").on_hover_text("Delete resource").clicked() {
                                remove = Some(i);
                            }
                        });
                    }
                });
                ui.horizontal(|ui| {
                    ui.add(egui::TextEdit::singleline(&mut self.new_resource_name).hint_text("new resource name").desired_width(160.0));
                    if ui.button("Add resource").clicked() {
                        let name = self.new_resource_name.trim().to_string();
                        if name.is_empty() {
                            self.toast_err("Enter a name for the new resource");
                        } else if self.project.resource_by_name(&name).is_some() {
                            self.toast_err("A resource with that name already exists");
                        } else {
                            let id = self.project.alloc_resource_id();
                            let index = self.project.resources.len();
                            if self.exec(Command::InsertResource { index, resource: Resource { id, name, kind: ResourceKind::Work, max_units: 1.0, std_rate: 0.0 } }) {
                                self.new_resource_name.clear();
                                self.selected_resource = Some(id);
                            }
                        }
                    }
                });
                if let Some((old, new)) = edit {
                    self.exec(Command::SetResource { old, new });
                }
                if let Some(i) = remove {
                    if let Some(r) = self.project.resources.get(i).cloned() {
                        let assigns = self.project.assignments.iter().filter(|a| a.resource == r.id).cloned().collect();
                        if self.selected_resource == Some(r.id) {
                            self.selected_resource = None;
                        }
                        self.exec(Command::DeleteResource { index: i, resource: r, assigns });
                    }
                }
            });
            ui.separator();
            ui.vertical(|ui| self.ui_histogram(ui));
        });
    }

    fn ui_histogram(&mut self, ui: &mut egui::Ui) {
        let Some(rid) = self.selected_resource.and_then(|r| self.project.resource(r).cloned()) else {
            ui.label("Select a resource (\u{1F4CA}) to see its workload over time.");
            return;
        };
        let data = resource_histogram(&self.project, &self.schedule, rid.id);
        ui.label(RichText::new(format!("Workload: {}   (max {}%)", rid.name, (rid.max_units * 100.0).round())).strong());
        if data.is_empty() {
            ui.label("No assigned work.");
            return;
        }
        let size = Vec2::new(ui.available_width().max(200.0), (ui.available_height() - 6.0).max(80.0));
        let (rect, resp) = ui.allocate_exact_size(size, Sense::hover());
        let p = ui.painter_at(rect);
        let dark = ui.visuals().dark_mode;
        p.rect_filled(rect, 2.0, if dark { Color32::from_rgb(30, 30, 34) } else { Color32::from_rgb(250, 250, 252) });
        let peak = data.iter().map(|(_, u)| *u).fold(rid.max_units, f64::max).max(0.1) * 1.15;
        let n = data.len();
        let bw = ((rect.width() - 40.0) / n as f32).clamp(2.0, 28.0);
        let base_y = rect.bottom() - 14.0;
        let h = rect.height() - 24.0;
        let y_of = |u: f64| base_y - (u / peak) as f32 * h;
        let mut hover_text = None;
        for (i, (d, u)) in data.iter().enumerate() {
            let x0 = rect.left() + 34.0 + i as f32 * bw;
            let over = *u > rid.max_units + 1e-9;
            let col = if over { Color32::from_rgb(214, 69, 65) } else { Color32::from_rgb(66, 133, 207) };
            let r = Rect::from_min_max(Pos2::new(x0, y_of(*u)), Pos2::new(x0 + bw - 1.0, base_y));
            p.rect_filled(r, 1.0, col);
            if bw >= 14.0 {
                p.text(Pos2::new(x0 + bw / 2.0, base_y + 2.0), Align2::CENTER_TOP, d.format("%d").to_string(), FontId::proportional(8.5), ui.visuals().weak_text_color());
            }
            if let Some(h) = resp.hover_pos() {
                if h.x >= x0 && h.x < x0 + bw {
                    hover_text = Some(format!("{}: {}%{}", d.format("%a %Y-%m-%d"), (u * 100.0).round(), if over { "  \u{26A0} overallocated" } else { "" }));
                }
            }
        }
        let ym = y_of(rid.max_units);
        p.line_segment([Pos2::new(rect.left() + 30.0, ym), Pos2::new(rect.right() - 4.0, ym)], Stroke::new(1.5, Color32::from_rgb(230, 160, 30)));
        p.text(Pos2::new(rect.left() + 28.0, ym), Align2::RIGHT_CENTER, format!("{}%", (rid.max_units * 100.0).round()), FontId::proportional(9.0), ui.visuals().text_color());
        p.text(Pos2::new(rect.left() + 28.0, base_y), Align2::RIGHT_CENTER, "0", FontId::proportional(9.0), ui.visuals().text_color());
        if let Some(t) = hover_text {
            resp.on_hover_text(t);
        }
    }
}
