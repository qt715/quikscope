//! Screencopy DMA-BUFs: GBM allocations the compositor renders into, and
//! their readback to CPU memory.
//!
//! Readback goes through Vulkan: each buffer is imported as an image with
//! its exact DRM modifier and plane layout, and the GPU copies it into a
//! host-cached staging buffer. Reading a (linear) DMA-BUF mapping with the
//! CPU instead is only a fallback for machines without a usable Vulkan
//! driver: those mappings are uncached or write-combined VRAM on discrete
//! GPUs, and reading 15 MB per frame through them takes tens of
//! milliseconds.

use std::ffi::CStr;
use std::fs::File;
use std::os::fd::{AsFd as _, AsRawFd as _, BorrowedFd, FromRawFd as _, IntoRawFd as _, OwnedFd};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context as _, Result, bail, ensure};
use ash::vk;
use memmap2::Mmap;

use crate::media::PixelFormat;

const fn fourcc(code: &[u8; 4]) -> u32 {
    u32::from_le_bytes(*code)
}

pub(super) const XRGB8888: u32 = fourcc(b"XR24");
pub(super) const ARGB8888: u32 = fourcc(b"AR24");
pub(super) const XBGR8888: u32 = fourcc(b"XB24");
pub(super) const ABGR8888: u32 = fourcc(b"AB24");

pub(super) const MOD_LINEAR: u64 = 0;
/// `DRM_FORMAT_MOD_INVALID`: "implicit modifier" entries in format tables.
pub(super) const MOD_INVALID: u64 = (1 << 56) - 1;

/// How long the compositor's GPU may still be writing a frame it reported
/// ready, and how long our own copy may take, before the capture fails.
const GPU_TIMEOUT: Duration = Duration::from_secs(2);

/// Byte order in memory of a DRM fourcc (wl_shm uses the same codes, apart
/// from its own 0 and 1 for ARGB8888/XRGB8888). DRM names list channels from
/// the most significant bit of a little-endian word, so XRGB8888 is stored
/// B, G, R, X. An output has no transparency, so alpha variants are tagged
/// opaque: whatever the compositor leaves in that byte is not image content.
pub(super) fn pixel_format(fourcc: u32) -> Option<PixelFormat> {
    match fourcc {
        XRGB8888 | ARGB8888 => Some(PixelFormat::Bgrx),
        XBGR8888 | ABGR8888 => Some(PixelFormat::Rgbx),
        _ => None,
    }
}

/// The Vulkan format whose texel bytes match `fourcc`'s memory order.
fn vk_format(fourcc: u32) -> Option<vk::Format> {
    match fourcc {
        XRGB8888 | ARGB8888 => Some(vk::Format::B8G8R8A8_UNORM),
        XBGR8888 | ABGR8888 => Some(vk::Format::R8G8B8A8_UNORM),
        _ => None,
    }
}

/// `XR24`-style text for logs and errors.
pub(super) fn fourcc_name(code: u32) -> String {
    let bytes = code.to_le_bytes();
    if bytes.iter().all(u8::is_ascii_graphic) {
        String::from_utf8_lossy(&bytes).into_owned()
    } else {
        format!("{code:#x}")
    }
}

#[link(name = "gbm")]
unsafe extern "C" {
    fn gbm_create_device(fd: i32) -> *mut libc::c_void;
    fn gbm_device_destroy(device: *mut libc::c_void);
    fn gbm_bo_create_with_modifiers2(
        device: *mut libc::c_void,
        width: u32,
        height: u32,
        format: u32,
        modifiers: *const u64,
        count: u32,
        flags: u32,
    ) -> *mut libc::c_void;
    fn gbm_bo_create(
        device: *mut libc::c_void,
        width: u32,
        height: u32,
        format: u32,
        flags: u32,
    ) -> *mut libc::c_void;
    fn gbm_bo_destroy(bo: *mut libc::c_void);
    fn gbm_bo_get_fd(bo: *mut libc::c_void) -> i32;
    fn gbm_bo_get_plane_count(bo: *mut libc::c_void) -> i32;
    fn gbm_bo_get_stride(bo: *mut libc::c_void) -> u32;
    fn gbm_bo_get_offset(bo: *mut libc::c_void, plane: i32) -> u32;
    fn gbm_bo_get_modifier(bo: *mut libc::c_void) -> u64;
}

const GBM_BO_USE_RENDERING: u32 = 1 << 2;
const GBM_BO_USE_LINEAR: u32 = 1 << 4;

