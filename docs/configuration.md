# Configuration

`$XDG_CONFIG_HOME/quickscope/config.toml` (usually `~/.config/quickscope/config.toml`).
Every key is optional; unknown keys are rejected so typos do not pass
silently. The daemon reads the file at start-up (`quickscope quit` to reload).

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
```

Per-capture command-line flags (`--edit`, `--no-notify`, `--no-clipboard`,
`--cursor`) override these defaults for one request.

`quickscope notifications on|off|toggle` is the one setting that changes
without a restart: the daemon applies it immediately and rewrites only the
`notify` key in this file (comments and other keys are preserved; a file the
change would make invalid is left untouched and the command fails).

## Environment

| Variable | Effect |
| --- | --- |
| `QUICKSCOPE_SOCKET` | IPC socket path. |
| `QUICKSCOPE_LOG` | Log filter (`env_logger` syntax), e.g. `quickscope=debug` for per-step timings. |
| `XDG_RUNTIME_DIR`, `XDG_CONFIG_HOME`, `XDG_CACHE_HOME` | Standard locations. |
| `HYPRLAND_INSTANCE_SIGNATURE` | Hyprland instance to query; when unset, a single running instance is found automatically. |
