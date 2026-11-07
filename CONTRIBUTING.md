# Contributing

Bug reports, focused fixes and feature proposals are welcome. Discuss substantial
behavior changes in an issue before implementation. Follow the [Code of Conduct](CODE_OF_CONDUCT.md).
Report vulnerabilities privately as described in [SECURITY.md](SECURITY.md).

## Build

Use Rust 1.95 or newer and keep `Cargo.lock` intact. On Linux install the build
libraries in `scripts/ci-deps-ubuntu.sh` (or their distribution equivalents).
On Windows install Visual Studio C++ Build Tools. See the [README](README.md)
for backend/runtime requirements.

```sh
cargo build --locked
cargo fmt --all --check
cargo clippy --locked --all-targets --features e2e -- -D warnings
cargo test --locked
```

On Windows omit `--features e2e`; the Wayland helper is Linux-only. Offscreen
GPU tests require Mesa software rendering or a supported GPU; GPUI currently
excludes those tests on Windows. OCR tests need Tesseract and language models.

## Private end-to-end desktops

Never drive an existing graphical session for automated tests. The test harness
creates isolated HOME/XDG directories, a private D-Bus and headless labwc/sway
or Xvfb. Install labwc (or sway), grim, dbus-daemon, Xvfb and Mesa. Bubblewrap
adds namespace isolation when available.

```sh
QUIKSCOPE_E2E=1 cargo test --locked --features e2e --test e2e
QUIKSCOPE_E2E=1 cargo test --locked --features e2e --test x11_e2e
# If Xvfb is not on PATH:
QUIKSCOPE_XVFB=/absolute/path/to/Xvfb QUIKSCOPE_E2E=1 cargo test --locked --features e2e --test x11_e2e
```

`QUIKSCOPE_E2E=1` fails rather than skips when required tools are missing.
See [docs/testing.md](docs/testing.md) for optional dependencies, platform limits,
artifacts and targeted tests.

## Golden images

After an intentional rendering change, regenerate reference images with:

```sh
QUIKSCOPE_BLESS=1 cargo test --locked --test render_regression
```

Inspect every changed PNG in `tests/golden/` and include the intended images in
the same change. Never bless an unexplained regression. Run the test again
without `QUIKSCOPE_BLESS` before submitting.

## Pull requests

Keep changes focused and add tests that fail without the fix. Update user-facing
docs for changed behavior. Use conventional descriptive commits, such as
`fix(ipc): reject oversized requests` or `docs: explain portal limitations`.
Describe the behavior, relevant issue, exact checks run and any unverified
platforms in the pull request. Do not include personal paths, private screenshots
or live-session logs without reviewing them first.
