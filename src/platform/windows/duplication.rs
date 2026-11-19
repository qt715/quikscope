//! Desktop Duplication of one output. DXGI allows a single
//! IDXGIOutputDuplication per output and process, so the capture worker owns
//! exactly one per output and serves screenshots and recordings from it.
//!
//! Each acquired desktop image is copied on the GPU into `latest` and the
//! DXGI frame is released at once; readbacks (a whole output for
//! screenshots, a region for recordings) come from that copy, so any reader
//! can still read the newest image after another reader consumed the update.
use super::desktop::Monitor;
use crate::{
    geom::IRect,
    platform::windows_pixels::{self as pixels, PointerKind, PointerShape, Rotation},
    raster::Image,
};
use anyhow::{Context, Result, bail};
use std::{sync::Arc, time::Duration};
use windows::{
    Win32::{
        Foundation::HMODULE,
        Graphics::{
            Direct3D::D3D_DRIVER_TYPE_UNKNOWN,
            Direct3D11::*,
            Dxgi::{Common::*, *},
        },
    },
    core::Interface,
};

/// What one [`Duplication::acquire`] brought.
#[derive(Clone, Copy, Debug, Default)]
pub(super) struct Update {
    /// New desktop pixels.
    pub desktop: bool,
    /// The hardware pointer moved, appeared, hid or changed shape.
    pub pointer: bool,
}

/// The hardware pointer as DXGI last reported it; desktop coordinates
/// relative to the output's top-left.
#[derive(Default)]
struct Pointer {
    visible: bool,
    position: (i32, i32),
    shape: Option<PointerShape>,
}

pub(super) struct Duplication {
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    duplication: IDXGIOutputDuplication,
    rotation: Rotation,
    /// Texture size in scan-out orientation.
    size: (u32, u32),
    latest: Option<ID3D11Texture2D>,
    /// Whole-output readback of `latest`, kept until the next desktop image.
    image: Option<Arc<Image>>,
    staging: Option<ID3D11Texture2D>,
    region_staging: Option<((u32, u32), ID3D11Texture2D)>,
    pointer: Pointer,
}

impl Duplication {
    pub fn new(monitor: &Monitor) -> Result<Self> {
        unsafe {
            let factory: IDXGIFactory1 = CreateDXGIFactory1()?;
            let mut index = 0;
            loop {
                let adapter = match factory.EnumAdapters1(index) {
                    Ok(adapter) => adapter,
                    Err(e) if e.code() == DXGI_ERROR_NOT_FOUND => break,
                    Err(e) => return Err(e.into()),
                };
                index += 1;
                let mut output_index = 0;
                loop {
                    let output = match adapter.EnumOutputs(output_index) {
                        Ok(output) => output,
                        Err(e) if e.code() == DXGI_ERROR_NOT_FOUND => break,
                        Err(e) => return Err(e.into()),
                    };
                    output_index += 1;
                    let desc = output.GetDesc()?;
                    let end = desc
                        .DeviceName
                        .iter()
                        .position(|c| *c == 0)
                        .unwrap_or(desc.DeviceName.len());
                    if String::from_utf16_lossy(&desc.DeviceName[..end]) != monitor.name {
                        continue;
                    }
                    let mut device = None;
                    let mut context = None;
                    D3D11CreateDevice(
                        &adapter,
                        D3D_DRIVER_TYPE_UNKNOWN,
                        HMODULE::default(),
                        D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                        None,
                        D3D11_SDK_VERSION,
                        Some(&mut device),
                        None,
                        Some(&mut context),
                    )?;
                    let device = device.context("D3D11 returned no device")?;
                    let duplication = output.cast::<IDXGIOutput1>()?.DuplicateOutput(&device)?;
                    let rotation = match duplication.GetDesc().Rotation {
                        DXGI_MODE_ROTATION_ROTATE90 => Rotation::Rotate90,
                        DXGI_MODE_ROTATION_ROTATE180 => Rotation::Rotate180,
                        DXGI_MODE_ROTATION_ROTATE270 => Rotation::Rotate270,
                        _ => Rotation::Identity,
                    };
                    return Ok(Self {
                        device,
                        context: context.context("D3D11 returned no context")?,
                        duplication,
                        rotation,
                        size: (0, 0),
                        latest: None,
                        image: None,
                        staging: None,
                        region_staging: None,
                        pointer: Pointer::default(),
                    });
                }
            }
        }
        bail!("no DXGI output for {}", monitor.name)
    }

