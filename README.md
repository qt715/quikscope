![quikscope — Capture. Annotate. Carry on.](assets/branding/banner.svg)

# quikscope

A fast screenshot tool for Linux (wlroots compositors such as Hyprland and
sway, GNOME, KDE Plasma, X11) and Windows 10/11, with a built-in annotation
editor written in [GPUI](https://gpui.rs).

```text
keybind ─▶ quikscope capture region ─▶ frozen-screen selection ─▶ clipboard + notification
                                                                     │ click / --edit
                                                                     ▼
                                      editor (arrows, boxes, text, pen, pixelate, crop…)
                                                                     │ Ctrl+S
                                                                     ▼
                                                 ~/Pictures/Screenshots/screenshot_*.png
```

- **Fast.** A resident daemon keeps the display connection, capture buffers
  and GPU device warm. A 2560×1440 capture reaches the clipboard in ~15 ms
  end to end, and the editor paints its first frame ~5 ms after it is created
  (see [Performance](#performance)).
- **Native on each desktop.** `wlr-screencopy` on wlroots compositors, the
  Screenshot portal on GNOME and KDE Plasma, MIT-SHM on X11, DXGI on Windows;
  the backend is picked at start-up ([details](#desktops)).
- **Frozen or live region selection** on every output at once (frozen by
  default), with window snapping on Hyprland, X11 and Windows (click a window
  to take exactly that window). Live selection dims outside the rectangle and
  leaves a transparent hole; pixels are captured after selection closes.
- **Excalidraw-style editor**: rectangle, ellipse, arrow, line, pen,
  highlighter, text, numbered steps, pixelate (redaction) and crop, with
  undo/redo, zoom and pan. What you see is exactly what gets saved — the
  editor and the PNG exporter fill the same geometry.
- **Text extraction**: copy the text out of a region or a screenshot, in
  English and other European languages, Chinese, Japanese, Korean,
  Vietnamese, Thai and Arabic, each line read in its own language
  ([details](docs/text-extraction.md)).
- **Plain IPC**: one text line over a local socket; usable from keybinds,
  scripts or `socat`.

## Install

Build with Rust ≥ 1.95. On Linux, see [Desktops](#desktops) for what each
desktop needs; on Windows, see [Windows 10/11](#windows-1011).

```sh
cargo install --locked --path .
# optional: start the daemon with the graphical session
install -Dm644 packaging/quikscope.service ~/.config/systemd/user/quikscope.service
systemctl --user enable --now quikscope.service
# optional: launcher entry ("Open last screenshot") and notification icon
install -Dm644 packaging/quikscope.desktop ~/.local/share/applications/quikscope.desktop
```

The daemon is optional at login: any `quikscope` command starts it on demand
(the first capture then pays the daemon start-up, about 0.3 s, once).

On first start, a short welcome window lets you choose frozen or live region
selection and common capture defaults. Done or closing the welcome window
writes your configuration; subsequent starts stay in the background.
Run `quikscope settings` any time (also the editor's gear button or Windows
tray **Settings…**). Changes save and apply to the running daemon immediately.

Text extraction additionally needs Tesseract and a model per language,
loaded at run time (nothing extra to build):

```sh
# Arch
sudo pacman -S tesseract tesseract-data-eng tesseract-data-jpn tesseract-data-chi_sim \
  tesseract-data-kor tesseract-data-vie tesseract-data-tha tesseract-data-ara
# Debian/Ubuntu
sudo apt install tesseract-ocr tesseract-ocr-eng tesseract-ocr-jpn tesseract-ocr-chi-sim \
  tesseract-ocr-kor tesseract-ocr-vie tesseract-ocr-tha tesseract-ocr-ara
```

## Desktops

The daemon picks a backend at start-up and logs it (`platform backend: …`);
`QUIKSCOPE_BACKEND=wlroots|portal|x11` on the daemon forces one.

| Desktop | Capture | Region overlay | Window snapping, `window` mode |
| --- | --- | --- | --- |
| Hyprland, sway, river, labwc, … | `wlr-screencopy` | layer-shell, all outputs | Hyprland only |
| KDE Plasma | Screenshot portal | layer-shell, all outputs | no |
| GNOME | Screenshot portal | one fullscreen window, one output | no |
| X11 | MIT-SHM (GetImage remotely) | override-redirect popups | EWMH window managers |
| Windows 10/11 | DXGI duplication (GDI fallback) | topmost popups | yes |

### wlroots compositors

Needs `wlr-screencopy`, `wlr-layer-shell` and `wlr-data-control`, which every
wlroots compositor and Hyprland provide. Bind keys as shown for
[Hyprland](#hyprland); on sway:

```ini
bindsym Print exec quikscope capture region
bindsym Shift+Print exec quikscope capture screen
bindsym $mod+Shift+e exec quikscope open-last
for_window [app_id="quikscope"] floating enable
```

### Hyprland

`hyprland.lua`:

```lua
hl.bind(mainMod .. " + SHIFT + S", hl.dsp.exec_cmd("quikscope capture region"), { description = "Screenshot region" })
hl.bind(mainMod .. " + SHIFT + CTRL + S", hl.dsp.exec_cmd("quikscope capture output"), { description = "Screenshot monitor" })
hl.bind(mainMod .. " + SHIFT + ALT + S", hl.dsp.exec_cmd("quikscope capture region --edit"), { description = "Annotate region" })
hl.bind(mainMod .. " + SHIFT + E", hl.dsp.exec_cmd("quikscope open-last"), { description = "Edit last screenshot" })
hl.bind(mainMod .. " + SHIFT + X", hl.dsp.exec_cmd("quikscope capture region --text"), { description = "Copy text from region" })

hl.window_rule({ match = { class = "^(quikscope)$" }, float = true, center = true })
hl.layer_rule({ match = { namespace = "^(quikscope-selection)$" }, no_anim = true })
```

`hyprland.conf`:

```ini
bind = $mainMod SHIFT, S, exec, quikscope capture region
bind = $mainMod SHIFT CTRL, S, exec, quikscope capture output
bind = $mainMod SHIFT ALT, S, exec, quikscope capture region --edit
bind = $mainMod SHIFT, E, exec, quikscope open-last
bind = $mainMod SHIFT, X, exec, quikscope capture region --text
windowrule = float on, match:class ^(quikscope)$
windowrule = center on, match:class ^(quikscope)$
layerrule = no_anim on, match:namespace ^(quikscope-selection)$
```

`no_anim` on the selection layer makes the frozen overlay appear instantly
instead of fading in.

### GNOME / KDE Plasma (portal)

Needs a session D-Bus, `xdg-desktop-portal` and the desktop's backend
(`xdg-desktop-portal-gnome` or `xdg-desktop-portal-kde`), which those
desktops install by default. Bind keys in Settings → Keyboard → Custom
Shortcuts (GNOME) or System Settings → Shortcuts → Add New → Command (KDE)
with commands such as `quikscope capture region`. On GNOME from a terminal:

```sh
p=/org/gnome/settings-daemon/plugins/media-keys/custom-keybindings/quikscope/
gsettings set org.gnome.settings-daemon.plugins.media-keys custom-keybindings "['$p']"
gsettings set org.gnome.settings-daemon.plugins.media-keys.custom-keybinding:$p name 'Screenshot region'
gsettings set org.gnome.settings-daemon.plugins.media-keys.custom-keybinding:$p command 'quikscope capture region'
gsettings set org.gnome.settings-daemon.plugins.media-keys.custom-keybinding:$p binding '<Shift><Super>s'
```

(The first command replaces existing custom shortcuts; merge the list by hand
if you have some.)

The daemon falls back to `org.freedesktop.portal.Screenshot` when
`wlr-screencopy` is unavailable. Set `QUIKSCOPE_BACKEND=portal` on the
daemon to force it (or `QUIKSCOPE_BACKEND=wlroots` to require native capture).
The desktop can still ask permission despite `interactive=false`; accept
its permission dialog to capture. Cancelling or denying returns a clear error.
With live region selection (`--no-freeze`), the permission dialog appears
only after selection closes; denying it still fails the capture.

Portal capture decodes a PNG file and is slower than native screencopy.
The portal controls cursor inclusion: `--cursor` / `--no-cursor` cannot
override it. On KDE and other desktops offering layer-shell, region
selection spans all outputs. Without layer-shell (normally GNOME), a
single fullscreen normal window shows the output where the compositor
places it; selection is confined to that output. Frozen mode shows its
snapshot; live mode is transparent only when the compositor permits
transparent fullscreen windows.
`screen` and fixed geometry captures can still span outputs. Desktop
fullscreen/focus animations can delay appearance; disable them in desktop
settings if needed. There is no window list/snapping or active-window
capture; `output` defaults to the first output. The portal raster must
cover the logical desktop at a uniform scale; incompatible mixed-scale
rasters are rejected rather than cropped incorrectly.

Clipboard uses data-control where available (including supported KDE
versions), otherwise Xwayland CLIPBOARD when DISPLAY is set. GNOME normally
bridges that selection to native Wayland apps. Without either backend, use
`--no-clipboard --save` (or edit/save). Notifications still work.
For diagnostics, `QUIKSCOPE_DEBUG_NO_LAYER_SHELL=1` on the daemon forces
the single-output fullscreen path, even on a layer-shell compositor.
Portal PNGs are not deleted: the API does not transfer ownership, and
GNOME/KDE may save into Pictures rather than create disposable files.

### X11

On X11, `DISPLAY` selects the backend when `WAYLAND_DISPLAY` is unset.
`QUIKSCOPE_BACKEND=x11` explicitly selects it (including on Xwayland).
MIT-SHM accelerates local captures; remote servers fall back to GetImage.
RandR 1.5 monitor names identify outputs; older servers expose one `screen-N`
output. X11 coordinates are physical pixels (scale 1). An EWMH window manager
enables active-window capture, window snapping and focused-output detection;
without one those queries are unknown. No X11 command-line helpers are needed
at runtime. Clipboard PNGs are served without re-encoding, including large
images via the X11 INCR protocol.
Live selection requires a compositor, an alpha visual and the X Present
extension; otherwise quikscope logs the limitation and uses frozen selection.

Bind keys in your window manager or desktop settings, e.g. i3:

```ini
bindsym Print exec --no-startup-id quikscope capture region
bindsym Shift+Print exec --no-startup-id quikscope capture screen
```

### Windows 10/11

Build on Windows 10 (1803+) or Windows 11 with Rust ≥ 1.95 and the Visual Studio C++ Build Tools:

```powershell
cargo install --locked --path .
quikscope autostart on
quikscope capture screen --save
```

`autostart on|off|status` manages only the current user's `quikscope` value
under `HKCU\Software\Microsoft\Windows\CurrentVersion\Run`. The daemon has no
console window; commands attach to the invoking terminal.

The notification-area icon offers region, screen and window capture, the
latest screenshot, notifications on/off and quit. Default global shortcuts:
`PrintScreen` = region, `Ctrl+PrintScreen` = screen, `Alt+PrintScreen` =
foreground window, `Shift+PrintScreen` = open last. Configure or disable them
in [`[hotkeys]`](docs/configuration.md); Windows or another application can
reserve a shortcut, in which case startup logs the registration error.

If Windows opens Snipping Tool instead, turn off **Settings → Accessibility →
Keyboard → Use the Print screen key to open screen capture**, then restart
quikscope (Windows may also require refreshing Explorer or signing in again).
Windows 11 enables this setting by default. quikscope logs a startup warning
and shows one notification when it is enabled; Settings also shows the remedy
beside the Windows hotkeys. quikscope does not change that Windows setting.

Configuration is `%APPDATA%\quikscope\config.toml`, cache/log/IPC live in
`%LOCALAPPDATA%\quikscope`, and saves use the Windows Pictures known folder
plus `Screenshots` (including redirected Pictures folders). Clipboard images
offer registered `PNG`, `CF_DIBV5` and `CF_DIB`; text uses `CF_UNICODETEXT`.
Capture notifications use Windows shell balloons; their display duration and
visibility follow Windows notification settings.

Capture retains DXGI duplication resources and falls back to reusable GDI
buffers when duplication is unavailable (including some virtual displays).
`QUIKSCOPE_WINDOWS_CAPTURE=gdi` forces GDI for comparison. Coordinates passed
to `--geometry` on Windows are **physical desktop pixels**, including negative
monitor origins. The per-monitor-v2-aware overlay converts surface DIPs at its
boundary, so mixed-DPI monitors retain their actual desktop layout and saved
pixel resolution. Protected/secure desktops and protected video cannot be
captured. Text extraction optionally needs Tesseract's `libtesseract-5.dll`
(or `tesseract55.dll` / `libtesseract.dll`) on `PATH` and traineddata models.

## Usage

```text
quikscope capture <region|output|window|screen> [options]
quikscope open-last          open the latest capture in the editor, with the tool you used last
quikscope open <FILE>        annotate any PNG/JPEG/WebP
quikscope settings          open or focus capture/editor preferences
quikscope notifications <on|off|toggle|status>
quikscope daemon             run the daemon in the foreground
quikscope ping | quit
```

| Mode | Captures |
| --- | --- |
| `region` | Freezes all outputs by default (`--no-freeze` selects over the live desktop); drag a rectangle, click a window (Hyprland or X11/EWMH) or click empty space for the whole output. Right click, `Esc` or `q` cancels; `Enter` takes the output under the keyboard focus. |
| `output` | The focused output on Hyprland or X11/EWMH, the first output elsewhere, or `-o NAME`. |
| `window` | The active window (Hyprland or X11/EWMH). |
| `screen` | All outputs composed in their layout. |

| Option | Effect |
| --- | --- |
| `--edit` / `--no-edit` | Open the editor immediately instead of on notification click. |
| `--notify` / `--no-notify` | Desktop notification with a thumbnail; clicking it opens the editor. |
| `--clipboard` / `--no-clipboard` | Put the capture on the clipboard as `image/png`. |
| `--cursor` / `--no-cursor` | Include the pointer. |
| `--freeze` / `--no-freeze` | Freeze before region selection (default) / select over the live desktop and capture after the overlay closes. Overrides `freeze` in the config. |
| `--save` | Also write the raw capture to the screenshots folder (prints the path). |
| `--text` | Extract the capture's text: copy it as plain text (instead of the image), print it, and show it in the notification. Exit status 1 when no text was found. |
| `-o, --output NAME` | Output for `output` mode. |
| `-g, --geometry "X,Y WxH"` | Capture a logical rectangle without interaction (`slurp` format). |

Exit status is 0 on success, 1 when the selection was cancelled or an error
occurred. Defaults come from the [configuration](docs/configuration.md).

### Notifications on/off

`notify = false` in the config file turns capture notifications off.
`quikscope notifications toggle` (or `on`/`off`) flips the setting in the
running daemon at once and writes it back to the config file, keeping your
other settings and comments, so it survives restarts; `status` prints it.
Bind it to a key to switch notifications off while gaming or presenting:

```lua
hl.bind(mainMod .. " + SHIFT + N", hl.dsp.exec_cmd("quikscope notifications toggle"), { description = "Toggle screenshot notifications" })
```

`--notify` / `--no-notify` still override the setting for a single capture.

## Editor

| Tool | Keys |
| --- | --- |
| Select / move | `V` `1` — drag empty canvas to select fully-contained items; `Shift`+click toggles membership, `Ctrl+A` selects all. Drag any selected item to move the whole selection (including from inside its box); `Del` deletes, arrows nudge (`Shift` ×10), double-click text to edit. Each gesture is one undo step |
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
| Copy text (from the crop, else the whole screenshot; annotations are not read) | `Ctrl+Shift+C` or the scan button; the lines read light up briefly |
| Undo / redo | `Ctrl+Z` / `Ctrl+Shift+Z`, `Ctrl+Y` |
| Fill shapes | `F` |
| Stroke / text size | `[` `]` or the toolbar dots |
| Duplicate selection | `Ctrl+D` — new copies offset by 10 logical pixels, selected together; `Ctrl+C` still copies the image |
| Bring selection to front / send to back | `Ctrl+Shift+]` / `Ctrl+Shift+[` |
| Shortcut help | `?` / `F1` or the toolbar `?`; `Esc` dismisses help without closing the editor. Fits small windows with scrolling and a single-column layout; `F1` also works while editing text |
| Text format (while typing or with text selected) | `Ctrl+B` bold, `Ctrl+I` italic, `Ctrl+Shift+.` / `Ctrl+Shift+,` bigger / smaller |
| Zoom | `Ctrl`+scroll (one level per wheel notch), `Ctrl+=` / `Ctrl+-`, `Ctrl+0` fit, `Ctrl+1` actual pixels. Levels are 10% steps up to 100% and 25% steps above (100% = actual pixels); zoom changes ease in with a `cubic-bezier(0.22, 1, 0.36, 1)` curve and keep the point under the cursor fixed |
| Close | `Esc` (when nothing is selected), `Ctrl+W` |

Style changes (color, size, fill) apply to every selected shape as one undo step.
The font bar targets exactly one selected text shape (or text being edited). Closing the
editor keeps its annotations in the daemon: `quikscope open-last` or another
notification click brings them back.

**Drawing past the edges.** Annotations may extend beyond the screenshot,
for example a label written beside it. The saved and copied image grows to
include them, with a transparent margin where there is no screenshot, and
the editor refits the view to show everything (until you zoom or pan
yourself). A crop can include that margin.

**Colors.** Besides the eight swatches, the palette button opens a color
picker: a hue/saturation wheel, value and alpha sliders, a hex field
(`#rrggbb` or `#rrggbbaa`) and R, G, B, A fields (Up/Down step by 1,
Shift+Up/Down by 10). Dragging recolors the selected shapes live and counts as
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
| `quikscope ping` round trip (client start-up + IPC) | ~1 ms |
| `quikscope capture screen` round trip | ~15 ms (13 ms is the compositor delivering the next frame) |
| Clipboard PNG ready (encoded in the background) | ~22 ms after capture |
| Region overlay shows the frozen frame after the request | ~17 ms (window creation overlaps the capture on layer-shell) |
| Editor first frame after its window is created | ~5 ms |

How: no per-capture setup (bound globals, reused `wl_shm` buffers), all
outputs captured concurrently, zero-copy hand-off of whole-output captures,
BGRA kept end to end (the compositor's byte order is GPUI's texture order),
GPU tiles built off the main thread before the editor asks for them, a GPU
prewarm at daemon start, and the clipboard claimed before its PNG is encoded.
Details in [docs/architecture.md](docs/architecture.md).

On a private Xvfb server at 2560×1440 (optimized test profile), 30 warm
captures measured a median of 1.36 ms with MIT-SHM (p95 1.69 ms), versus
2.51 ms with plain GetImage (p95 4.40 ms). These capture-only numbers exclude
PNG encoding and IPC. Reproduce with the `capture_latency_2560x1440` X11
test described in [docs/testing.md](docs/testing.md).

## Limitations

- On Wayland, window snapping, `window` mode and focused-output detection need
  Hyprland. GNOME confines region selection to one output (see
  [GNOME / KDE Plasma](#gnome--kde-plasma-portal)).
- X11 capture supports 24/32-bit TrueColor; monitors have no independent scale.
- Live selection on Windows compiles but has not been verified natively.
- macOS is not supported.
- Characters no installed font has are drawn as the font's missing-glyph
  box. Text is always left-aligned, also right-to-left text.

## Development

```sh
cargo test                                   # unit, golden-image and GPU parity tests
cargo test --features e2e --test e2e         # end-to-end, in a private headless labwc/sway
QUIKSCOPE_BLESS=1 cargo test --test render_regression   # regenerate golden images
```

See [docs/testing.md](docs/testing.md), [docs/ipc.md](docs/ipc.md) and
[docs/architecture.md](docs/architecture.md).

### Branding assets

Run `scripts/render-icons.sh` with resvg, ImageMagick 7 (`magick`) and Python 3
to regenerate the committed PNG, multi-resolution ICO and PNG-backed ICNS
assets in `assets/branding/`. The 16 px PNG uses the pixel-aligned symbolic
variant; larger sizes use `icon.svg`. All icons have transparent backgrounds
and use the UI accent (`#8f8cf2`). The banner's wordmark and tagline are
outlines of the bundled Inter variable font (weight 650, optical size 32),
so rendering needs no installed fonts.

Linux launchers and notifications request the `quikscope` theme icon; notification
servers choose their normal fallback if it is not installed, and screenshot
notifications still carry their `image-data` thumbnail. GPUI 0.3.10 supports
`WindowOptions.icon` on X11 only; Wayland uses the desktop entry, while the
Windows executable and tray use the embedded ICO resource.

## License

MIT or Apache-2.0, at your option. Bundled assets: Inter variable and Inter
italic (SIL Open Font License, `assets/fonts/INTER-LICENSE.txt`) and Lucide
icons (ISC, `assets/icons/LICENSE`).
