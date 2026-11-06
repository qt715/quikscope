//! A separate, persistent Wayland connection: GPUI's event queue belongs to the UI thread.
use anyhow::{Context as _, Result, ensure};
use wayland_client::globals::{GlobalListContents, registry_queue_init};
use wayland_client::protocol::{wl_output, wl_registry};
use wayland_client::{Connection, Dispatch, EventQueue, Proxy, QueueHandle, WEnum};
use wayland_protocols::xdg::xdg_output::zv1::client::{zxdg_output_manager_v1, zxdg_output_v1};

use crate::{geom::IRect, raster::Transform};

#[derive(Clone, Debug)]
pub(super) struct OutputInfo {
    pub name: String,
    pub logical: IRect,
    pub physical_width: u32,
}

#[derive(Default)]
struct Output {
    global: u32,
    wl: Option<wl_output::WlOutput>,
    xdg: Option<zxdg_output_v1::ZxdgOutputV1>,
    name: Option<String>,
    position: (i32, i32),
    mode: (i32, i32),
    scale: i32,
    transform: Transform,
    logical_position: Option<(i32, i32)>,
    logical_size: Option<(i32, i32)>,
}

struct State {
    outputs: Vec<Output>,
    manager: Option<zxdg_output_manager_v1::ZxdgOutputManagerV1>,
}

pub(super) struct Layout {
    _connection: Connection,
    queue: EventQueue<State>,
    state: State,
}

pub(crate) fn supports_layer_shell() -> Result<bool> {
    let connection = Connection::connect_to_env()?;
    let (globals, _queue) = registry_queue_init::<State>(&connection)?;
    Ok(globals
        .contents()
        .with_list(|list| list.iter().any(|g| g.interface == "zwlr_layer_shell_v1")))
}

/// Output metadata without asking the Screenshot portal for pixels or permission.
pub(crate) fn selection_outputs() -> Result<Vec<(String, IRect, u32)>> {
    Ok(Layout::connect()?
        .outputs()?
        .into_iter()
        .map(|o| (o.name, o.logical, o.physical_width))
        .collect())
}

impl Layout {
    pub fn connect() -> Result<Self> {
        let connection = Connection::connect_to_env().context("connecting portal output layout")?;
        let (globals, mut queue) = registry_queue_init::<State>(&connection)?;
        let qh = queue.handle();
        let mut state = State {
            outputs: Vec::new(),
            manager: globals.bind(&qh, 2..=3, ()).ok(),
        };
        globals.contents().with_list(|list| {
            for g in list.iter().filter(|g| g.interface == "wl_output") {
                state.add(globals.registry(), g.name, g.version, &qh);
            }
        });
        queue.roundtrip(&mut state)?;
        queue.roundtrip(&mut state)?;
        Ok(Self {
            _connection: connection,
            queue,
            state,
        })
    }

    pub fn outputs(&mut self) -> Result<Vec<OutputInfo>> {
        self.queue.roundtrip(&mut self.state)?;
        self.queue.roundtrip(&mut self.state)?;
        let outputs: Vec<_> = self
            .state
            .outputs
            .iter()
            .filter_map(|o| {
                let (x, y) = o.logical_position.unwrap_or(o.position);
                let (mut w, mut h) = o.mode;
                if o.transform.swaps_axes() {
                    std::mem::swap(&mut w, &mut h);
                }
                let (w, h) = o
                    .logical_size
                    .unwrap_or((w / o.scale.max(1), h / o.scale.max(1)));
                (w > 0 && h > 0).then(|| OutputInfo {
                    name: o
                        .name
                        .clone()
                        .unwrap_or_else(|| format!("output-{}", o.global)),
                    logical: IRect::new(x, y, w, h),
                    physical_width: if o.transform.swaps_axes() {
                        o.mode.1
                    } else {
                        o.mode.0
                    } as u32,
                })
            })
            .collect();
        ensure!(
            !outputs.is_empty(),
            "portal: compositor reported no usable outputs"
        );
        Ok(outputs)
    }
}

impl State {
    fn add(
        &mut self,
        registry: &wl_registry::WlRegistry,
        global: u32,
        version: u32,
        qh: &QueueHandle<Self>,
    ) {
        let wl = registry.bind::<wl_output::WlOutput, _, _>(global, version.min(4), qh, global);
        let xdg = self
            .manager
            .as_ref()
            .map(|m| m.get_xdg_output(&wl, qh, global));
        self.outputs.push(Output {
            global,
            wl: Some(wl),
            xdg,
            scale: 1,
            ..Default::default()
        });
    }
}

impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for State {
    fn event(
        state: &mut Self,
        registry: &wl_registry::WlRegistry,
        event: wl_registry::Event,
        _: &GlobalListContents,
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        match event {
            wl_registry::Event::Global {
                name,
                interface,
                version,
            } if interface == "wl_output" => state.add(registry, name, version, qh),
            wl_registry::Event::GlobalRemove { name } => {
                if let Some(i) = state.outputs.iter().position(|o| o.global == name) {
                    let o = state.outputs.remove(i);
                    if let Some(xdg) = o.xdg {
                        xdg.destroy();
                    }
                    if let Some(wl) = o.wl
                        && wl.version() >= 3
                    {
                        wl.release();
                    }
                }
            }
            _ => {}
        }
    }
}
impl Dispatch<wl_output::WlOutput, u32> for State {
    fn event(
        state: &mut Self,
        _: &wl_output::WlOutput,
        event: wl_output::Event,
        global: &u32,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let Some(o) = state.outputs.iter_mut().find(|o| o.global == *global) else {
            return;
        };
        match event {
            wl_output::Event::Geometry {
                x, y, transform, ..
            } => {
                o.position = (x, y);
                if let WEnum::Value(t) = transform {
                    o.transform = Transform::from_wl(t.into());
                }
            }
            wl_output::Event::Mode {
                flags: WEnum::Value(flags),
                width,
                height,
                ..
            } if flags.contains(wl_output::Mode::Current) => o.mode = (width, height),
            wl_output::Event::Scale { factor } => o.scale = factor,
            wl_output::Event::Name { name } => o.name = Some(name),
            _ => {}
        }
    }
}
impl Dispatch<zxdg_output_v1::ZxdgOutputV1, u32> for State {
    fn event(
        state: &mut Self,
        _: &zxdg_output_v1::ZxdgOutputV1,
        event: zxdg_output_v1::Event,
        global: &u32,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let Some(o) = state.outputs.iter_mut().find(|o| o.global == *global) else {
            return;
        };
        match event {
            zxdg_output_v1::Event::LogicalPosition { x, y } => o.logical_position = Some((x, y)),
            zxdg_output_v1::Event::LogicalSize { width, height } => {
                o.logical_size = Some((width, height))
            }
            zxdg_output_v1::Event::Name { name } => o.name = Some(name),
            _ => {}
        }
    }
}
wayland_client::delegate_noop!(State: ignore zxdg_output_manager_v1::ZxdgOutputManagerV1);
