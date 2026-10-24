//! Menus, toolbar, status bar, modal dialogs and toasts.

use crate::app::*;
use crate::model::*;
use crate::view::StatusFilter;
use eframe::egui;
use egui::{Align2, Color32, Context, RichText};

/// DatePickerButton works on jiff dates; the rest of the app uses chrono. Returns true if changed.
pub fn date_picker(ui: &mut egui::Ui, salt: &str, d: &mut chrono::NaiveDate) -> bool {
    use chrono::Datelike;
    let Ok(mut j) = jiff::civil::Date::new(d.year() as i16, d.month() as i8, d.day() as i8) else { return false };
    let before = j;
    ui.add(egui_extras::DatePickerButton::new(&mut j).id_salt(salt));
    if j != before {
        if let Some(n) = chrono::NaiveDate::from_ymd_opt(j.year() as i32, j.month() as u32, j.day() as u32) {
            *d = n;
            return true;
        }
    }
    false
}

impl App {
    pub fn ui_menu(&mut self, ui: &mut egui::Ui) {
        egui::MenuBar::new().ui(ui, |ui| {
            ui.menu_button("File", |ui| {
                if ui.button("New project").clicked() {
                    self.request(Pending::New);
                    ui.close();
                }
                if ui.button("Open\u{2026}").clicked() {
                    let start = self.path.as_ref().map(|p| p.display().to_string()).unwrap_or_else(|| "project.pcraft".into());
                    self.dialog = Some(Dialog::Open(start));
                    ui.close();
                }
                if ui.button("Save   (Ctrl+S)").clicked() {
                    self.save();
                    ui.close();
                }
                if ui.button("Save as\u{2026}").clicked() {
                    let p = self.path.as_ref().map(|p| p.display().to_string()).unwrap_or_else(|| "project.pcraft".into());
                    self.dialog = Some(Dialog::SaveAs { path: p, then_close: false });
                    ui.close();
                }
                ui.separator();
                if ui.button("Export Gantt as SVG\u{2026}").clicked() {
                    self.dialog = Some(Dialog::ExportSvg("gantt.svg".into()));
                    ui.close();
                }
                ui.separator();
                if ui.button("Load sample project").clicked() {
                    self.request(Pending::Sample);
                    ui.close();
                }
                if ui.button("Load 1,000-task stress project").clicked() {
                    self.request(Pending::Stress);
                    ui.close();
                }
                ui.separator();
                if ui.button("Quit").clicked() {
                    ui.ctx().send_viewport_cmd(egui::ViewportCommand::Close);
                    ui.close();
                }
            });
            ui.menu_button("Edit", |ui| {
                let ul = self.history.undo_label();
                let rl = self.history.redo_label();
                if ui.add_enabled(ul.is_some(), egui::Button::new(format!("Undo {}", ul.clone().unwrap_or_default()))).clicked() {
                    self.undo();
                    ui.close();
                }
                if ui.add_enabled(rl.is_some(), egui::Button::new(format!("Redo {}", rl.clone().unwrap_or_default()))).clicked() {
                    self.redo();
                    ui.close();
                }
                ui.separator();
                if ui.button("Delete selected tasks").clicked() {
                    self.delete_selected();
                    ui.close();
                }
            });
            ui.menu_button("Project", |ui| {
                if ui.button("Project info & calendar\u{2026}").clicked() {
                    self.dialog = Some(Dialog::Project(ProjectDraft { meta: self.project.meta(), new_holiday: self.project.start_date }));
                    ui.close();
                }
                ui.separator();
                if ui.button("Set baseline (snapshot current schedule)").clicked() {
                    self.set_baseline();
                    ui.close();
                }
                if ui.add_enabled(self.project.baseline.is_some(), egui::Button::new("Clear baseline")).clicked() {
                    self.clear_baseline();
                    ui.close();
                }
                ui.separator();
                if ui.button("Level resources").clicked() {
                    self.level_resources();
                    ui.close();
                }
                if ui.button("Clear leveling delays").clicked() {
                    self.clear_leveling();
                    ui.close();
                }
            });
            ui.menu_button("View", |ui| {
                if ui.button("Zoom in").clicked() {
                    self.ppd = (self.ppd * 1.4).min(90.0);
                }
                if ui.button("Zoom out").clicked() {
                    self.ppd = (self.ppd / 1.4).max(1.5);
                }
                if ui.button("Day / Week / Month presets: Day").clicked() {
                    self.ppd = 30.0;
                }
                if ui.button("Week view").clicked() {
                    self.ppd = 10.0;
                }
                if ui.button("Month view").clicked() {
                    self.ppd = 3.0;
                }
                if ui.button("Go to today").clicked() {
                    self.goto_today();
                }
                if ui.button("Go to project start").clicked() {
                    self.view_x0 = self.schedule.project_start_cal - 2.0;
                }
            });
        });
    }

