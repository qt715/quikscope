# Configuration

`$XDG_CONFIG_HOME/quikscope/config.toml` (usually `~/.config/quikscope/config.toml`).
On Windows: `%APPDATA%\quikscope\config.toml`; the default save directory
is the Pictures known folder plus `Screenshots`, and `~` means `%USERPROFILE%`.
Every key is optional; unknown keys are rejected so typos do not pass
silently. The daemon reads the file at start-up; hand edits require
`quikscope quit` to reload (it stops and saves a running recording first, as
SIGTERM does), but changes through `quikscope settings` apply
immediately to the running daemon and future captures/editors.

```toml
# Where Ctrl+S in the editor (and `capture --save`) writes. `~` expands to $HOME.
save_dir = "~/Pictures/Screenshots"

# strftime-style name: %Y %m %d %H %M %S, %s (Unix time) and %%.
# A numeric suffix is added instead of overwriting an existing file.
filename = "screenshot_%Y%m%d_%H%M%S.png"

# Put every capture on the clipboard as image/png.
copy_to_clipboard = true

# Show a notification after each capture; clicking it opens the editor.
# `quikscope notifications on|off|toggle` changes this at runtime and saves it here.
notify = true

# Open the editor right after capturing instead of waiting for a click.
open_editor = false

# Include the pointer in captures.
cursor = false

# Freeze before region selection. False selects over the live desktop and
# captures pixels only after the selection overlay closes.
freeze = true

# Notification timeout in milliseconds (-1: the server's default).
notification_timeout_ms = 8000

[editor]
# Initial annotation color: "#rrggbb" or "#rrggbbaa".
color = "#e03131"
# Initial size: 0 thin, 1 medium, 2 thick.
stroke = 1
# Close the editor after a successful save (Ctrl+S or Save as).
close_on_save = false
# Starting font for new text: any installed family (unknown families fall
# back to the bundled "Inter"), weight 1-1000, size 4-512 logical pixels.
font_family = "Inter"
font_weight = 600
font_size = 28
font_italic = false

[ocr]
# Tesseract models for text extraction, e.g. ["eng", "jpn"]: the first reads
# everything, the others re-read lines it is unsure of. Empty (the default):
# English first, then every installed model.
languages = []

[record]
# Where recordings are written (Windows: Videos\Screencasts). The file name is
# strftime-style without extension.
save_dir = "~/Videos/Screencasts"
filename = "recording_%Y%m%d_%H%M%S"
# "mp4", "mkv" or "mov": H.264 video with Opus audio.
video_format = "mp4"
# MP4 layout. Fragmented (the default) is written under its final name as
# it records, about one fragment a second, so a crash or kill leaves a file
# that plays up to its last fragment; false writes fragments to a partial
# file while recording and rewrites them as a classic MP4 with its index at
# the front when the recording ends (some older players need that). MOV
# always ends classic; MKV is written in place like fragmented MP4.
mp4_fragmented = true
# "gif", "webp", "avif" or "jxl".
animation_format = "gif"
# Frames per second: 1–240 for video, 1–60 for animations.
video_fps = 60
animation_fps = 30
# Audio recorded into videos: "default-output" (what you hear),
# "default-input" (the default microphone) or ids from
# `quikscope audio-devices`. Several devices are mixed. [] records none.
audio = ["default-output"]
# Include the pointer.
cursor = true
# Thin outline just outside the recorded area; never part of the recording.
outline = true
# Put the finished file on the clipboard (paste it into a chat or file manager).
copy_to_clipboard = true
# Notification with a thumbnail when saved; clicking it opens the editor.
notify = true
# Open the video editor / annotator as soon as a recording stops.
open_editor = false
# "auto" encodes H.264 on the GPU through Vulkan Video (NVIDIA, AMD, Intel)
# when that works, else with OpenH264 in software; "software" always uses
# OpenH264. Decoding (playback, thumbnails, exports) follows the same choice.
encoder = "auto"
# Linux: status-notifier tray icon with recording controls. Changing it in
# Settings shows or removes the icon at once.
tray = true

[hotkeys]
# Windows only (unused on Linux).
# Empty string disables a shortcut. Modifiers: Ctrl, Alt, Shift, Win.
# Keys: PrintScreen, F1–F24, A–Z, 0–9. Two shortcuts this file sets to the
# same keys are rejected; a shortcut it leaves out whose default is already
# bound here to something else (an older config predating the recording
# shortcuts) is left unbound instead.
region = "PrintScreen"
screen = "Ctrl+PrintScreen"
window = "Alt+PrintScreen"
open_last = "Shift+PrintScreen"
# Press again to stop. Win+Shift+R (Snipping Tool) and Win+Alt+R (Game Bar)
# belong to Windows' own recorders, Alt+Shift+PrintScreen toggles High
# Contrast.
record_video = "Ctrl+Shift+PrintScreen"
record_animation = "Ctrl+Alt+PrintScreen"
```

