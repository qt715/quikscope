//! Screenshot portal fallback for compositors without wlr-screencopy.
mod layout;

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use anyhow::{Context as _, Result, anyhow, bail, ensure};
use zbus::blocking::{Connection, MessageIterator, Proxy};
use zbus::zvariant::{OwnedObjectPath, OwnedValue, Value};

use crate::capture::{OutputFrame, Snapshot};
use crate::geom::IRect;
use crate::platform::Capture;
use crate::raster::Image;
use layout::{Layout, OutputInfo};

const DEST: &str = "org.freedesktop.portal.Desktop";
const PATH: &str = "/org/freedesktop/portal/desktop";
const INTERFACE: &str = "org.freedesktop.portal.Screenshot";
static TOKEN: AtomicU64 = AtomicU64::new(0);

pub struct Screenshot {
    connection: Connection,
    layout: Mutex<Layout>,
}

impl Screenshot {
    pub fn connect() -> Result<Self> {
        Ok(Self {
            connection: Connection::session()
                .context("connecting to screenshot portal session bus")?,
            layout: Mutex::new(Layout::connect()?),
        })
    }
}

impl Capture for Screenshot {
    fn capture(&self, outputs: Option<&[String]>, cursor: bool) -> Result<Snapshot> {
        let started = Instant::now();
        // Serialize layout updates and captures: portal backends may display permission UI.
        let mut layout = self.layout.lock().unwrap();
        let before = layout.outputs()?;
        if cursor {
            log::warn!(
                "Screenshot portal has no cursor option; pointer inclusion is controlled by the desktop"
            );
        }
        let proxy = Proxy::new(&self.connection, DEST, PATH, INTERFACE)?;
        // Subscribe to all Request responses before calling, also covering portals that
        // return a different handle from the recommended token-derived path.
        let rule = zbus::MatchRule::builder()
            .msg_type(zbus::message::Type::Signal)
            .sender(DEST)?
            .interface("org.freedesktop.portal.Request")?
            .member("Response")?
            .build();
        let responses = MessageIterator::for_match_rule(rule, &self.connection, Some(16))?;
        let token = format!(
            "quickscope_{}_{}",
            std::process::id(),
            TOKEN.fetch_add(1, Ordering::Relaxed)
        );
        let options = HashMap::from([
            ("handle_token", Value::from(token.as_str())),
            ("interactive", Value::from(false)),
            ("modal", Value::from(false)),
        ]);
        let handle: OwnedObjectPath = proxy
            .call("Screenshot", &("", options))
            .context("requesting screenshot from desktop portal")?;
        for response in responses {
            let response = response.context("waiting for screenshot portal response")?;
            if response.header().path().map(|p| p.as_str()) != Some(handle.as_str()) {
                continue;
            }
            let (code, results) = response
                .body()
                .deserialize::<(u32, HashMap<String, OwnedValue>)>()?;
            let uri = response_uri(code, &results)?;
            let path = url::Url::parse(&uri)
                .context("invalid screenshot portal URI")?
                .to_file_path()
                .map_err(|_| anyhow!("screenshot portal returned a non-local file URI"))?;
            // The portal does not transfer file ownership. KDE saves in Pictures;
            // GNOME may return a user-visible screenshot or document-portal file.
            // Never unlink a path merely because its parent happens to be /tmp.
            let image = Image::load(&path).context("reading screenshot portal PNG")?;
            let after = layout.outputs()?;
            ensure!(
                before
                    .iter()
                    .map(|o| (&o.name, o.logical))
                    .eq(after.iter().map(|o| (&o.name, o.logical))),
                "output layout changed during portal screenshot; capture again"
            );
            let snapshot = split(image, &after, outputs)?;
            log::debug!(
                "portal capture: {:.2} ms",
                started.elapsed().as_secs_f64() * 1000.
            );
            return Ok(snapshot);
        }
        bail!("screenshot portal disconnected without a response")
    }
}

fn response_uri(code: u32, results: &HashMap<String, OwnedValue>) -> Result<String> {
    match code {
        0 => results
            .get("uri")
            .and_then(|v| <&str>::try_from(v).ok())
            .map(str::to_owned)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| anyhow!("screenshot portal returned success without a URI")),
        1 => bail!("screenshot portal permission denied or request cancelled (response 1)"),
        2 => bail!("screenshot portal permission denied or capture failed (response 2)"),
        n => bail!("screenshot portal returned unknown response {n}"),
    }
}

