#!/usr/bin/env bash
#
# Treadmill workout widget for a tmux status bar.
#
# Reference implementation: renders the state/metrics of `tm widget` (see
# treadmill-bluetooth-macos docs/tasks/009) as a colour-coded powerline pill.
# Presentation only — the data + visibility contract lives in the treadmill
# CLI, which prints one 8-field TSV line while the treadmill is connected and
# the daemon heartbeat is fresh, or nothing otherwise:
#   state, workout_count, cur_walking_s, cur_steps, cur_distance_m,
#   day_walking_s, day_steps, day_distance_m
# (cur_* = current/latest workout today, day_* = today's totals across all
# workouts). cur_* are all zero when there is no LIVE workout (connected but
# idle — the last workout ended longer ago than the merge gap); the body
# below then falls back to showing day_* alone.
#
# HIDE-WHEN-OFF: when `tm widget` prints nothing (or fails), this script
# exits 0 with no output. Whether that actually hides the segment in your
# status bar depends on your tmux theme/plugin — see README.md in this
# directory for the Dracula recipe (frame-colour trick) and the generic
# tmux recipe (`#()` naturally renders empty output as nothing).
#
# All glyphs/colours below are tunables, not requirements — edit freely to
# match your theme. Defaults are tuned for the Dracula tmux theme (hex
# colours, a leading powerline arrow byte-matched to Dracula's own
# separator, Material Design Nerd Font glyphs) but nothing here requires
# Dracula specifically; see README.md Recipe B for a plain tmux example.
#
# Requires a Nerd Font for the glyphs below (Material Design icons,
# nf-md-*). Verified present in JetBrainsMono Nerd Font. Glyphs are built
# from raw UTF-8 bytes via `printf '\xHH'` — portable across shells and
# plain ASCII on disk (literal glyphs are private-use codepoints some tools
# mangle; `$'\u...'` needs bash 4.2+). To swap a glyph: look up its codepoint
# at nerdfonts.com and convert to UTF-8 bytes.

set -euo pipefail

# --- Tunables ------------------------------------------------------------------

# The treadmill binary. tmux runs `#()` with a minimal PATH that usually
# excludes ~/.bin, so resolve it explicitly (override with $TREADMILL_BIN).
TM="${TREADMILL_BIN:-$HOME/.bin/tm}"

# Status-bar background colour. Used only to draw the leading powerline
# separator (SEP) on the correct backdrop so it blends with neighbouring
# segments. Set to your theme's bar background; the Dracula default is
# '#44475a'. Leave SEP empty (see below) if you don't want a leading arrow.
BAR_BG='#44475a'

# Leading powerline separator (nf left half-circle-thick / U+E0B2). Drawn in
# the pill colour on $BAR_BG so this segment gets a left arrow matching the
# rest of the bar. Set SEP='' to disable it entirely (plain pill, no arrow).
SEP=$(printf '\xee\x82\xb2')  # U+E0B2 (powerline left-pointing filled arrow)

# Per-state look: glyph (shape) + pill background + text colour. State is
# double-encoded — colour AND shape — so it reads at a glance even for
# colourblind users or a monochrome terminal. Colours below are the Dracula
# palette; light backgrounds take dark text, the muted "unknown" takes light
# text. Swap freely to match your own theme.
ICON_WALKING=$(printf '\xf3\xb0\x9c\x8e')  # nf-md-run           U+F070E (person running)
ICON_PAUSED=$(printf '\xf3\xb0\x8f\xa4')   # nf-md-pause         U+F03E4
ICON_AWAY=$(printf '\xf3\xb0\xb6\x91')     # nf-md-motion-sensor U+F0D91 (belt moving, no steps)
ICON_UNKNOWN=$(printf '\xf3\xb0\x8b\x96')  # nf-md-help          U+F02D6 (connected, no data yet)
# Away-icon alternatives (all VERIFIED present in JetBrainsMono Nerd Font):
#   nf-md-shoe_print          U+F0DD9 (footprints on empty belt)
#   nf-md-exit_run            U+F0508 (person running out)
#   nf-md-account_arrow_right U+F0B70 (person leaving)
#   nf-md-walk                U+F05B4 (neutral) · nf-md-eye_off U+F0209 (not detected)

# `walking` uses an emerald green (#34d399) rather than Dracula's own lime
# green (#50fa7b), so two adjacent green segments stay visually separable;
# dark text stays readable on it.
BG_WALKING='#34d399'; FG_WALKING='#282a36'  # emerald / dark
BG_PAUSED='#f1fa8c';  FG_PAUSED='#282a36'   # yellow  / dark
BG_AWAY='#ffb86c';    FG_AWAY='#282a36'     # orange  / dark
BG_UNKNOWN='#6272a4'; FG_UNKNOWN='#f8f8f2'  # comment / light

# Dimmed foreground for the "today total" half of each metric in multi-workout
# mode: the current workout stays crisp (FG_*), the day total is muted so the
# `current/today` pairs read at a glance. DIM_DARK muted-slate reads faded-
# but-legible on the bright backgrounds; DIM_UNKNOWN is the muted-light
# variant for the dark "unknown" pill.
DIM_DARK='#4c566a'; DIM_UNKNOWN='#aeb6c8'

