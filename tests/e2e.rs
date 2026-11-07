//! End-to-end tests against a real (headless) Wayland compositor.
//!
//! Each test gets a private compositor, D-Bus and daemon (see
//! `support/session.rs`). They are skipped when no headless compositor is
//! installed unless `QUIKSCOPE_E2E=1`, which CI sets.

#[path = "support/session.rs"]
mod session;

use std::io::{BufRead as _, BufReader, Write as _};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use quikscope::raster::Image;
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

fn settings_visual(s: &Session, path: &std::path::Path) {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        assert!(s.command("grim").arg(path).status().unwrap().success());
        let image = decode_png(path);
        if image.pixel(640, 360) != pattern_pixel(640, 360) {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "settings never painted: {}",
            path.display()
        );
        std::thread::sleep(Duration::from_millis(30));
    }
}

fn settings_closed(s: &Session) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while s
        .toplevels()
        .iter()
        .any(|t| t.starts_with("quikscope-settings\t"))
    {
        assert!(Instant::now() < deadline, "settings window did not close");
        std::thread::sleep(Duration::from_millis(30));
    }
}

#[test]
fn settings_command_opens_and_refocuses_one_window() {
    let Some(mut s) = live_session() else { return };
    let artifacts = s.live_artifacts("settings");
    s.start_daemon();
    assert!(s.quikscope(&["settings"]).status.success());
    s.wait_for_window("quikscope-settings");
    settings_visual(&s, &artifacts.join("settings.png"));
    assert!(s.quikscope(&["settings"]).status.success());
    assert_eq!(
        s.toplevels()
            .iter()
            .filter(|t| t.starts_with("quikscope-settings\t"))
            .count(),
        1,
        "all settings requests must focus the existing window"
    );
    assert!(s.quikscope(&["notifications", "off"]).status.success());
    // External changes must refresh the open view: clicking the now-off card
    // turns notifications ON, rather than toggling stale local state to off.
    s.helper(&["click", "640", "350"]);
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if stdout(&s.quikscope(&["notifications", "status"])) == "notifications on" {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "settings displayed stale notification state"
        );
        std::thread::sleep(Duration::from_millis(30));
    }
    s.helper(&["close", "quikscope-settings"]);
    settings_closed(&s);
    assert!(s.quikscope(&["settings"]).status.success());
    s.wait_for_window("quikscope-settings");
    settings_visual(&s, &artifacts.join("reopened.png"));
    s.helper(&["scroll", "640", "400", "1000"]);
    settings_visual(&s, &artifacts.join("settings-advanced.png"));
}

#[test]
fn settings_text_saves_on_focus_loss_and_native_close() {
    for finish in ["blur", "native-close"] {
        let Some(mut s) = live_session() else { return };
        let artifacts = s.live_artifacts(&format!("settings-fields-{finish}"));
        s.start_daemon();
        assert!(s.quikscope(&["settings"]).status.success());
        s.wait_for_window("quikscope-settings");
        settings_visual(&s, &artifacts.join("before-scroll.png"));
        s.helper(&["scroll", "640", "400", "100"]);
        settings_visual(&s, &artifacts.join("fields.png"));
        if finish == "blur" {
            // A repaired/hand-edited file is read on the next UI save. Its
            // other values must refresh clean fields, not only toggle cards.
            let before = decode_png(&artifacts.join("fields.png"));
            let path = s.root().join("config/quikscope/config.toml");
            std::fs::write(&path, "save_dir = '/tmp/repaired-settings-save'\n").unwrap();
            s.helper(&["click", "640", "164"]);
            let deadline = Instant::now() + Duration::from_secs(5);
            loop {
                let image_path = artifacts.join("repaired-fields.png");
                assert!(
                    s.command("grim")
                        .arg(&image_path)
                        .status()
                        .unwrap()
                        .success()
                );
                let after = decode_png(&image_path);
                let changed = (352..372)
                    .flat_map(|y| (280..990).map(move |x| (x, y)))
                    .filter(|&(x, y)| before.pixel(x, y) != after.pixel(x, y))
                    .count();
                if quikscope::config::Config::parse(&std::fs::read_to_string(&path).unwrap())
                    .unwrap()
                    .cursor
                    && changed > 30
                {
                    break;
                }
                assert!(
                    Instant::now() < deadline,
                    "saved config never refreshed the save-directory field"
                );
                std::thread::sleep(Duration::from_millis(30));
            }
        }
        s.helper(&["click", "640", "417"]);
        s.type_text("changed.png");
        settings_visual(&s, &artifacts.join("pending-field.png"));
        let config_path = s.root().join("config/quikscope/config.toml");
        let read = || {
            quikscope::config::Config::parse(&std::fs::read_to_string(&config_path).unwrap())
                .unwrap()
        };
        assert_ne!(
            read().filename,
            "changed.png",
            "typing alone must not submit"
        );
        if finish == "blur" {
            s.helper(&["click", "640", "362"]);
            let deadline = Instant::now() + Duration::from_secs(5);
            while read().filename != "changed.png" {
                assert!(Instant::now() < deadline, "blur did not commit filename");
                std::thread::sleep(Duration::from_millis(30));
            }
        }
        s.helper(&["close", "quikscope-settings"]);
        settings_closed(&s);
        assert_eq!(
            read().filename,
            "changed.png",
            "{finish} discarded a focused edit"
        );
    }
}