fn split(image: Image, outputs: &[OutputInfo], requested: Option<&[String]>) -> Result<Snapshot> {
    let bounds = outputs
        .iter()
        .map(|o| o.logical)
        .reduce(|a, b| a.union(&b))
        .ok_or_else(|| anyhow!("portal: no outputs"))?;
    ensure!(
        bounds.w > 0 && bounds.h > 0,
        "portal: invalid desktop layout"
    );
    let sx = image.width as f64 / bounds.w as f64;
    let sy = image.height as f64 / bounds.h as f64;
    // A portal exposes a single raster, not per-output pixel transforms. Reject
    // nonuniform rasters rather than silently crop the wrong monitor.
    ensure!(
        (sx - sy).abs() <= 1. / bounds.w.max(bounds.h) as f64,
        "portal screenshot {}x{} does not match logical desktop {}x{}",
        image.width,
        image.height,
        bounds.w,
        bounds.h
    );
    let image = Arc::new(image);
    let mut frames = Vec::new();
    if let Some(names) = requested {
        for name in names {
            ensure!(
                outputs.iter().any(|o| &o.name == name),
                "unknown output {name}"
            );
        }
    }
    for output in outputs {
        if requested.is_some_and(|names| !names.contains(&output.name)) {
            continue;
        }
        let r = output.logical;
        let x = ((r.x - bounds.x) as f64 * sx).round() as i32;
        let y = ((r.y - bounds.y) as f64 * sy).round() as i32;
        let right = ((r.right() - bounds.x) as f64 * sx).round() as i32;
        let bottom = ((r.bottom() - bounds.y) as f64 * sy).round() as i32;
        let pixels = IRect::new(x, y, right - x, bottom - y);
        ensure!(
            pixels.w > 0 && pixels.h > 0,
            "portal returned an empty output image"
        );
        frames.push(OutputFrame {
            name: output.name.clone(),
            logical: r,
            image: if pixels == image.bounds() {
                image.clone()
            } else {
                Arc::new(image.crop(pixels))
            },
        });
    }
    ensure!(!frames.is_empty(), "portal: no outputs selected");
    Ok(Snapshot { outputs: frames })
}

#[cfg(test)]
mod tests {
    use super::*;
    fn output(name: &str, rect: IRect) -> OutputInfo {
        OutputInfo {
            name: name.into(),
            logical: rect,
        }
    }

    #[test]
    fn split_negative_origins_and_fractional_scale() {
        let outputs = [
            output("left", IRect::new(-4, 2, 4, 4)),
            output("right", IRect::new(0, 2, 4, 4)),
        ];
        let mut image = Image::filled(12, 6, [1, 2, 3, 255]);
        image.blit(&Image::filled(6, 6, [4, 5, 6, 255]), 6, 0);
        let snapshot = split(image, &outputs, None).unwrap();
        assert_eq!(snapshot.outputs[0].image.width, 6);
        assert_eq!(snapshot.outputs[0].image.pixel(5, 5), [1, 2, 3, 255]);
        assert_eq!(snapshot.outputs[1].image.pixel(0, 0), [4, 5, 6, 255]);
        assert_eq!(snapshot.outputs[0].logical, outputs[0].logical);
    }
    #[test]
    fn split_filter_gaps_and_invalid_dimensions() {
        let outputs = [
            output("a", IRect::new(0, 0, 2, 4)),
            output("b", IRect::new(4, 0, 2, 4)),
        ];
        let names = vec!["b".into()];
        let snapshot = split(Image::new(12, 8), &outputs, Some(&names)).unwrap();
        assert_eq!(snapshot.outputs.len(), 1);
        assert_eq!(snapshot.outputs[0].image.width, 4);
        assert!(split(Image::new(12, 4), &outputs, None).is_err());
        assert!(split(Image::new(12, 8), &outputs, Some(&["absent".into()])).is_err());
    }
    #[test]
    fn responses_report_denial_and_missing_uri() {
        let empty = HashMap::new();
        for code in [1, 2] {
            assert!(
                response_uri(code, &empty)
                    .unwrap_err()
                    .to_string()
                    .contains("permission denied")
            );
        }
        assert!(response_uri(0, &empty).is_err());
        assert!(response_uri(99, &empty).is_err());
        let values = HashMap::from([(
            "uri".into(),
            OwnedValue::from(zbus::zvariant::Str::from("file:///tmp/a%20b.png")),
        )]);
        assert_eq!(response_uri(0, &values).unwrap(), "file:///tmp/a%20b.png");
    }
}
