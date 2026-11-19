//! Direct-frame recording diagnostic (no encoder or daemon required).
//! `cargo run --example record_source_check` starts private headless labwc.
//! `--x11` uses DISPLAY only with QUIKSCOPE_RECORD_PRIVATE=1 (run inside Xvfb).
//! `--hyprland` explicitly permits a <=3s 1280x720 capture on the live session;
//! this mode never creates outlines or changes the clipboard. Its full-frame
//! still-reference capture is taken once, not continuously. No pixels are saved.
//! Private mode checks outline exclusion and wl-paste file MIME offers too.
//! `--force-copy` measures throughput on static content by requesting compositor
//! damage (diagnostic only). `--large` selects 2560x1440 instead of 1280x720.
#[cfg(target_os = "linux")]
mod linux {
    use anyhow::{Context, Result, ensure};
    use quikscope::{
        geom::IRect,
        platform::{self, Capture, Clipboard, RecordTarget, ScreenRecorder},
    };
    use std::{
        process::{Command, Stdio},
        time::{Duration, Instant},
    };

    fn cpu() -> f64 {
        unsafe {
            let mut ts: libc::timespec = std::mem::zeroed();
            libc::clock_gettime(libc::CLOCK_PROCESS_CPUTIME_ID, &mut ts);
            ts.tv_sec as f64 + ts.tv_nsec as f64 * 1e-9
        }
    }
    struct Child(std::process::Child);
    impl Drop for Child {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    pub fn run() -> Result<()> {
        let args: Vec<_> = std::env::args().collect();
        // Wayland recordings capture in a helper spawned from /proc/self/exe,
        // which here is this example.
        if args.get(1).map(String::as_str) == Some(platform::wlroots::helper::VERB) {
            env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("warn"))
                .init();
            return platform::wlroots::helper::helper_main(&args[2..]);
        }
        env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
        let live = args.iter().any(|a| a == "--hyprland");
        let x11 = args.iter().any(|a| a == "--x11");
        if !live && !x11 && std::env::var("QUIKSCOPE_RECORD_PRIVATE").as_deref() != Ok("1") {
            return private();
        }
        ensure!(
            live || std::env::var("QUIKSCOPE_RECORD_PRIVATE").as_deref() == Ok("1"),
            "refusing a non-private session without --hyprland"
        );
        let capture: Box<dyn Capture> = if x11 {
            Box::new(platform::x11::ScreenCapture::spawn()?)
        } else {
            Box::new(platform::wlroots::Screencopy::spawn()?)
        };
        let snapshot = capture.capture(None, false)?;
        let output = snapshot.outputs.first().context("no output")?;
        let (w, h) = if args.iter().any(|a| a == "--large") {
            (2560, 1440)
        } else {
            (1280, 720)
        };
        let r = output.logical;
        let region = if live {
            IRect::new(r.x, r.y, w.min(r.w), h.min(r.h))
        } else {
            IRect::new(r.x + 100, r.y + 100, 400, 300)
        };
        let recorder: Box<dyn ScreenRecorder> = if x11 {
            Box::new(platform::x11::Recorder)
        } else {
            Box::new(platform::wlroots::Recorder)
        };
        let _outline = if live {
            None
        } else {
            Some(recorder.outline(region)?)
        };
        let target = RecordTarget {
            output: output.name.clone(),
            output_rect: r,
            region,
        };
        let mut source = if !x11 && args.iter().any(|a| a == "--force-copy") {
            platform::wlroots::Recorder.open_benchmark(&target, 60, false)?
        } else {
            recorder.open(&target, 60, false)?
        };
        let mut matched = false;
        let mut count = 0;
        let started = Instant::now();
        let cpu_start = cpu();
        while started.elapsed() < Duration::from_secs(3) {
            let Some(frame) = source.next_frame(Duration::from_millis(50))? else {
                continue;
            };
            ensure!(frame.width() > 0 && frame.height() > 0, "empty frame");
            ensure!(
                source.size() == (frame.width(), frame.height()),
                "frame size changed"
            );
            if !matched {
                let image = frame.to_image();
                let reference = capture.capture(Some(std::slice::from_ref(&output.name)), false)?;
                let reference = &reference.outputs[0].image;
                let scale = reference.width as f64 / r.w as f64;
                let ox = ((region.x - r.x) as f64 * scale).round() as usize;
                let oy = ((region.y - r.y) as f64 * scale).round() as usize;
                if !live {
                    let x = ox - 1;
                    let y = oy + image.height as usize / 2;
                    ensure!(
                        reference.data[(y * reference.width as usize + x) * 4..][..3]
                            == [0xf2, 0x8c, 0x8f],
                        "outline not visible immediately outside region"
                    );
                    println!("outline: accent visible one pixel outside region");
                }
                let mut mismatches = 0usize;
                for y in 0..image.height as usize {
                    for x in 0..image.width as usize {
                        let a = &image.data[(y * image.width as usize + x) * 4..][..3];
                        let reference = if live { reference } else { &output.image };
                        let b = &reference.data
                            [((oy + y) * reference.width as usize + ox + x) * 4..][..3];
                        if a != b {
                            mismatches += 1;
                        }
                    }
                }
                let fraction = mismatches as f64 / (image.width as f64 * image.height as f64);
                if live {
                    ensure!(
                        fraction < 0.1,
                        "frame/still reference differ at {:.2}% pixels",
                        fraction * 100.0
                    );
                } else {
                    ensure!(
                        mismatches == 0,
                        "private pixels differ at {mismatches} pixels"
                    );
                }
                println!(
                    "frame/reference: {}x{}, differing pixels={mismatches} ({:.3}%)",
                    image.width,
                    image.height,
                    fraction * 100.0
                );
                matched = true;
            }
            count += 1;
        }
        println!(
            "frames={count}, seconds={:.3}, fps={:.2}, process CPU/frame={:.3}ms (capture transport is logged above)",
            started.elapsed().as_secs_f64(),
            count as f64 / started.elapsed().as_secs_f64(),
            (cpu() - cpu_start) * 1000.0 / count.max(1) as f64
        );
        ensure!(matched, "source produced no frames");
        if !live {
            let clipboard: Box<dyn Clipboard> = if x11 {
                Box::new(platform::x11::SelectionOwner::spawn()?)
            } else {
                Box::new(platform::wlroots::DataControl::spawn()?)
            };
            clipboard.set_files(&[std::path::PathBuf::from("/tmp/quikscope test#日本.mp4")]);
            let mut command = Command::new(if x11 { "xclip" } else { "wl-paste" });
            if x11 {
                command.args(["-selection", "clipboard", "-o", "-t", "text/uri-list"]);
            } else {
                command.args(["--type", "text/uri-list", "--no-newline"]);
            }
            let out = command.output()?;
            ensure!(
                out.status.success(),
                "clipboard reader failed: {}",
                String::from_utf8_lossy(&out.stderr)
            );
            ensure!(
                out.stdout == b"file:///tmp/quikscope%20test%23%E6%97%A5%E6%9C%AC.mp4\r\n",
                "clipboard URI mismatch: {:?}",
                out.stdout
            );
            if !x11 {
                let list = Command::new("wl-paste").arg("-l").output()?;
                let list = String::from_utf8(list.stdout)?;
                for mime in [
                    "text/uri-list",
                    "text/plain",
                    "x-special/gnome-copied-files",
                ] {
                    ensure!(list.contains(mime), "missing {mime}");
                }
            }
            println!("file clipboard: URI bytes and advertised types passed");
        }
        Ok(())
    }
    fn private() -> Result<()> {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir()?;
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700))?;
        let env = [
            ("XDG_RUNTIME_DIR", dir.path().to_str().unwrap()),
            ("WAYLAND_DISPLAY", "wayland-0"),
            ("WLR_BACKENDS", "headless"),
            ("WLR_RENDERER", "pixman"),
            ("WLR_LIBINPUT_NO_DEVICES", "1"),
            ("QUIKSCOPE_RECORD_PRIVATE", "1"),
        ];
        let _compositor = Child(
            Command::new("labwc")
                .args(["-C", dir.path().to_str().unwrap()])
                .envs(env)
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()?,
        );
        let deadline = Instant::now() + Duration::from_secs(5);
        while !dir.path().join("wayland-0").exists() {
            ensure!(Instant::now() < deadline, "private labwc failed to start");
            std::thread::sleep(Duration::from_millis(20));
        }
        let helper = std::env::current_exe()?
            .parent()
            .unwrap()
            .parent()
            .unwrap()
            .join("quikscope-e2e-helper");
        let _wallpaper = Child(
            Command::new(helper)
                .arg("pattern")
                .envs(env)
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .context("build quikscope-e2e-helper with --features e2e first")?,
        );
        std::thread::sleep(Duration::from_millis(200));
        let status = Command::new(std::env::current_exe()?).envs(env).status()?;
        ensure!(status.success(), "private source diagnostic failed");
        Ok(())
    }
}

fn main() -> anyhow::Result<()> {
    #[cfg(target_os = "linux")]
    return linux::run();
    #[cfg(not(target_os = "linux"))]
    anyhow::bail!("record_source_check is a Linux diagnostic")
}