/// A GBM device on the compositor's render node.
pub(super) struct Gbm {
    ptr: *mut libc::c_void,
    _file: File,
    pub node: String,
    /// `st_rdev` of the render node, to find the same GPU in Vulkan.
    pub rdev: u64,
}

// SAFETY: GBM devices are only used to create and destroy buffers, which
// libgbm serializes internally; the pointer has no thread affinity.
unsafe impl Send for Gbm {}
unsafe impl Sync for Gbm {}

impl Drop for Gbm {
    fn drop(&mut self) {
        unsafe { gbm_device_destroy(self.ptr) };
    }
}

impl Gbm {
    /// Opens the render node of `main_device` (the dev_t from DMA-BUF
    /// feedback, often a primary node), or the first render node when the
    /// compositor sent no feedback.
    pub fn open(main_device: Option<u64>) -> Result<Arc<Gbm>> {
        use std::os::unix::fs::MetadataExt as _;
        let mut node = "/dev/dri/renderD128".to_string();
        if let Some(dev) = main_device
            && let Ok(entries) = std::fs::read_dir(format!(
                "/sys/dev/char/{}:{}/device/drm",
                libc::major(dev),
                libc::minor(dev)
            ))
        {
            for entry in entries.flatten() {
                let name = entry.file_name();
                if name.as_encoded_bytes().starts_with(b"renderD") {
                    node = format!("/dev/dri/{}", name.to_string_lossy());
                    break;
                }
            }
        }
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&node)
            .with_context(|| format!("opening compositor render node {node}"))?;
        let rdev = file.metadata()?.rdev();
        let ptr = unsafe { gbm_create_device(file.as_raw_fd()) };
        ensure!(!ptr.is_null(), "creating a GBM device on {node}");
        Ok(Arc::new(Gbm {
            ptr,
            _file: file,
            node,
            rdev,
        }))
    }

    /// A single-plane buffer with one of `modifiers`, or a linear one when
    /// `modifiers` is empty.
    pub fn allocate(
        self: &Arc<Self>,
        width: u32,
        height: u32,
        format: u32,
        modifiers: &[u64],
    ) -> Result<Bo> {
        let bo = unsafe {
            if modifiers.is_empty() {
                gbm_bo_create(
                    self.ptr,
                    width,
                    height,
                    format,
                    GBM_BO_USE_RENDERING | GBM_BO_USE_LINEAR,
                )
            } else {
                gbm_bo_create_with_modifiers2(
                    self.ptr,
                    width,
                    height,
                    format,
                    modifiers.as_ptr(),
                    modifiers.len() as u32,
                    GBM_BO_USE_RENDERING,
                )
            }
        };
        ensure!(
            !bo.is_null(),
            "GBM cannot allocate a {width}x{height} {} buffer: {}",
            fourcc_name(format),
            std::io::Error::last_os_error()
        );
        // Owned from here on, so every error below frees it.
        let mut out = Bo {
            bo,
            fd: None,
            stride: unsafe { gbm_bo_get_stride(bo) },
            offset: unsafe { gbm_bo_get_offset(bo, 0) },
            modifier: unsafe { gbm_bo_get_modifier(bo) },
            _device: self.clone(),
        };
        ensure!(
            unsafe { gbm_bo_get_plane_count(bo) } == 1,
            "GBM allocated a multi-plane buffer for modifier {:#x}",
            out.modifier
        );
        if modifiers.is_empty() {
            out.modifier = MOD_LINEAR;
        } else {
            ensure!(
                modifiers.contains(&out.modifier),
                "GBM chose modifier {:#x}, which was not requested",
                out.modifier
            );
        }
        let fd = unsafe { gbm_bo_get_fd(bo) };
        ensure!(
            fd >= 0,
            "exporting a GBM buffer: {}",
            std::io::Error::last_os_error()
        );
        out.fd = Some(unsafe { OwnedFd::from_raw_fd(fd) });
        Ok(out)
    }
}

/// One GBM buffer object and its DMA-BUF.
pub(super) struct Bo {
    bo: *mut libc::c_void,
    fd: Option<OwnedFd>,
    pub stride: u32,
    pub offset: u32,
    pub modifier: u64,
    _device: Arc<Gbm>,
}

// SAFETY: the buffer object is immutable after creation; only its pixels
// change, written by the compositor and read by whoever holds the buffer.
unsafe impl Send for Bo {}
unsafe impl Sync for Bo {}

