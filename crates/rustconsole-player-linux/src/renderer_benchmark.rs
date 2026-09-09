use ash::{Entry, vk};
use rustconsole_codec_ffmpeg::{DmaBufFrameFormat, MappedDmaBufFrame};
use std::ffi::{CStr, c_char, c_void};
use std::ptr;
use std::time::{Duration, Instant};

type EglDisplay = *mut c_void;
type EglContext = *mut c_void;
type EglClientBuffer = *mut c_void;
type EglImage = *mut c_void;
type EglBoolean = u32;
type EglInt = i32;

const EGL_NONE: EglInt = 0x3038;
const EGL_EXTENSIONS: EglInt = 0x3055;
const EGL_WIDTH: EglInt = 0x3057;
const EGL_HEIGHT: EglInt = 0x3056;
const EGL_LINUX_DMA_BUF_EXT: u32 = 0x3270;
const EGL_LINUX_DRM_FOURCC_EXT: EglInt = 0x3271;
const EGL_DMA_BUF_PLANE0_FD_EXT: EglInt = 0x3272;
const EGL_DMA_BUF_PLANE0_OFFSET_EXT: EglInt = 0x3273;
const EGL_DMA_BUF_PLANE0_PITCH_EXT: EglInt = 0x3274;
const EGL_DMA_BUF_PLANE0_MODIFIER_LO_EXT: EglInt = 0x3443;
const EGL_DMA_BUF_PLANE0_MODIFIER_HI_EXT: EglInt = 0x3444;

type EglCreateImageKhr =
    unsafe extern "C" fn(EglDisplay, EglContext, u32, EglClientBuffer, *const EglInt) -> EglImage;
type EglDestroyImageKhr = unsafe extern "C" fn(EglDisplay, EglImage) -> EglBoolean;

#[link(name = "EGL")]
unsafe extern "C" {
    fn eglGetDisplay(native_display: *mut c_void) -> EglDisplay;
    fn eglInitialize(display: EglDisplay, major: *mut EglInt, minor: *mut EglInt) -> EglBoolean;
    fn eglTerminate(display: EglDisplay) -> EglBoolean;
    fn eglQueryString(display: EglDisplay, name: EglInt) -> *const c_char;
    fn eglGetProcAddress(name: *const c_char) -> *const c_void;
}