#[test]
fn settings_live_apply_changes_the_next_capture_without_restart() {
    let Some(mut s) = live_session() else { return };
    let artifacts = s.live_artifacts("settings-live-apply");
    s.start_daemon();
    assert!(s.quikscope(&["settings"]).status.success());
    s.wait_for_window("quikscope-settings");
    settings_visual(&s, &artifacts.join("settings.png"));
    // The centered 760x660 form's second card chooses live selection.
    s.helper(&["click", "640", "230"]);
    let path = s.root().join("config/quikscope/config.toml");
    let deadline = Instant::now() + Duration::from_secs(5);
    while quikscope::config::Config::parse(&std::fs::read_to_string(&path).unwrap())
        .unwrap()
        .freeze
    {
        assert!(Instant::now() < deadline, "live choice did not save");
        std::thread::sleep(Duration::from_millis(30));
    }
    s.helper(&["close", "quikscope-settings"]);
    settings_closed(&s);
    s.repaint_pattern(0);
    let capture = s.spawn_quikscope(&[
        "capture",
        "region",
        "--save",
        "--no-notify",
        "--no-clipboard",
    ]);
    s.wait_overlay(1);
    s.repaint_pattern(1);
    s.helper(&["drag", "100", "100", "420", "330"]);
    let image = decode_png(&saved_path(&s.finish(capture, Duration::from_secs(15))));
    assert_eq!((image.width, image.height), (320, 230));
    assert_exact_phase(&image, 100, 100, 1);
}

#[test]
fn welcome_done_and_native_close_persist_the_chosen_mode_once() {
    for finish in ["done", "native-close"] {
        let Some(mut s) = live_session() else { return };
        let artifacts = s.live_artifacts(&format!("welcome-{finish}"));
        let config_path = s.root().join("config/quikscope/config.toml");
        std::fs::remove_file(&config_path).unwrap();
        s.start_daemon();
        s.wait_for_window("quikscope-settings");
        assert!(
            s.toplevels()
                .iter()
                .any(|t| t.contains("Welcome to quikscope"))
        );
        settings_visual(&s, &artifacts.join("welcome.png"));
        assert!(!config_path.exists(), "opening welcome is not completion");
        // The centered 640x540 welcome form uses the same second mode card.
        s.helper(&["click", "640", "290"]);
        assert!(
            !config_path.exists(),
            "choices preview until Done/native close"
        );
        settings_visual(&s, &artifacts.join("welcome-live.png"));
        if finish == "done" {
            s.helper(&["click", "640", "610"]);
        } else {
            s.helper(&["close", "quikscope-settings"]);
        }
        settings_closed(&s);
        let config =
            quikscope::config::Config::parse(&std::fs::read_to_string(&config_path).unwrap())
                .unwrap();
        assert!(!config.freeze, "{finish} lost the live choice");
        s.stop_daemon();
        s.start_daemon();
        // Startup logs the open before "daemon ready", independently of when
        // the compositor publishes its toplevel, so this negative check cannot race.
        assert!(!s.daemon_log().contains("welcome window opened"));
        assert!(
            !s.toplevels()
                .iter()
                .any(|t| t.starts_with("quikscope-settings\t")),
            "completed onboarding must not reappear"
        );
    }
}