Per-capture command-line flags (`--edit`, `--no-notify`, `--no-clipboard`,
`--cursor`, `--freeze` / `--no-freeze`) override these defaults for one request.
`freeze` affects interactive region selection only; fixed geometry and other
capture modes do not open a selection overlay. Live selection dims outside
the selected rectangle and leaves the inside transparent. Portal permission
dialogs appear after live selection closes. GNOME's fullscreen overlay is
transparent only when its compositor permits it. On X11 without a compositor,
an alpha visual or X Present, live requests fall back to frozen selection
with a log message. Windows live selection was verified on one Windows 11
monitor at 96 DPI; mixed-DPI and multi-monitor behavior remains unverified.

## Settings and first run

Open preferences with `quikscope settings`, the editor's gear button (bottom right)
or **Settings…** in the Windows tray. A second request focuses the same window.
Capture toggles apply as soon as clicked; text values apply when submitted
with Enter, Tab or leaving the field. Invalid values show an inline error and
leave the file and running defaults unchanged.
Escape restores the last saved value in a field. Closing submits any current
field edit; fix or discard invalid edits before closing.

Settings include frozen/live selection, clipboard, notifications, automatic
editor opening, cursor inclusion, save folder/name, editor colour/stroke/font
defaults, OCR languages and recording (formats, frame rates, folder, audio
devices, encoder, crash-safe MP4, outline, pointer, clipboard, notification,
editor opening and, on Linux, the tray icon). OCR controls appear only when Tesseract is found;
otherwise the window explains that installation is needed. Windows displays
the configured global shortcuts with enable/disable controls.

If no config file exists, the daemon opens a short welcome form: choose the
region mode and common toggles, then Done. Done or closing the window writes
the chosen values, so onboarding does not recur. Choices preview immediately
but the config file is created only when onboarding is finished.

All UI edits (and `quikscope notifications on|off|toggle`) validate before an
atomic config replacement. Comments, ordering and formatting are preserved,
including inline comments and unrelated sections. Unknown keys still fail
strict validation. If loading fails, the daemon logs the error and uses
defaults; settings shows the parse error and refuses to overwrite the file
until it is repaired.

## Recordings

Every file quikscope writes (screenshots, recordings, exports) gets a name
of its own before writing starts: when the configured name is taken, `-1`,
`-2`, … is added, so nothing is overwritten, also when two saves happen in
the same second. Until it is complete a file is written next to that name
as `<name>.<pid>.<n>.part`, except fragmented MP4 and MKV recordings, which
are playable at every fragment and so are written under their final name.

A capture larger than any H.264 encoder on the machine accepts (very wide
or multi-output areas) is recorded scaled down, and a notification says so.

Stopping a recording saves it before anything else: `quikscope quit`, the
tray's Quit, SIGTERM (`systemctl --user stop quikscope`; the unit allows ten
minutes for a long animation to encode) and logout wait for it, and for
exports or saves still running from an editor. `record status` and the tray
show how far the save is (`saving 42%`); `record cancel` or the tray's
**Cancel saving** stop it, discarding a video and keeping an animation's
lossless recording to export later.

### Crash recovery

When the daemon starts, it looks for what a daemon that died (crash, `kill
-9`, power loss) left in the screenshot and recording folders. Only files
named like the partial files above, of a process that no longer runs and
older than the daemon's start, are considered:

- a partial MP4, MOV or MKV recording that plays is published under a free
  name;
- an empty partial file and the empty placeholder reserved with it are
  removed;
- an animation's lossless recording that never became a GIF/WebP/AVIF/JPEG
  XL (the daemon died while recording or encoding it, or its save was
  cancelled) opens in the annotator, from which it can be exported;
- anything else is left alone.

One notification lists what was recovered; clicking it opens the newest.

## Environment

| Variable | Effect |
| --- | --- |
| `QUIKSCOPE_SOCKET` | IPC socket path. |
| `QUIKSCOPE_LOG` | Log filter (`env_logger` syntax), e.g. `quikscope=debug` for per-step timings. |
| `XDG_RUNTIME_DIR`, `XDG_CONFIG_HOME`, `XDG_CACHE_HOME`, `XDG_STATE_HOME` | Standard locations (the state directory keeps the ScreenCast portal's recording permission). |
| `QUIKSCOPE_BACKEND` | Linux capture backend for the daemon: `wlroots`, `portal` or `x11` (see the README's Desktops section). |
| `HYPRLAND_INSTANCE_SIGNATURE` | Hyprland instance to query; when unset, a single running instance is found automatically. |
| `APPDATA`, `LOCALAPPDATA`, `USERPROFILE` | Windows config, cache/IPC and home locations. |
| `QUIKSCOPE_WINDOWS_CAPTURE` | Windows capture method: `dxgi` (default with GDI availability fallback) or forced `gdi`, for measurements. |
| `QUIKSCOPE_NO_PREWARM` | `1` skips the invisible window the daemon opens for 250 ms at start-up to create its GPU device and compile shaders before the first capture. The first selection overlay or editor then opens more slowly, but the idle daemon holds no GPU device until a window needs one: for A/B tests of GPU residency (e.g. alongside games). |