impl Drop for Bo {
    fn drop(&mut self) {
        self.fd = None;
        unsafe { gbm_bo_destroy(self.bo) };
    }
}

impl Bo {
    pub fn fd(&self) -> BorrowedFd<'_> {
        self.fd.as_ref().expect("exported in allocate").as_fd()
    }
}

/// Waits until every GPU write the compositor queued into `dmabuf` is done
/// (implicit sync: a DMA-BUF polls readable once its write fences signal).
fn wait_written(dmabuf: BorrowedFd<'_>) -> Result<()> {
    use rustix::event::{PollFd, PollFlags, Timespec, poll};
    let timeout = Timespec::try_from(GPU_TIMEOUT)?;
    loop {
        let mut fds = [PollFd::new(&dmabuf, PollFlags::IN)];
        match poll(&mut fds, Some(&timeout)) {
            Ok(0) => bail!("the compositor did not finish rendering a screencopy"),
            Ok(_) => return Ok(()),
            Err(rustix::io::Errno::INTR) => {}
            Err(e) => return Err(e).context("waiting for a screencopy"),
        }
    }
}

/// A linear DMA-BUF read through a CPU mapping (fallback without Vulkan).
pub(super) struct Mapped {
    map: Mmap,
    file: File,
}

impl Mapped {
    pub fn new(bo: &Bo) -> Result<Mapped> {
        let file = File::from(bo.fd().try_clone_to_owned()?);
        let map = unsafe { Mmap::map(&file) }.context("mapping a linear DMA-BUF")?;
        Ok(Mapped { map, file })
    }

    /// Calls `f` with the pixels from `offset` on, bracketed by DMA-BUF
    /// CPU-access syncs (which also wait for the compositor's writes).
    pub fn read<R>(&self, offset: usize, f: impl FnOnce(&[u8]) -> R) -> Result<R> {
        const SYNC_READ: u64 = 1;
        const SYNC_START: u64 = 0;
        const SYNC_END: u64 = 4;
        // _IOW('b', 0, struct dma_buf_sync { __u64 flags; })
        const DMA_BUF_IOCTL_SYNC: u64 = 0x4008_6200;
        let sync = |flags: u64| -> Result<()> {
            let rc = unsafe {
                libc::ioctl(
                    self.file.as_raw_fd(),
                    DMA_BUF_IOCTL_SYNC as _,
                    &flags as *const u64,
                )
            };
            ensure!(rc == 0, "DMA-BUF sync: {}", std::io::Error::last_os_error());
            Ok(())
        };
        sync(SYNC_START | SYNC_READ)?;
        let result = f(self.map.get(offset..).context("short DMA-BUF")?);
        sync(SYNC_END | SYNC_READ)?;
        Ok(result)
    }
}

/// A Vulkan device on the compositor's GPU that can import its DMA-BUFs.
pub(super) struct Vulkan {
    _entry: ash::Entry,
    instance: ash::Instance,
    physical: vk::PhysicalDevice,
    device: ash::Device,
    memory_fd: ash::khr::external_memory_fd::Device,
    queue: vk::Queue,
    family: u32,
    /// Queue family that owns the images while the compositor writes them.
    foreign: u32,
    pool: vk::CommandPool,
    memory: vk::PhysicalDeviceMemoryProperties,
    pub description: String,
}

// SAFETY: Vulkan handles have no thread affinity. The queue and command
// buffers are only used by the readback thread once set up, and every
// other call here is thread-safe per the Vulkan spec.
unsafe impl Send for Vulkan {}
unsafe impl Sync for Vulkan {}

impl Drop for Vulkan {
    fn drop(&mut self) {
        unsafe {
            let _ = self.device.device_wait_idle();
            self.device.destroy_command_pool(self.pool, None);
            self.device.destroy_device(None);
            self.instance.destroy_instance(None);
        }
    }
}