#[test]
fn settings_reports_invalid_existing_config_without_overwriting_it() {
    let Some(mut s) = live_session() else { return };
    let artifacts = s.live_artifacts("settings-invalid");
    let path = s.root().join("config/quikscope/config.toml");
    let invalid = "# repair this typo\nfreze = false\n";
    std::fs::write(&path, invalid).unwrap();
    s.start_daemon();
    assert!(s.daemon_log().contains("configuration could not be loaded"));
    assert!(s.quikscope(&["settings"]).status.success());
    s.wait_for_window("quikscope-settings");
    settings_visual(&s, &artifacts.join("invalid-config.png"));
    let before = decode_png(&artifacts.join("invalid-config.png"));
    s.helper(&["click", "640", "230"]);
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let image_path = artifacts.join("refused-edit.png");
        assert!(
            s.command("grim")
                .arg(&image_path)
                .status()
                .unwrap()
                .success()
        );
        let after = decode_png(&image_path);
        let changed = (520..638)
            .step_by(2)
            .flat_map(|y| (276..1000).step_by(2).map(move |x| (x, y)))
            .filter(|&(x, y)| before.pixel(x, y) != after.pixel(x, y))
            .count();
        if changed > 30 {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "invalid edit never produced updated inline error text"
        );
        std::thread::sleep(Duration::from_millis(30));
    }
    assert_eq!(std::fs::read_to_string(&path).unwrap(), invalid);
    s.helper(&["close", "quikscope-settings"]);
    settings_closed(&s);
}

fn phase_pixel(x: u32, y: u32, phase: u8) -> [u8; 4] {
    let mut pixel = pattern_pixel(x, y);
    if phase % 2 == 1 {
        pixel[..3].iter_mut().for_each(|c| *c = 255 - *c);
    }
    pixel
}

fn assert_exact_phase(img: &Image, ox: u32, oy: u32, phase: u8) {
    for y in 0..img.height {
        for x in 0..img.width {
            assert_eq!(
                img.pixel(x, y),
                phase_pixel(ox + x, oy + y, phase),
                "phase {phase}, pixel ({x},{y}) at origin ({ox},{oy}); overlay must not be captured"
            );
        }
    }
}

fn live_session() -> Option<Session> {
    let s = Session::start()?;
    if !s.has("grim") {
        assert_ne!(
            std::env::var("QUIKSCOPE_E2E").as_deref(),
            Ok("1"),
            "grim required for live selection e2e"
        );
        eprintln!("skipped: live selection e2e requires grim");
        return None;
    }
    Some(s)
}

