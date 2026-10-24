//! ProjectCraft - a Microsoft Project style desktop scheduler in pure Rust (egui + SQLite).
//! Module map: model (domain) / calendar + scheduler (CPM brain) / commands (undo) /
//! persistence (SQLite) / view + ui_* (egui panes) / export (SVG).

mod app;
mod calendar;
mod commands;
mod export;
mod geometry;
mod model;
mod parse;
mod persistence;
mod scheduler;
mod ui_detail;
mod ui_gantt;
mod ui_shell;
mod ui_sheet;
mod view;

fn main() -> eframe::Result<()> {
    // optional first argument: a .pcraft file to open
    let arg = std::env::args().nth(1);
    let (project, path) = match arg {
        Some(p) if p == "--stress" => (model::Project::stress(1000), None),
        Some(p) => match persistence::load(std::path::Path::new(&p)) {
            Ok(proj) => (proj, Some(std::path::PathBuf::from(p))),
            Err(e) => {
                eprintln!("{}", e);
                (model::Project::sample(), None)
            }
        },
        None => (model::Project::sample(), None),
    };
    let options = eframe::NativeOptions {
        viewport: eframe::egui::ViewportBuilder::default().with_inner_size([1500.0, 900.0]).with_min_inner_size([900.0, 500.0]).with_title("ProjectCraft"),
        ..Default::default()
    };
    eframe::run_native("ProjectCraft", options, Box::new(move |_cc| Ok(Box::new(app::App::new(project, path)))))
}
