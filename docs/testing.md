# Testing

| Layer | Where | Needs |
| --- | --- | --- |
| Unit | `src/**` `#[cfg(test)]` | nothing; script shaping tests use installed Noto fonts and skip without them |
| CLI parsing | `src/main.rs` | nothing |
| Golden-image regression | `tests/render_regression.rs`, `tests/golden/` | nothing |
| GPU/CPU parity | `tests/editor_render.rs` | Vulkan or GL (Mesa lavapipe/llvmpipe is enough) |
| Editor interaction | `tests/editor_ui.rs` | Vulkan or GL, as above |
| Settings interaction | `tests/settings_ui.rs` | Vulkan or GL, as above |
| Text extraction | `tests/ocr.rs` | Tesseract and its language models; skips without them |
| End to end | `tests/e2e.rs` (feature `e2e`) | `labwc` or `sway`, `dbus-daemon`; `grim` for reference comparisons; `bwrap` optional; Tesseract for the text extraction test |
| X11 end to end | `tests/x11_e2e.rs` (feature `e2e`) | Xvfb, Mesa software GL/Vulkan |

```sh
cargo test                                    # everything but e2e
cargo test --features e2e --test e2e          # e2e (skips without a compositor)
QUIKSCOPE_E2E=1 cargo test --features e2e --test e2e   # e2e, missing tools are failures
```

`cargo test --features e2e --test x11_e2e` starts its own Xvfb server and
isolates HOME and XDG directories. It draws a known root pattern, compares
screen and dragged-region captures pixel-for-pixel, reads PNGs through the
real CLIPBOARD protocol (including INCR), and checks Escape/Enter keyboard
input on override-redirect overlays at 2× UI scale. Set `QUIKSCOPE_XVFB` to
an absolute Xvfb binary path when it is not installed in PATH;
`QUIKSCOPE_E2E=1` makes its absence a failure. RandR multi-monitor composition
is checked when the server supports monitor creation. The
`capture_latency_2560x1440` test compares SHM and plain GetImage pixels and
prints warm-capture median/p95 timings with `-- --nocapture`.

Xvfb has no compositor, so a requested live region must fall back to frozen
selection; it does not verify composited X11 transparency or the Present
teardown barrier. Those require a private composited X11 desktop.

The text-shaping unit tests (Japanese, Arabic, Hebrew, Devanagari, emoji)
need a font for each script, which they find among installed fonts; without
one a test prints `skipped:` and passes. `QUIKSCOPE_REQUIRE_FONTS=1` (set in CI,
which installs `fonts-noto-core`, `fonts-noto-cjk` and
`fonts-noto-color-emoji`) turns a missing font into a failure.

