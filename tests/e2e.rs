//! End-to-end tests against a real (headless) Wayland compositor.
//!
//! Each test gets a private compositor, D-Bus and daemon (see
//! `support/session.rs`). They are skipped when no headless compositor is
//! installed unless `QUICKSCOPE_E2E=1`, which CI sets.

#[path = "support/session.rs"]
mod session;

use std::io::{BufRead as _, BufReader, Write as _};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use quickscope::raster::Image;
use session::{OUTPUT_H, OUTPUT_W, Session, decode_png, pattern_pixel};

fn stdout(out: &std::process::Output) -> String {
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

fn assert_matches_pattern(img: &Image, ox: u32, oy: u32) {
    for (x, y) in [
        (0, 0),
        (img.width - 1, 0),
        (0, img.height - 1),
        (img.width / 2, img.height / 2),
        (img.width - 1, img.height - 1),
    ] {
        assert_eq!(
            img.pixel(x, y),
            pattern_pixel(ox + x, oy + y),
            "pixel ({x},{y}) of a capture at ({ox},{oy})"
        );
    }
}

fn saved_path(out: &std::process::Output) -> PathBuf {
    assert!(out.status.success(), "capture failed");
    PathBuf::from(stdout(out))
}

#[test]
fn screen_capture_notifies_caches_and_fills_the_clipboard() {
    let Some(mut s) = Session::start() else {
        return;
    };
    s.start_daemon();

    let out = s.quickscope(&["capture", "screen"]);
    assert!(out.status.success());
    assert_eq!(stdout(&out), format!("shot 1: {OUTPUT_W}x{OUTPUT_H}"));

    let n = s.wait_notification();
    assert_eq!(n.summary, "Screenshot captured");
    assert!(
        n.actions.contains(&"default".to_string()),
        "{:?}",
        n.actions
    );
    assert!(n.hints.contains(&"image-data".to_string()), "{:?}", n.hints);

    // The cache that `open-last` reads after a restart is written in the background.
    let cache = s.cache_file();
    for _ in 0..100 {
        if cache.exists() {
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    let (cached, scale) = Image::read_raw(&cache).unwrap();
    assert_eq!(scale, 1.0);
    assert_eq!((cached.width, cached.height), (OUTPUT_W, OUTPUT_H));
    assert_matches_pattern(&cached, 0, 0);

    let png = s.helper(&["paste", "image/png"]).stdout;
    assert_eq!(
        Image::decode(&png).unwrap(),
        cached,
        "clipboard holds the capture"
    );

    if s.has("grim") {
        let reference = s.root().join("grim.png");
        let st = s.command("grim").arg(&reference).status().unwrap();
        assert!(st.success());
        assert_eq!(decode_png(&reference), cached, "pixel-identical to grim");
    }
}

#[test]
fn geometry_capture_crops_exactly_and_saves() {
    let Some(mut s) = Session::start() else {
        return;
    };
    s.start_daemon();
    let out = s.quickscope(&[
        "capture",
        "-g",
        "70,40 100x50",
        "--save",
        "--no-notify",
        "--no-clipboard",
    ]);
    let path = saved_path(&out);
    assert!(path.starts_with(s.shots_dir()), "{}", path.display());
    let img = decode_png(&path);
    assert_eq!((img.width, img.height), (100, 50));
    assert_matches_pattern(&img, 70, 40);
    assert!(s.notifications().is_empty(), "--no-notify");

    let out = s.quickscope(&["capture", "-g", "5000,5000 10x10", "--no-notify"]);
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("outside every output"));
}

#[test]
fn region_drag_selects_a_rectangle() {
    let Some(mut s) = Session::start() else {
        return;
    };
    s.start_daemon();
    let child = s.spawn_quickscope(&["capture", "region", "--save", "--no-notify"]);
    s.wait_overlay(1);
    s.helper(&["drag", "100", "100", "420", "330"]);
    let out = s.finish(child, Duration::from_secs(15));
    let img = decode_png(&saved_path(&out));
    assert_eq!((img.width, img.height), (320, 230));
    // The capture comes from the frozen snapshot, not the dimmed overlay.
    assert_matches_pattern(&img, 100, 100);
}

#[test]
fn region_click_takes_the_whole_output_and_right_click_cancels() {
    let Some(mut s) = Session::start() else {
        return;
    };
    s.start_daemon();

    let child = s.spawn_quickscope(&["capture", "region", "--no-notify"]);
    s.wait_overlay(1);
    s.helper(&["click", "640", "360"]);
    let out = s.finish(child, Duration::from_secs(15));
    assert!(out.status.success());
    assert_eq!(stdout(&out), format!("shot 1: {OUTPUT_W}x{OUTPUT_H}"));

    let child = s.spawn_quickscope(&["capture", "region", "--no-notify"]);
    s.wait_overlay(2);
    s.helper(&["click", "200", "200", "right"]);
    let out = s.finish(child, Duration::from_secs(15));
    assert_eq!(out.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&out.stderr).contains("cancelled"));

    let child = s.spawn_quickscope(&["capture", "region", "--no-notify"]);
    s.wait_overlay(3);
    s.keys(&["escape"]);
    let out = s.finish(child, Duration::from_secs(15));
    assert_eq!(out.status.code(), Some(1), "Escape cancels");
}

#[test]
fn concurrent_region_selection_is_refused() {
    let Some(mut s) = Session::start() else {
        return;
    };
    s.start_daemon();
    let first = s.spawn_quickscope(&["capture", "region", "--no-notify"]);
    s.wait_overlay(1);
    let second = s.quickscope(&["capture", "region", "--no-notify"]);
    assert!(!second.status.success());
    assert!(String::from_utf8_lossy(&second.stderr).contains("already in progress"));
    s.helper(&["click", "10", "10", "right"]);
    assert_eq!(
        s.finish(first, Duration::from_secs(15)).status.code(),
        Some(1)
    );
}

#[test]
fn editor_draws_copies_saves_and_undo_reaches_disk() {
    let Some(mut s) = Session::start() else {
        return;
    };
    s.start_daemon();
    let out = s.quickscope(&["capture", "screen", "--edit", "--no-notify"]);
    assert!(out.status.success());
    s.wait_for_window("quickscope");
    s.wait_log("editor window opened", Duration::from_secs(10));
    std::thread::sleep(Duration::from_millis(300));

    // Rectangle tool, drag inside the image (the editor is centred), then
    // a text label.
    s.keys(&["r"]);
    s.helper(&["drag", "520", "300", "760", "460"]);
    s.keys(&["t"]);
    s.helper(&["click", "300", "200"]);
    s.type_text("Hi there");
    s.keys(&["escape", "ctrl+s"]);
    s.wait_log("saved ", Duration::from_secs(10));

    let files: Vec<PathBuf> = std::fs::read_dir(s.shots_dir())
        .unwrap()
        .map(|e| e.unwrap().path())
        .collect();
    assert_eq!(files.len(), 1, "{files:?}");
    let saved = &files[0];
    let img = decode_png(saved);
    assert_eq!((img.width, img.height), (OUTPUT_W, OUTPUT_H));
    let red = [0x31, 0x31, 0xe0, 0xff];
    let red_pixels = |img: &Image| img.data.chunks_exact(4).filter(|p| *p == red).count();
    let with_both = red_pixels(&img);
    assert!(
        with_both > 600,
        "rectangle and text visible: {with_both} red pixels"
    );
    assert_eq!(
        img.pixel(5, 5),
        pattern_pixel(5, 5),
        "outside the annotations"
    );

    // Ctrl+C puts exactly the saved rendering on the clipboard.
    s.keys(&["ctrl+c"]);
    std::thread::sleep(Duration::from_millis(300));
    let clip = Image::decode(&s.helper(&["paste", "image/png"]).stdout).unwrap();
    assert_eq!(clip, img);

    // Undo the text, save again: the same file loses only the text.
    s.keys(&["ctrl+z", "ctrl+s"]);
    s.wait_log_count("saved ", 2, Duration::from_secs(10));
    assert_eq!(std::fs::read_dir(s.shots_dir()).unwrap().count(), 1);
    let rect_only = red_pixels(&decode_png(saved));
    assert!(
        rect_only > 400 && rect_only < with_both,
        "{rect_only} vs {with_both}"
    );

    // Undo the rectangle too: the file is the untouched capture again.
    s.keys(&["ctrl+z", "ctrl+s"]);
    s.wait_log_count("saved ", 3, Duration::from_secs(10));
    let img = decode_png(saved);
    assert_eq!(red_pixels(&img), 0);
    assert_matches_pattern(&img, 0, 0);

    // Escape with nothing selected closes the editor.
    s.keys(&["escape"]);
    std::thread::sleep(Duration::from_millis(300));
    assert!(!s.toplevels().iter().any(|l| l.starts_with("quickscope")));
}

#[test]
fn notification_click_opens_the_editor() {
    let Some(mut s) = Session::start() else {
        return;
    };
    s.start_daemon();
    assert!(s.quickscope(&["capture", "screen"]).status.success());
    let n = s.wait_notification();
    assert!(s.toplevels().is_empty(), "no editor before the click");
    s.click_notification(n.id, "default");
    s.wait_for_window("quickscope");

    // Clicking again focuses the same editor instead of opening another.
    s.click_notification(n.id, "edit");
    std::thread::sleep(Duration::from_millis(300));
    let editors = s
        .toplevels()
        .iter()
        .filter(|l| l.starts_with("quickscope"))
        .count();
    assert_eq!(editors, 1);
}

#[test]
fn open_last_starts_the_daemon_and_restores_the_cached_capture() {
    let Some(mut s) = Session::start() else {
        return;
    };
    s.start_daemon();
    assert!(
        s.quickscope(&["capture", "screen", "--no-notify"])
            .status
            .success()
    );
    std::thread::sleep(Duration::from_millis(300)); // background cache write
    s.stop_daemon();
    assert!(!s.socket().exists(), "daemon removes its socket on quit");

    // No daemon running: the client starts one, which loads the cache.
    let out = s.quickscope(&["open-last"]);
    assert!(out.status.success());
    assert_eq!(stdout(&out), format!("shot 1: {OUTPUT_W}x{OUTPUT_H}"));
    s.wait_for_window("quickscope");
}

#[test]
fn malformed_requests_get_an_error_line() {
    let Some(mut s) = Session::start() else {
        return;
    };
    s.start_daemon();
    let mut stream = UnixStream::connect(s.socket()).unwrap();
    writeln!(stream, "launch rockets").unwrap();
    let mut line = String::new();
    BufReader::new(&stream).read_line(&mut line).unwrap();
    assert!(line.starts_with("err unknown command"), "{line}");
    assert_eq!(stdout(&s.quickscope(&["ping"])), "ok");
}

#[test]
fn rotated_and_scaled_outputs_match_grim() {
    let reconfigure = |s: &Session| {
        s.helper(&["output", "HEADLESS-1", "1", "2"]);
        std::thread::sleep(Duration::from_millis(200));
    };
    let Some(mut s) = Session::start_with(1, reconfigure) else {
        return;
    };
    if !s.has("grim") {
        eprintln!("skipping: grim not installed");
        return;
    }
    s.start_daemon();
    // 1280x720 rotated 90° at scale 2 is a 360x640 logical output.
    let out = s.quickscope(&["capture", "screen", "--save", "--no-notify"]);
    let ours = decode_png(&saved_path(&out));
    assert_eq!((ours.width, ours.height), (720, 1280));

    let reference = s.root().join("grim.png");
    assert!(
        s.command("grim")
            .arg(&reference)
            .status()
            .unwrap()
            .success()
    );
    assert_eq!(
        decode_png(&reference),
        ours,
        "same orientation and pixels as grim"
    );

    let out = s.quickscope(&["capture", "-g", "10,20 100x50", "--save", "--no-notify"]);
    let ours = decode_png(&saved_path(&out));
    assert_eq!(
        (ours.width, ours.height),
        (200, 100),
        "logical geometry at scale 2"
    );
    let reference = s.root().join("grim-g.png");
    assert!(
        s.command("grim")
            .args(["-g", "10,20 100x50"])
            .arg(&reference)
            .status()
            .unwrap()
            .success()
    );
    assert_eq!(decode_png(&reference), ours);
}

#[test]
fn two_outputs_compose_and_a_drag_can_cross_them() {
    let Some(mut s) = Session::start_with(2, |_| {}) else {
        return;
    };
    s.start_daemon();

    // Both outputs side by side; each shows the pattern in its own coordinates.
    let out = s.quickscope(&["capture", "screen", "--save", "--no-notify"]);
    let screen = decode_png(&saved_path(&out));
    assert_eq!((screen.width, screen.height), (2 * OUTPUT_W, OUTPUT_H));
    assert_eq!(
        screen.pixel(OUTPUT_W - 1, 10),
        pattern_pixel(OUTPUT_W - 1, 10)
    );
    assert_eq!(screen.pixel(OUTPUT_W + 5, 10), pattern_pixel(5, 10));
    if s.has("grim") {
        let reference = s.root().join("grim.png");
        assert!(
            s.command("grim")
                .arg(&reference)
                .status()
                .unwrap()
                .success()
        );
        assert_eq!(decode_png(&reference), screen, "same composition as grim");
    }

    // The overlay on the first output keeps the pointer grab across the edge.
    let child = s.spawn_quickscope(&["capture", "region", "--save", "--no-notify"]);
    s.wait_overlay(1);
    let x0 = OUTPUT_W - 100;
    s.helper(&[
        "drag",
        &x0.to_string(),
        "100",
        &(OUTPUT_W + 100).to_string(),
        "300",
    ]);
    let img = decode_png(&saved_path(&s.finish(child, Duration::from_secs(15))));
    assert_eq!((img.width, img.height), (200, 200));
    assert_eq!(img.pixel(0, 0), pattern_pixel(x0, 100));
    assert_eq!(img.pixel(99, 50), pattern_pixel(OUTPUT_W - 1, 150));
    assert_eq!(
        img.pixel(100, 50),
        pattern_pixel(0, 150),
        "second output starts mid-image"
    );

    // `output` mode without Hyprland takes the first output.
    let out = s.quickscope(&["capture", "output", "--save", "--no-notify"]);
    let first = decode_png(&saved_path(&out));
    assert_eq!((first.width, first.height), (OUTPUT_W, OUTPUT_H));
    let out = s.quickscope(&[
        "capture",
        "output",
        "-o",
        "HEADLESS-2",
        "--save",
        "--no-notify",
    ]);
    assert_eq!(
        decode_png(&saved_path(&out)).pixel(0, 0),
        pattern_pixel(0, 0)
    );
}

#[test]
fn rapid_captures_then_restart_open_the_newest() {
    let Some(mut s) = Session::start() else {
        return;
    };
    s.start_daemon();
    // Overlapping captures of different sizes race to the cache…
    let racing: Vec<_> = [(100, 50), (640, 360), (1280, 720), (320, 200)]
        .iter()
        .map(|(w, h)| {
            let g = format!("0,0 {w}x{h}");
            s.spawn_quickscope(&["capture", "-g", &g, "--no-notify", "--no-clipboard"])
        })
        .collect();
    for child in racing {
        assert!(s.finish(child, Duration::from_secs(15)).status.success());
    }
    // …then back-to-back ones; the last capture taken must win.
    for g in ["0,0 900x500", "0,0 1200x700", "10,10 333x111"] {
        let out = s.quickscope(&["capture", "-g", g, "--no-notify", "--no-clipboard"]);
        assert!(out.status.success());
    }
    // `quit` returns only after the cache holds the newest capture.
    s.stop_daemon();
    let (cached, _) = Image::read_raw(&s.cache_file()).unwrap();
    assert_eq!((cached.width, cached.height), (333, 111));
    assert_matches_pattern(&cached, 10, 10);
    let leftovers: Vec<_> = std::fs::read_dir(s.cache_file().parent().unwrap())
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|n| n.contains(".part"))
        .collect();
    assert!(leftovers.is_empty(), "{leftovers:?}");

    let out = s.quickscope(&["open-last"]);
    assert_eq!(stdout(&out), "shot 1: 333x111");
}