    pub fn goto_today(&mut self) {
        let today = chrono::Local::now().date_naive();
        self.view_x0 = crate::calendar::cal_of(today) - 4.0;
    }

    pub fn ui_toolbar(&mut self, ui: &mut egui::Ui) {
        ui.horizontal_wrapped(|ui| {
            if ui.button("\u{2795} Task").on_hover_text("Add a task below the selection").clicked() {
                self.add_task(false);
            }
            if ui.button("\u{21B3} Sub-task").on_hover_text("Add a child of the selected task").clicked() {
                self.add_task(true);
            }
            if ui.button("\u{1F5D1} Delete").on_hover_text("Delete selected tasks (Del)").clicked() {
                self.delete_selected();
            }
            ui.separator();
            if ui.button("\u{2192} Indent").clicked() {
                self.indent(false);
            }
            if ui.button("\u{2190} Outdent").clicked() {
                self.indent(true);
            }
            ui.separator();
            if ui.button("\u{26D3} Link (FS)").on_hover_text("Link the selected tasks in selection order (Ctrl/Shift-click to multi-select)").clicked() {
                self.link_selected(DepKind::FS);
            }
            if ui.button("Unlink").on_hover_text("Remove links between selected tasks (or a single task's predecessors)").clicked() {
                self.unlink_selected();
            }
            ui.separator();
            if ui.add_enabled(self.history.can_undo(), egui::Button::new("\u{21B6} Undo")).clicked() {
                self.undo();
            }
            if ui.add_enabled(self.history.can_redo(), egui::Button::new("\u{21B7} Redo")).clicked() {
                self.redo();
            }
            ui.separator();
            if ui.button("\u{1F50D}+").on_hover_text("Zoom in (Ctrl+wheel over the chart)").clicked() {
                self.ppd = (self.ppd * 1.4).min(90.0);
            }
            if ui.button("\u{1F50D}\u{2212}").clicked() {
                self.ppd = (self.ppd / 1.4).max(1.5);
            }
            for (label, v) in [("Day", 30.0), ("Week", 10.0), ("Month", 3.0)] {
                let active = (self.ppd - v).abs() < 0.01;
                if ui.selectable_label(active, label).clicked() {
                    self.ppd = v;
                }
            }
            if ui.button("Today").clicked() {
                self.goto_today();
            }
            ui.separator();

            let before = (self.opts.status, self.opts.resource, self.opts.group_by_resource);
            ui.label("Filter:");
            egui::ComboBox::from_id_salt("flt_status").selected_text(self.opts.status.label()).show_ui(ui, |ui| {
                for s in StatusFilter::ALL {
                    ui.selectable_value(&mut self.opts.status, s, s.label());
                }
            });
            let res_label = self.opts.resource.and_then(|r| self.project.resource(r).map(|x| x.name.clone())).unwrap_or_else(|| "Any resource".into());
            egui::ComboBox::from_id_salt("flt_res").selected_text(res_label).show_ui(ui, |ui| {
                ui.selectable_value(&mut self.opts.resource, None, "Any resource");
                for r in &self.project.resources {
                    ui.selectable_value(&mut self.opts.resource, Some(r.id), r.name.clone());
                }
            });
            ui.checkbox(&mut self.opts.group_by_resource, "Group by resource");
            if before != (self.opts.status, self.opts.resource, self.opts.group_by_resource) {
                self.rows_dirty = true;
                self.scroll_y = 0.0;
            }
        });
    }