#[test]
fn region_live_repaints_beneath_dimming_and_clear_hole_but_frozen_keeps_old_pixels() {
    let Some(mut s) = live_session() else { return };
    let artifacts = s.live_artifacts("pixels");
    s.start_daemon();
    for (i, flag, phase) in [(1, "--freeze", 0), (2, "--no-freeze", 1)] {
        s.repaint_pattern(0);
        let child = s.spawn_quikscope(&[
            "capture",
            "region",
            flag,
            "--save",
            "--no-notify",
            "--no-clipboard",
        ]);
        s.wait_overlay(i);
        // Crucially, this is a commit by the already-resident background
        // client only after the selector accepts input.
        s.repaint_pattern(1);
        let mut drag = s.hold_drag((100, 100), (420, 330));
        let path = artifacts.join(format!("selection-{flag}.png"));
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            assert!(s.command("grim").arg(&path).status().unwrap().success());
            let shot = decode_png(&path);
            let inside = shot.pixel(180, 170);
            let outside = shot.pixel(700, 500);
            let dimmed_new = outside[0] > 0
                && outside[0] < 255
                && (1..3).all(|c| {
                    let expected = phase_pixel(700, 500, 1)[c] as f64 * outside[0] as f64 / 255.0;
                    (outside[c] as f64 - expected).abs() <= 2.0
                });
            let outside_correct = if phase == 1 {
                dimmed_new
            } else {
                outside[0] == 0 && outside[1] < pattern_pixel(700, 500)[1]
            };
            if inside == phase_pixel(180, 170, phase) && outside_correct {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "{flag}: expected clear phase-{phase} hole and dimmed background; inside={inside:?}, outside={outside:?}; {}",
                path.display()
            );
            std::thread::sleep(Duration::from_millis(50));
        }
        drag.stdin.take().unwrap().write_all(b"\n").unwrap();
        assert!(s.finish(drag, Duration::from_secs(5)).status.success());
        let out = s.finish(child, Duration::from_secs(15));
        let final_path = artifacts.join(format!("capture-{flag}.png"));
        std::fs::copy(saved_path(&out), &final_path).unwrap();
        let img = decode_png(&final_path);
        assert_eq!((img.width, img.height), (320, 230));
        assert_exact_phase(&img, 100, 100, phase);
    }
}

#[test]
fn region_config_live_and_freeze_override_capture_exact_pixels() {
    let Some(mut s) = live_session() else { return };
    let artifacts = s.live_artifacts("config");
    let config = s.root().join("config/quikscope/config.toml");
    let mut contents = std::fs::read_to_string(&config).unwrap();
    contents.push_str("freeze = false\n");
    std::fs::write(config, contents).unwrap();
    s.start_daemon();
    for (i, flag, phase) in [(1, None, 1), (2, Some("--freeze"), 0)] {
        s.repaint_pattern(0);
        let mut args = vec![
            "capture",
            "region",
            "--save",
            "--no-notify",
            "--no-clipboard",
        ];
        if let Some(flag) = flag {
            args.push(flag);
        }
        let child = s.spawn_quikscope(&args);
        s.wait_overlay(i);
        s.repaint_pattern(1);
        s.helper(&["drag", "100", "100", "420", "330"]);
        let path = artifacts.join(format!("capture-{i}.png"));
        std::fs::copy(saved_path(&s.finish(child, Duration::from_secs(15))), &path).unwrap();
        let image = decode_png(&path);
        assert_eq!((image.width, image.height), (320, 230));
        assert_exact_phase(&image, 100, 100, phase);
    }
}

#[test]
fn region_live_composes_changed_pixels_across_outputs() {
    let Some(mut s) = Session::start_with(2, |_| {}) else {
        return;
    };
    let artifacts = s.live_artifacts("multi-output");
    s.start_daemon();
    let child = s.spawn_quikscope(&[
        "capture",
        "region",
        "--no-freeze",
        "--save",
        "--no-notify",
        "--no-clipboard",
    ]);
    s.wait_overlay(1);
    s.repaint_pattern(1);
    s.helper(&["drag", "1100", "100", "1450", "330"]);
    let path = artifacts.join("capture.png");
    std::fs::copy(saved_path(&s.finish(child, Duration::from_secs(15))), &path).unwrap();
    let image = decode_png(&path);
    assert_eq!((image.width, image.height), (350, 230));
    for y in 0..image.height {
        for x in 0..image.width {
            assert_eq!(
                image.pixel(x, y),
                phase_pixel((1100 + x) % OUTPUT_W, 100 + y, 1)
            );
        }
    }
}