#[test]
fn notification_toggle_applies_immediately_and_persists() {
    let Some(mut s) = Session::start() else {
        return;
    };
    s.start_daemon();
    let captured = |s: &Session| {
        s.notifications()
            .iter()
            .filter(|n| n.summary == "Screenshot captured")
            .count()
    };

    assert_eq!(
        stdout(&s.quickscope(&["notifications", "status"])),
        "notifications on"
    );
    assert_eq!(
        stdout(&s.quickscope(&["notifications", "toggle"])),
        "notifications off"
    );
    let config = std::fs::read_to_string(s.root().join("config/quickscope/config.toml")).unwrap();
    assert!(config.contains("notify = false"), "{config}");
    assert!(config.contains("save_dir"), "other settings kept: {config}");

    assert!(s.quickscope(&["capture", "screen"]).status.success());
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(captured(&s), 0, "no capture notification while off");
    // An explicit flag still wins for one capture.
    assert!(
        s.quickscope(&["capture", "screen", "--notify"])
            .status
            .success()
    );
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(captured(&s), 1);

    // The setting survives a daemon restart.
    s.stop_daemon();
    s.start_daemon();
    assert_eq!(
        stdout(&s.quickscope(&["notifications", "status"])),
        "notifications off"
    );
    assert_eq!(
        stdout(&s.quickscope(&["notifications", "on"])),
        "notifications on"
    );
    assert!(s.quickscope(&["capture", "screen"]).status.success());
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(captured(&s), 2);

    let bad = s.quickscope(&["notifications", "maybe"]);
    assert!(!bad.status.success());
}