impl Vulkan {
    /// The Vulkan device whose DRM render node is `rdev`.
    pub fn new(rdev: u64) -> Result<Arc<Vulkan>> {
        let entry = unsafe { ash::Entry::load() }.context("loading the Vulkan loader")?;
        let app = vk::ApplicationInfo::default()
            .application_name(c"quikscope")
            .api_version(vk::API_VERSION_1_1);
        let instance = unsafe {
            entry.create_instance(
                &vk::InstanceCreateInfo::default().application_info(&app),
                None,
            )
        }
        .context("creating a Vulkan instance")?;
        let (physical, device, family, foreign, description) =
            match unsafe { Self::open_device(&instance, rdev) } {
                Ok(found) => found,
                Err(e) => {
                    unsafe { instance.destroy_instance(None) };
                    return Err(e);
                }
            };
        let pool = match unsafe {
            device.create_command_pool(
                &vk::CommandPoolCreateInfo::default().queue_family_index(family),
                None,
            )
        } {
            Ok(pool) => pool,
            Err(e) => {
                unsafe {
                    device.destroy_device(None);
                    instance.destroy_instance(None);
                }
                return Err(e.into());
            }
        };
        Ok(Arc::new(Vulkan {
            memory_fd: ash::khr::external_memory_fd::Device::new(&instance, &device),
            queue: unsafe { device.get_device_queue(family, 0) },
            memory: unsafe { instance.get_physical_device_memory_properties(physical) },
            _entry: entry,
            instance,
            physical,
            device,
            family,
            foreign,
            pool,
            description,
        }))
    }

    unsafe fn open_device(
        instance: &ash::Instance,
        rdev: u64,
    ) -> Result<(vk::PhysicalDevice, ash::Device, u32, u32, String)> {
        let (major, minor) = (i64::from(libc::major(rdev)), i64::from(libc::minor(rdev)));
        for physical in unsafe { instance.enumerate_physical_devices() }? {
            let properties = unsafe { instance.get_physical_device_properties(physical) };
            let name = properties
                .device_name_as_c_str()
                .map_or_else(|_| "?".into(), |n| n.to_string_lossy().into_owned());
            if properties.api_version < vk::API_VERSION_1_1 {
                continue;
            }
            let extensions: Vec<_> =
                unsafe { instance.enumerate_device_extension_properties(physical) }?
                    .iter()
                    .filter_map(|e| e.extension_name_as_c_str().ok().map(CStr::to_owned))
                    .collect();
            let has = |name: &CStr| extensions.iter().any(|e| e.as_c_str() == name);
            if !has(ash::ext::physical_device_drm::NAME) {
                continue;
            }
            let mut drm = vk::PhysicalDeviceDrmPropertiesEXT::default();
            let mut properties2 = vk::PhysicalDeviceProperties2::default().push_next(&mut drm);
            unsafe { instance.get_physical_device_properties2(physical, &mut properties2) };
            let render = drm.has_render == vk::TRUE
                && (drm.render_major, drm.render_minor) == (major, minor);
            let primary = drm.has_primary == vk::TRUE
                && (drm.primary_major, drm.primary_minor) == (major, minor);
            if !render && !primary {
                continue;
            }
            let mut enable = vec![
                ash::khr::external_memory_fd::NAME,
                ash::ext::external_memory_dma_buf::NAME,
                ash::ext::image_drm_format_modifier::NAME,
                ash::khr::image_format_list::NAME,
            ];
            if let Some(missing) = enable.iter().find(|e| !has(e)) {
                bail!(
                    "the Vulkan driver of {name} cannot import DMA-BUFs (no {})",
                    missing.to_string_lossy()
                );
            }
            let foreign = if has(ash::ext::queue_family_foreign::NAME) {
                enable.push(ash::ext::queue_family_foreign::NAME);
                vk::QUEUE_FAMILY_FOREIGN_EXT
            } else {
                vk::QUEUE_FAMILY_EXTERNAL
            };
            // A transfer-only family runs on the copy engines, next to
            // rather than inside the graphics work of a game being recorded.
            let families =
                unsafe { instance.get_physical_device_queue_family_properties(physical) };
            let usable = |f: &vk::QueueFamilyProperties| f.queue_count > 0;
            let family = families
                .iter()
                .position(|f| {
                    usable(f)
                        && f.queue_flags.contains(vk::QueueFlags::TRANSFER)
                        && !f
                            .queue_flags
                            .intersects(vk::QueueFlags::GRAPHICS | vk::QueueFlags::COMPUTE)
                })
                .or_else(|| {
                    families.iter().position(|f| {
                        usable(f)
                            && f.queue_flags
                                .intersects(vk::QueueFlags::GRAPHICS | vk::QueueFlags::COMPUTE)
                    })
                })
                .context("the Vulkan device has no queue that can copy")?
                as u32;
            let names: Vec<_> = enable.iter().map(|e| e.as_ptr()).collect();
            let priorities = [1.0f32];
            let queues = [vk::DeviceQueueCreateInfo::default()
                .queue_family_index(family)
                .queue_priorities(&priorities)];
            let device = unsafe {
                instance.create_device(
                    physical,
                    &vk::DeviceCreateInfo::default()
                        .queue_create_infos(&queues)
                        .enabled_extension_names(&names),
                    None,
                )
            }
            .with_context(|| format!("creating a Vulkan device on {name}"))?;
            let kind = if families[family as usize]
                .queue_flags
                .intersects(vk::QueueFlags::GRAPHICS | vk::QueueFlags::COMPUTE)
            {
                "general queue"
            } else {
                "transfer queue"
            };
            return Ok((physical, device, family, foreign, format!("{name}, {kind}")));
        }
        bail!("no Vulkan device drives render node {major}:{minor}")
    }