#[test]
fn portal_live_fullscreen_tracks_live_desktop() {
    let Some(mut s) = live_session() else { return };
    let artifacts = s.live_artifacts("fullscreen");
    s.enable_portal();
    s.force_fullscreen();
    s.start_daemon();
    let child = s.spawn_quikscope(&[
        "capture",
        "region",
        "--no-freeze",
        "--save",
        "--no-notify",
        "--no-clipboard",
    ]);
    s.wait_overlay(1);
    s.repaint_pattern(1);
    let path = artifacts.join("selection.png");
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        assert!(s.command("grim").arg(&path).status().unwrap().success());
        let pixel = decode_png(&path).pixel(700, 500);
        if pixel[0] > 0 && pixel[0] < 255 {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "fullscreen surface never showed the live wallpaper"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
    s.keys(&["enter"]);
    let path = artifacts.join("capture.png");
    std::fs::copy(saved_path(&s.finish(child, Duration::from_secs(15))), &path).unwrap();
    let image = decode_png(&path);
    assert_eq!((image.width, image.height), (OUTPUT_W, OUTPUT_H));
    assert_exact_phase(&image, 0, 0, 1);
}

#[test]
fn region_live_click_enter_and_cancellation() {
    let Some(mut s) = live_session() else { return };
    let artifacts = s.live_artifacts("input");
    s.start_daemon();
    for (i, action) in ["click", "enter", "escape", "right"]
        .into_iter()
        .enumerate()
    {
        s.repaint_pattern(0);
        let before = std::fs::read_dir(s.shots_dir()).unwrap().count();
        let cached = std::fs::read(s.cache_file()).ok();
        let child = s.spawn_quikscope(&[
            "capture",
            "region",
            "--no-freeze",
            "--save",
            "--no-notify",
            "--no-clipboard",
        ]);
        s.wait_overlay(i + 1);
        s.repaint_pattern(1);
        match action {
            "click" => {
                s.helper(&["click", "640", "360"]);
            }
            "right" => {
                s.helper(&["click", "200", "200", "right"]);
            }
            key => s.keys(&[key]),
        }
        let out = s.finish(child, Duration::from_secs(15));
        if action == "escape" || action == "right" {
            assert_eq!(out.status.code(), Some(1));
            assert!(String::from_utf8_lossy(&out.stderr).contains("cancelled"));
            assert_eq!(std::fs::read_dir(s.shots_dir()).unwrap().count(), before);
            assert_eq!(std::fs::read(s.cache_file()).ok(), cached);
        } else {
            let path = artifacts.join(format!("{action}.png"));
            std::fs::copy(saved_path(&out), &path).unwrap();
            let img = decode_png(&path);
            assert_eq!((img.width, img.height), (OUTPUT_W, OUTPUT_H));
            assert_exact_phase(&img, 0, 0, 1);
        }
    }
    assert!(s.notifications().is_empty());
}

#[test]
fn portal_live_permission_follows_selection_and_overlay_teardown() {
    let Some(mut s) = live_session() else { return };
    let artifacts = s.live_artifacts("portal");
    s.enable_portal_with_dialog(true);
    s.start_daemon();
    let child = s.spawn_quikscope(&[
        "capture",
        "region",
        "--no-freeze",
        "--save",
        "--no-notify",
        "--no-clipboard",
    ]);
    s.wait_overlay(1);
    assert!(
        !s.toplevels()
            .iter()
            .any(|t| t.starts_with("quikscope-e2e-dialog")),
        "portal permission must not precede live selection"
    );
    s.repaint_pattern(1);
    s.helper(&["drag", "100", "100", "420", "330"]);
    s.wait_for_window("quikscope-e2e-dialog");
    let competing = s.spawn_quikscope(&[
        "capture",
        "region",
        "--no-freeze",
        "--no-notify",
        "--no-clipboard",
    ]);
    let rejected = s.finish(competing, Duration::from_secs(3));
    assert!(!rejected.status.success());
    assert!(
        String::from_utf8_lossy(&rejected.stderr).contains("a selection is already in progress"),
        "a selector must stay claimed through portal permission: {rejected:?}"
    );
    // Dialog is centered; these probes are outside it and the old selection.
    // Permission must not be shown while a dimming layer still covers them.
    let path = artifacts.join("permission.png");
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        assert!(s.command("grim").arg(&path).status().unwrap().success());
        let img = decode_png(&path);
        if [(80, 500), (1100, 500)]
            .into_iter()
            .all(|(x, y)| img.pixel(x, y) == phase_pixel(x, y, 1))
        {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "overlay still visible during portal permission"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
    s.keys(&["enter"]);
    let path = artifacts.join("capture.png");
    std::fs::copy(saved_path(&s.finish(child, Duration::from_secs(15))), &path).unwrap();
    let img = decode_png(&path);
    assert_eq!((img.width, img.height), (320, 230));
    assert_exact_phase(&img, 100, 100, 1);
}

#[test]
fn screen_capture_notifies_caches_and_fills_the_clipboard() {
    let Some(mut s) = Session::start() else {
        return;
    };
    s.start_daemon();

    let out = s.quikscope(&["capture", "screen"]);
    assert!(out.status.success());
    assert_eq!(stdout(&out), format!("shot 1: {OUTPUT_W}x{OUTPUT_H}"));

    let n = s.wait_notification();
    assert_eq!(n.summary, "Screenshot captured");
    assert_eq!(n.app_icon, "quikscope");
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
    let out = s.quikscope(&[
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

    let out = s.quikscope(&["capture", "-g", "5000,5000 10x10", "--no-notify"]);
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("outside every output"));
}

#[test]
fn region_drag_selects_a_rectangle() {
    let Some(mut s) = Session::start() else {
        return;
    };
    s.start_daemon();
    let child = s.spawn_quikscope(&["capture", "region", "--save", "--no-notify"]);
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

    let child = s.spawn_quikscope(&["capture", "region", "--no-notify"]);
    s.wait_overlay(1);
    s.helper(&["click", "640", "360"]);
    let out = s.finish(child, Duration::from_secs(15));
    assert!(out.status.success());
    assert_eq!(stdout(&out), format!("shot 1: {OUTPUT_W}x{OUTPUT_H}"));

    let child = s.spawn_quikscope(&["capture", "region", "--no-notify"]);
    s.wait_overlay(2);
    s.helper(&["click", "200", "200", "right"]);
    let out = s.finish(child, Duration::from_secs(15));
    assert_eq!(out.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&out.stderr).contains("cancelled"));

    let child = s.spawn_quikscope(&["capture", "region", "--no-notify"]);
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
    let first = s.spawn_quikscope(&["capture", "region", "--no-notify"]);
    s.wait_overlay(1);
    let second = s.quikscope(&["capture", "region", "--no-notify"]);
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
    let out = s.quikscope(&["capture", "screen", "--edit", "--no-notify"]);
    assert!(out.status.success());
    s.wait_for_window("quikscope");
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
    let red_pixels = |img: &Image| {
        img.data
            .as_chunks::<4>()
            .0
            .iter()
            .filter(|p| **p == red)
            .count()
    };
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
    assert!(!s.toplevels().iter().any(|l| l.starts_with("quikscope")));
}

#[test]
fn notification_click_opens_the_editor() {
    let Some(mut s) = Session::start() else {
        return;
    };
    s.start_daemon();
    assert!(s.quikscope(&["capture", "screen"]).status.success());
    let n = s.wait_notification();
    assert!(s.toplevels().is_empty(), "no editor before the click");
    s.click_notification(n.id, "default");
    s.wait_for_window("quikscope");

    // Clicking again focuses the same editor instead of opening another.
    s.click_notification(n.id, "edit");
    std::thread::sleep(Duration::from_millis(300));
    let editors = s
        .toplevels()
        .iter()
        .filter(|l| l.starts_with("quikscope"))
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
        s.quikscope(&["capture", "screen", "--no-notify"])
            .status
            .success()
    );
    std::thread::sleep(Duration::from_millis(300)); // background cache write
    s.stop_daemon();
    assert!(!s.socket().exists(), "daemon removes its socket on quit");

    // No daemon running: the client starts one, which loads the cache.
    let out = s.quikscope(&["open-last"]);
    assert!(out.status.success());
    assert_eq!(stdout(&out), format!("shot 1: {OUTPUT_W}x{OUTPUT_H}"));
    s.wait_for_window("quikscope");
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
    assert_eq!(stdout(&s.quikscope(&["ping"])), "ok");
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
    let out = s.quikscope(&["capture", "screen", "--save", "--no-notify"]);
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

    let out = s.quikscope(&["capture", "-g", "10,20 100x50", "--save", "--no-notify"]);
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
    let out = s.quikscope(&["capture", "screen", "--save", "--no-notify"]);
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
    let child = s.spawn_quikscope(&["capture", "region", "--save", "--no-notify"]);
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
    let out = s.quikscope(&["capture", "output", "--save", "--no-notify"]);
    let first = decode_png(&saved_path(&out));
    assert_eq!((first.width, first.height), (OUTPUT_W, OUTPUT_H));
    let out = s.quikscope(&[
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
            s.spawn_quikscope(&["capture", "-g", &g, "--no-notify", "--no-clipboard"])
        })
        .collect();
    for child in racing {
        assert!(s.finish(child, Duration::from_secs(15)).status.success());
    }
    // …then back-to-back ones; the last capture taken must win.
    for g in ["0,0 900x500", "0,0 1200x700", "10,10 333x111"] {
        let out = s.quikscope(&["capture", "-g", g, "--no-notify", "--no-clipboard"]);
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

    let out = s.quikscope(&["open-last"]);
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
        stdout(&s.quikscope(&["notifications", "status"])),
        "notifications on"
    );
    assert_eq!(
        stdout(&s.quikscope(&["notifications", "toggle"])),
        "notifications off"
    );
    let config = std::fs::read_to_string(s.root().join("config/quikscope/config.toml")).unwrap();
    assert!(config.contains("notify = false"), "{config}");
    assert!(config.contains("save_dir"), "other settings kept: {config}");

    assert!(s.quikscope(&["capture", "screen"]).status.success());
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(captured(&s), 0, "no capture notification while off");
    // An explicit flag still wins for one capture.
    assert!(
        s.quikscope(&["capture", "screen", "--notify"])
            .status
            .success()
    );
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(captured(&s), 1);

    // The setting survives a daemon restart.
    s.stop_daemon();
    s.start_daemon();
    assert_eq!(
        stdout(&s.quikscope(&["notifications", "status"])),
        "notifications off"
    );
    assert_eq!(
        stdout(&s.quikscope(&["notifications", "on"])),
        "notifications on"
    );
    assert!(s.quikscope(&["capture", "screen"]).status.success());
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(captured(&s), 2);

    let bad = s.quikscope(&["notifications", "maybe"]);
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
    let out = s.quikscope(&["capture", "screen", "--edit", "--no-notify"]);
    assert!(out.status.success());
    assert_eq!(stdout(&out), format!("shot 1: {OUTPUT_W}x{OUTPUT_H}"));
    s.wait_for_window("quikscope");
    s.wait_log("editor first frame", Duration::from_secs(10));
    assert_eq!(stdout(&s.quikscope(&["ping"])), "ok", "daemon still alive");
}

#[test]
fn input_method_composes_and_commits_into_editor_text() {
    let Some(mut s) = Session::start() else {
        return;
    };
    s.start_daemon();
    // The input method runs before the editor exists, like fcitx5 does.
    let mut ime = s.input_method();
    let out = s.quikscope(&["capture", "screen", "--edit", "--no-notify"]);
    assert!(out.status.success());
    s.wait_for_window("quikscope");
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
            .as_chunks::<4>()
            .0
            .iter()
            .zip(during.data.as_chunks::<4>().0)
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
        .as_chunks::<4>()
        .0
        .iter()
        .enumerate()
        .filter(|(_, p)| **p == red)
        .map(|(i, _)| i as u32 % img.width)
        .collect();
    assert!(!xs.is_empty(), "no text in the saved image");
    let ink = (xs.iter().max().unwrap() - xs.iter().min().unwrap() + 1) as f32;
    let layout = quikscope::text::layout("A日本語!", &quikscope::fonts::FontSpec::default());
    let expected = quikscope::text::ink(&layout, quikscope::geom::Pt::new(0.0, 0.0))
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
        s.quikscope(&["capture", "screen", "--edit", "--no-notify"])
            .status
            .success()
    );
    s.wait_for_window("quikscope");
    s.wait_log("editor window opened", Duration::from_secs(10));
    std::thread::sleep(Duration::from_millis(300));
    // Pick the rectangle, then close the editor without drawing.
    s.keys(&["r", "escape"]);
    std::thread::sleep(Duration::from_millis(300));
    assert!(!s.toplevels().iter().any(|l| l.starts_with("quikscope")));

    assert!(s.quikscope(&["open-last"]).status.success());
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
        .map(|y| {
            img.row(y)
                .as_chunks::<4>()
                .0
                .iter()
                .filter(|p| **p == red)
                .count()
        })
        .max()
        .unwrap();
    assert!(
        longest_row > 150,
        "no rectangle edge: longest red row {longest_row} px"
    );
}

