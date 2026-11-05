# Configuration

`$XDG_CONFIG_HOME/moment/config.toml` (usually `~/.config/moment/config.toml`).
Every key is optional; unknown keys are rejected so typos do not pass
silently. The daemon reads the file at start-up (`moment quit` to reload).

```toml
# Where Ctrl+S in the editor (and `capture --save`) writes. `~` expands to $HOME.
save_dir = "~/Pictures/Screenshots"

# strftime-style name: %Y %m %d %H %M %S, %s (Unix time) and %%.
# A numeric suffix is added instead of overwriting an existing file.
filename = "screenshot_%Y%m%d_%H%M%S.png"

# Put every capture on the clipboard as image/png.
copy_to_clipboard = true

# Show a notification after each capture; clicking it opens the editor.
# `moment notifications on|off|toggle` changes this at runtime and saves it here.
notify = true

# Open the editor right after capturing instead of waiting for a click.
open_editor = false

# Include the pointer in captures.
cursor = false

# Notification timeout in milliseconds (-1: the server's default).
notification_timeout_ms = 8000

[editor]
# Initial annotation color.
color = "#e03131"
# Initial size: 0 thin, 1 medium, 2 thick.
stroke = 1
# Close the editor after a successful Ctrl+S.
close_on_save = false
```

Per-capture command-line flags (`--edit`, `--no-notify`, `--no-clipboard`,
`--cursor`) override these defaults for one request.

`moment notifications on|off|toggle` is the one setting that changes
without a restart: the daemon applies it immediately and rewrites only the
`notify` key in this file (comments and other keys are preserved; a file the
change would make invalid is left untouched and the command fails).

## Environment

| Variable | Effect |
| --- | --- |
| `MOMENT_SOCKET` | IPC socket path. |
| `MOMENT_LOG` | Log filter (`env_logger` syntax), e.g. `moment=debug` for per-step timings. |
| `XDG_RUNTIME_DIR`, `XDG_CONFIG_HOME`, `XDG_CACHE_HOME` | Standard locations. |
| `HYPRLAND_INSTANCE_SIGNATURE` | Hyprland instance to query; when unset, a single running instance is found automatically. |