pub fn benchmark_egl_import(
    frame: &MappedDmaBufFrame,
    repetitions: usize,
) -> Result<Duration, Box<dyn std::error::Error>> {
    if repetitions == 0 {
        return Err("EGL benchmark requires at least one repetition".into());
    }
    // SAFETY: EGL accepts a null native display as its platform default.
    let display = unsafe { eglGetDisplay(ptr::null_mut()) };
    if display.is_null() {
        return Err("EGL could not open the default display".into());
    }
    let mut major = 0;
    let mut minor = 0;
    // SAFETY: display is non-null and both version pointers are writable.
    if unsafe { eglInitialize(display, &mut major, &mut minor) } == 0 {
        return Err("EGL could not initialize the default display".into());
    }
    let initialized = EglDisplayGuard(display);
    // SAFETY: initialized.0 is a live display and EGL owns the returned string.
    let extensions = unsafe { eglQueryString(initialized.0, EGL_EXTENSIONS) };
    if extensions.is_null()
        // SAFETY: non-null EGL extension strings are null-terminated.
        || !unsafe { CStr::from_ptr(extensions) }
            .to_bytes()
            .split(|byte| *byte == b' ')
            .any(|extension| extension == b"EGL_EXT_image_dma_buf_import")
    {
        return Err("EGL_EXT_image_dma_buf_import is unavailable".into());
    }
    let create = load_egl_create_image()?;
    let destroy = load_egl_destroy_image()?;
    let started = Instant::now();
    for _ in 0..repetitions {
        for (layer_index, layer) in frame.layers().iter().enumerate() {
            if layer.planes.len() != 1 {
                return Err("EGL benchmark requires one plane per DRM layer".into());
            }
            let plane = layer.planes[0];
            let object = frame
                .objects()
                .get(plane.object_index)
                .ok_or("EGL benchmark received a DRM plane with no backing object")?;
            let (width, height) = if layer_index == 0 {
                (frame.width(), frame.height())
            } else {
                (frame.width().div_ceil(2), frame.height().div_ceil(2))
            };
            let attributes = [
                EGL_WIDTH,
                i32::try_from(width)?,
                EGL_HEIGHT,
                i32::try_from(height)?,
                EGL_LINUX_DRM_FOURCC_EXT,
                layer.format as i32,
                EGL_DMA_BUF_PLANE0_FD_EXT,
                object.fd,
                EGL_DMA_BUF_PLANE0_OFFSET_EXT,
                i32::try_from(plane.offset)?,
                EGL_DMA_BUF_PLANE0_PITCH_EXT,
                i32::try_from(plane.pitch)?,
                EGL_DMA_BUF_PLANE0_MODIFIER_LO_EXT,
                object.format_modifier as u32 as i32,
                EGL_DMA_BUF_PLANE0_MODIFIER_HI_EXT,
                (object.format_modifier >> 32) as u32 as i32,
                EGL_NONE,
            ];
            // SAFETY: the descriptor remains live, attributes are terminated,
            // and EGL_LINUX_DMA_BUF_EXT requires no context or client buffer.
            let image = unsafe {
                create(
                    initialized.0,
                    ptr::null_mut(),
                    EGL_LINUX_DMA_BUF_EXT,
                    ptr::null_mut(),
                    attributes.as_ptr(),
                )
            };
            if image.is_null() {
                return Err(format!("EGL rejected DRM layer {layer_index}").into());
            }
            // SAFETY: image was created by this display and is destroyed once.
            if unsafe { destroy(initialized.0, image) } == 0 {
                return Err("EGL could not destroy an imported image".into());
            }
        }
    }
    Ok(started.elapsed())
}

