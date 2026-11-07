#![cfg_attr(windows, windows_subsystem = "windows")]

use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use quikscope::config;
use quikscope::ipc::{self, CaptureOptions, Mode, Request, Response, Switch};

const USAGE: &str = "\
quikscope — screenshot daemon and annotation editor

USAGE:
    quikscope capture <region|output|window|screen> [OPTIONS]
    quikscope open-last
    quikscope open <FILE>
    quikscope settings
    quikscope notifications <on|off|toggle|status>
    quikscope daemon
    quikscope ping | quit

CAPTURE OPTIONS:
    --edit / --no-edit           Open the editor right away (default: on notification click)
    --notify / --no-notify       Show a desktop notification
    --clipboard / --no-clipboard Copy the capture to the clipboard
    --cursor / --no-cursor       Include the pointer
    --freeze / --no-freeze       Freeze before region selection / select over the live desktop
    --save                       Also save the raw capture to the screenshots directory
    --text                       Extract the text in the capture: copy it and print it
    -o, --output <NAME>          Output for `output` mode (default: focused output)
    -g, --geometry <\"X,Y WxH\">   Capture this logical rectangle (slurp format), no selection

Commands talk to the daemon over $XDG_RUNTIME_DIR/quikscope-$WAYLAND_DISPLAY.sock
(override with QUIKSCOPE_SOCKET) and start it on demand.
";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    #[cfg(windows)]
    if args.first().map(String::as_str) != Some("daemon") {
        // Attach to the caller, but never allocate a console for the daemon.
        unsafe {
            let _ = windows::Win32::System::Console::AttachConsole(
                windows::Win32::System::Console::ATTACH_PARENT_PROCESS,
            );
        }
    }
    let level = if args.first().map(String::as_str) == Some("daemon") {
        "quikscope=info,warn"
    } else {
        "warn"
    };
    env_logger::Builder::from_env(env_logger::Env::new().filter_or("QUIKSCOPE_LOG", level))
        .format_timestamp_millis()
        .init();

    match run(args) {
        Ok(code) => code,
        Err(e) => {
            eprintln!("quikscope: {e:#}");
            ExitCode::FAILURE
        }
    }
}

fn run(args: Vec<String>) -> anyhow::Result<ExitCode> {
    let mut it = args.into_iter();
    let Some(cmd) = it.next() else {
        eprint!("{USAGE}");
        return Ok(ExitCode::from(2));
    };
    let socket = config::socket_path();
    let rest: Vec<String> = it.collect();
    let request = match cmd.as_str() {
        "-h" | "--help" | "help" => {
            print!("{USAGE}");
            return Ok(ExitCode::SUCCESS);
        }
        "-V" | "--version" => {
            println!("quikscope {}", env!("CARGO_PKG_VERSION"));
            return Ok(ExitCode::SUCCESS);
        }
        "daemon" => {
            no_extra(&rest)?;
            quikscope::daemon::run(socket)?;
            return Ok(ExitCode::SUCCESS);
        }
        #[cfg(windows)]
        "autostart" => {
            let [action] = rest.as_slice() else {
                anyhow::bail!("usage: quikscope autostart <on|off|status>");
            };
            let on = quikscope::platform::windows::autostart(action)?;
            println!("autostart {}", if on { "on" } else { "off" });
            return Ok(ExitCode::SUCCESS);
        }
        "capture" => parse_capture(&rest)?,
        "open-last" | "last" => {
            no_extra(&rest)?;
            Request::OpenLast
        }
        "settings" => parse_settings(&rest)?,
        "open" => match rest.as_slice() {
            [path] => {
                Request::Open(std::fs::canonicalize(path).unwrap_or_else(|_| PathBuf::from(path)))
            }
            _ => anyhow::bail!("usage: quikscope open <FILE>"),
        },
        "notifications" | "notify" => match rest.as_slice() {
            [s] => Request::Notifications(Switch::parse(s).ok_or_else(|| {
                anyhow::anyhow!("usage: quikscope notifications <on|off|toggle|status>")
            })?),
            _ => anyhow::bail!("usage: quikscope notifications <on|off|toggle|status>"),
        },
        "ping" => {
            no_extra(&rest)?;
            Request::Ping
        }
        "quit" => {
            no_extra(&rest)?;
            // Never start a daemon just to stop it.
            return report(ipc::send(
                &socket,
                &Request::Quit,
                Some(Duration::from_secs(5)),
            ));
        }
        other => anyhow::bail!("unknown command {other:?}; see `quikscope --help`"),
    };
    // Interactive selection waits on the user; everything else is quick.
    let timeout = match &request {
        Request::Capture(Mode::Region, o) if o.geometry.is_none() => None,
        _ => Some(Duration::from_secs(30)),
    };
    report(ipc::send_or_spawn(&socket, &request, timeout))
}

