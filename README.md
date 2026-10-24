# ProjectCraft
A Microsoft-Project-style scheduler in pure Rust: egui/eframe UI, SQLite working file, CPM scheduler.

    cargo run --release            # sample project
    cargo run --release -- --stress   # 1,000 tasks / 499 links
    cargo run --release -- my.pcraft
    cargo test                     # 62 tests

Modules: model (domain) · calendar + scheduler (CPM, cycles, constraints, leveling) · commands (undo/redo) ·
persistence (SQLite, FKs, versioned) · view (rows/filter/group) · ui_sheet / ui_gantt / ui_detail / ui_shell · export (SVG).

Gantt: drag a bar = move (sets Start-No-Earlier-Than), drag right edge = duration, drag background = pan,
Ctrl+wheel = zoom. Sheet: click a cell to edit. Predecessors syntax: `3FS+2d, 5SS-1d`. Resources: `Alice[50%], Bob`.
Ctrl/Shift-click selects several tasks, then Link. Ctrl+Z / Ctrl+Y / Ctrl+S / Del.

Known limits: no ALAP constraint, links only between leaf tasks (summaries roll up), leveling never extends the
project end, export is SVG only (no PDF), open/save use a typed path (no native file dialog).