    /// Whether a desktop image has arrived since the duplication was made.
    pub fn has_image(&self) -> bool {
        self.latest.is_some()
    }

    /// Desktop-oriented size of the newest image.
    pub fn desktop_size(&self) -> (u32, u32) {
        self.rotation.turn(self.size)
    }

    /// Whether DXGI draws the pointer separately (a hardware pointer on this
    /// output), so the desktop pixels do not contain it.
    pub fn separate_pointer(&self) -> bool {
        self.pointer.visible
    }

    /// Waits up to `wait` for the next update. Errors (ACCESS_LOST on mode
    /// changes and desktop switches, device removal) leave this duplication
    /// unusable: the owner drops it and makes a new one.
    pub fn acquire(&mut self, wait: Duration) -> Result<Update> {
        let mut info = DXGI_OUTDUPL_FRAME_INFO::default();
        let mut resource = None;
        let timeout = u32::try_from(wait.as_millis()).unwrap_or(u32::MAX);
        match unsafe {
            self.duplication
                .AcquireNextFrame(timeout, &mut info, &mut resource)
        } {
            Err(e) if e.code() == DXGI_ERROR_WAIT_TIMEOUT => return Ok(Update::default()),
            Err(e) => return Err(e.into()),
            Ok(()) => {}
        }
        let result = self.take(&info, resource);
        let released = unsafe { self.duplication.ReleaseFrame() };
        let update = result?;
        released?;
        Ok(update)
    }

    fn take(
        &mut self,
        info: &DXGI_OUTDUPL_FRAME_INFO,
        resource: Option<IDXGIResource>,
    ) -> Result<Update> {
        let mut update = Update::default();
        if info.LastMouseUpdateTime != 0 {
            let visible = info.PointerPosition.Visible.as_bool();
            let position = (
                info.PointerPosition.Position.x,
                info.PointerPosition.Position.y,
            );
            update.pointer =
                visible != self.pointer.visible || (visible && position != self.pointer.position);
            self.pointer.visible = visible;
            self.pointer.position = position;
        }
        if info.PointerShapeBufferSize > 0 {
            let mut data = vec![0u8; info.PointerShapeBufferSize as usize];
            let mut required = 0;
            let mut shape = DXGI_OUTDUPL_POINTER_SHAPE_INFO::default();
            unsafe {
                self.duplication.GetFramePointerShape(
                    data.len() as u32,
                    data.as_mut_ptr().cast(),
                    &mut required,
                    &mut shape,
                )?;
            }
            let kind = match DXGI_OUTDUPL_POINTER_SHAPE_TYPE(shape.Type as i32) {
                DXGI_OUTDUPL_POINTER_SHAPE_TYPE_MONOCHROME => Some(PointerKind::Monochrome),
                DXGI_OUTDUPL_POINTER_SHAPE_TYPE_COLOR => Some(PointerKind::Color),
                DXGI_OUTDUPL_POINTER_SHAPE_TYPE_MASKED_COLOR => Some(PointerKind::MaskedColor),
                _ => None,
            };
            self.pointer.shape = kind.map(|kind| PointerShape {
                kind,
                width: shape.Width,
                height: shape.Height,
                pitch: shape.Pitch as usize,
                data,
            });
            update.pointer = true;
        }
        // Pointer-only updates carry no desktop pixels. A new duplication may
        // deliver one before its first desktop image; that texture must not
        // be read as a black desktop.
        if info.LastPresentTime == 0 {
            return Ok(update);
        }
        let texture: ID3D11Texture2D = resource.context("DXGI returned no frame")?.cast()?;
        let mut desc = D3D11_TEXTURE2D_DESC::default();
        unsafe {
            texture.GetDesc(&mut desc);
        }
        if desc.Format != DXGI_FORMAT_B8G8R8A8_UNORM {
            bail!("unsupported duplication format {:?}", desc.Format);
        }
        if self.latest.is_none() || self.size != (desc.Width, desc.Height) {
            desc.Usage = D3D11_USAGE_DEFAULT;
            desc.BindFlags = 0;
            desc.CPUAccessFlags = 0;
            desc.MiscFlags = 0;
            desc.MipLevels = 1;
            desc.ArraySize = 1;
            let mut latest = None;
            unsafe {
                self.device
                    .CreateTexture2D(&desc, None, Some(&mut latest))?;
            }
            self.latest = latest;
            self.staging = None;
            self.region_staging = None;
            self.size = (desc.Width, desc.Height);
        }
        let latest = self.latest.as_ref().context("missing desktop copy")?;
        unsafe {
            self.context.CopyResource(latest, &texture);
        }
        self.image = None;
        update.desktop = true;
        Ok(update)
    }

