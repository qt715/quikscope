# Configuration

`$XDG_CONFIG_HOME/quickscope/config.toml` (usually `~/.config/quickscope/config.toml`).
On Windows: `%APPDATA%\quickscope\config.toml`; the default save directory
is the Pictures known folder plus `Screenshots`, and `~` means `%USERPROFILE%`.
Every key is optional; unknown keys are rejected so typos do not pass
silently. The daemon reads the file at start-up; hand edits require
`quickscope quit` to reload, but changes through `quickscope settings` apply
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
# `quickscope notifications on|off|toggle` changes this at runtime and saves it here.
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
# Close the editor after a successful Ctrl+S.
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

[hotkeys]
# Windows-only global shortcuts; accepted/validated but unused on Linux.
# Empty string disables a shortcut. Modifiers: Ctrl, Alt, Shift, Win.
# Keys: PrintScreen, F1–F24, A–Z, 0–9. Duplicate bindings are rejected.
region = "PrintScreen"
screen = "Ctrl+PrintScreen"
window = "Alt+PrintScreen"
open_last = "Shift+PrintScreen"
```

Per-capture command-line flags (`--edit`, `--no-notify`, `--no-clipboard`,
`--cursor`, `--freeze` / `--no-freeze`) override these defaults for one request.
`freeze` affects interactive region selection only; fixed geometry and other
capture modes do not open a selection overlay. Live selection dims outside
the selected rectangle and leaves the inside transparent. Portal permission
dialogs appear after live selection closes. GNOME's fullscreen overlay is
transparent only when its compositor permits it. On X11 without a compositor,
an alpha visual or X Present, live requests fall back to frozen selection
with a log message. Windows live selection compiles but is unverified natively.

## Settings and first run

Open preferences with `quickscope settings`, the editor toolbar's gear button
or **Settings…** in the Windows tray. A second request focuses the same window.
Capture toggles apply as soon as clicked; text values apply when submitted
with Enter, Tab or leaving the field. Invalid values show an inline error and
leave the file and running defaults unchanged.
Escape restores the last saved value in a field. Closing submits any current
field edit; fix or discard invalid edits before closing.

Settings include frozen/live selection, clipboard, notifications, automatic
editor opening, cursor inclusion, save folder/name, editor colour/stroke/font
defaults and OCR languages. OCR controls appear only when Tesseract is found;
otherwise the window explains that installation is needed. Windows displays
the configured global shortcuts with enable/disable controls.

If no config file exists, the daemon opens a short welcome form: choose the
region mode and common toggles, then Done. Done or closing the window writes
the chosen values, so onboarding does not recur. Choices preview immediately
but the config file is created only when onboarding is finished.

All UI edits (and `quickscope notifications on|off|toggle`) validate before an
atomic config replacement. Comments, ordering and formatting are preserved,
including inline comments and unrelated sections. Unknown keys still fail
strict validation. If loading fails, the daemon logs the error and uses
defaults; settings shows the parse error and refuses to overwrite the file
until it is repaired.

## Environment

| Variable | Effect |
| --- | --- |
| `QUICKSCOPE_SOCKET` | IPC socket path. |
| `QUICKSCOPE_LOG` | Log filter (`env_logger` syntax), e.g. `quickscope=debug` for per-step timings. |
| `XDG_RUNTIME_DIR`, `XDG_CONFIG_HOME`, `XDG_CACHE_HOME` | Standard locations. |
| `HYPRLAND_INSTANCE_SIGNATURE` | Hyprland instance to query; when unset, a single running instance is found automatically. |
| `APPDATA`, `LOCALAPPDATA`, `USERPROFILE` | Windows config, cache/IPC and home locations. |
| `QUICKSCOPE_WINDOWS_CAPTURE` | Windows capture method: `dxgi` (default with GDI availability fallback) or forced `gdi`, for measurements. |