pub fn benchmark_vulkan_import(
    frame: &MappedDmaBufFrame,
    repetitions: usize,
) -> Result<Duration, Box<dyn std::error::Error>> {
    if repetitions == 0 {
        return Err("Vulkan benchmark requires at least one repetition".into());
    }
    // SAFETY: loading the process Vulkan loader does not create driver objects.
    let entry = unsafe { Entry::load()? };
    let application = vk::ApplicationInfo::default()
        .application_name(c"Rust Console renderer benchmark")
        .api_version(vk::API_VERSION_1_2);
    let instance_info = vk::InstanceCreateInfo::default().application_info(&application);
    // SAFETY: all create-info pointers remain live for the call.
    let instance = unsafe { entry.create_instance(&instance_info, None)? };
    let instance = VulkanInstanceGuard(instance);
    // SAFETY: instance is live.
    let physical_devices = unsafe { instance.0.enumerate_physical_devices()? };
    let physical_device = physical_devices
        .into_iter()
        .find(|device| vulkan_device_supports_import(&instance.0, *device))
        .ok_or("no Vulkan device supports DMA-BUF modifier import")?;
    // SAFETY: physical_device belongs to instance.
    let queue_families = unsafe {
        instance
            .0
            .get_physical_device_queue_family_properties(physical_device)
    };
    let queue_family = queue_families
        .iter()
        .position(|family| family.queue_flags.contains(vk::QueueFlags::GRAPHICS))
        .ok_or("Vulkan device has no graphics queue")? as u32;
    let priorities = [1.0_f32];
    let queue = vk::DeviceQueueCreateInfo::default()
        .queue_family_index(queue_family)
        .queue_priorities(&priorities);
    let extensions = [
        ash::khr::external_memory_fd::NAME.as_ptr(),
        ash::ext::external_memory_dma_buf::NAME.as_ptr(),
        ash::ext::image_drm_format_modifier::NAME.as_ptr(),
    ];
    let device_info = vk::DeviceCreateInfo::default()
        .queue_create_infos(std::slice::from_ref(&queue))
        .enabled_extension_names(&extensions);
    // SAFETY: physical device and create-info pointers are valid.
    let device = unsafe {
        instance
            .0
            .create_device(physical_device, &device_info, None)?
    };
    let device = VulkanDeviceGuard(device);
    let external_memory = ash::khr::external_memory_fd::Device::new(&instance.0, &device.0);
    // SAFETY: physical_device belongs to the live instance.
    let memory_properties = unsafe {
        instance
            .0
            .get_physical_device_memory_properties(physical_device)
    };
    let (y_format, uv_format) = match frame.frame_format() {
        Some(DmaBufFrameFormat::Nv12) => (vk::Format::R8_UNORM, vk::Format::R8G8_UNORM),
        Some(DmaBufFrameFormat::P010) => (vk::Format::R16_UNORM, vk::Format::R16G16_UNORM),
        None => return Err("Vulkan benchmark received unsupported DRM layer formats".into()),
    };

    let started = Instant::now();
    for _ in 0..repetitions {
        for (layer_index, layer) in frame.layers().iter().enumerate() {
            if layer.planes.len() != 1 {
                return Err("Vulkan benchmark requires one plane per DRM layer".into());
            }
            let plane = layer.planes[0];
            let object = frame
                .objects()
                .get(plane.object_index)
                .ok_or("Vulkan benchmark received a DRM plane with no backing object")?;
            let (width, height, format) = if layer_index == 0 {
                (frame.width(), frame.height(), y_format)
            } else {
                (
                    frame.width().div_ceil(2),
                    frame.height().div_ceil(2),
                    uv_format,
                )
            };
            let layouts = [vk::SubresourceLayout {
                offset: u64::try_from(plane.offset)?,
                size: object.size as u64,
                row_pitch: u64::try_from(plane.pitch)?,
                array_pitch: 0,
                depth_pitch: 0,
            }];
            let mut modifier = vk::ImageDrmFormatModifierExplicitCreateInfoEXT::default()
                .drm_format_modifier(object.format_modifier)
                .plane_layouts(&layouts);
            let mut external = vk::ExternalMemoryImageCreateInfo::default()
                .handle_types(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT);
            let image_info = vk::ImageCreateInfo::default()
                .push_next(&mut modifier)
                .push_next(&mut external)
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
                .usage(vk::ImageUsageFlags::SAMPLED)
                .sharing_mode(vk::SharingMode::EXCLUSIVE)
                .initial_layout(vk::ImageLayout::UNDEFINED);
            // SAFETY: device and chained create infos are live.
            let image = unsafe { device.0.create_image(&image_info, None)? };
            let image = VulkanImageGuard {
                device: &device.0,
                image,
            };
            // SAFETY: image belongs to device.
            let requirements = unsafe { device.0.get_image_memory_requirements(image.image) };
            // SAFETY: dup creates an independently owned descriptor for Vulkan.
            let imported_fd = unsafe { libc::dup(object.fd) };
            if imported_fd < 0 {
                return Err(std::io::Error::last_os_error().into());
            }
            let mut fd_guard = FdGuard(imported_fd);
            let mut fd_info = vk::MemoryFdPropertiesKHR::default();
            unsafe {
                external_memory.get_memory_fd_properties(
                    vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT,
                    fd_guard.0,
                    &mut fd_info,
                )?
            };
            let memory_type_bits = fd_info.memory_type_bits & requirements.memory_type_bits;
            let memory_type = (0..memory_properties.memory_type_count)
                .find(|index| memory_type_bits & (1 << index) != 0)
                .ok_or("Vulkan found no compatible memory type for DMA-BUF")?;
            let mut import = vk::ImportMemoryFdInfoKHR::default()
                .handle_type(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT)
                .fd(fd_guard.0);
            let allocation = vk::MemoryAllocateInfo::default()
                .push_next(&mut import)
                .allocation_size(requirements.size)
                .memory_type_index(memory_type);
            // SAFETY: the duplicated descriptor and allocation info are valid.
            let memory = unsafe { device.0.allocate_memory(&allocation, None)? };
            fd_guard.0 = -1;
            let memory = VulkanMemoryGuard {
                device: &device.0,
                memory,
            };
            // SAFETY: imported memory satisfies this image's requirements.
            unsafe { device.0.bind_image_memory(image.image, memory.memory, 0)? };
            drop(image);
            drop(memory);
        }
    }
    // SAFETY: all submitted work is absent; this confirms clean device lifecycle.
    unsafe { device.0.device_wait_idle()? };
    Ok(started.elapsed())
}