#[test]
fn editor_opens_on_a_small_hidpi_display() {
    // 1280x720 at scale 2 is 640x360 logical: smaller than the editor's
    // preferred minimum window.
    let scale2 = |s: &Session| {
        s.helper(&["output", "HEADLESS-1", "0", "2"]);
        std::thread::sleep(Duration::from_millis(200));
    };
    let Some(mut s) = Session::start_with(1, scale2) else {
        return;
    };
    s.start_daemon();
    let out = s.quickscope(&["capture", "screen", "--edit", "--no-notify"]);
    assert!(out.status.success());
    assert_eq!(stdout(&out), format!("shot 1: {OUTPUT_W}x{OUTPUT_H}"));
    s.wait_for_window("quickscope");
    s.wait_log("editor first frame", Duration::from_secs(10));
    assert_eq!(stdout(&s.quickscope(&["ping"])), "ok", "daemon still alive");
}

#[test]
fn input_method_composes_and_commits_into_editor_text() {
    let Some(mut s) = Session::start() else {
        return;
    };
    s.start_daemon();
    // The input method runs before the editor exists, like fcitx5 does.
    let mut ime = s.input_method();
    let out = s.quickscope(&["capture", "screen", "--edit", "--no-notify"]);
    assert!(out.status.success());
    s.wait_for_window("quickscope");
    s.wait_log("editor window opened", Duration::from_secs(10));

    // Focused but not typing text: the editor is no text input, so the
    // input method stays inactive and single-key tool shortcuts reach the
    // editor instead of being composed.
    assert_eq!(ime.next(Duration::from_millis(1000)), None);
    s.keys(&["t"]);
    s.helper(&["click", "300", "200"]);
    assert_eq!(
        ime.next(Duration::from_secs(5)).as_deref(),
        Some("active"),
        "the text tool did not enable the input method"
    );
    s.type_text("A");

    let grim = s.has("grim");
    let shot = |name: &str| {
        let path = s.root().join(name);
        assert!(s.command("grim").arg(&path).status().unwrap().success());
        decode_png(&path)
    };
    let before = grim.then(|| shot("before.png"));
    // Compose "nihongo": the pre-edit is drawn in place...
    ime.step("pre=にほ");
    ime.step("pre=にほんご");
    if let Some(before) = before {
        let during = shot("during.png");
        let changed = before
            .data
            .chunks_exact(4)
            .zip(during.data.chunks_exact(4))
            .filter(|(a, b)| a != b)
            .count();
        assert!(changed > 300, "pre-edit drawn: {changed} pixels changed");
    }
    // ...and converting commits "日本語" in its place. The ASCII "!"
    // takes GPUI's key-event path for single-byte commits.
    ime.step("commit=日本語");
    ime.step("commit=!");

    // Finishing the text turns the input method off again.
    s.keys(&["escape"]);
    assert_eq!(
        ime.next(Duration::from_secs(5)).as_deref(),
        Some("inactive")
    );
    ime.stop();
    s.keys(&["ctrl+s"]);
    s.wait_log("saved ", Duration::from_secs(10));
    let saved = std::fs::read_dir(s.shots_dir())
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    let img = decode_png(&saved);
    // The text's ink (pure annotation red) spans exactly what "A日本語!"
    // lays out to: neither the pre-edit nor both strings survived.
    let red = [0x31, 0x31, 0xe0, 0xff];
    let xs: Vec<u32> = img
        .data
        .chunks_exact(4)
        .enumerate()
        .filter(|(_, p)| *p == red)
        .map(|(i, _)| i as u32 % img.width)
        .collect();
    assert!(!xs.is_empty(), "no text in the saved image");
    let ink = (xs.iter().max().unwrap() - xs.iter().min().unwrap() + 1) as f32;
    let layout = quickscope::text::layout("A日本語!", &quickscope::fonts::FontSpec::default());
    let expected = quickscope::text::ink(&layout, quickscope::geom::Pt::new(0.0, 0.0))
        .path
        .unwrap()
        .bounds()
        .width();
    assert!(
        (ink - expected).abs() <= 3.0,
        "text ink {ink}px wide, \"A日本語!\" lays out {expected}px"
    );
}