    /// The `offered` modifiers of `fourcc` this device can import as a
    /// single-plane image and copy from. Multi-plane (compressed) modifiers
    /// are left out: the compositor then renders uncompressed, and one
    /// import path stays enough.
    pub fn importable(&self, fourcc: u32, offered: &[u64]) -> Vec<u64> {
        let Some(format) = vk_format(fourcc) else {
            return Vec::new();
        };
        let mut list = vk::DrmFormatModifierPropertiesListEXT::default();
        unsafe {
            self.instance.get_physical_device_format_properties2(
                self.physical,
                format,
                &mut vk::FormatProperties2::default().push_next(&mut list),
            );
        }
        let mut entries = vec![
            vk::DrmFormatModifierPropertiesEXT::default();
            list.drm_format_modifier_count as usize
        ];
        let mut list = vk::DrmFormatModifierPropertiesListEXT::default()
            .drm_format_modifier_properties(&mut entries);
        unsafe {
            self.instance.get_physical_device_format_properties2(
                self.physical,
                format,
                &mut vk::FormatProperties2::default().push_next(&mut list),
            );
        }
        let count = list.drm_format_modifier_count as usize;
        entries
            .iter()
            .take(count)
            .filter(|e| {
                e.drm_format_modifier_plane_count == 1
                    && e.drm_format_modifier_tiling_features
                        .contains(vk::FormatFeatureFlags::TRANSFER_SRC)
                    && offered.contains(&e.drm_format_modifier)
                    && self.can_import(format, e.drm_format_modifier)
            })
            .map(|e| e.drm_format_modifier)
            .collect()
    }

    fn can_import(&self, format: vk::Format, modifier: u64) -> bool {
        let mut drm = vk::PhysicalDeviceImageDrmFormatModifierInfoEXT::default()
            .drm_format_modifier(modifier)
            .sharing_mode(vk::SharingMode::EXCLUSIVE);
        let mut external = vk::PhysicalDeviceExternalImageFormatInfo::default()
            .handle_type(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT);
        let info = vk::PhysicalDeviceImageFormatInfo2::default()
            .format(format)
            .ty(vk::ImageType::TYPE_2D)
            .tiling(vk::ImageTiling::DRM_FORMAT_MODIFIER_EXT)
            .usage(vk::ImageUsageFlags::TRANSFER_SRC)
            .push_next(&mut external)
            .push_next(&mut drm);
        let mut external_properties = vk::ExternalImageFormatProperties::default();
        let mut properties =
            vk::ImageFormatProperties2::default().push_next(&mut external_properties);
        unsafe {
            self.instance.get_physical_device_image_format_properties2(
                self.physical,
                &info,
                &mut properties,
            )
        }
        .is_ok()
            && external_properties
                .external_memory_properties
                .external_memory_features
                .contains(vk::ExternalMemoryFeatureFlags::IMPORTABLE)
    }
}

/// The memory type for staging buffers among `bits`: host-visible, cached
/// if any is (uncached host memory is as slow to read as VRAM mappings),
/// coherent if possible. Returns the index and whether it is coherent.
fn staging_memory(types: &[vk::MemoryPropertyFlags], bits: u32) -> Option<(u32, bool)> {
    use vk::MemoryPropertyFlags as F;
    let preferences = [
        F::HOST_VISIBLE | F::HOST_CACHED | F::HOST_COHERENT,
        F::HOST_VISIBLE | F::HOST_CACHED,
        F::HOST_VISIBLE | F::HOST_COHERENT,
        F::HOST_VISIBLE,
    ];
    preferences.iter().find_map(|&wanted| {
        types
            .iter()
            .enumerate()
            .find(|&(i, flags)| bits & (1 << i) != 0 && flags.contains(wanted))
            .map(|(i, flags)| (i as u32, flags.contains(F::HOST_COHERENT)))
    })
}