fn report(result: anyhow::Result<Response>) -> anyhow::Result<ExitCode> {
    match result? {
        Response::Shot {
            id,
            width,
            height,
            saved,
        } => {
            match saved {
                Some(p) => println!("{}", p.display()),
                None => println!("shot {id}: {width}x{height}"),
            }
            Ok(ExitCode::SUCCESS)
        }
        Response::Text { text, .. } if text.is_empty() => {
            eprintln!("quikscope: no text found");
            Ok(ExitCode::from(1))
        }
        Response::Text { text, .. } => {
            println!("{text}");
            Ok(ExitCode::SUCCESS)
        }
        Response::Pong => {
            println!("ok");
            Ok(ExitCode::SUCCESS)
        }
        Response::Notifications(on) => {
            println!("notifications {}", if on { "on" } else { "off" });
            Ok(ExitCode::SUCCESS)
        }
        Response::Cancelled => {
            eprintln!("quikscope: cancelled");
            Ok(ExitCode::from(1))
        }
        Response::Error(e) => anyhow::bail!(e),
    }
}

fn no_extra(rest: &[String]) -> anyhow::Result<()> {
    match rest.first() {
        Some(extra) => anyhow::bail!("unexpected argument {extra:?}"),
        None => Ok(()),
    }
}

fn parse_settings(args: &[String]) -> anyhow::Result<Request> {
    no_extra(args)?;
    Ok(Request::Settings)
}

fn parse_capture(args: &[String]) -> anyhow::Result<Request> {
    let mut mode = None;
    let mut o = CaptureOptions::default();
    let mut it = args.iter();
    while let Some(a) = it.next() {
        let mut value = |name: &str| {
            it.next()
                .cloned()
                .ok_or_else(|| anyhow::anyhow!("{name} needs a value"))
        };
        match a.as_str() {
            "--edit" => o.edit = Some(true),
            "--no-edit" => o.edit = Some(false),
            "--notify" => o.notify = Some(true),
            "--no-notify" => o.notify = Some(false),
            "--clipboard" => o.clipboard = Some(true),
            "--no-clipboard" => o.clipboard = Some(false),
            "--cursor" => o.cursor = Some(true),
            "--no-cursor" => o.cursor = Some(false),
            "--freeze" => o.freeze = Some(true),
            "--no-freeze" => o.freeze = Some(false),
            "--save" => o.save = true,
            "--text" => o.text = true,
            "-o" | "--output" => o.output = Some(value(a)?),
            "-g" | "--geometry" => {
                o.geometry = Some(value(a)?.parse().map_err(anyhow::Error::msg)?)
            }
            flag if flag.starts_with('-') => anyhow::bail!("unknown option {flag:?}"),
            m if mode.is_none() => {
                mode = Some(
                    Mode::parse(m).ok_or_else(|| anyhow::anyhow!("unknown capture mode {m:?}"))?,
                )
            }
            extra => anyhow::bail!("unexpected argument {extra:?}"),
        }
    }
    // `--geometry` alone implies a non-interactive region capture.
    let mode = match (mode, &o.geometry) {
        (Some(m), _) => m,
        (None, Some(_)) => Mode::Region,
        (None, None) => anyhow::bail!("capture needs a mode: region, output, window or screen"),
    };
    Ok(Request::Capture(mode, o))
}

#[cfg(test)]
mod tests {
    use super::*;
    use quikscope::geom::IRect;

    fn args(s: &str) -> Vec<String> {
        s.split_whitespace().map(String::from).collect()
    }

    #[test]
    fn settings_opens_without_extra_arguments() {
        assert_eq!(parse_settings(&args("")).unwrap(), Request::Settings);
        assert!(parse_settings(&args("--bogus")).is_err());
        assert!(parse_settings(&args("extra")).is_err());
    }

    #[test]
    fn capture_flags_map_to_request_options() {
        let req = parse_capture(&args(
            "output --edit --no-notify --cursor -o DP-2 --save --text",
        ))
        .unwrap();
        let Request::Capture(Mode::Output, o) = req else {
            panic!("{req:?}")
        };
        assert_eq!(o.edit, Some(true));
        assert_eq!(o.notify, Some(false));
        assert_eq!(o.cursor, Some(true));
        assert_eq!(o.clipboard, None);
        assert_eq!(o.output.as_deref(), Some("DP-2"));
        assert!(o.save);
        assert!(o.text);
    }

    #[test]
    fn freeze_flags_override_in_order_and_round_trip() {
        for (flags, expected) in [
            ("region", None),
            ("region --freeze", Some(true)),
            ("region --no-freeze", Some(false)),
            ("region --freeze --no-freeze", Some(false)),
            ("region --no-freeze --freeze", Some(true)),
        ] {
            let req = parse_capture(&args(flags)).unwrap();
            let Request::Capture(_, options) = &req else {
                unreachable!()
            };
            assert_eq!(options.freeze, expected, "{flags}");
            assert_eq!(Request::parse(&req.to_string()), Ok(req));
        }
    }

    #[test]
    fn geometry_accepts_slurp_output_and_implies_region() {
        let mut a = args("-g");
        a.push("10,20 30x40".into());
        let Request::Capture(mode, o) = parse_capture(&a).unwrap() else {
            unreachable!()
        };
        assert_eq!(mode, Mode::Region);
        assert_eq!(o.geometry, Some(IRect::new(10, 20, 30, 40)));
    }

    #[test]
    fn capture_rejects_bad_input() {
        assert!(parse_capture(&args("")).is_err());
        assert!(parse_capture(&args("region screen")).is_err());
        assert!(parse_capture(&args("region --bogus")).is_err());
        assert!(parse_capture(&args("region --output")).is_err());
        assert!(parse_capture(&args("everything")).is_err());
    }
}