#[test]
fn open_last_keeps_the_last_used_tool() {
    let Some(mut s) = Session::start() else {
        return;
    };
    s.start_daemon();
    assert!(
        s.quickscope(&["capture", "screen", "--edit", "--no-notify"])
            .status
            .success()
    );
    s.wait_for_window("quickscope");
    s.wait_log("editor window opened", Duration::from_secs(10));
    std::thread::sleep(Duration::from_millis(300));
    // Pick the rectangle, then close the editor without drawing.
    s.keys(&["r", "escape"]);
    std::thread::sleep(Duration::from_millis(300));
    assert!(!s.toplevels().iter().any(|l| l.starts_with("quickscope")));

    assert!(s.quickscope(&["open-last"]).status.success());
    s.wait_log_count("editor window opened", 2, Duration::from_secs(10));
    std::thread::sleep(Duration::from_millis(300));
    // No tool key: the drag draws with whatever tool the editor opened with.
    s.helper(&["drag", "520", "300", "760", "460"]);
    s.keys(&["ctrl+s"]);
    s.wait_log("saved ", Duration::from_secs(10));
    let saved = std::fs::read_dir(s.shots_dir())
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    let img = decode_png(&saved);
    // A rectangle has a long horizontal edge; the default arrow drawn by
    // the same drag is diagonal and never fills 150 px of one row.
    let red = [0x31, 0x31, 0xe0, 0xff];
    let longest_row = (0..img.height)
        .map(|y| img.row(y).chunks_exact(4).filter(|p| *p == red).count())
        .max()
        .unwrap();
    assert!(
        longest_row > 150,
        "no rectangle edge: longest red row {longest_row} px"
    );
}

