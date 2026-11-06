# Testing

| Layer | Where | Needs |
| --- | --- | --- |
| Unit | `src/**` `#[cfg(test)]` | nothing; script shaping tests use installed Noto fonts and skip without them |
| CLI parsing | `src/main.rs` | nothing |
| Golden-image regression | `tests/render_regression.rs`, `tests/golden/` | nothing |
| GPU/CPU parity | `tests/editor_render.rs` | Vulkan or GL (Mesa lavapipe/llvmpipe is enough) |
| Editor interaction | `tests/editor_ui.rs` | Vulkan or GL, as above |
| Text extraction | `tests/ocr.rs` | Tesseract and its language models; skips without them |
| End to end | `tests/e2e.rs` (feature `e2e`) | `labwc` or `sway`, `dbus-daemon`; `grim` for reference comparisons; `bwrap` optional; Tesseract for the text extraction test |
| X11 end to end | `tests/x11_e2e.rs` (feature `e2e`) | Xvfb, Mesa software GL/Vulkan |

```sh
cargo test                                    # everything but e2e
cargo test --features e2e --test e2e          # e2e (skips without a compositor)
QUICKSCOPE_E2E=1 cargo test --features e2e --test e2e   # e2e, missing tools are failures
```

`cargo test --features e2e --test x11_e2e` starts its own Xvfb server and
isolates HOME and XDG directories. It draws a known root pattern, compares
screen and dragged-region captures pixel-for-pixel, reads PNGs through the
real CLIPBOARD protocol (including INCR), and checks Escape/Enter keyboard
input on override-redirect overlays at 2× UI scale. Set `QUICKSCOPE_XVFB` to
an absolute Xvfb binary path when it is not installed in PATH;
`QUICKSCOPE_E2E=1` makes its absence a failure. RandR multi-monitor composition
is checked when the server supports monitor creation. The
`capture_latency_2560x1440` test compares SHM and plain GetImage pixels and
prints warm-capture median/p95 timings with `-- --nocapture`.

The text-shaping unit tests (Japanese, Arabic, Hebrew, Devanagari, emoji)
need a font for each script, which they find among installed fonts; without
one a test prints `skipped:` and passes. `QUICKSCOPE_REQUIRE_FONTS=1` (set in CI,
which installs `fonts-noto-core`, `fonts-noto-cjk` and
`fonts-noto-color-emoji`) turns a missing font into a failure.

`tests/ocr.rs` renders chat-like screenshots (dark and light themes) with
lines in English, Japanese, Chinese, Korean, Arabic, Thai, Vietnamese and
German, extracts them with the real Tesseract, and checks every line's text
and the model that read it, region extraction and its coordinates, and
`ocr.languages`. A missing Tesseract or language model skips the affected
checks; `QUICKSCOPE_REQUIRE_OCR=1` (set in CI) makes it a failure. CI installs
Debian's "fast" models without `chi_tra`, see
[text extraction](text-extraction.md#limits).

## Windows

Native gates are `cargo build --release`, `cargo clippy --all-targets -- -D
warnings`, and `cargo test`. The `windows-latest` CI job runs those gates
with `--locked`: library tests, CLI parsing, CPU golden-image regressions and
optional OCR tests. Missing Tesseract/models skip OCR unless
`QUICKSCOPE_REQUIRE_OCR=1`. `editor_render` and `editor_ui` are cfg-excluded:
GPUI 0.3.10 returns no headless renderer on Windows, regardless of GPU.
The Wayland-only `e2e` feature is not enabled by Windows CI.

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
QUICKSCOPE_BLESS=1 cargo test --test render_regression
```

and review the changed PNGs in the diff. On failure the actual image is
written to `$TMPDIR/quickscope-<name>-actual.png`.

## GPU/CPU parity

`editor_render` opens the real `Editor` view in GPUI's headless test context
with the WGPU offscreen renderer, screenshots it, and compares the image area
with the exporter's output for the same document. A 3-pixel offset in the
editor's transform already fails it. On failure both images are written to
`$TMPDIR/quickscope-parity-{gpu,cpu}.png`.

## Editor interaction

`editor_ui` drives the same offscreen editor through GPUI's real event
dispatch (key and mouse events, painted between steps so hit-testing uses
current layout): text formatting shortcuts while typing and on a selected
text shape, a color-picker wheel drag that must recolor the selection as
exactly one undo step, an input-method composition (pre-edit, candidate
window position, commit) through the editor's `EntityInputHandler`, and a
text field receiving Backspace mid-composition (its pre-edit range must not
outlive the text).

## End to end

Each test builds a private desktop in a temporary directory and never
touches the developer's session:

- a headless compositor (`WLR_BACKENDS=headless`, pixman renderer) with its
  own `XDG_RUNTIME_DIR`; labwc runs inside `bwrap` with a private `/tmp` and
  network namespace when available, so its Xwayland sockets stay private;
- a private D-Bus with no service directories and an in-process fake
  `org.freedesktop.Notifications` that records notifications and can emit
  clicks;
- `quickscope-e2e-helper` (built with the `e2e` feature), which paints a known
  test pattern as wallpaper, keeps a virtual keyboard attached, drives a
  virtual pointer and keyboard, lists toplevels, reads the clipboard through
  data-control, reconfigures outputs, and plays a resident input method
  (`zwp_input_method_v2`, the protocol fcitx5 and IBus use) that reports
  when the compositor activates it and sends pre-edit and commit strings;
- the real `quickscope daemon` and `quickscope` client binaries.

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

Set `QUICKSCOPE_E2E_KEEP=1` to keep a session's directory (compositor and daemon
logs) after the test, and `QUICKSCOPE_E2E_COMPOSITOR=sway` to pick the
compositor.

## CI

`.github/workflows/ci.yml` runs formatting and clippy (`-D warnings`), the
test suite with Mesa lavapipe, Noto fonts and Tesseract on Ubuntu, an MSRV
check, the Wayland e2e suite in an Arch Linux container with labwc and
Tesseract, the X11 e2e suite on Ubuntu with private Xvfb servers, and a
release build that is attached to `v*` tags.