/// One imported screencopy buffer, its staging buffer and the recorded
/// command buffer that copies the first into the second.
pub(super) struct VkCopy {
    vk: Arc<Vulkan>,
    image: vk::Image,
    image_memory: vk::DeviceMemory,
    buffer: vk::Buffer,
    buffer_memory: vk::DeviceMemory,
    mapped: *const u8,
    len: usize,
    coherent: bool,
    commands: vk::CommandBuffer,
    fence: vk::Fence,
}

// SAFETY: one thread at a time owns a buffer (it is handed between the
// Wayland and readback threads through channels); the mapping stays valid
// until drop.
unsafe impl Send for VkCopy {}
unsafe impl Sync for VkCopy {}

impl Drop for VkCopy {
    fn drop(&mut self) {
        let d = &self.vk.device;
        unsafe {
            // Nothing is pending once the readback thread has finished, but
            // an error path may leave one submission running.
            if self.fence != vk::Fence::null() {
                let _ = d.wait_for_fences(&[self.fence], true, GPU_TIMEOUT.as_nanos() as u64);
            }
            d.destroy_fence(self.fence, None);
            d.free_command_buffers(self.vk.pool, &[self.commands]);
            d.destroy_buffer(self.buffer, None);
            d.free_memory(self.buffer_memory, None);
            d.destroy_image(self.image, None);
            d.free_memory(self.image_memory, None);
        }
    }
}

impl VkCopy {
    pub fn new(vk: &Arc<Vulkan>, bo: &Bo, width: u32, height: u32, fourcc: u32) -> Result<VkCopy> {
        let format = vk_format(fourcc).context("unsupported DMA-BUF format")?;
        let d = &vk.device;
        // Every handle starts null; Drop tolerates null handles, so any
        // error below releases exactly what was created.
        let mut copy = VkCopy {
            vk: vk.clone(),
            image: vk::Image::null(),
            image_memory: vk::DeviceMemory::null(),
            buffer: vk::Buffer::null(),
            buffer_memory: vk::DeviceMemory::null(),
            mapped: std::ptr::null(),
            len: width as usize * height as usize * 4,
            coherent: true,
            commands: vk::CommandBuffer::null(),
            fence: vk::Fence::null(),
        };
        unsafe {
            // Signaled until the first submission, so Drop never waits on it.
            copy.fence = d.create_fence(
                &vk::FenceCreateInfo::default().flags(vk::FenceCreateFlags::SIGNALED),
                None,
            )?;
            let planes = [vk::SubresourceLayout {
                offset: u64::from(bo.offset),
                size: 0,
                row_pitch: u64::from(bo.stride),
                array_pitch: 0,
                depth_pitch: 0,
            }];
            let mut explicit = vk::ImageDrmFormatModifierExplicitCreateInfoEXT::default()
                .drm_format_modifier(bo.modifier)
                .plane_layouts(&planes);
            let mut external = vk::ExternalMemoryImageCreateInfo::default()
                .handle_types(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT);
            copy.image = d
                .create_image(
                    &vk::ImageCreateInfo::default()
                        .image_type(vk::ImageType::TYPE_2D)
                        .format(format)
                        .extent(vk::Extent3D {
                            width,
                            height,
                            depth: 1,
                        })
                        .mip_levels(1)
                        .array_layers(1)
                        .samples(vk::SampleCountFlags::TYPE_1)
                        .tiling(vk::ImageTiling::DRM_FORMAT_MODIFIER_EXT)
                        .usage(vk::ImageUsageFlags::TRANSFER_SRC)
                        .sharing_mode(vk::SharingMode::EXCLUSIVE)
                        .initial_layout(vk::ImageLayout::UNDEFINED)
                        .push_next(&mut external)
                        .push_next(&mut explicit),
                    None,
                )
                .context("importing a screencopy DMA-BUF into Vulkan")?;
            let requirements = d.get_image_memory_requirements(copy.image);
            let fd = bo.fd().try_clone_to_owned()?;
            let mut fd_properties = vk::MemoryFdPropertiesKHR::default();
            vk.memory_fd.get_memory_fd_properties(
                vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT,
                fd.as_raw_fd(),
                &mut fd_properties,
            )?;
            let bits = requirements.memory_type_bits & fd_properties.memory_type_bits;
            ensure!(bits != 0, "no Vulkan memory type can hold the DMA-BUF");
            let mut import = vk::ImportMemoryFdInfoKHR::default()
                .handle_type(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT)
                .fd(fd.as_raw_fd());
            let mut dedicated = vk::MemoryDedicatedAllocateInfo::default().image(copy.image);
            copy.image_memory = d
                .allocate_memory(
                    &vk::MemoryAllocateInfo::default()
                        .allocation_size(requirements.size)
                        .memory_type_index(bits.trailing_zeros())
                        .push_next(&mut import)
                        .push_next(&mut dedicated),
                    None,
                )
                .context("importing DMA-BUF memory into Vulkan")?;
            // A successful import owns the descriptor.
            let _ = fd.into_raw_fd();
            d.bind_image_memory(copy.image, copy.image_memory, 0)?;

            copy.buffer = d.create_buffer(
                &vk::BufferCreateInfo::default()
                    .size(copy.len as u64)
                    .usage(vk::BufferUsageFlags::TRANSFER_DST)
                    .sharing_mode(vk::SharingMode::EXCLUSIVE),
                None,
            )?;
            let requirements = d.get_buffer_memory_requirements(copy.buffer);
            let types: Vec<_> = vk.memory.memory_types[..vk.memory.memory_type_count as usize]
                .iter()
                .map(|t| t.property_flags)
                .collect();
            let (index, coherent) = staging_memory(&types, requirements.memory_type_bits)
                .context("no host-visible Vulkan memory for frame readback")?;
            copy.coherent = coherent;
            copy.buffer_memory = d.allocate_memory(
                &vk::MemoryAllocateInfo::default()
                    .allocation_size(requirements.size)
                    .memory_type_index(index),
                None,
            )?;
            d.bind_buffer_memory(copy.buffer, copy.buffer_memory, 0)?;
            copy.mapped = d
                .map_memory(
                    copy.buffer_memory,
                    0,
                    vk::WHOLE_SIZE,
                    vk::MemoryMapFlags::empty(),
                )?
                .cast();

            copy.commands = d.allocate_command_buffers(
                &vk::CommandBufferAllocateInfo::default()
                    .command_pool(vk.pool)
                    .level(vk::CommandBufferLevel::PRIMARY)
                    .command_buffer_count(1),
            )?[0];
            copy.record(width, height)?;
        }
        Ok(copy)
    }