/// Whether text extraction can run; without Tesseract the OCR tests skip
/// unless `QUICKSCOPE_REQUIRE_OCR=1`.
fn ocr_available() -> bool {
    let ok = quickscope::ocr::tess::version().is_ok() && quickscope::ocr::tessdata_dir().is_some();
    if !ok {
        assert!(
            std::env::var_os("QUICKSCOPE_REQUIRE_OCR").is_none(),
            "Tesseract or its language data is missing"
        );
        eprintln!("skipped: Tesseract is not installed");
    }
    ok
}

/// A small light-theme "screenshot" with two lines of 16 px text, shown
/// at 100% in the editor.
fn text_image(path: &std::path::Path) {
    use quickscope::document::{Document, Rgba, ShapeKind, Style};
    let mut d = Document::new(
        std::sync::Arc::new(Image::filled(560, 90, [250, 250, 250, 255])),
        1.0,
    );
    for (i, line) in [
        "The quick brown fox jumps over 13 lazy dogs.",
        "Copy this text, please",
    ]
    .iter()
    .enumerate()
    {
        let s = d.make_shape(
            ShapeKind::Text {
                origin: quickscope::geom::Pt::new(14.0, 14.0 + i as f32 * 34.0),
                text: (*line).into(),
                font: quickscope::fonts::FontSpec {
                    weight: 400,
                    ..Default::default()
                }
                .with_size(16.0),
            },
            Style {
                color: Rgba::rgb(0x111111),
                size: 1,
                fill: false,
            },
        );
        d.add(s);
    }
    quickscope::export::render(&d.snapshot())
        .save_png(path, quickscope::raster::Compression::Fast)
        .unwrap();
}