/// Whether text extraction can run; without Tesseract the OCR tests skip
/// unless `QUIKSCOPE_REQUIRE_OCR=1`.
fn ocr_available() -> bool {
    let ok = quikscope::ocr::tess::version().is_ok() && quikscope::ocr::tessdata_dir().is_some();
    if !ok {
        assert!(
            std::env::var_os("QUIKSCOPE_REQUIRE_OCR").is_none(),
            "Tesseract or its language data is missing"
        );
        eprintln!("skipped: Tesseract is not installed");
    }
    ok
}

/// A small light-theme "screenshot" with two lines of 16 px text, shown
/// at 100% in the editor.
fn text_image(path: &std::path::Path) {
    use quikscope::document::{Document, Rgba, ShapeKind, Style};
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
                origin: quikscope::geom::Pt::new(14.0, 14.0 + i as f32 * 34.0),
                text: (*line).into(),
                font: quikscope::fonts::FontSpec {
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
    quikscope::export::render(&d.snapshot())
        .save_png(path, quikscope::raster::Compression::Fast)
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
        s.quikscope(&["open", png.to_str().unwrap()])
            .status
            .success()
    );
    s.wait_for_window("quikscope");
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
    let out = s.quikscope(&["capture", "screen", "--text"]);
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
            std::env::var("QUIKSCOPE_E2E").as_deref() != Ok("1"),
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
        let out = s.quikscope(&[
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
    let child = s.spawn_quikscope(&[
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
        decode_png(&reference).crop(quikscope::geom::IRect::new(100, 100, 320, 230))
    );

    let child = s.spawn_quikscope(&["capture", "region", "--no-notify", "--no-clipboard"]);
    s.wait_overlay(2);
    s.keys(&["escape"]);
    assert_eq!(
        s.finish(child, Duration::from_secs(15)).status.code(),
        Some(1)
    );

    let child = s.spawn_quikscope(&["capture", "region", "--no-notify", "--no-clipboard"]);
    s.wait_overlay(3);
    s.keys(&["enter"]);
    assert!(
        s.finish(child, Duration::from_secs(15)).status.success(),
        "Enter captures focused output"
    );
    for code in [1, 2] {
        response.store(code, std::sync::atomic::Ordering::Relaxed);
        let out = s.quikscope(&["capture", "screen", "--no-notify", "--no-clipboard"]);
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
            std::env::var("QUIKSCOPE_E2E").as_deref() != Ok("1"),
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
    let child = s.spawn_quikscope(&[
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
    let logical: quikscope::geom::IRect = mapped
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
            std::env::var("QUIKSCOPE_E2E").as_deref() != Ok("1"),
            "grim required for portal e2e"
        );
        return;
    }
    s.enable_portal();
    s.start_daemon();
    let path = s.root().join("whole-desktop.png");
    assert!(s.command("grim").arg(&path).status().unwrap().success());
    let reference = decode_png(&path);
    let child = s.spawn_quikscope(&[
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
        reference.crop(quikscope::geom::IRect::new(1100, 100, 350, 230))
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
    let child = s.spawn_quikscope(&["capture", "region", "--no-notify"]);
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
            std::env::var("QUIKSCOPE_E2E").as_deref() != Ok("1"),
            "grim required for portal e2e"
        );
        eprintln!("skipped: portal e2e requires grim");
        return;
    }
    // labwc offers layer-shell, the KDE case where overlays could open early.
    s.enable_portal_with_dialog(true);
    s.start_daemon();
    let child = s.spawn_quikscope(&["capture", "region", "--no-notify"]);
    s.wait_for_window("quikscope-e2e-dialog");
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