    /// The newest desktop image of the whole output, read back once per
    /// desktop update.
    pub fn image(&mut self) -> Result<Option<Arc<Image>>> {
        if let Some(image) = &self.image {
            return Ok(Some(image.clone()));
        }
        let Some(latest) = &self.latest else {
            return Ok(None);
        };
        if self.staging.is_none() {
            self.staging = Some(self.create_staging(self.size)?);
        }
        let staging = self.staging.as_ref().context("missing staging texture")?;
        unsafe {
            self.context.CopyResource(staging, latest);
        }
        let (width, height) = self.size;
        let rotation = self.rotation;
        let image = Arc::new(self.read(staging, height, |bytes, pitch| {
            pixels::to_image(bytes, pitch, width, height, rotation)
        })?);
        self.image = Some(image.clone());
        Ok(Some(image))
    }

    /// Writes `region` (desktop orientation, relative to the output) of the
    /// newest image into `dst` as packed opaque BGRX rows, the pointer drawn
    /// in when `cursor` is set and DXGI draws it separately.
    pub fn read_region(&mut self, region: IRect, cursor: bool, dst: &mut [u8]) -> Result<()> {
        let latest = self.latest.clone().context("no desktop image yet")?;
        let rect = self.rotation.texture_rect(region, self.desktop_size());
        let size = (rect.w as u32, rect.h as u32);
        if self
            .region_staging
            .as_ref()
            .is_none_or(|(staged, _)| *staged != size)
        {
            self.region_staging = Some((size, self.create_staging(size)?));
        }
        let (_, staging) = self.region_staging.as_ref().context("missing staging")?;
        let source = D3D11_BOX {
            left: rect.x as u32,
            top: rect.y as u32,
            front: 0,
            right: rect.right() as u32,
            bottom: rect.bottom() as u32,
            back: 1,
        };
        unsafe {
            self.context
                .CopySubresourceRegion(staging, 0, 0, 0, 0, &latest, 0, Some(&source));
        }
        let rotation = self.rotation;
        self.read(staging, size.1, |bytes, pitch| {
            pixels::rotate_into(bytes, pitch, size.0, size.1, rotation, dst)
        })?;
        if cursor
            && self.pointer.visible
            && let Some(shape) = &self.pointer.shape
        {
            pixels::draw_pointer(
                dst,
                region.w as u32,
                region.h as u32,
                region.w as usize * 4,
                shape,
                self.pointer.position.0 - region.x,
                self.pointer.position.1 - region.y,
            );
        }
        Ok(())
    }

    fn create_staging(&self, (width, height): (u32, u32)) -> Result<ID3D11Texture2D> {
        let desc = D3D11_TEXTURE2D_DESC {
            Width: width,
            Height: height,
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_B8G8R8A8_UNORM,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Usage: D3D11_USAGE_STAGING,
            BindFlags: 0,
            CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
            MiscFlags: 0,
        };
        let mut staging = None;
        unsafe {
            self.device
                .CreateTexture2D(&desc, None, Some(&mut staging))?;
        }
        staging.context("D3D11 returned no staging texture")
    }

    /// Maps `staging` (`rows` rows) for reading while `read` runs.
    fn read<T>(
        &self,
        staging: &ID3D11Texture2D,
        rows: u32,
        read: impl FnOnce(&[u8], usize) -> T,
    ) -> Result<T> {
        let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
        unsafe {
            self.context
                .Map(staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))?;
        }
        let pitch = mapped.RowPitch as usize;
        let bytes =
            unsafe { std::slice::from_raw_parts(mapped.pData as *const u8, pitch * rows as usize) };
        let result = read(bytes, pitch);
        unsafe {
            self.context.Unmap(staging, 0);
        }
        Ok(result)
    }
}