    /// Records the reusable copy: take the image from the compositor's
    /// (foreign) queue family, copy it, give it back, make the bytes
    /// visible to the host. The image stays in GENERAL, the layout DMA-BUF
    /// sharing agrees on, so no transition can touch its contents.
    unsafe fn record(&self, width: u32, height: u32) -> Result<()> {
        let d = &self.vk.device;
        let range = vk::ImageSubresourceRange {
            aspect_mask: vk::ImageAspectFlags::COLOR,
            base_mip_level: 0,
            level_count: 1,
            base_array_layer: 0,
            layer_count: 1,
        };
        let image_barrier = |from: u32, to: u32, src: vk::AccessFlags, dst: vk::AccessFlags| {
            vk::ImageMemoryBarrier::default()
                .src_access_mask(src)
                .dst_access_mask(dst)
                .old_layout(vk::ImageLayout::GENERAL)
                .new_layout(vk::ImageLayout::GENERAL)
                .src_queue_family_index(from)
                .dst_queue_family_index(to)
                .image(self.image)
                .subresource_range(range)
        };
        unsafe {
            d.begin_command_buffer(self.commands, &vk::CommandBufferBeginInfo::default())?;
            d.cmd_pipeline_barrier(
                self.commands,
                vk::PipelineStageFlags::TOP_OF_PIPE,
                vk::PipelineStageFlags::TRANSFER,
                vk::DependencyFlags::empty(),
                &[],
                &[],
                &[image_barrier(
                    self.vk.foreign,
                    self.vk.family,
                    vk::AccessFlags::empty(),
                    vk::AccessFlags::TRANSFER_READ,
                )],
            );
            d.cmd_copy_image_to_buffer(
                self.commands,
                self.image,
                vk::ImageLayout::GENERAL,
                self.buffer,
                &[vk::BufferImageCopy {
                    buffer_offset: 0,
                    buffer_row_length: 0,
                    buffer_image_height: 0,
                    image_subresource: vk::ImageSubresourceLayers {
                        aspect_mask: vk::ImageAspectFlags::COLOR,
                        mip_level: 0,
                        base_array_layer: 0,
                        layer_count: 1,
                    },
                    image_offset: vk::Offset3D::default(),
                    image_extent: vk::Extent3D {
                        width,
                        height,
                        depth: 1,
                    },
                }],
            );
            d.cmd_pipeline_barrier(
                self.commands,
                vk::PipelineStageFlags::TRANSFER,
                vk::PipelineStageFlags::HOST | vk::PipelineStageFlags::BOTTOM_OF_PIPE,
                vk::DependencyFlags::empty(),
                &[],
                &[vk::BufferMemoryBarrier::default()
                    .src_access_mask(vk::AccessFlags::TRANSFER_WRITE)
                    .dst_access_mask(vk::AccessFlags::HOST_READ)
                    .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                    .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                    .buffer(self.buffer)
                    .offset(0)
                    .size(vk::WHOLE_SIZE)],
                &[image_barrier(
                    self.vk.family,
                    self.vk.foreign,
                    vk::AccessFlags::TRANSFER_READ,
                    vk::AccessFlags::empty(),
                )],
            );
            d.end_command_buffer(self.commands)?;
        }
        Ok(())
    }