#[test]
fn text_extraction_copies_plain_text_from_the_editor_and_from_a_capture() {
    if !ocr_available() {
        return;
    }
    let Some(mut s) = Session::start() else {
        return;
    };
    s.start_daemon();
    let png = s.root().join("text.png");
    text_image(&png);
    assert!(
        s.quickscope(&["open", png.to_str().unwrap()])
            .status
            .success()
    );
    s.wait_for_window("quickscope");
    s.wait_log("editor window opened", Duration::from_secs(10));
    std::thread::sleep(Duration::from_millis(300));

    // Ctrl+Shift+C in the editor: the screenshot's text, as plain text.
    s.keys(&["ctrl+shift+c"]);
    s.wait_log("extracted ", Duration::from_secs(20));
    std::thread::sleep(Duration::from_millis(200));
    let pasted = s.helper(&["paste", "text/plain;charset=utf-8"]).stdout;
    assert_eq!(
        String::from_utf8(pasted).unwrap(),
        "The quick brown fox jumps over 13 lazy dogs.\nCopy this text, please"
    );

    // `capture --text` reads the screen (the editor showing that image):
    // printed, on the clipboard, and summarised in the notification.
    let out = s.quickscope(&["capture", "screen", "--text"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let printed = stdout(&out);
    assert!(
        printed.contains("The quick brown fox jumps over 13 lazy dogs."),
        "{printed}"
    );
    let clip = String::from_utf8(s.helper(&["paste", "UTF8_STRING"]).stdout).unwrap();
    assert_eq!(clip, printed);
    let n = s.wait_notification();
    assert!(
        n.summary.starts_with("Copied ") && n.summary.ends_with(" of text"),
        "{n:?}"
    );
    assert!(n.body.contains("quick brown fox"), "{n:?}");
}

#[test]
fn portal_capture_fullscreen_selection_and_denial() {
    let Some(mut s) = Session::start() else {
        return;
    };
    if !s.has("grim") {
        assert!(
            std::env::var("QUICKSCOPE_E2E").as_deref() != Ok("1"),
            "grim required for portal e2e"
        );
        eprintln!("skipped: portal e2e requires grim");
        return;
    }
    let response = s.enable_portal();
    s.force_fullscreen();
    s.start_daemon();
    assert!(s.daemon_log().contains("platform backend: portal"));
    let reference = s.root().join("reference.png");
    assert!(
        s.command("grim")
            .arg(&reference)
            .status()
            .unwrap()
            .success()
    );
    let mut times = Vec::new();
    for _ in 0..5 {
        let start = std::time::Instant::now();
        let out = s.quickscope(&[
            "capture",
            "screen",
            "--save",
            "--no-notify",
            "--no-clipboard",
        ]);
        times.push(start.elapsed().as_secs_f64() * 1000.);
        assert_eq!(decode_png(&saved_path(&out)), decode_png(&reference));
    }
    eprintln!("fake portal capture screen end-to-end ms: {times:?}");
    let child = s.spawn_quickscope(&[
        "capture",
        "region",
        "--save",
        "--no-notify",
        "--no-clipboard",
    ]);
    s.wait_overlay(1);
    s.helper(&["drag", "100", "100", "420", "330"]);
    let out = s.finish(child, Duration::from_secs(15));
    let selected = decode_png(&saved_path(&out));
    assert_eq!(
        selected,
        decode_png(&reference).crop(quickscope::geom::IRect::new(100, 100, 320, 230))
    );

    let child = s.spawn_quickscope(&["capture", "region", "--no-notify", "--no-clipboard"]);
    s.wait_overlay(2);
    s.keys(&["escape"]);
    assert_eq!(
        s.finish(child, Duration::from_secs(15)).status.code(),
        Some(1)
    );

    let child = s.spawn_quickscope(&["capture", "region", "--no-notify", "--no-clipboard"]);
    s.wait_overlay(3);
    s.keys(&["enter"]);
    assert!(
        s.finish(child, Duration::from_secs(15)).status.success(),
        "Enter captures focused output"
    );
    for code in [1, 2] {
        response.store(code, std::sync::atomic::Ordering::Relaxed);
        let out = s.quickscope(&["capture", "screen", "--no-notify", "--no-clipboard"]);
        assert_eq!(out.status.code(), Some(1));
        assert!(
            String::from_utf8_lossy(&out.stderr).contains("permission denied"),
            "{out:?}"
        );
    }
}

#[test]
fn portal_fullscreen_uses_the_output_it_lands_on() {
    let Some(mut s) = Session::start_with(2, |s| {
        s.helper(&["output", "HEADLESS-2", "1", "1"]);
        std::thread::sleep(Duration::from_millis(200));
    }) else {
        return;
    };
    if !s.has("grim") {
        assert!(
            std::env::var("QUICKSCOPE_E2E").as_deref() != Ok("1"),
            "grim required for portal e2e"
        );
        return;
    }
    s.enable_portal();
    s.force_fullscreen();
    let references: Vec<_> = ["HEADLESS-1", "HEADLESS-2"]
        .into_iter()
        .map(|name| {
            let path = s.root().join(format!("{name}.png"));
            assert!(
                s.command("grim")
                    .args(["-o", name])
                    .arg(&path)
                    .status()
                    .unwrap()
                    .success()
            );
            decode_png(&path)
        })
        .collect();
    assert_ne!(
        references[0].width, references[1].width,
        "distinguishable output frames"
    );
    s.start_daemon();
    s.helper(&["move", "1400", "200"]);
    let child = s.spawn_quickscope(&[
        "capture",
        "region",
        "--save",
        "--no-notify",
        "--no-clipboard",
    ]);
    s.wait_log("fullscreen selection mapped to", Duration::from_secs(10));
    let log = s.daemon_log();
    let mapped = log
        .lines()
        .rev()
        .find(|line| line.contains("fullscreen selection mapped to"))
        .unwrap();
    let selected = if mapped.contains("HEADLESS-2") {
        1
    } else {
        assert!(mapped.contains("HEADLESS-1"));
        0
    };
    eprintln!("actual fullscreen placement: {mapped}");
    let logical: quickscope::geom::IRect = mapped
        .rsplit_once('(')
        .unwrap()
        .1
        .trim_end_matches(')')
        .parse()
        .unwrap();
    s.wait_overlay_at(1, logical.x + 1, logical.y + 1);
    for (i, name) in ["HEADLESS-1", "HEADLESS-2"].into_iter().enumerate() {
        let path = s.root().join(format!("overlay-{name}.png"));
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            assert!(
                s.command("grim")
                    .args(["-o", name])
                    .arg(&path)
                    .status()
                    .unwrap()
                    .success()
            );
            let visible = decode_png(&path);
            let matches = if i != selected {
                visible == references[i]
            } else {
                [(150, 200), (350, 450), (500, 300)]
                    .into_iter()
                    .all(|(x, y)| {
                        let before = references[i].pixel(x, y);
                        let after = visible.pixel(x, y);
                        (0..3).all(|channel| {
                            (after[channel] as f32 - before[channel] as f32 * (143. / 255.)).abs()
                                <= 2.
                        })
                    })
            };
            if matches {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "output {name} never presented its expected frozen frame: pixel {:?}, reference {:?}",
                visible.pixel(350, 450),
                references[i].pixel(350, 450)
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }
    s.keys(&["enter"]);
    let image = decode_png(&saved_path(&s.finish(child, Duration::from_secs(15))));
    assert_eq!(
        image, references[selected],
        "Enter captures the output actually displaying the frozen frame"
    );
}

#[test]
fn portal_layer_shell_selects_across_outputs() {
    let Some(mut s) = Session::start_with(2, |_| {}) else {
        return;
    };
    if !s.has("grim") {
        assert!(
            std::env::var("QUICKSCOPE_E2E").as_deref() != Ok("1"),
            "grim required for portal e2e"
        );
        return;
    }
    s.enable_portal();
    s.start_daemon();
    let path = s.root().join("whole-desktop.png");
    assert!(s.command("grim").arg(&path).status().unwrap().success());
    let reference = decode_png(&path);
    let child = s.spawn_quickscope(&[
        "capture",
        "region",
        "--save",
        "--no-notify",
        "--no-clipboard",
    ]);
    s.wait_overlay(1);
    s.helper(&["drag", "1100", "100", "1450", "330"]);
    let image = decode_png(&saved_path(&s.finish(child, Duration::from_secs(15))));
    assert_eq!(
        image,
        reference.crop(quickscope::geom::IRect::new(1100, 100, 350, 230))
    );
    assert!(
        !s.daemon_log().contains("fullscreen selection mapped"),
        "layer-shell offered by compositor must be used"
    );
}

#[test]
fn selection_overlay_covers_a_large_output_edge_to_edge() {
    // Overlays are created while the capture runs; they must still span
    // the whole output rather than GPUI's default window size.
    let big = |s: &Session| {
        s.helper(&["output", "HEADLESS-1", "0", "1", "2560x1440"]);
        std::thread::sleep(Duration::from_millis(200));
    };
    let Some(mut s) = Session::start_with(1, big) else {
        return;
    };
    s.start_daemon();
    let child = s.spawn_quickscope(&["capture", "region", "--no-notify"]);
    s.wait_overlay(1);
    // Probe near every corner: dimmed, not live. The frozen frame reaches
    // the screen after input starts, so retain the coverage check's retry.
    let probes = [(40, 1400), (2520, 40), (2520, 1400), (1280, 1400)];
    let sum = |p: [u8; 4]| p[..3].iter().map(|&c| c as u32).sum::<u32>();
    let grim = s.root().join("overlay.png");
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        assert!(s.command("grim").arg(&grim).status().unwrap().success());
        let shot = decode_png(&grim);
        assert_eq!((shot.width, shot.height), (2560, 1440));
        let live: Vec<_> = probes
            .iter()
            .filter(|&&(x, y)| sum(shot.pixel(x, y)) >= sum(pattern_pixel(x, y)))
            .collect();
        if live.is_empty() {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "not dimmed at {live:?}: the overlay does not cover the output"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
    s.keys(&["escape"]);
    let out = s.finish(child, Duration::from_secs(15));
    assert_eq!(out.status.code(), Some(1));
}

#[test]
fn portal_permission_dialog_gets_the_keyboard_before_any_overlay() {
    // GNOME and KDE may ask before a portal screenshot. Nothing of ours may
    // take the keyboard while they do, or the user cannot approve.
    let Some(mut s) = Session::start() else {
        return;
    };
    if !s.has("grim") {
        assert!(
            std::env::var("QUICKSCOPE_E2E").as_deref() != Ok("1"),
            "grim required for portal e2e"
        );
        eprintln!("skipped: portal e2e requires grim");
        return;
    }
    // labwc offers layer-shell, the KDE case where overlays could open early.
    s.enable_portal_with_dialog(true);
    s.start_daemon();
    let child = s.spawn_quickscope(&["capture", "region", "--no-notify"]);
    s.wait_for_window("quickscope-e2e-dialog");
    std::thread::sleep(Duration::from_millis(300));
    s.keys(&["enter"]);
    s.wait_overlay(1);
    s.keys(&["escape"]);
    let out = s.finish(child, Duration::from_secs(15));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(1), "{stderr}");
    assert!(
        stderr.contains("cancelled"),
        "Enter reached the dialog: {stderr}"
    );
}
