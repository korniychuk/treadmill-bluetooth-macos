# tmux workout widget

Renders live treadmill state (walking / paused / away / unknown) and workout
metrics (time, steps, distance) as a segment in your tmux status bar. The
segment appears while the treadmill is connected and the daemon is alive, and
disappears otherwise — no manual toggling.

`treadmill-widget.sh` is a **reference implementation**: colours, icons, and
layout are tunable variables at the top of the file. Copy it, tweak it, make
it yours.

## Requirements

- The background daemon (`tm daemon`, normally installed as a LaunchAgent via
  `scripts/install-daemon.sh`) must be running — the widget reads its state,
  it does not talk to the treadmill directly.
- `tm` (the treadmill CLI) resolvable on disk. tmux runs `#()` commands with a
  minimal `PATH` that usually excludes `~/.bin`, so the script takes the
  binary path as `$TREADMILL_BIN` (default `$HOME/.bin/tm`) rather than
  relying on `PATH` lookup.
- A Nerd Font in your terminal (the default icon set uses Material Design
  glyphs, verified present in JetBrainsMono Nerd Font). Swap the `ICON_*`
  variables if your font is missing one.

## The `tm widget` contract

The script is presentation only. All data and the show/hide decision come
from one CLI command:

```
tm widget
```

- Prints **one TSV line, 11 tab-separated fields**, while the treadmill is
  connected and the daemon's heartbeat is fresh:

  ```
  STATE  WORKOUT_COUNT  CUR_WALKING_S  CUR_STEPS  CUR_DISTANCE_M  DAY_WALKING_S  DAY_STEPS  DAY_DISTANCE_M  HR_BPM  HR_BATTERY_PCT  HR_ZONE
  ```

  - `STATE` ∈ `walking | paused | away | unknown`.
  - `WORKOUT_COUNT` — number of merged workouts so far today.
  - `CUR_*` — the current/latest workout's filtered walking time, steps,
    distance.
  - `DAY_*` — today's calendar totals (same filtering). `CUR_* <= DAY_*` by
    construction.
  - `HR_BPM` — live bpm from a worn heart-rate sensor (e.g. Polar H10), or
    **empty** when no sensor is worn or its last reading has gone stale (same
    freshness gate as the rest of this line). Always present as a field —
    emptiness is the signal to hide the heart glyph, not to hide the whole
    segment.
  - `HR_BATTERY_PCT` — the sensor's last-read battery level (0-100), or
    **empty** if not yet read or no sensor connected. Always the raw
    percentage; the reference script only turns it into a small low-battery
    glyph once it drops to/below its own `LOW_BATTERY_PCT` tunable (default
    20) — the exact number belongs to `tm status`, not the widget.
  - `HR_ZONE` ∈ `below | in | above`, or **empty** unless Zone Hold (задача
    027) is actively driving speed corrections in the `walking` state. The
    reference script recolours the whole `♥ NNN` token by this value — empty
    leaves it in the plain per-state colour, unchanged from задачи 025/026.

- Prints **nothing** (exit 0) whenever the treadmill is off, the daemon is
  dead, or its heartbeat is stale — the unambiguous signal to hide the
  segment.

See `docs/tasks/009-tmux-workout-widget.md`, `docs/tasks/025-heart-rate-polar-h10.md`,
`docs/tasks/026-hr-battery-level.md` and `docs/tasks/027-zone-hold-hr-adaptive-speed.md`
in this repo for the full contract history and design rationale. If you change
the field count/order in `tm widget`, update this script's
`IFS=$'\t' read -r ...` line to match.

## Recipe A — Dracula tmux theme (custom plugin)

[Dracula for tmux](https://draculatheme.com/tmux) supports a
`custom:<script>` plugin slot, but it only resolves custom scripts that live
inside **its own** `scripts/` directory — so symlink this script there rather
than pointing at it directly:

```bash
ln -sf /path/to/treadmill-bluetooth-macos/scripts/tmux/treadmill-widget.sh \
  ~/.tmux/plugins/tmux/scripts/treadmill.sh
```

(Adjust `~/.tmux/plugins/tmux` if Dracula is installed elsewhere, e.g. via a
different plugin manager path.)

Then in `.tmux.conf`:

```tmux
set -g @dracula-plugins "treadmill.sh ..."   # add alongside your other plugins
set -g @dracula-refresh-rate 2               # poll every 2s; tm widget is a cheap read
```

Dracula draws a frame around every plugin segment. For the segment to
actually *vanish* when the script prints nothing (rather than showing an
empty coloured box), keep `@dracula-show-empty-plugins` at its default
(`true`) and instead make this plugin's frame colour match your bar
background, e.g.:

```tmux
set -g @dracula-custom-plugin-colors "gray white"
```

With the frame the same colour as the bar, empty output blends in and
disappears; when the script does output something, it paints its own
state-coloured pill (`#[bg=...]`) on top of that invisible frame. (Setting
`@dracula-show-empty-plugins false` instead seems tempting, but at least as
of Dracula tmux `ce10069` it wraps every segment's output in a
`#{?#{==:...},,...}` conditional, and the literal `#` in this script's hex
colour codes breaks tmux's format parser — corrupting the *entire* status
bar, not just this segment. Prefer the frame-colour approach above.)

## Recipe B — plain tmux (no Dracula)

Any tmux status bar can call the script directly via `#()`; no plugin
manager involved. In `.tmux.conf`:

```tmux
set -g status-right '#(~/path/to/treadmill-bluetooth-macos/scripts/tmux/treadmill-widget.sh)#[default] | %H:%M '
set -g status-interval 2
```

When the script prints nothing, `#()` naturally renders as an empty string,
so the segment disappears on its own — no extra configuration needed. Adjust
`status-right`/`status-left` composition and `status-interval` to taste.

## Customizing

Open `treadmill-widget.sh` and edit the "Tunables" block near the top:

- `TREADMILL_BIN` — path to the `tm` binary.
- `BAR_BG` — your status bar's background colour, used only to draw the
  leading powerline arrow so it blends in; irrelevant if you set `SEP=''`.
- `SEP` — the leading powerline separator glyph; set to `''` to disable it.
- `ICON_WALKING` / `ICON_PAUSED` / `ICON_AWAY` / `ICON_UNKNOWN` — per-state
  glyphs (Nerd Font codepoints, encoded as raw UTF-8 bytes).
- `BG_*` / `FG_*` / `DIM_*` — per-state pill background/foreground colours
  and the dimmed colour used for "today total" figures in multi-workout mode.

Everything else (parsing, the 3-case metrics layout, `fmt_time`/`fmt_dist`)
is generic and shouldn't need touching unless you want a different layout.
