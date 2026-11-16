# Activity — notes for working on this repo

- GTK4 (gtk4-rs 0.11) + Rust. No libadwaita. Nyx Design 2.0 (`use_design_2`, see
  `STYLE.md`): one surface, no hairlines; cards and spacing separate the parts. The
  unified header (`chrome::app_header`) has the app with the page as its subtitle and the
  sidebar toggle; processes/CPU/memory, the live/paused pill, the page's config file,
  search and Settings at the end. No status bar, no sidebar head. Numbers use tabular
  figures (`tnum`); mono is for commands and paths only. Pages have no title header. Settings
  is a card over the window (`settings_dialog.rs`) listing the settings page's groups;
  `navigate("settings")` opens it. Ctrl+F filters processes, or opens Go to elsewhere.
  The theme, widgets, settings card and window frame come from the `nyx-*` crates.
  Every colour is a `@theme_*` token; graphs get theirs from `theme::palette()`.
- `sampler/` runs on its own thread and never touches GTK. It sends a `Snapshot` per
  interval over an async channel. `live.rs` keeps graph history (ring buffers by key,
  e.g. `cpu`, `disk.nvme0n1.read`, `gpu.<pci>.util`) and calls `live::on_tick`
  subscribers only while their widget is mapped. Pages whose layout depends on the
  hardware build inside `live::ready`.
- Processes: one `ProcObject` per pid in a `ListStore`. `layout()` computes order,
  depth, visibility and collapsed totals; the GTK sorter/filter just follow `order` and
  `visible`. GTK re-binds rows synchronously, so never hold a `STATE` borrow across
  store/filter/sorter changes. `apply_view` also keeps the scroll position still
  (ListView otherwise follows its anchor row after a re-sort).
- Don't wake the dGPU: no `lspci`, no Vulkan renderer (main.rs sets `GSK_RENDERER=gl`),
  `nvidia-smi` only while `runtime_status` isn't `suspended`. Check with
  `cat /sys/bus/pci/devices/0000:01:00.0/power/runtime_status` while the app runs.
- Never edit the user's Hyprland files except the one `require` line added on request
  (`startup::add_include`, with a backup), and `startup::move_old_include` renaming it from
  `hypr.tasks` once (the app was called Tasks). Managed items are generated into `hypr/activity.lua`.
- Hyprland dispatch: `hyprctl eval 'hl.dispatch(hl.dsp.focus({ window = "address:0x…" }))'`
  (and `hl.dsp.window.close`).
- `nyx-activity --top cpu|memory` prints the busiest processes and the totals as JSON from two samples (Nyx Assistant's `tasks.top`); no window.
- Checks: `cargo clippy -- -D warnings`, `cargo test`. Visual check: a test copy on a
  private virtual monitor, `NYX_ACTIVITY_APP_ID=io.github.design_nexus.nyx.ActivityTest
  nyx-activity --section cpu` with scratch XDG dirs (a separate instance, so the open copy
  is untouched). A scratch `XDG_CONFIG_HOME` with `theme = "catppuccin-latte"`,
  `mode = "theme"` checks light mode.