fn vulkan_device_supports_import(instance: &ash::Instance, device: vk::PhysicalDevice) -> bool {
    // SAFETY: device belongs to instance; Vulkan owns returned extension names.
    let Ok(properties) = (unsafe { instance.enumerate_device_extension_properties(device) }) else {
        return false;
    };
    [
        ash::khr::external_memory_fd::NAME,
        ash::ext::external_memory_dma_buf::NAME,
        ash::ext::image_drm_format_modifier::NAME,
    ]
    .iter()
    .all(|required| {
        properties.iter().any(|property| {
            // SAFETY: Vulkan extension_name is a fixed null-terminated array.
            (unsafe { CStr::from_ptr(property.extension_name.as_ptr()) }) == *required
        })
    })
}

struct VulkanInstanceGuard(ash::Instance);

impl Drop for VulkanInstanceGuard {
    fn drop(&mut self) {
        // SAFETY: this guard owns one instance and all child objects are gone.
        unsafe { self.0.destroy_instance(None) };
    }
}

struct VulkanDeviceGuard(ash::Device);

impl Drop for VulkanDeviceGuard {
    fn drop(&mut self) {
        // SAFETY: this guard owns one device and all child objects are gone.
        unsafe { self.0.destroy_device(None) };
    }
}

struct VulkanImageGuard<'a> {
    device: &'a ash::Device,
    image: vk::Image,
}

impl Drop for VulkanImageGuard<'_> {
    fn drop(&mut self) {
        // SAFETY: this guard owns one image.
        unsafe { self.device.destroy_image(self.image, None) };
    }
}

struct VulkanMemoryGuard<'a> {
    device: &'a ash::Device,
    memory: vk::DeviceMemory,
}

impl Drop for VulkanMemoryGuard<'_> {
    fn drop(&mut self) {
        // SAFETY: this guard owns one memory allocation and bound images are gone.
        unsafe { self.device.free_memory(self.memory, None) };
    }
}

struct FdGuard(i32);

impl Drop for FdGuard {
    fn drop(&mut self) {
        if self.0 >= 0 {
            // SAFETY: this guard owns one duplicated descriptor.
            unsafe { libc::close(self.0) };
        }
    }
}

struct EglDisplayGuard(EglDisplay);

impl Drop for EglDisplayGuard {
    fn drop(&mut self) {
        // SAFETY: this guard owns one initialized display.
        unsafe {
            eglTerminate(self.0);
        }
    }
}

fn load_egl_create_image() -> Result<EglCreateImageKhr, Box<dyn std::error::Error>> {
    // SAFETY: the static name is null-terminated; the extension signature is fixed by EGL.
    let address = unsafe { eglGetProcAddress(c"eglCreateImageKHR".as_ptr()) };
    if address.is_null() {
        return Err("eglCreateImageKHR is unavailable".into());
    }
    // SAFETY: EGL returned the address for this exact extension function.
    Ok(unsafe { std::mem::transmute::<*const c_void, EglCreateImageKhr>(address) })
}

fn load_egl_destroy_image() -> Result<EglDestroyImageKhr, Box<dyn std::error::Error>> {
    // SAFETY: the static name is null-terminated; the extension signature is fixed by EGL.
    let address = unsafe { eglGetProcAddress(c"eglDestroyImageKHR".as_ptr()) };
    if address.is_null() {
        return Err("eglDestroyImageKHR is unavailable".into());
    }
    // SAFETY: EGL returned the address for this exact extension function.
    Ok(unsafe { std::mem::transmute::<*const c_void, EglDestroyImageKhr>(address) })
}