`tests/ocr.rs` renders chat-like screenshots (dark and light themes) with
lines in English, Japanese, Chinese, Korean, Arabic, Thai, Vietnamese and
German, extracts them with the real Tesseract, and checks every line's text
and the model that read it, region extraction and its coordinates, and
`ocr.languages`. A missing Tesseract or language model skips the affected
checks; `QUIKSCOPE_REQUIRE_OCR=1` (set in CI) makes it a failure. CI installs
Debian's "fast" models without `chi_tra`, see
[text extraction](text-extraction.md#limits).

## Windows

Native gates are `cargo build --release`, `cargo clippy --all-targets -- -D
warnings`, and `cargo test`. The `windows-latest` CI job runs those gates
with `--locked`: library tests, CLI parsing, CPU golden-image regressions and
optional OCR tests. Missing Tesseract/models skip OCR unless
`QUIKSCOPE_REQUIRE_OCR=1`. `editor_render` and `editor_ui` are cfg-excluded:
GPUI 0.3.10 returns no headless renderer on Windows, regardless of GPU.
The Wayland-only `e2e` feature is not enabled by Windows CI.

Native verification on a Windows 11 virtual machine at 2560×1440 / 96 DPI passed release build, warning-denying Clippy
and 161 tests (one interactive benchmark ignored). Screen capture matched a
GDI reference visually, and the PowerShell STA clipboard bitmap matched the
saved PNG exactly. A real 600×400 drag saved exactly 600×400 in frozen and live
modes: repainting a red reference window blue during selection preserved red
when frozen and captured clean blue when live. Escape cancelled both modes.
This does not verify mixed-DPI or multiple-monitor operation.

Notification activation opened the editor; a real rectangle drag and Ctrl+S
saved the annotated 2560×1440 image into Pictures\\Screenshots. The tray
Settings entry opened the native settings window. Physical PrintScreen
opened quikscope's overlay after disabling Windows' competing keyboard
setting and refreshing Explorer; the original absent registry override was
restored afterward. Startup with the Windows 11 default emitted the warning
and displayed its notification. See the [Windows setup note](../README.md#windows-1011).

Native overlay activation and teardown must occur outside GPUI App borrows.
Live captures also flush composition before removing their GPUI windows.
After these fixes, repeated frozen/live drags and cancellation produced no
`RefCell already borrowed` or window-not-found lifecycle errors.

The ignored benchmark passed in an interactive session at 2560×1440: GDI
median/p95 **33.620/36.249 ms**, DXGI-requested **0.032/0.086 ms** (100 warm
captures after ten warmups). The desktop was static, so the DXGI path can
reuse its cached desktop; these are not fresh-frame or changing-screen
latencies. The benchmark permits GDI fallback and does not independently
prove duplication provenance.

Interactive verification must use a private Windows VM desktop, never a
non-interactive service session or another user's live desktop. Check the tray,
screen capture PNG against the real desktop, delayed image clipboard,
region drag, Esc cancellation, PrintScreen, notification click to editor,
and editor Ctrl+S to the Pictures known folder's `Screenshots` directory.
Native verification is distinct from a Linux `cargo xwin check` pre-check.

For capture-method timings, run the ignored native test in that interactive
session: `cargo test --release capture_latency -- --ignored --nocapture`.
It reports resolution and median/p95 for 100 warm captures after ten
warmups, separately for GDI and DXGI. DXGI availability fallback is explicitly
labeled; do not call fallback timings duplication measurements. No latency
numbers are asserted without running the benchmark on the target desktop.

## Golden images

`render_regression` renders a fixed scene containing every annotation kind
(and a cropped variant) with the CPU exporter and compares it with
`tests/golden/*.png`, allowing anti-aliasing noise only. After an
intentional rendering change:

```sh
QUIKSCOPE_BLESS=1 cargo test --test render_regression
```

and review the changed PNGs in the diff. On failure the actual image is
written to `$TMPDIR/quikscope-<name>-actual.png`.

## GPU/CPU parity

`editor_render` opens the real `Editor` view in GPUI's headless test context
with the WGPU offscreen renderer, screenshots it, and compares the image area
with the exporter's output for the same document. A 3-pixel offset in the
editor's transform already fails it. On failure both images are written to
`$TMPDIR/quikscope-parity-{gpu,cpu}.png`.

## Editor interaction

`editor_ui` drives the same offscreen editor through GPUI's real event
dispatch (key and mouse events, painted between steps so hit-testing uses
current layout): text formatting shortcuts while typing and on a selected
text shape, a color-picker wheel drag that must recolor the selection as
exactly one undo step, an input-method composition (pre-edit, candidate
window position, commit) through the editor's `EntityInputHandler`, and a
text field receiving Backspace mid-composition (its pre-edit range must not
outlive the text).

Selection regressions dispatch real marquee, Shift-click, move, nudge,
delete, duplicate and stacking shortcuts. Multi-shape actions must restore
every member with one undo, with the next undo reaching the preceding user
action. Help toggling and Escape are checked without closing the window.
Mutation checks: forcing a checkpoint on every live `translate_all` /
`style_all` update must fail the drag-undo tests; adding a checkpoint after
`style_all` / `duplicate_all` must fail the palette/picker and duplicate
single-undo tests. Restore each mutation before running the full suite.

The small-window help test opens a 480×320 offscreen window and checks the
actual scroll-panel bounds against the 16-pixel window margin.

## Settings interaction

Offscreen GPUI tests dispatch real clicks and field edits: toggles persist
without losing comments, invalid values display an inline error and leave the
file unchanged, and successful edits invoke live config application. Welcome
Done persists the selected region mode. Native close and focus-loss submission
are covered in private-compositor e2e: GPUI's offscreen backend has no active
window and does not dispatch blur or native-close callbacks.

Persistence unit tests cover typed paths in top-level keys, tables, dotted
keys and inline tables, arrays, atomic creation and invalid-batch rejection.
Settings CLI/IPC parsing and serialization have unit coverage. Real daemon
tests open/refocus the settings window, complete onboarding without an
existing file, and verify the next capture uses the changed freeze default.
Mutation checks remove the changed value's comment suffix, omit inline error
painting, force welcome's saved mode to frozen, and skip the daemon config
swap. The comment-preserving toggle, visible-error, welcome-choice and real
next-capture live-apply tests must respectively fail. Restore every mutation
before running the complete suite.

## End to end

Each test builds a private desktop in a temporary directory and never
touches the developer's session:

- a headless compositor (`WLR_BACKENDS=headless`, pixman renderer) with its
  own `XDG_RUNTIME_DIR`; labwc runs inside `bwrap` with a private `/tmp` and
  network namespace when available, so its Xwayland sockets stay private;
- a private D-Bus with no service directories and an in-process fake
  `org.freedesktop.Notifications` that records notifications and can emit
  clicks;
- `quikscope-e2e-helper` (built with the `e2e` feature), which paints a known
  test pattern as wallpaper, keeps a virtual keyboard attached, drives a
  virtual pointer and keyboard, lists toplevels, reads the clipboard through
  data-control, reconfigures outputs, and plays a resident input method
  (`zwp_input_method_v2`, the protocol fcitx5 and IBus use) that reports
  when the compositor activates it and sends pre-edit and commit strings;
- the real `quikscope daemon` and `quikscope` client binaries.

The suite covers: screen capture → notification, cache and clipboard
(pixel-identical to `grim`); exact geometry crops; region drag, click,
right-click and `Esc`; refusal of concurrent selections; drawing, text,
`Ctrl+C`, `Ctrl+S`, undo and re-save in the editor; notification clicks
opening (and refocusing) the editor; `open-last` auto-starting a daemon from
the cache; malformed requests; rotated + scaled outputs against `grim`; and
two outputs, including a selection dragged across them; and input methods:
inactive while the editor is focused but no text is being typed, active
with the text tool, pre-edit drawn in place, commits (CJK and single-byte)
replacing it, and the saved text measuring exactly the committed string;
`open-last` reopening with the last tool used; and text extraction:
`Ctrl+Shift+C` in the editor pasted back as `text/plain`, and
`capture screen --text` printing, copying and notifying the text.

Live-selection cases repaint the resident wallpaper while selection is open
and compare exact final pixels: frozen mode keeps the old frame, live mode
captures the new one after teardown. `grim` checks the live overlay's outside
dimming and clear selected hole, including the fullscreen toplevel path on
labwc. Cases exercise live click/Enter, right-click/Esc cancellation,
`freeze = false`, per-request `--freeze` overrides, composition across two
outputs, and rejection of another selector during portal permission.
Mutation checks force live requests down the frozen path, then ignore the
config freeze value: the repaint/hole and config-default tests respectively
must fail. Restore each mutation before running the full suite.

Unit and CLI parsing tests cover the true config default, explicit boolean
config values and invalid types, omitted per-request overrides, both freeze
flags, last-flag-wins parsing and IPC serialization roundtrips.

Set `QUIKSCOPE_E2E_KEEP=1` to keep a session's directory (compositor and daemon
logs) after the test, and `QUIKSCOPE_E2E_COMPOSITOR=sway` to pick the
compositor.

### Portal fallback

`QUIKSCOPE_E2E=1 cargo test --features e2e --test e2e portal_ -- --nocapture`
runs the real daemon forced to `QUIKSCOPE_BACKEND=portal` in that same
private labwc desktop. A fake Screenshot service on the private D-Bus
captures real pixels with `grim`, writes a percent-escaped local file URI,
and emits Response **before** returning the method reply (catching late
subscription races). Tests compare screen pixels with grim; drag through
fullscreen normal windows; check Esc, Enter and both denial response
codes; and cross outputs through layer-shell overlays. A two-output test
forces `QUIKSCOPE_DEBUG_NO_LAYER_SHELL=1` with a rotated, distinct second
output: only the landed-on output is overlaid, its visible dimmed pixels
match that output's frozen frame, and Enter returns that exact output.
Another test makes the fake portal ask first through a permission-dialog
stand-in (the helper's `dialog`, an ordinary toplevel that answers on Enter
and denies after 5 s): Enter must reach the dialog, not a selection overlay.
The live portal case checks that no permission request occurs while selection
is open, completes selection before answering the permission-dialog stand-in,
then compares the final capture against the updated desktop pixels.
Five screen captures print end-to-end latency samples. Missing grim skips
unless `QUIKSCOPE_E2E=1`. Unit tests cover negative origins, fractional
scales, layout gaps, filters, incompatible raster dimensions and responses.

`QUIKSCOPE_E2E=1 cargo test --features e2e --test x11_e2e portal_`
exercises the clipboard fallback on a private Xvfb: a subprocess forced to
portal mode has no Wayland/data-control connection but has DISPLAY, offers
a PNG through the platform clipboard, and an independent X client reads
it back pixel-for-pixel. This tests X selection transport and fallback
choice, not GNOME's own Xwayland-to-Wayland bridge.

This fake exercises transport and real compositor pixels, not GNOME/KDE's
permission UI or their multi-monitor raster conventions. Real desktop
portal behavior and animation timing need testing on those desktops.
An isolated, extracted Mutter 51.1 was also started headless with a
1280×720 virtual output and software rendering. This was only a compositor
startup smoke: GNOME Shell and the GNOME/KDE portal backends were not
installed, so real GNOME/KDE Screenshot requests were not exercised.

On the private 1280×720 labwc desktop, five fake-portal screen captures
(`--save --no-notify --no-clipboard`, optimized test build) took
25.35, 20.51, 20.14, 20.53 and 20.59 ms end to end. This includes spawning
grim, PNG encode/decode, IPC and saving. Real desktop permission dialogs
and capture effects add unmeasured latency; these are not GNOME/KDE numbers.

## CI

`.github/workflows/ci.yml` runs formatting and clippy (`-D warnings`), the
test suite with Mesa lavapipe, Noto fonts and Tesseract on Ubuntu, an MSRV
check, the Wayland e2e suite in an Arch Linux container with labwc and
Tesseract, the X11 e2e suite on Ubuntu with private Xvfb servers, a Windows
build with clippy and the non-GPU tests, and release builds for Linux
(`quikscope-x86_64-linux.tar.gz`) and Windows (`quikscope-x86_64-windows.zip`)
that are attached to `v*` tags.