    /// Copies the buffer's current pixels (tightly packed rows) to host
    /// memory and calls `f` with them. Only one thread may call this at a
    /// time per buffer.
    pub fn read<R>(&self, dmabuf: BorrowedFd<'_>, f: impl FnOnce(&[u8]) -> R) -> Result<R> {
        wait_written(dmabuf)?;
        let d = &self.vk.device;
        unsafe {
            d.reset_fences(&[self.fence])?;
            d.queue_submit(
                self.vk.queue,
                &[vk::SubmitInfo::default().command_buffers(std::slice::from_ref(&self.commands))],
                self.fence,
            )
            .context("submitting a frame readback")?;
            match d.wait_for_fences(&[self.fence], true, GPU_TIMEOUT.as_nanos() as u64) {
                Ok(()) => {}
                Err(vk::Result::TIMEOUT) => bail!("the GPU did not finish a frame readback"),
                Err(e) => return Err(e).context("frame readback"),
            }
            if !self.coherent {
                d.invalidate_mapped_memory_ranges(&[vk::MappedMemoryRange::default()
                    .memory(self.buffer_memory)
                    .offset(0)
                    .size(vk::WHOLE_SIZE)])?;
            }
            Ok(f(std::slice::from_raw_parts(self.mapped, self.len)))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fourcc_byte_order() {
        // Little-endian XRGB8888 is stored B, G, R, X; XBGR8888 R, G, B, X.
        assert_eq!(fourcc_name(XRGB8888), "XR24");
        assert_eq!(pixel_format(XRGB8888), Some(PixelFormat::Bgrx));
        assert_eq!(pixel_format(ARGB8888), Some(PixelFormat::Bgrx));
        assert_eq!(pixel_format(XBGR8888), Some(PixelFormat::Rgbx));
        assert_eq!(pixel_format(ABGR8888), Some(PixelFormat::Rgbx));
        assert_eq!(vk_format(XRGB8888), Some(vk::Format::B8G8R8A8_UNORM));
        assert_eq!(vk_format(XBGR8888), Some(vk::Format::R8G8B8A8_UNORM));
        // 10-bit formats are not 4-byte BGRA/RGBA.
        assert_eq!(pixel_format(fourcc(b"XR30")), None);
        assert_eq!(vk_format(fourcc(b"XR30")), None);
        assert_eq!(fourcc_name(1), "0x1");
    }

    #[test]
    fn staging_prefers_cached_host_memory() {
        use vk::MemoryPropertyFlags as F;
        let types = [
            F::DEVICE_LOCAL,
            F::HOST_VISIBLE | F::HOST_COHERENT,
            F::HOST_VISIBLE | F::HOST_CACHED,
            F::HOST_VISIBLE | F::HOST_COHERENT | F::HOST_CACHED,
        ];
        assert_eq!(staging_memory(&types, 0b1111), Some((3, true)));
        assert_eq!(staging_memory(&types, 0b0111), Some((2, false)));
        assert_eq!(staging_memory(&types, 0b0011), Some((1, true)));
        assert_eq!(staging_memory(&types, 0b0001), None);
    }
}
