# Activity

A task manager for [Omarchy](https://omarchy.org). It takes its colors from your
Omarchy theme and fits a half-screen tile.

## What it does

- **Dashboard:** what needs a look comes first, in plain words with a button to deal
  with it (a hot CPU, memory or a disk filling up, a process stuck busy, a failed
  service). Then big cards: the CPU with a strip of every thread, memory split into
  apps, kernel and cache, every disk's fullness, the network, the GPU, the sensors, the
  busiest apps and what starts at login (battery and load average too, if you want
  them). *Edit* shows, hides and drags the cards around, and sets when to warn.
- **Processes:** every process with its CPU, memory, disk, GPU, threads and state.
  - **Views:** *Apps* (grouped under their window), *All*, *Tree* and *Mine*.
  - **Controls:** sort by any column, show, hide, resize or reorder columns (all
    remembered), and filter by name, command, PID or user. Filtering in *Apps* and
    *Tree* keeps the tree, opened down to each match.
  - **Totals:** column headers show the machine's CPU, memory, disk and GPU use, and
    busy cells are shaded.
  - **Actions:** end, kill, pause/resume, change priority, switch to or close the
    window, open its location, show it in the tree, or copy its command line, from the
    details panel or a right-click. Signals and priority go to every selected process.
    Other users' processes go through `pkexec`.
  - **Details:** live CPU and memory graphs, open files, start time and the parent
    process. The panel collapses to one line, and sits beside the table in wide windows.
  - **Keys:** <kbd>Delete</kbd> ends a process and <kbd>Shift</kbd>+<kbd>Delete</kbd>
    kills it. Either needs a second press unless you turn that off in Settings.
    <kbd>←</kbd>/<kbd>→</kbd> close and open branches, and <kbd>/</kbd> filters.
- **Performance pages:**
  - **CPU:** where the time goes (user, kernel, I/O wait, interrupts), clocks, load and
    pressure, the busiest apps and processes, each thread grouped by core type
    (performance, efficient, low-power) with its clock and temperature, and the
    processor's caches, power profile and energy preference.
  - **Memory:** what's using it: apps (each added up from its cgroup), shared memory,
    the kernel and cache; the apps and processes using the most; swap and zram
    (with its compression ratio), commit, pressure and swapping over time.
  - **Storage:** each disk's throughput, requests, response time, temperature and how
    full each mount is, plus who's reading and writing and how long programs wait.
  - **Network:** traffic, packets and Wi-Fi signal over time, band, channel and link
    speed, gateway and DNS, the apps with connections, and what's listening.
  - **GPU:** Intel/AMD usage per engine from DRM fdinfo and sysfs, clocks, power saving
    and GPU memory by app; NVIDIA through `nvidia-smi` (only while it's awake).
  - **Sensors:** every temperature sensor, fans, battery charge, draw and health, the
    charger, USB-C power and the power profile.
- **Startup:** what runs at login.
  - Apps from XDG autostart can be turned off with a user override; the system file
    is never touched.
  - Items you add are started by Hyprland from `~/.config/hypr/activity.lua`.
  - Omarchy's own startup and your `autostart.lua` are shown for reference.
  - Also: user services, and what took longest at your last login.
- **Services:** systemd user and system services and timers. Start, stop, restart,
  enable/disable, see a unit's PID, memory, CPU time and command, edit it, and read
  its logs (filtered by text or priority, or following live). Timers show their next
  and last run. The list refreshes by itself while it's open.
- **Warnings and alerts:** CPU temperature, memory use and disk fullness turn red past
  limits you set, and optional desktop notifications cover those, a process stuck at
  high CPU, and failed services.
- **Go to:** <kbd>Ctrl</kbd>+<kbd>K</kbd> finds any page, process, service or startup
  item.

Graphs keep 1, 5 or 10 minutes of history and update every 0.5–5 s. Hover any graph
to read past values, and right-click one to export it as CSV (Settings exports them
all). <kbd>Ctrl</kbd>+<kbd>P</kbd> pauses updates, and <kbd>F1</kbd> lists every shortcut.

### Easy on the battery

On hybrid laptops the discrete GPU sleeps when nothing uses it. Activity keeps it asleep:

- It uses GTK's GL renderer, because the Vulkan renderer probes every GPU and wakes it.
- It never runs `lspci`; GPU names come from `pci.ids`.
- It only asks `nvidia-smi` for numbers while the GPU is already awake, and backs off
  once it's idle.

While the window is hidden, the per-process and GPU sampling stops too.

## Install

```sh
curl -fsSL https://raw.githubusercontent.com/design-nexus/nyx-activity/main/install.sh | bash -s -- activity
```

This builds with Cargo and installs `nyx-activity` to `~/.local/bin`, along with a launcher
entry. Add `-s -- --bind` after `bash` to also print a binding that opens it with
Ctrl+Shift+Esc:

```lua
o.bind("CTRL + SHIFT + Escape", "Task manager", hl.dsp.exec_cmd("nyx-activity --toggle"))
```

To remove it, run the same line with `uninstall.sh` in place of `install.sh`. Add
`-s -- --purge` to also remove its settings and startup items.

## Usage

```
nyx-activity [--section ID] [--toggle]
```

`--section` opens (or switches the open window to) `dashboard`, `processes`, `cpu`,
`memory`, `storage`, `network`, `gpu`, `sensors`, `startup`, `services` or `settings`.

## Files

| Path | What |
| --- | --- |
| `~/.config/nyx-activity/settings.toml` | This app's preferences |
| `~/.config/nyx-activity/themes/*.toml` | Your own themes |
| `~/.config/nyx-activity/state.json` | Startup items added in Activity |
| `~/.config/hypr/activity.lua` | Generated from the above; loaded by `require("hypr.activity")` |
| `~/.config/autostart/*.desktop` | XDG autostart overrides (`Hidden=true`) |

## License

MIT
