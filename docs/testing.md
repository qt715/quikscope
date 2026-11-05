# Testing

| Layer | Where | Needs |
| --- | --- | --- |
| Unit | `src/**` `#[cfg(test)]` | nothing |
| CLI parsing | `src/main.rs` | nothing |
| Golden-image regression | `tests/render_regression.rs`, `tests/golden/` | nothing |
| GPU/CPU parity | `tests/editor_render.rs` | Vulkan or GL (Mesa lavapipe/llvmpipe is enough) |
| End to end | `tests/e2e.rs` (feature `e2e`) | `labwc` or `sway`, `dbus-daemon`; `grim` for reference comparisons; `bwrap` optional |

```sh
cargo test                                    # everything but e2e
cargo test --features e2e --test e2e          # e2e (skips without a compositor)
MOMENT_E2E=1 cargo test --features e2e --test e2e   # e2e, missing tools are failures
```

## Golden images

`render_regression` renders a fixed scene containing every annotation kind
(and a cropped variant) with the CPU exporter and compares it with
`tests/golden/*.png`, allowing anti-aliasing noise only. After an
intentional rendering change:

```sh
MOMENT_BLESS=1 cargo test --test render_regression
```

and review the changed PNGs in the diff. On failure the actual image is
written to `$TMPDIR/moment-<name>-actual.png`.

## GPU/CPU parity

`editor_render` opens the real `Editor` view in GPUI's headless test context
with the WGPU offscreen renderer, screenshots it, and compares the image area
with the exporter's output for the same document. A 3-pixel offset in the
editor's transform already fails it. On failure both images are written to
`$TMPDIR/moment-parity-{gpu,cpu}.png`.

## End to end

Each test builds a private desktop in a temporary directory and never
touches the developer's session:

- a headless compositor (`WLR_BACKENDS=headless`, pixman renderer) with its
  own `XDG_RUNTIME_DIR`; labwc runs inside `bwrap` with a private `/tmp` and
  network namespace when available, so its Xwayland sockets stay private;
- a private D-Bus with no service directories and an in-process fake
  `org.freedesktop.Notifications` that records notifications and can emit
  clicks;
- `moment-e2e-helper` (built with the `e2e` feature), which paints a known
  test pattern as wallpaper, keeps a virtual keyboard attached, drives a
  virtual pointer and keyboard, lists toplevels, reads the clipboard through
  data-control and reconfigures outputs;
- the real `moment daemon` and `moment` client binaries.

The suite covers: screen capture → notification, cache and clipboard
(pixel-identical to `grim`); exact geometry crops; region drag, click,
right-click and `Esc`; refusal of concurrent selections; drawing, text,
`Ctrl+C`, `Ctrl+S`, undo and re-save in the editor; notification clicks
opening (and refocusing) the editor; `open-last` auto-starting a daemon from
the cache; malformed requests; rotated + scaled outputs against `grim`; and
two outputs, including a selection dragged across them.

Set `MOMENT_E2E_KEEP=1` to keep a session's directory (compositor and daemon
logs) after the test, and `MOMENT_E2E_COMPOSITOR=sway` to pick the
compositor.

## CI

`.github/workflows/ci.yml` runs formatting and clippy (`-D warnings`), the
test suite with Mesa lavapipe on Ubuntu, an MSRV check, the e2e suite in an
Arch Linux container with labwc, and a release build that is attached to
`v*` tags.
