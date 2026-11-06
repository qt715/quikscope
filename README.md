# moment

A fast screenshot tool for wlroots-based Wayland compositors (Hyprland, sway,
labwc, river, …) with a built-in annotation editor written in
[GPUI](https://gpui.rs).

```text
keybind ─▶ moment capture region ─▶ frozen-screen selection ─▶ clipboard + notification
                                                                     │ click / --edit
                                                                     ▼
                                      editor (arrows, boxes, text, pen, pixelate, crop…)
                                                                     │ Ctrl+S
                                                                     ▼
                                                 ~/Pictures/Screenshots/screenshot_*.png
```

- **Fast.** A resident daemon keeps the Wayland connection, GPU device and
  shared-memory buffers warm. A 2560×1440 capture reaches the clipboard in
  ~15 ms end to end, and the editor paints its first frame ~5 ms after it is
  created (see [Performance](#performance)).
- **Freeze-frame region selection** on every output at once, with window
  snapping on Hyprland (click a window to take exactly that window).
- **Excalidraw-style editor**: rectangle, ellipse, arrow, line, pen,
  highlighter, text, numbered steps, pixelate (redaction) and crop, with
  undo/redo, zoom and pan. What you see is exactly what gets saved — the
  editor and the PNG exporter fill the same geometry.
- **Plain IPC**: one text line over a Unix socket; usable from keybinds,
  scripts or `socat`.

## Install

Requires a compositor with `wlr-screencopy`, `wlr-layer-shell` and
`wlr-data-control` (all wlroots compositors and Hyprland), a session D-Bus
with a notification server, and Rust ≥ 1.95 to build.

```sh
cargo install --locked --path .
# optional: start the daemon with the graphical session
install -Dm644 packaging/moment.service ~/.config/systemd/user/moment.service
systemctl --user enable --now moment.service
# optional: launcher entry ("Open last screenshot") and notification icon
install -Dm644 packaging/moment.desktop ~/.local/share/applications/moment.desktop
```

The daemon is optional at login: any `moment` command starts it on demand
(the first capture then pays the daemon start-up, about 0.3 s, once).

## Hyprland

`hyprland.lua`:

```lua
hl.bind(mainMod .. " + SHIFT + S", hl.dsp.exec_cmd("moment capture region"), { description = "Screenshot region" })
hl.bind(mainMod .. " + SHIFT + CTRL + S", hl.dsp.exec_cmd("moment capture output"), { description = "Screenshot monitor" })
hl.bind(mainMod .. " + SHIFT + ALT + S", hl.dsp.exec_cmd("moment capture region --edit"), { description = "Annotate region" })
hl.bind(mainMod .. " + SHIFT + E", hl.dsp.exec_cmd("moment open-last"), { description = "Edit last screenshot" })

hl.window_rule({ match = { class = "^(moment)$" }, float = true, center = true })
hl.layer_rule({ match = { namespace = "^(moment-selection)$" }, no_anim = true })
```

`hyprland.conf`:

```ini
bind = $mainMod SHIFT, S, exec, moment capture region
bind = $mainMod SHIFT CTRL, S, exec, moment capture output
bind = $mainMod SHIFT ALT, S, exec, moment capture region --edit
bind = $mainMod SHIFT, E, exec, moment open-last
windowrule = float on, match:class ^(moment)$
windowrule = center on, match:class ^(moment)$
layerrule = no_anim on, match:namespace ^(moment-selection)$
```

`no_anim` on the selection layer makes the frozen overlay appear instantly
instead of fading in. On sway, add `for_window [app_id="moment"] floating enable`.

## Usage

```text
moment capture <region|output|window|screen> [options]
moment open-last          open the latest capture in the editor, with the tool you used last
moment open <FILE>        annotate any PNG/JPEG/WebP
moment notifications <on|off|toggle|status>
moment daemon             run the daemon in the foreground
moment ping | quit
```

| Mode | Captures |
| --- | --- |
| `region` | Freezes all outputs; drag a rectangle, click a window (Hyprland) or click empty space for the whole output. Right click, `Esc` or `q` cancels; `Enter` takes the output under the keyboard focus. |
| `output` | The focused output on Hyprland, the first output elsewhere, or `-o NAME`. |
| `window` | The active window (Hyprland). |
| `screen` | All outputs composed in their layout. |

| Option | Effect |
| --- | --- |
| `--edit` / `--no-edit` | Open the editor immediately instead of on notification click. |
| `--notify` / `--no-notify` | Desktop notification with a thumbnail; clicking it opens the editor. |
| `--clipboard` / `--no-clipboard` | Put the capture on the clipboard as `image/png`. |
| `--cursor` / `--no-cursor` | Include the pointer. |
| `--save` | Also write the raw capture to the screenshots folder (prints the path). |
| `-o, --output NAME` | Output for `output` mode. |
| `-g, --geometry "X,Y WxH"` | Capture a logical rectangle without interaction (`slurp` format). |

Exit status is 0 on success, 1 when the selection was cancelled or an error
occurred. Defaults come from the [configuration](docs/configuration.md).

### Notifications on/off

`notify = false` in the config file turns capture notifications off.
`moment notifications toggle` (or `on`/`off`) flips the setting in the
running daemon at once and writes it back to the config file, keeping your
other settings and comments, so it survives restarts; `status` prints it.
Bind it to a key to switch notifications off while gaming or presenting:

```lua
hl.bind(mainMod .. " + SHIFT + N", hl.dsp.exec_cmd("moment notifications toggle"), { description = "Toggle screenshot notifications" })
```

`--notify` / `--no-notify` still override the setting for a single capture.

## Editor

| Tool | Keys |
| --- | --- |
| Select / move | `V` `1` — click to select, drag to move, `Del` deletes, arrows nudge (`Shift` ×10), double-click text to edit |
| Pan | `H`, `Space`+drag, middle drag, scroll |
| Rectangle | `R` `2` (`Shift` = square) |
| Ellipse | `O` `3` (`Shift` = circle) |
| Arrow | `A` `4` (`Shift` snaps to 15°) |
| Line | `L` `5` (`Shift` snaps to 15°) |
| Pen | `P` `6` |
| Text | `T` `7` — click, type, `Enter` for a new line, `Esc` or click away to finish |
| Highlighter | `M` `8` |
| Step number | `N` `9` — numbered badges that count up |
| Pixelate | `X` `0` — irreversible mosaic for redaction |
| Crop | `C` — drag the area to keep; click without dragging to reset |

| Action | Keys |
| --- | --- |
| Save to `~/Pictures/Screenshots` | `Ctrl+S` (repeat saves update the same file); `Ctrl+Shift+S` always writes a new file |
| Copy annotated image | `Ctrl+C` |
| Undo / redo | `Ctrl+Z` / `Ctrl+Shift+Z`, `Ctrl+Y` |
| Fill shapes | `F` |
| Stroke / text size | `[` `]` or the toolbar dots |
| Text format (while typing or with text selected) | `Ctrl+B` bold, `Ctrl+I` italic, `Ctrl+Shift+.` / `Ctrl+Shift+,` bigger / smaller |
| Zoom | `Ctrl`+scroll (one level per wheel notch), `Ctrl+=` / `Ctrl+-`, `Ctrl+0` fit, `Ctrl+1` actual pixels. Levels are 10% steps up to 100% and 25% steps above (100% = actual pixels); zoom changes ease in with a `cubic-bezier(0.22, 1, 0.36, 1)` curve and keep the point under the cursor fixed |
| Close | `Esc` (when nothing is selected), `Ctrl+W` |

Style changes (color, size, fill, font) apply to the selected shape. Closing the
editor keeps its annotations in the daemon: `moment open-last` or another
notification click brings them back.

**Drawing past the edges.** Annotations may extend beyond the screenshot,
for example a label written beside it. The saved and copied image grows to
include them, with a transparent margin where there is no screenshot, and
the editor refits the view to show everything (until you zoom or pan
yourself). A crop can include that margin.

**Colors.** Besides the eight swatches, the palette button opens a color
picker: a hue/saturation wheel, value and alpha sliders, a hex field
(`#rrggbb` or `#rrggbbaa`) and R, G, B, A fields (Up/Down step by 1,
Shift+Up/Down by 10). Dragging recolors the selected shape live and counts as
a single undo step. While the current color is not one of the swatches, the
palette button shows it.

**Fonts.** While text is being edited or a text shape is selected, a font bar
appears above it: family (every installed font plus the bundled Inter,
filterable by typing), weight, italic and size. A font without an italic
face gets a slanted version. New text uses the last font chosen; the starting
font comes from the [configuration](docs/configuration.md).

**Any language.** The text tool works with input methods (fcitx5 with Mozc,
Anthy, Hangul, Pinyin…; IBus): pre-edit appears underlined at the caret and
the candidate window opens there. Text is shaped properly, so Arabic and
Hebrew run right to left (mixed with left-to-right text), Arabic letters
join, and Indic scripts form conjuncts. Characters the chosen font lacks are
drawn from Inter, then from whichever installed font has them, so CJK
appears in your CJK font and emoji in colour.

## Performance

Measured in a headless labwc session at 2560×1440 (software GL, release
build); a real GPU and a high-refresh compositor are faster still.

| Step | Time |
| --- | --- |
| `moment ping` round trip (client start-up + IPC) | ~1 ms |
| `moment capture screen` round trip | ~15 ms (13 ms is the compositor delivering the next frame) |
| Clipboard PNG ready (encoded in the background) | ~22 ms after capture |
| Region overlay windows open after the freeze | ~15 ms |
| Editor first frame after its window is created | ~5 ms |

How: no per-capture setup (bound globals, reused `wl_shm` buffers), all
outputs captured concurrently, zero-copy hand-off of whole-output captures,
BGRA kept end to end (the compositor's byte order is GPUI's texture order),
GPU tiles built off the main thread before the editor asks for them, a GPU
prewarm at daemon start, and the clipboard claimed before its PNG is encoded.
Details in [docs/architecture.md](docs/architecture.md).

## Limitations

- wlroots-protocol compositors only; GNOME and KDE lack `wlr-screencopy`.
- Window snapping, `window` mode and focused-output detection need Hyprland.
- Characters no installed font has are drawn as the font's missing-glyph
  box. Text is always left-aligned, also right-to-left text.

## Development

```sh
cargo test                                   # unit, golden-image and GPU parity tests
cargo test --features e2e --test e2e         # end-to-end, in a private headless labwc/sway
MOMENT_BLESS=1 cargo test --test render_regression   # regenerate golden images
```

See [docs/testing.md](docs/testing.md), [docs/ipc.md](docs/ipc.md) and
[docs/architecture.md](docs/architecture.md).

## License

MIT or Apache-2.0, at your option. Bundled assets: Inter variable and Inter
italic (SIL Open Font License, `assets/fonts/INTER-LICENSE.txt`) and Lucide
icons (ISC, `assets/icons/LICENSE`).
