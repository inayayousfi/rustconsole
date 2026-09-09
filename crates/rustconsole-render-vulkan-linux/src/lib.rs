//! Linux DMA-BUF frame import for the shared Vulkan renderer.

use ash::vk;
use rustconsole_codec_ffmpeg::{DmaBufFrameFormat, MappedDmaBufFrame};
use rustconsole_render_vulkan::{
    ImportedVulkanFrame, VulkanFrameImporter, VulkanImportContext, VulkanSampledPlane,
};
use std::ffi::CStr;

const REQUIRED_EXTENSIONS: &[&CStr] = &[
    ash::khr::external_memory_fd::NAME,
    ash::ext::external_memory_dma_buf::NAME,
    ash::ext::image_drm_format_modifier::NAME,
    ash::ext::queue_family_foreign::NAME,
];

#[derive(Clone, Copy, Default)]
pub struct DmaBufFrameImporter;

impl VulkanFrameImporter<MappedDmaBufFrame> for DmaBufFrameImporter {
    type Imported = ImportedDmaBufFrame;

    fn required_device_extensions(&self) -> &'static [&'static CStr] {
        REQUIRED_EXTENSIONS
    }

    fn import(
        &self,
        context: VulkanImportContext<'_>,
        frame: &MappedDmaBufFrame,
    ) -> Result<Self::Imported, String> {
        let (y_format, uv_format) = match frame.frame_format() {
            Some(DmaBufFrameFormat::Nv12) => (vk::Format::R8_UNORM, vk::Format::R8G8_UNORM),
            Some(DmaBufFrameFormat::P010) => (vk::Format::R16_UNORM, vk::Format::R16G16_UNORM),
            None => return Err("Vulkan renderer received unsupported DRM layer formats".into()),
        };
        let external_memory =
            ash::khr::external_memory_fd::Device::new(context.instance, context.device);
        let y = ImportedPlane::new(
            &context,
            &external_memory,
            frame,
            0,
            y_format,
            frame.width(),
            frame.height(),
        )?;
        let uv = ImportedPlane::new(
            &context,
            &external_memory,
            frame,
            1,
            uv_format,
            frame.width().div_ceil(2),
            frame.height().div_ceil(2),
        )?;
        Ok(ImportedDmaBufFrame { planes: [y, uv] })
    }
}

pub struct ImportedDmaBufFrame {
    planes: [ImportedPlane; 2],
}

impl ImportedVulkanFrame for ImportedDmaBufFrame {
    fn sampled_planes(&self) -> [VulkanSampledPlane; 2] {
        [
            VulkanSampledPlane {
                image: self.planes[0].image,
                view: self.planes[0].view,
                source_queue_family: vk::QUEUE_FAMILY_FOREIGN_EXT,
            },
            VulkanSampledPlane {
                image: self.planes[1].image,
                view: self.planes[1].view,
                source_queue_family: vk::QUEUE_FAMILY_FOREIGN_EXT,
            },
        ]
    }
}

struct ImportedPlane {
    device: ash::Device,
    image: vk::Image,
    memory: vk::DeviceMemory,
    view: vk::ImageView,
}

impl ImportedPlane {
    #[allow(clippy::too_many_arguments)]
    fn new(
        context: &VulkanImportContext<'_>,
        external_memory: &ash::khr::external_memory_fd::Device,
        frame: &MappedDmaBufFrame,
        layer_index: usize,
        format: vk::Format,
        width: u32,
        height: u32,
    ) -> Result<Self, String> {
        let layer = frame
            .layers()
            .get(layer_index)
            .ok_or("Vulkan frame layer is missing")?;
        if layer.planes.len() != 1 {
            return Err("Vulkan renderer requires one plane per DRM layer".into());
        }
        let plane = layer.planes[0];
        let object = frame
            .objects()
            .get(plane.object_index)
            .ok_or("Vulkan frame layer has no backing object")?;
        let layouts = [vk::SubresourceLayout {
            offset: u64::try_from(plane.offset).map_err(|_| "Vulkan DMA-BUF offset is negative")?,
            size: object.size as u64,
            row_pitch: u64::try_from(plane.pitch)
                .map_err(|_| "Vulkan DMA-BUF pitch is negative")?,
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
        let image = unsafe { context.device.create_image(&image_info, None) }
            .map_err(|error| error.to_string())?;
        let mut imported = Self {
            device: context.device.clone(),
            image,
            memory: vk::DeviceMemory::null(),
            view: vk::ImageView::null(),
        };
        let requirements = unsafe { context.device.get_image_memory_requirements(image) };
        let imported_fd = unsafe { libc::dup(object.fd) };
        if imported_fd < 0 {
            return Err(std::io::Error::last_os_error().to_string());
        }
        let mut fd_guard = FdGuard(imported_fd);
        let mut fd_properties = vk::MemoryFdPropertiesKHR::default();
        unsafe {
            external_memory
                .get_memory_fd_properties(
                    vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT,
                    fd_guard.0,
                    &mut fd_properties,
                )
                .map_err(|error| error.to_string())?;
        }
        let memory_bits = fd_properties.memory_type_bits & requirements.memory_type_bits;
        let memory_type = (0..context.memory_properties.memory_type_count)
            .find(|index| memory_bits & (1 << index) != 0)
            .ok_or("Vulkan found no compatible DMA-BUF memory type")?;
        let mut import = vk::ImportMemoryFdInfoKHR::default()
            .handle_type(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT)
            .fd(fd_guard.0);
        let allocation = vk::MemoryAllocateInfo::default()
            .push_next(&mut import)
            .allocation_size(requirements.size)
            .memory_type_index(memory_type);
        imported.memory = unsafe { context.device.allocate_memory(&allocation, None) }
            .map_err(|error| error.to_string())?;
        fd_guard.0 = -1;
        unsafe {
            context
                .device
                .bind_image_memory(image, imported.memory, 0)
                .map_err(|error| error.to_string())?;
        }
        let view_info = vk::ImageViewCreateInfo::default()
            .image(image)
            .view_type(vk::ImageViewType::TYPE_2D)
            .format(format)
            .subresource_range(color_subresource_range());
        imported.view = unsafe { context.device.create_image_view(&view_info, None) }
            .map_err(|error| error.to_string())?;
        Ok(imported)
    }
}

impl Drop for ImportedPlane {
    fn drop(&mut self) {
        unsafe {
            if self.view != vk::ImageView::null() {
                self.device.destroy_image_view(self.view, None);
            }
            self.device.destroy_image(self.image, None);
            if self.memory != vk::DeviceMemory::null() {
                self.device.free_memory(self.memory, None);
            }
        }
    }
}

struct FdGuard(i32);

impl Drop for FdGuard {
    fn drop(&mut self) {
        if self.0 >= 0 {
            unsafe { libc::close(self.0) };
        }
    }
}

fn color_subresource_range() -> vk::ImageSubresourceRange {
    vk::ImageSubresourceRange {
        aspect_mask: vk::ImageAspectFlags::COLOR,
        base_mip_level: 0,
        level_count: 1,
        base_array_layer: 0,
        layer_count: 1,
    }
}