# Day-steps emphasis. The day's total steps is the single most important metric
# (it tracks the daily goal), so it's rendered BOLD in a fixed near-black that
# stays high-contrast on every state background (emerald/yellow/orange/muted).
# Sampled from the design mockup. Applied to the lone steps number in single-
# workout / idle mode, or the after-slash day total in multi-workout mode.
STEPS_FG='#181818'

# --- Helpers -------------------------------------------------------------------

# Seconds -> `M:SS`, or `H:MM:SS` past an hour.
fmt_time() {
  local s=$1 h m sec
  h=$(( s / 3600 )); m=$(( (s % 3600) / 60 )); sec=$(( s % 60 ))
  if (( h > 0 )); then printf '%d:%02d:%02d' "$h" "$m" "$sec"
  else printf '%d:%02d' "$m" "$sec"; fi
}

# Metres -> `X.XXkm` at/above a kilometre, else `Xm`.
fmt_dist() {
  local m=$1
  if (( m >= 1000 )); then printf '%d.%02dkm' $(( m / 1000 )) $(( (m % 1000) / 10 ))
  else printf '%dm' "$m"; fi
}

# --- Main ----------------------------------------------------------------------

# No binary yet (fresh machine, not built/installed) -> render nothing rather
# than error.
[[ -x "$TM" ]] || exit 0

# `tm widget` prints nothing when the treadmill is off; a non-zero exit is
# also treated as "hide" so a transient DB hiccup never paints a broken
# segment.
line="$("$TM" widget 2>/dev/null || true)"
[[ -n "$line" ]] || exit 0

# `tm widget` emits 8 tab-separated fields (see treadmill repo docs/tasks/009):
# state, workout_count today, then the CURRENT workout's (walking_s, steps,
# distance_m), then TODAY's totals (walking_s, steps, distance_m).
IFS=$'\t' read -r state wcount cur_s cur_steps cur_dist day_s day_steps day_dist <<<"$line"

# Defend against a malformed line: any missing/non-numeric numeric field -> hide.
for n in "$wcount" "$cur_s" "$cur_steps" "$cur_dist" "$day_s" "$day_steps" "$day_dist"; do
  [[ "$n" =~ ^[0-9]+$ ]] || exit 0
done

case "$state" in
  walking) icon=$ICON_WALKING; bg=$BG_WALKING; fg=$FG_WALKING; dim=$DIM_DARK ;;
  paused)  icon=$ICON_PAUSED;  bg=$BG_PAUSED;  fg=$FG_PAUSED;  dim=$DIM_DARK ;;
  away)    icon=$ICON_AWAY;    bg=$BG_AWAY;    fg=$FG_AWAY;    dim=$DIM_DARK ;;
  *)       icon=$ICON_UNKNOWN; bg=$BG_UNKNOWN; fg=$FG_UNKNOWN; dim=$DIM_UNKNOWN ;;
esac

# Metrics body. Three cases:
#  1. No LIVE workout AND not walking (all cur_* zero, state != walking): the
#     treadmill is connected but idle — e.g. just reconnected after a pause
#     longer than the merge gap, so `tm widget` reports no current workout
#     (cur_* = 0). Show today's TOTALS alone, not a phantom `0:00` current line.
#     The `state != walking` guard matters: at the very start of a walk presence
#     flips to `walking` a beat before the first step is credited into a new
#     segment (credit is buffered for step-confirmation), so cur_* is briefly 0
#     WHILE walking — that is a starting workout, not idle, and must show the
#     current line (ticking up from 0:00), never the day-totals masquerade.
#  2. A single workout today: show just the current one (as before).
#  3. 2+ workouts: show `current/today` per metric — the today half dimmed so the
#     pairs read at a glance.
# Distinct suffixes (`:` in time, `km`/`m` on distance, bare steps) disambiguate
# the three values with no labels.
if [[ "$state" != walking ]] && (( cur_s == 0 && cur_steps == 0 && cur_dist == 0 )); then
  body=$(printf '%s  #[fg=%s,bold]%s#[nobold,fg=%s]  %s' \
    "$(fmt_time "$day_s")" "$STEPS_FG" "$day_steps" "$fg" "$(fmt_dist "$day_dist")")
elif (( wcount >= 2 )); then
  body=$(printf '%s#[fg=%s]/%s#[fg=%s]  %s#[fg=%s]/#[fg=%s,bold]%s#[nobold,fg=%s]  %s#[fg=%s]/%s' \
    "$(fmt_time "$cur_s")"    "$dim" "$(fmt_time "$day_s")"    "$fg" \
    "$cur_steps"              "$dim" "$STEPS_FG" "$day_steps"  "$fg" \
    "$(fmt_dist "$cur_dist")" "$dim" "$(fmt_dist "$day_dist")")
else
  body=$(printf '%s  #[fg=%s,bold]%s#[nobold,fg=%s]  %s' \
    "$(fmt_time "$cur_s")" "$STEPS_FG" "$cur_steps" "$fg" "$(fmt_dist "$cur_dist")")
fi

# Paint the pill: leading powerline arrow (pill colour on $BAR_BG, skipped if
# SEP is empty), then the pill body with its own padding + colours, then
# reset so anything drawn after this segment is unaffected.
printf '#[fg=%s,bg=%s]%s#[bg=%s,fg=%s] %s %s #[default]' \
  "$bg" "$BAR_BG" "$SEP" "$bg" "$fg" "$icon" "$body"
