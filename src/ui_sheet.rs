//! Left pane: the task sheet. A virtualized `egui_extras::TableBuilder` (only visible rows are
//! laid out, so thousands of rows stay cheap), with inline cell editing and outline indent /
//! expand-collapse toggles.
//!
//! Row height equals `ROW_H` and item spacing is zeroed so row N here is exactly row N in the
//! Gantt; the vertical scroll offset is shared through `App::scroll_y`.

use crate::app::*;
use crate::geometry::*;
use crate::model::*;
use crate::view::RowKind;
use eframe::egui;
use egui::{Color32, Key, RichText, Sense, TextEdit};
use egui_extras::{Column, TableBuilder};

const COLS: [(Col, &str); 7] = [
    (Col::Id, "ID"),
    (Col::Name, "Task name"),
    (Col::Duration, "Duration"),
    (Col::Start, "Start"),
    (Col::Finish, "Finish"),
    (Col::Preds, "Predecessors"),
    (Col::Resources, "Resource names"),
];

impl App {
    pub fn ui_sheet(&mut self, ui: &mut egui::Ui) {
        // Take UI-mutable state out of `self` so the table closures can borrow the project.
        let rows = std::mem::take(&mut self.rows);
        let mut edit = self.edit.take();
        let selected = self.selected.clone();

        let mut clicked: Option<(TaskId, Col, egui::Modifiers)> = None;
        let mut toggle: Option<TaskId> = None;
        let mut commit: Option<(TaskId, Col, String)> = None;
        let mut cancel = false;

        ui.spacing_mut().item_spacing.y = 0.0;
        ui.spacing_mut().interact_size.y = ROW_H - 2.0;

        let table_rect = ui.available_rect_before_wrap();
        let out = {
            let app: &App = self;
            let tbl = TableBuilder::new(ui)
                .striped(true)
                .resizable(true)
                .vertical_scroll_offset(app.scroll_y)
                .column(Column::exact(42.0))
                .column(Column::initial(230.0).at_least(80.0).clip(true))
                .column(Column::initial(66.0).at_least(40.0).clip(true))
                .column(Column::initial(88.0).at_least(60.0).clip(true))
                .column(Column::initial(88.0).at_least(60.0).clip(true))
                .column(Column::initial(100.0).at_least(50.0).clip(true))
                .column(Column::remainder().at_least(60.0).clip(true))
                .min_scrolled_height(0.0);
            tbl.header(HEADER_H, |mut header| {
                for (_, label) in COLS {
                    header.col(|ui| {
                        ui.strong(label);
                    });
                }
            })
            .body(|body| {
                body.rows(ROW_H, rows.len(), |mut row| {
                    let ri = row.index();
                    let r = &rows[ri];
                    match (&r.kind, r.task) {
                        (RowKind::Group, _) => {
                            for (ci, _) in COLS.iter().enumerate() {
                                row.col(|ui| {
                                    ui.painter().rect_filled(ui.max_rect(), 0.0, Color32::from_rgba_unmultiplied(70, 110, 170, 40));
                                    if ci == 1 {
                                        ui.label(RichText::new(&r.label).strong());
                                    }
                                });
                            }
                        }
                        (_, Some(id)) => {
                            let Some(t) = app.project.task(id) else { return };
                            let ts = app.schedule.tasks.get(&id);
                            let is_summary = ts.map_or(false, |x| x.is_summary);
                            let is_critical = ts.map_or(false, |x| x.critical);
                            row.set_selected(selected.contains(&id));
                            for (col, _) in COLS {
                                row.col(|ui| {
                                    let cell_rect = ui.max_rect();
                                    let editing_here = edit.as_ref().map_or(false, |e| e.task == id && e.col == col);
                                    if editing_here {
                                        if let Some(e) = edit.as_mut() {
                                            let resp = ui.add(TextEdit::singleline(&mut e.buf).desired_width(f32::INFINITY).margin(egui::Margin::symmetric(2, 2)));
                                            if e.fresh {
                                                resp.request_focus();
                                                e.fresh = false;
                                            }
                                            let esc = ui.input(|i| i.key_pressed(Key::Escape));
                                            if esc {
                                                cancel = true;
                                            } else if resp.lost_focus() {
                                                commit = Some((id, col, e.buf.clone()));
                                            }
                                        }
                                        return;
                                    }
                                    // ---- display ----
                                    let text = app.cell_text(id, col);
                                    match col {
                                        Col::Name => {
                                          ui.horizontal(|ui| {
                                            ui.spacing_mut().item_spacing.x = 4.0;
                                            ui.add_space(r.level as f32 * 14.0);
                                            if r.has_children {
                                                let collapsed = app.opts.collapsed.contains(&id);
                                                let arrow = if collapsed { "\u{25B6}" } else { "\u{25BC}" };
                                                let resp = ui.add(egui::Label::new(RichText::new(arrow).size(10.0)).sense(Sense::click()));
                                                if resp.clicked() {
                                                    toggle = Some(id);
                                                }
                                            } else {
                                                ui.add_space(14.0);
                                            }
                                            let mut rt = RichText::new(if t.is_milestone { format!("\u{25C6} {}", text) } else { text });
                                            if is_summary {
                                                rt = rt.strong();
                                            }
                                            if is_critical && !is_summary {
                                                rt = rt.color(Color32::from_rgb(190, 50, 50));
                                            }
                                            ui.add(egui::Label::new(rt).truncate());
                                          });
                                        }
                                        Col::Id => {
                                            ui.add(egui::Label::new(RichText::new(text).weak()));
                                        }
                                        Col::Start | Col::Finish => {
                                            let bad = ts.map_or(false, |x| x.constraint_violated);
                                            let mut rt = RichText::new(text);
                                            if bad {
                                                rt = rt.color(Color32::RED);
                                            }
                                            if is_summary {
                                                rt = rt.strong();
                                            }
                                            ui.add(egui::Label::new(rt).truncate());
                                        }
                                        _ => {
                                            let mut rt = RichText::new(text);
                                            if is_summary {
                                                rt = rt.strong();
                                            }
                                            ui.add(egui::Label::new(rt).truncate());
                                        }
                                    }
                                    // cell-wide click target (drawn after the content so toggles win)
                                    let resp = ui.interact(cell_rect, ui.id().with(("cell", id, col as u8)), Sense::click());
                                    if resp.clicked() {
                                        clicked = Some((id, col, ui.input(|i| i.modifiers)));
                                    }
                                });
                            }
                        }
                        _ => {}
                    }
                });
            })
        };

        // ---- apply intents (project is no longer borrowed) ----
        let hovered = ui.rect_contains_pointer(table_rect);
        let new_off = out.state.offset().y;
        if hovered && (new_off - self.scroll_y).abs() > 0.01 {
            self.scroll_y = new_off;
        }
        let max_scroll = (rows.len() as f32 * ROW_H - (out.inner_rect.height() - HEADER_H).max(0.0)).max(0.0);
        self.scroll_y = self.scroll_y.clamp(0.0, max_scroll);
        self.rows = rows;
        self.edit = edit;

        if cancel {
            self.edit = None;
        }
        let mut start_edit: Option<(TaskId, Col)> = None;
        if let Some((id, col, mods)) = clicked {
            // clicking a different cell commits the one being edited
            if commit.is_none() {
                if let Some(e) = self.edit.take() {
                    if e.task != id || e.col != col {
                        commit = Some((e.task, e.col, e.buf));
                    } else {
                        self.edit = Some(e);
                    }
                }
            }
            let multi = mods.shift || mods.command || mods.ctrl;
            let was_selected_only = self.selected == vec![id];
            self.select_click(id, mods);
            if !multi && self.is_editable(id, col) && !(self.edit.as_ref().map_or(false, |e| e.task == id && e.col == col)) {
                let _ = was_selected_only;
                start_edit = Some((id, col));
            }
        }
        if let Some((id, col, text)) = commit {
            self.edit = None;
            if let Err(msg) = self.commit_cell(id, col, &text) {
                self.toast_err(msg);
            }
        }
        if let Some((id, col)) = start_edit {
            let buf = self.cell_text(id, col);
            self.edit = Some(CellEdit { task: id, col, buf, fresh: true });
        }
        if let Some(id) = toggle {
            if !self.opts.collapsed.remove(&id) {
                self.opts.collapsed.insert(id);
            }
            self.rows_dirty = true;
        }
    }
}