    pub fn ui_status(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            let p = &self.project;
            let crit = self.schedule.tasks.values().filter(|t| t.critical && !t.is_summary).count();
            let finish = crate::calendar::date_of_cal(self.schedule.project_finish_cal - 0.001);
            let start = crate::calendar::date_of_cal(self.schedule.project_start_cal);
            ui.label(format!("{} tasks \u{2022} {} links \u{2022} {} resources", p.tasks.len(), p.deps.len(), p.resources.len()));
            ui.separator();
            ui.label(format!("{} \u{2192} {}", start.format("%a %d %b %Y"), finish.format("%a %d %b %Y")));
            ui.separator();
            ui.label(RichText::new(format!("{} critical", crit)).color(Color32::from_rgb(200, 60, 60)));
            if !self.schedule.violated.is_empty() {
                ui.separator();
                ui.label(RichText::new(format!("\u{26A0} {} violated link(s)", self.schedule.violated.len())).color(Color32::RED));
            }
            if let Some(b) = &p.baseline {
                ui.separator();
                ui.label(format!("baseline {}", b.saved_at));
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.label(format!("{:.1} ms/frame", self.frame_ms));
                if self.dirty {
                    ui.label(RichText::new("\u{25CF} unsaved changes").color(Color32::from_rgb(210, 140, 20)));
                }
            });
        });
    }

    pub fn ui_toasts(&mut self, ctx: &Context) {
        let now = ctx.input(|i| i.time);
        for t in &mut self.toasts {
            if t.until == f64::MAX {
                t.until = now + if t.error { 9.0 } else { 3.5 };
            }
        }
        self.toasts.retain(|t| t.until > now);
        if self.toasts.is_empty() {
            return;
        }
        ctx.request_repaint_after(std::time::Duration::from_millis(250));
        let mut dismiss: Option<usize> = None;
        egui::Area::new(egui::Id::new("toasts")).anchor(Align2::RIGHT_BOTTOM, [-14.0, -40.0]).order(egui::Order::Foreground).show(ctx, |ui| {
            ui.with_layout(egui::Layout::top_down(egui::Align::Max), |ui| {
                for (i, t) in self.toasts.iter().enumerate().rev().take(5) {
                    let (bg, fg) = if t.error { (Color32::from_rgb(120, 30, 30), Color32::WHITE) } else { (Color32::from_rgb(40, 60, 90), Color32::WHITE) };
                    egui::Frame::new().fill(bg).corner_radius(6.0).inner_margin(8.0).show(ui, |ui| {
                        ui.set_max_width(460.0);
                        ui.horizontal(|ui| {
                            ui.label(RichText::new(if t.error { "\u{26A0}" } else { "\u{2139}" }).color(fg));
                            ui.label(RichText::new(&t.msg).color(fg));
                            if ui.small_button("\u{2715}").clicked() {
                                dismiss = Some(i);
                            }
                        });
                    });
                    ui.add_space(4.0);
                }
            });
        });
        if let Some(i) = dismiss {
            if i < self.toasts.len() {
                self.toasts.remove(i);
            }
        }
    }

    pub fn ui_dialogs(&mut self, ctx: &Context) {
        let Some(mut dialog) = self.dialog.take() else { return };
        let mut keep = true;
        let mut after: Option<Box<dyn FnOnce(&mut App)>> = None;

        let title = match &dialog {
            Dialog::Open(_) => "Open project",
            Dialog::SaveAs { .. } => "Save project as",
            Dialog::ExportSvg(_) => "Export Gantt chart (SVG)",
            Dialog::ConfirmClose => "Unsaved changes",
            Dialog::ConfirmDiscard(_) => "Unsaved changes",
            Dialog::Project(_) => "Project info & calendar",
            Dialog::LevelReport(_) => "Resource leveling",
        };
        egui::Window::new(title).collapsible(false).resizable(false).anchor(Align2::CENTER_CENTER, [0.0, 0.0]).show(ctx, |ui| match &mut dialog {
            Dialog::Open(path) => {
                ui.label("Path of a .pcraft project file:");
                let r = ui.add(egui::TextEdit::singleline(path).desired_width(420.0));
                let enter = r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
                ui.horizontal(|ui| {
                    if ui.button("Open").clicked() || enter {
                        let p = path.clone();
                        after = Some(Box::new(move |a: &mut App| a.request(Pending::Open(p))));
                        keep = false;
                    }
                    if ui.button("Cancel").clicked() {
                        keep = false;
                    }
                });
            }
            Dialog::SaveAs { path, then_close } => {
                ui.label("Save to (SQLite file, .pcraft):");
                let r = ui.add(egui::TextEdit::singleline(path).desired_width(420.0));
                let enter = r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
                ui.horizontal(|ui| {
                    if ui.button("Save").clicked() || enter {
                        let p = std::path::PathBuf::from(path.trim());
                        let close = *then_close;
                        after = Some(Box::new(move |a: &mut App| {
                            if a.save_to(p) && close {
                                a.allow_close = true;
                                a.close_requested_again = true;
                            }
                        }));
                        keep = false;
                    }
                    if ui.button("Cancel").clicked() {
                        keep = false;
                    }
                });
            }
            Dialog::ExportSvg(path) => {
                ui.label("Write the current Gantt view (respecting filters/grouping) to:");
                let r = ui.add(egui::TextEdit::singleline(path).desired_width(420.0));
                let enter = r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
                ui.horizontal(|ui| {
                    if ui.button("Export").clicked() || enter {
                        let p = path.clone();
                        after = Some(Box::new(move |a: &mut App| a.export_svg(&p)));
                        keep = false;
                    }
                    if ui.button("Cancel").clicked() {
                        keep = false;
                    }
                });
            }
            Dialog::ConfirmClose => {
                ui.label("This project has unsaved changes.");
                ui.horizontal(|ui| {
                    if ui.button("Save and quit").clicked() {
                        keep = false;
                        after = Some(Box::new(|a: &mut App| match a.path.clone() {
                            Some(p) => {
                                if a.save_to(p) {
                                    a.allow_close = true;
                                    a.close_requested_again = true;
                                }
                            }
                            None => a.dialog = Some(Dialog::SaveAs { path: "project.pcraft".into(), then_close: true }),
                        }));
                    }
                    if ui.button("Discard changes and quit").clicked() {
                        keep = false;
                        after = Some(Box::new(|a: &mut App| {
                            a.allow_close = true;
                            a.close_requested_again = true;
                        }));
                    }
                    if ui.button("Cancel").clicked() {
                        keep = false;
                    }
                });
            }
            Dialog::ConfirmDiscard(what) => {
                ui.label("Discard the unsaved changes in the current project?");
                ui.horizontal(|ui| {
                    if ui.button("Discard and continue").clicked() {
                        let w = what.clone();
                        keep = false;
                        after = Some(Box::new(move |a: &mut App| {
                            a.dirty = false;
                            a.do_pending(w);
                        }));
                    }
                    if ui.button("Cancel").clicked() {
                        keep = false;
                    }
                });
            }
            Dialog::LevelReport(msg) => {
                ui.label(msg.as_str());
                ui.label("Undo (Ctrl+Z) reverts the leveling in one step.");
                if ui.button("OK").clicked() {
                    keep = false;
                }
            }
            Dialog::Project(d) => {
                ui.horizontal(|ui| {
                    ui.label("Name:");
                    ui.text_edit_singleline(&mut d.meta.name);
                });
                ui.horizontal(|ui| {
                    ui.label("Start date:");
                    date_picker(ui, "proj_start", &mut d.meta.start_date);
                });
                ui.separator();
                ui.label(RichText::new("Working calendar").strong());
                ui.horizontal(|ui| {
                    ui.label("Hours per day:");
                    ui.add(egui::DragValue::new(&mut d.meta.calendar.hours_per_day).range(1..=24));
                });
                ui.horizontal(|ui| {
                    for (i, n) in ["Mon", "Tue", "Wed", "Thu", "Fri", "Sat", "Sun"].iter().enumerate() {
                        ui.checkbox(&mut d.meta.calendar.work_week[i], *n);
                    }
                });
                ui.label("Holidays (non-working days):");
                ui.horizontal(|ui| {
                    date_picker(ui, "new_hol", &mut d.new_holiday);
                    if ui.button("Add holiday").clicked() {
                        d.meta.calendar.holidays.insert(d.new_holiday);
                    }
                });
                let mut remove = None;
                egui::ScrollArea::vertical().max_height(120.0).show(ui, |ui| {
                    for h in &d.meta.calendar.holidays {
                        ui.horizontal(|ui| {
                            ui.label(h.format("%a %Y-%m-%d").to_string());
                            if ui.small_button("\u{2715}").clicked() {
                                remove = Some(*h);
                            }
                        });
                    }
                });
                if let Some(h) = remove {
                    d.meta.calendar.holidays.remove(&h);
                }
                let valid = d.meta.calendar.is_valid();
                if !valid {
                    ui.colored_label(Color32::RED, "The calendar needs at least one working day.");
                }
                ui.horizontal(|ui| {
                    if ui.add_enabled(valid, egui::Button::new("Apply")).clicked() {
                        let new = d.meta.clone();
                        keep = false;
                        after = Some(Box::new(move |a: &mut App| {
                            let old = a.project.meta();
                            if old != new {
                                a.exec(crate::commands::Command::SetProjectMeta { old, new });
                            }
                        }));
                    }
                    if ui.button("Cancel").clicked() {
                        keep = false;
                    }
                });
            }
        });
        if keep {
            // a callback (e.g. SaveAs from ConfirmClose) may have set a new dialog; don't clobber it
            if self.dialog.is_none() {
                self.dialog = Some(dialog);
            }
        }
        if let Some(f) = after {
            f(self);
        }
    }
}
