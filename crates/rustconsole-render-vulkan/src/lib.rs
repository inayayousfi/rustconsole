//! Cross-platform Vulkan presentation through an SDL window.

mod color;
mod overlay;

pub use color::{VideoColorMode, VideoColorParameters};

use ash::{Entry, vk};
use std::ffi::{CStr, CString};
use std::io::Cursor;
use std::marker::PhantomData;

use overlay::VulkanOverlay;

const FRAMES_IN_FLIGHT: usize = 2;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum VulkanOutputPreference {
    #[default]
    Sdr,
    Hdr10IfAvailable,
}

#[derive(Clone, Copy)]
pub struct VulkanSampledPlane {
    pub image: vk::Image,
    pub view: vk::ImageView,
    pub source_queue_family: u32,
}

pub trait ImportedVulkanFrame {
    fn sampled_planes(&self) -> [VulkanSampledPlane; 2];
}

pub struct VulkanImportContext<'a> {
    pub instance: &'a ash::Instance,
    pub device: &'a ash::Device,
    pub physical_device: vk::PhysicalDevice,
    pub memory_properties: &'a vk::PhysicalDeviceMemoryProperties,
    pub queue_family: u32,
}

pub trait VulkanFrameImporter<Frame> {
    type Imported: ImportedVulkanFrame;

    fn required_device_extensions(&self) -> &'static [&'static CStr];

    fn import(
        &self,
        context: VulkanImportContext<'_>,
        frame: &Frame,
    ) -> Result<Self::Imported, String>;
}

pub struct VulkanRenderer<Importer, Frame>
where
    Importer: VulkanFrameImporter<Frame>,
{
    instance: ash::Instance,
    surface_loader: ash::khr::surface::Instance,
    surface: vk::SurfaceKHR,
    physical_device: vk::PhysicalDevice,
    device: ash::Device,
    queue_family: u32,
    queue: vk::Queue,
    swapchain_loader: ash::khr::swapchain::Device,
    swapchain: Option<SwapchainState>,
    render_pass: vk::RenderPass,
    descriptor_set_layout: vk::DescriptorSetLayout,
    pipeline_layout: vk::PipelineLayout,
    pipeline: vk::Pipeline,
    loading_pipeline_layout: vk::PipelineLayout,
    loading_pipeline: vk::Pipeline,
    descriptor_pool: vk::DescriptorPool,
    sampler: vk::Sampler,
    command_pool: vk::CommandPool,
    overlay: VulkanOverlay,
    frames: Vec<FrameState<Importer::Imported>>,
    frame_index: usize,
    memory_properties: vk::PhysicalDeviceMemoryProperties,
    swapchain_format: vk::Format,
    swapchain_color_space: vk::ColorSpaceKHR,
    hdr10_output: bool,
    importer: Importer,
    color_parameters: VideoColorParameters,
    frame_type: PhantomData<fn(&Frame)>,
}

impl<Importer, Frame> VulkanRenderer<Importer, Frame>
where
    Importer: VulkanFrameImporter<Frame>,
{
    pub fn new(
        window: &sdl3::video::Window,
        importer: Importer,
        output_preference: VulkanOutputPreference,
    ) -> Result<Self, String> {
        let entry = unsafe { Entry::load() }.map_err(|error| error.to_string())?;
        let application = vk::ApplicationInfo::default()
            .application_name(c"Rust Console Vulkan renderer")
            .api_version(vk::API_VERSION_1_2);
        let mut instance_extension_names = window
            .vulkan_instance_extensions()
            .map_err(|error| error.to_string())?
            .into_iter()
            .map(CString::new)
            .collect::<Result<Vec<_>, _>>()
            .map_err(|error| error.to_string())?;
        let supports_extended_color_space = unsafe { entry.enumerate_instance_extension_properties(None) }
            .map_err(|error| error.to_string())?
            .iter()
            .any(|extension| unsafe { CStr::from_ptr(extension.extension_name.as_ptr()) } == c"VK_EXT_swapchain_colorspace");
        if output_preference == VulkanOutputPreference::Hdr10IfAvailable
            && supports_extended_color_space
        {
            instance_extension_names.push(CString::from(c"VK_EXT_swapchain_colorspace"));
        }
        let instance_extensions = instance_extension_names
            .iter()
            .map(|name| name.as_ptr())
            .collect::<Vec<_>>();
        let instance_info = vk::InstanceCreateInfo::default()
            .application_info(&application)
            .enabled_extension_names(&instance_extensions);
        let instance = unsafe { entry.create_instance(&instance_info, None) }
            .map_err(|error| error.to_string())?;
        let surface_loader = ash::khr::surface::Instance::new(&entry, &instance);
        let surface = unsafe { window.vulkan_create_surface(instance.handle()) }
            .map_err(|error| error.to_string())?;

        let mut required_extensions = vec![ash::khr::swapchain::NAME];
        required_extensions.extend_from_slice(importer.required_device_extensions());
        let (physical_device, queue_family) =
            select_device(&instance, &surface_loader, surface, &required_extensions)?;
        let priorities = [1.0_f32];
        let queue_info = vk::DeviceQueueCreateInfo::default()
            .queue_family_index(queue_family)
            .queue_priorities(&priorities);
        let device_extensions = required_extensions
            .iter()
            .map(|extension| extension.as_ptr())
            .collect::<Vec<_>>();
        let device_info = vk::DeviceCreateInfo::default()
            .queue_create_infos(std::slice::from_ref(&queue_info))
            .enabled_extension_names(&device_extensions);
        let device = unsafe { instance.create_device(physical_device, &device_info, None) }
            .map_err(|error| error.to_string())?;
        let queue = unsafe { device.get_device_queue(queue_family, 0) };
        let swapchain_loader = ash::khr::swapchain::Device::new(&instance, &device);
        let memory_properties =
            unsafe { instance.get_physical_device_memory_properties(physical_device) };
        let surface_format = select_surface_format(
            &surface_loader,
            physical_device,
            surface,
            output_preference == VulkanOutputPreference::Hdr10IfAvailable
                && supports_extended_color_space,
        )?;
        let swapchain_format = surface_format.format;
        let swapchain_color_space = surface_format.color_space;
        let hdr10_output = swapchain_color_space == vk::ColorSpaceKHR::HDR10_ST2084_EXT;
        let render_pass = create_render_pass(&device, swapchain_format)?;
        let descriptor_set_layout = create_descriptor_set_layout(&device)?;
        let pipeline_layout = create_pipeline_layout(&device, descriptor_set_layout)?;
        let pipeline = create_video_pipeline(&device, render_pass, pipeline_layout)?;
        let loading_pipeline_layout = create_loading_pipeline_layout(&device)?;
        let loading_pipeline =
            create_loading_pipeline(&device, render_pass, loading_pipeline_layout)?;
        let descriptor_pool = create_descriptor_pool(&device)?;
        let descriptor_layouts = [descriptor_set_layout; FRAMES_IN_FLIGHT];
        let descriptor_info = vk::DescriptorSetAllocateInfo::default()
            .descriptor_pool(descriptor_pool)
            .set_layouts(&descriptor_layouts);
        let descriptor_sets = unsafe { device.allocate_descriptor_sets(&descriptor_info) }
            .map_err(|error| error.to_string())?;
        let sampler_info = vk::SamplerCreateInfo::default()
            .mag_filter(vk::Filter::LINEAR)
            .min_filter(vk::Filter::LINEAR)
            .mipmap_mode(vk::SamplerMipmapMode::NEAREST)
            .address_mode_u(vk::SamplerAddressMode::CLAMP_TO_EDGE)
            .address_mode_v(vk::SamplerAddressMode::CLAMP_TO_EDGE)
            .address_mode_w(vk::SamplerAddressMode::CLAMP_TO_EDGE);
        let sampler = unsafe { device.create_sampler(&sampler_info, None) }
            .map_err(|error| error.to_string())?;
        let command_pool_info = vk::CommandPoolCreateInfo::default()
            .queue_family_index(queue_family)
            .flags(vk::CommandPoolCreateFlags::RESET_COMMAND_BUFFER);
        let command_pool = unsafe { device.create_command_pool(&command_pool_info, None) }
            .map_err(|error| error.to_string())?;
        let command_info = vk::CommandBufferAllocateInfo::default()
            .command_pool(command_pool)
            .level(vk::CommandBufferLevel::PRIMARY)
            .command_buffer_count(FRAMES_IN_FLIGHT as u32);
        let command_buffers = unsafe { device.allocate_command_buffers(&command_info) }
            .map_err(|error| error.to_string())?;
        let overlay = VulkanOverlay::new(
            &device,
            &memory_properties,
            queue,
            command_pool,
            render_pass,
            FRAMES_IN_FLIGHT,
            hdr10_output,
        )?;
        let mut frames = Vec::with_capacity(FRAMES_IN_FLIGHT);
        for (descriptor_set, command_buffer) in descriptor_sets.into_iter().zip(command_buffers) {
            let semaphore_info = vk::SemaphoreCreateInfo::default();
            let fence_info = vk::FenceCreateInfo::default().flags(vk::FenceCreateFlags::SIGNALED);
            frames.push(FrameState {
                image_available: unsafe { device.create_semaphore(&semaphore_info, None) }
                    .map_err(|error| error.to_string())?,
                render_finished: unsafe { device.create_semaphore(&semaphore_info, None) }
                    .map_err(|error| error.to_string())?,
                fence: unsafe { device.create_fence(&fence_info, None) }
                    .map_err(|error| error.to_string())?,
                command_buffer,
                descriptor_set,
                imported: None,
            });
        }

        let mut renderer = Self {
            instance,
            surface_loader,
            surface,
            physical_device,
            device,
            queue_family,
            queue,
            swapchain_loader,
            swapchain: None,
            render_pass,
            descriptor_set_layout,
            pipeline_layout,
            pipeline,
            loading_pipeline_layout,
            loading_pipeline,
            descriptor_pool,
            sampler,
            command_pool,
            overlay,
            frames,
            frame_index: 0,
            memory_properties,
            swapchain_format,
            swapchain_color_space,
            hdr10_output,
            importer,
            color_parameters: VideoColorParameters::default(),
            frame_type: PhantomData,
        };
        let (width, height) = window.size_in_pixels();
        renderer.recreate_swapchain(width.max(1), height.max(1))?;
        Ok(renderer)
    }

    pub fn set_video_color_parameters(&mut self, parameters: VideoColorParameters) {
        self.color_parameters = parameters;
    }

    #[must_use]
    pub const fn is_hdr10_output(&self) -> bool {
        self.hdr10_output
    }

    pub fn present_loading(
        &mut self,
        width: u32,
        height: u32,
        elapsed_seconds: f32,
        status: &str,
    ) -> Result<(), String> {
        if self.swapchain.as_ref().is_none_or(|swapchain| {
            swapchain.extent.width != width || swapchain.extent.height != height
        }) {
            self.recreate_swapchain(width, height)?;
        }
        let swapchain = self
            .swapchain
            .as_ref()
            .ok_or("Vulkan swapchain is unavailable")?;
        let frame_state = &mut self.frames[self.frame_index];
        let command_buffer = frame_state.command_buffer;
        let image_available = frame_state.image_available;
        let render_finished = frame_state.render_finished;
        let fence = frame_state.fence;
        unsafe {
            self.device
                .wait_for_fences(&[fence], true, u64::MAX)
                .map_err(|error| error.to_string())?;
        }
        frame_state.imported = None;

        let acquired = unsafe {
            self.swapchain_loader.acquire_next_image(
                swapchain.handle,
                u64::MAX,
                image_available,
                vk::Fence::null(),
            )
        };
        let (image_index, suboptimal) = match acquired {
            Ok(value) => value,
            Err(vk::Result::ERROR_OUT_OF_DATE_KHR) => {
                self.recreate_swapchain(width, height)?;
                return Ok(());
            }
            Err(error) => return Err(error.to_string()),
        };
        unsafe {
            self.device
                .reset_fences(&[fence])
                .map_err(|error| error.to_string())?;
        }
        self.record_loading_commands(command_buffer, image_index, elapsed_seconds, status)?;

        let wait_semaphores = [image_available];
        let signal_semaphores = [render_finished];
        let wait_stages = [vk::PipelineStageFlags::COLOR_ATTACHMENT_OUTPUT];
        let command_buffers = [command_buffer];
        let submit = vk::SubmitInfo::default()
            .wait_semaphores(&wait_semaphores)
            .wait_dst_stage_mask(&wait_stages)
            .command_buffers(&command_buffers)
            .signal_semaphores(&signal_semaphores);
        unsafe {
            self.device
                .queue_submit(self.queue, &[submit], fence)
                .map_err(|error| error.to_string())?;
        }

        let swapchains = [swapchain.handle];
        let indices = [image_index];
        let present = vk::PresentInfoKHR::default()
            .wait_semaphores(&signal_semaphores)
            .swapchains(&swapchains)
            .image_indices(&indices);
        let changed = match unsafe { self.swapchain_loader.queue_present(self.queue, &present) } {
            Ok(changed) => changed || suboptimal,
            Err(vk::Result::ERROR_OUT_OF_DATE_KHR) => true,
            Err(error) => return Err(error.to_string()),
        };
        self.frame_index = (self.frame_index + 1) % self.frames.len();
        if changed {
            self.recreate_swapchain(width, height)?;
        }
        Ok(())
    }

    pub fn present(
        &mut self,
        frame: &Frame,
        width: u32,
        height: u32,
        overlay_text: &str,
    ) -> Result<(), String> {
        if self.swapchain.as_ref().is_none_or(|swapchain| {
            swapchain.extent.width != width || swapchain.extent.height != height
        }) {
            self.recreate_swapchain(width, height)?;
        }
        let swapchain = self
            .swapchain
            .as_ref()
            .ok_or("Vulkan swapchain is unavailable")?;
        let frame_state = &mut self.frames[self.frame_index];
        let command_buffer = frame_state.command_buffer;
        let descriptor_set = frame_state.descriptor_set;
        let image_available = frame_state.image_available;
        let render_finished = frame_state.render_finished;
        let fence = frame_state.fence;
        unsafe {
            self.device
                .wait_for_fences(&[fence], true, u64::MAX)
                .map_err(|error| error.to_string())?;
        }
        frame_state.imported = None;

        let acquired = unsafe {
            self.swapchain_loader.acquire_next_image(
                swapchain.handle,
                u64::MAX,
                image_available,
                vk::Fence::null(),
            )
        };
        let (image_index, suboptimal) = match acquired {
            Ok(value) => value,
            Err(vk::Result::ERROR_OUT_OF_DATE_KHR) => {
                self.recreate_swapchain(width, height)?;
                return Ok(());
            }
            Err(error) => return Err(error.to_string()),
        };
        unsafe {
            self.device
                .reset_fences(&[fence])
                .map_err(|error| error.to_string())?;
        }

        let imported = self.importer.import(
            VulkanImportContext {
                instance: &self.instance,
                device: &self.device,
                physical_device: self.physical_device,
                memory_properties: &self.memory_properties,
                queue_family: self.queue_family,
            },
            frame,
        )?;
        let planes = imported.sampled_planes();
        let image_infos = [
            vk::DescriptorImageInfo::default()
                .sampler(self.sampler)
                .image_view(planes[0].view)
                .image_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL),
            vk::DescriptorImageInfo::default()
                .sampler(self.sampler)
                .image_view(planes[1].view)
                .image_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL),
        ];
        let writes = [
            vk::WriteDescriptorSet::default()
                .dst_set(descriptor_set)
                .dst_binding(0)
                .descriptor_type(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
                .image_info(std::slice::from_ref(&image_infos[0])),
            vk::WriteDescriptorSet::default()
                .dst_set(descriptor_set)
                .dst_binding(1)
                .descriptor_type(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
                .image_info(std::slice::from_ref(&image_infos[1])),
        ];
        unsafe { self.device.update_descriptor_sets(&writes, &[]) };
        self.record_commands(
            command_buffer,
            descriptor_set,
            &imported,
            image_index,
            overlay_text,
        )?;

        let wait_semaphores = [image_available];
        let signal_semaphores = [render_finished];
        let wait_stages = [vk::PipelineStageFlags::COLOR_ATTACHMENT_OUTPUT];
        let command_buffers = [command_buffer];
        let submit = vk::SubmitInfo::default()
            .wait_semaphores(&wait_semaphores)
            .wait_dst_stage_mask(&wait_stages)
            .command_buffers(&command_buffers)
            .signal_semaphores(&signal_semaphores);
        unsafe {
            self.device
                .queue_submit(self.queue, &[submit], fence)
                .map_err(|error| error.to_string())?;
        }
        self.frames[self.frame_index].imported = Some(imported);

        let swapchains = [swapchain.handle];
        let indices = [image_index];
        let present = vk::PresentInfoKHR::default()
            .wait_semaphores(&signal_semaphores)
            .swapchains(&swapchains)
            .image_indices(&indices);
        let changed = match unsafe { self.swapchain_loader.queue_present(self.queue, &present) } {
            Ok(changed) => changed || suboptimal,
            Err(vk::Result::ERROR_OUT_OF_DATE_KHR) => true,
            Err(error) => return Err(error.to_string()),
        };
        self.frame_index = (self.frame_index + 1) % self.frames.len();
        if changed {
            self.recreate_swapchain(width, height)?;
        }
        Ok(())
    }

    fn record_commands(
        &self,
        command_buffer: vk::CommandBuffer,
        descriptor_set: vk::DescriptorSet,
        imported: &Importer::Imported,
        image_index: u32,
        overlay_text: &str,
    ) -> Result<(), String> {
        let swapchain = self.swapchain.as_ref().unwrap();
        unsafe {
            self.device
                .reset_command_buffer(command_buffer, vk::CommandBufferResetFlags::empty())
                .map_err(|error| error.to_string())?;
            self.device
                .begin_command_buffer(
                    command_buffer,
                    &vk::CommandBufferBeginInfo::default()
                        .flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT),
                )
                .map_err(|error| error.to_string())?;
            let planes = imported.sampled_planes();
            let barriers = planes
                .iter()
                .map(|plane| {
                    vk::ImageMemoryBarrier::default()
                        .src_access_mask(vk::AccessFlags::empty())
                        .dst_access_mask(vk::AccessFlags::SHADER_READ)
                        .old_layout(vk::ImageLayout::UNDEFINED)
                        .new_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)
                        .src_queue_family_index(plane.source_queue_family)
                        .dst_queue_family_index(self.queue_family)
                        .image(plane.image)
                        .subresource_range(color_subresource_range())
                })
                .collect::<Vec<_>>();
            self.device.cmd_pipeline_barrier(
                command_buffer,
                vk::PipelineStageFlags::TOP_OF_PIPE,
                vk::PipelineStageFlags::FRAGMENT_SHADER,
                vk::DependencyFlags::empty(),
                &[],
                &[],
                &barriers,
            );
            let clear = [vk::ClearValue {
                color: vk::ClearColorValue {
                    float32: [0.02, 0.02, 0.03, 1.0],
                },
            }];
            let render_pass = vk::RenderPassBeginInfo::default()
                .render_pass(self.render_pass)
                .framebuffer(swapchain.framebuffers[image_index as usize])
                .render_area(vk::Rect2D {
                    offset: vk::Offset2D::default(),
                    extent: swapchain.extent,
                })
                .clear_values(&clear);
            self.device.cmd_begin_render_pass(
                command_buffer,
                &render_pass,
                vk::SubpassContents::INLINE,
            );
            self.device.cmd_bind_pipeline(
                command_buffer,
                vk::PipelineBindPoint::GRAPHICS,
                self.pipeline,
            );
            self.device.cmd_bind_descriptor_sets(
                command_buffer,
                vk::PipelineBindPoint::GRAPHICS,
                self.pipeline_layout,
                0,
                &[descriptor_set],
                &[],
            );
            self.device.cmd_push_constants(
                command_buffer,
                self.pipeline_layout,
                vk::ShaderStageFlags::FRAGMENT,
                0,
                self.color_parameters.as_bytes(),
            );
            self.device.cmd_set_viewport(
                command_buffer,
                0,
                &[vk::Viewport {
                    x: 0.0,
                    y: swapchain.extent.height as f32,
                    width: swapchain.extent.width as f32,
                    height: -(swapchain.extent.height as f32),
                    min_depth: 0.0,
                    max_depth: 1.0,
                }],
            );
            self.device.cmd_set_scissor(
                command_buffer,
                0,
                &[vk::Rect2D {
                    offset: vk::Offset2D::default(),
                    extent: swapchain.extent,
                }],
            );
            self.device.cmd_draw(command_buffer, 3, 1, 0, 0);
            self.overlay.record(
                command_buffer,
                self.frame_index,
                swapchain.extent,
                overlay_text,
            );
            self.device.cmd_end_render_pass(command_buffer);
            self.device
                .end_command_buffer(command_buffer)
                .map_err(|error| error.to_string())?;
        }
        Ok(())
    }

    fn record_loading_commands(
        &self,
        command_buffer: vk::CommandBuffer,
        image_index: u32,
        elapsed_seconds: f32,
        status: &str,
    ) -> Result<(), String> {
        let swapchain = self.swapchain.as_ref().unwrap();
        let constants = [
            elapsed_seconds,
            swapchain.extent.width as f32 / swapchain.extent.height.max(1) as f32,
            if self.hdr10_output { 1.0 } else { 0.0 },
        ];
        let constant_bytes = unsafe {
            std::slice::from_raw_parts(
                constants.as_ptr().cast::<u8>(),
                std::mem::size_of_val(&constants),
            )
        };
        unsafe {
            self.device
                .reset_command_buffer(command_buffer, vk::CommandBufferResetFlags::empty())
                .map_err(|error| error.to_string())?;
            self.device
                .begin_command_buffer(
                    command_buffer,
                    &vk::CommandBufferBeginInfo::default()
                        .flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT),
                )
                .map_err(|error| error.to_string())?;
            let clear = [vk::ClearValue {
                color: vk::ClearColorValue {
                    float32: [0.012, 0.013, 0.016, 1.0],
                },
            }];
            let render_pass = vk::RenderPassBeginInfo::default()
                .render_pass(self.render_pass)
                .framebuffer(swapchain.framebuffers[image_index as usize])
                .render_area(vk::Rect2D {
                    offset: vk::Offset2D::default(),
                    extent: swapchain.extent,
                })
                .clear_values(&clear);
            self.device.cmd_begin_render_pass(
                command_buffer,
                &render_pass,
                vk::SubpassContents::INLINE,
            );
            self.device.cmd_bind_pipeline(
                command_buffer,
                vk::PipelineBindPoint::GRAPHICS,
                self.loading_pipeline,
            );
            self.device.cmd_push_constants(
                command_buffer,
                self.loading_pipeline_layout,
                vk::ShaderStageFlags::FRAGMENT,
                0,
                constant_bytes,
            );
            self.device.cmd_set_viewport(
                command_buffer,
                0,
                &[vk::Viewport {
                    x: 0.0,
                    y: swapchain.extent.height as f32,
                    width: swapchain.extent.width as f32,
                    height: -(swapchain.extent.height as f32),
                    min_depth: 0.0,
                    max_depth: 1.0,
                }],
            );
            self.device.cmd_set_scissor(
                command_buffer,
                0,
                &[vk::Rect2D {
                    offset: vk::Offset2D::default(),
                    extent: swapchain.extent,
                }],
            );
            self.device.cmd_draw(command_buffer, 3, 1, 0, 0);
            let text = format!("{status}\nWaiting {:.1} seconds", elapsed_seconds);
            self.overlay
                .record(command_buffer, self.frame_index, swapchain.extent, &text);
            self.device.cmd_end_render_pass(command_buffer);
            self.device
                .end_command_buffer(command_buffer)
                .map_err(|error| error.to_string())?;
        }
        Ok(())
    }

    fn recreate_swapchain(&mut self, width: u32, height: u32) -> Result<(), String> {
        unsafe { self.device.device_wait_idle() }.map_err(|error| error.to_string())?;
        for frame in &mut self.frames {
            frame.imported = None;
        }
        if let Some(mut swapchain) = self.swapchain.take() {
            swapchain.destroy(&self.device, &self.swapchain_loader);
        }
        self.swapchain = Some(SwapchainState::new(
            &self.surface_loader,
            &self.swapchain_loader,
            &self.device,
            self.physical_device,
            self.surface,
            self.swapchain_format,
            self.swapchain_color_space,
            self.render_pass,
            width,
            height,
        )?);
        Ok(())
    }
}

impl<Importer, Frame> Drop for VulkanRenderer<Importer, Frame>
where
    Importer: VulkanFrameImporter<Frame>,
{
    fn drop(&mut self) {
        unsafe {
            let _ = self.device.device_wait_idle();
        }
        for frame in &mut self.frames {
            frame.imported = None;
            unsafe {
                self.device.destroy_fence(frame.fence, None);
                self.device.destroy_semaphore(frame.render_finished, None);
                self.device.destroy_semaphore(frame.image_available, None);
            }
        }
        if let Some(mut swapchain) = self.swapchain.take() {
            swapchain.destroy(&self.device, &self.swapchain_loader);
        }
        self.overlay.destroy();
        unsafe {
            self.device.destroy_command_pool(self.command_pool, None);
            self.device.destroy_sampler(self.sampler, None);
            self.device
                .destroy_descriptor_pool(self.descriptor_pool, None);
            self.device.destroy_pipeline(self.pipeline, None);
            self.device.destroy_pipeline(self.loading_pipeline, None);
            self.device
                .destroy_pipeline_layout(self.pipeline_layout, None);
            self.device
                .destroy_pipeline_layout(self.loading_pipeline_layout, None);
            self.device
                .destroy_descriptor_set_layout(self.descriptor_set_layout, None);
            self.device.destroy_render_pass(self.render_pass, None);
            self.device.destroy_device(None);
            self.surface_loader.destroy_surface(self.surface, None);
            self.instance.destroy_instance(None);
        }
    }
}

struct FrameState<Imported> {
    image_available: vk::Semaphore,
    render_finished: vk::Semaphore,
    fence: vk::Fence,
    command_buffer: vk::CommandBuffer,
    descriptor_set: vk::DescriptorSet,
    imported: Option<Imported>,
}

struct SwapchainState {
    handle: vk::SwapchainKHR,
    image_views: Vec<vk::ImageView>,
    framebuffers: Vec<vk::Framebuffer>,
    extent: vk::Extent2D,
}

impl SwapchainState {
    #[allow(clippy::too_many_arguments)]
    fn new(
        surface_loader: &ash::khr::surface::Instance,
        swapchain_loader: &ash::khr::swapchain::Device,
        device: &ash::Device,
        physical_device: vk::PhysicalDevice,
        surface: vk::SurfaceKHR,
        format: vk::Format,
        color_space: vk::ColorSpaceKHR,
        render_pass: vk::RenderPass,
        width: u32,
        height: u32,
    ) -> Result<Self, String> {
        let capabilities = unsafe {
            surface_loader.get_physical_device_surface_capabilities(physical_device, surface)
        }
        .map_err(|error| error.to_string())?;
        let present_modes = unsafe {
            surface_loader.get_physical_device_surface_present_modes(physical_device, surface)
        }
        .map_err(|error| error.to_string())?;
        let extent = if capabilities.current_extent.width != u32::MAX {
            capabilities.current_extent
        } else {
            vk::Extent2D {
                width: width.clamp(
                    capabilities.min_image_extent.width,
                    capabilities.max_image_extent.width,
                ),
                height: height.clamp(
                    capabilities.min_image_extent.height,
                    capabilities.max_image_extent.height,
                ),
            }
        };
        let mut image_count = capabilities.min_image_count.saturating_add(1);
        if capabilities.max_image_count != 0 {
            image_count = image_count.min(capabilities.max_image_count);
        }
        let present_mode = if present_modes.contains(&vk::PresentModeKHR::MAILBOX) {
            vk::PresentModeKHR::MAILBOX
        } else {
            vk::PresentModeKHR::FIFO
        };
        let composite_alpha = [
            vk::CompositeAlphaFlagsKHR::OPAQUE,
            vk::CompositeAlphaFlagsKHR::PRE_MULTIPLIED,
            vk::CompositeAlphaFlagsKHR::POST_MULTIPLIED,
            vk::CompositeAlphaFlagsKHR::INHERIT,
        ]
        .into_iter()
        .find(|mode| capabilities.supported_composite_alpha.contains(*mode))
        .ok_or("Vulkan surface exposes no composite alpha mode")?;
        let info = vk::SwapchainCreateInfoKHR::default()
            .surface(surface)
            .min_image_count(image_count)
            .image_format(format)
            .image_color_space(color_space)
            .image_extent(extent)
            .image_array_layers(1)
            .image_usage(vk::ImageUsageFlags::COLOR_ATTACHMENT)
            .image_sharing_mode(vk::SharingMode::EXCLUSIVE)
            .pre_transform(capabilities.current_transform)
            .composite_alpha(composite_alpha)
            .present_mode(present_mode)
            .clipped(true);
        let handle = unsafe { swapchain_loader.create_swapchain(&info, None) }
            .map_err(|error| error.to_string())?;
        let images = unsafe { swapchain_loader.get_swapchain_images(handle) }
            .map_err(|error| error.to_string())?;
        let mut image_views = Vec::with_capacity(images.len());
        for image in images {
            let view_info = vk::ImageViewCreateInfo::default()
                .image(image)
                .view_type(vk::ImageViewType::TYPE_2D)
                .format(format)
                .subresource_range(color_subresource_range());
            image_views.push(
                unsafe { device.create_image_view(&view_info, None) }
                    .map_err(|error| error.to_string())?,
            );
        }
        let mut framebuffers = Vec::with_capacity(image_views.len());
        for view in &image_views {
            let attachments = [*view];
            let framebuffer_info = vk::FramebufferCreateInfo::default()
                .render_pass(render_pass)
                .attachments(&attachments)
                .width(extent.width)
                .height(extent.height)
                .layers(1);
            framebuffers.push(
                unsafe { device.create_framebuffer(&framebuffer_info, None) }
                    .map_err(|error| error.to_string())?,
            );
        }
        Ok(Self {
            handle,
            image_views,
            framebuffers,
            extent,
        })
    }

    fn destroy(&mut self, device: &ash::Device, loader: &ash::khr::swapchain::Device) {
        unsafe {
            for framebuffer in self.framebuffers.drain(..) {
                device.destroy_framebuffer(framebuffer, None);
            }
            for view in self.image_views.drain(..) {
                device.destroy_image_view(view, None);
            }
            loader.destroy_swapchain(self.handle, None);
        }
    }
}

fn select_device(
    instance: &ash::Instance,
    surface_loader: &ash::khr::surface::Instance,
    surface: vk::SurfaceKHR,
    required: &[&CStr],
) -> Result<(vk::PhysicalDevice, u32), String> {
    for device in
        unsafe { instance.enumerate_physical_devices() }.map_err(|error| error.to_string())?
    {
        let extensions = unsafe { instance.enumerate_device_extension_properties(device) }
            .map_err(|error| error.to_string())?;
        if !required.iter().all(|required| {
            extensions.iter().any(|extension| {
                (unsafe { CStr::from_ptr(extension.extension_name.as_ptr()) }) == *required
            })
        }) {
            continue;
        }
        let families = unsafe { instance.get_physical_device_queue_family_properties(device) };
        for (index, family) in families.iter().enumerate() {
            let presentation = unsafe {
                surface_loader.get_physical_device_surface_support(device, index as u32, surface)
            }
            .map_err(|error| error.to_string())?;
            if family.queue_flags.contains(vk::QueueFlags::GRAPHICS) && presentation {
                return Ok((device, index as u32));
            }
        }
    }
    Err("no Vulkan device supports this SDL surface and frame importer".into())
}

fn select_surface_format(
    loader: &ash::khr::surface::Instance,
    physical_device: vk::PhysicalDevice,
    surface: vk::SurfaceKHR,
    hdr10_requested: bool,
) -> Result<vk::SurfaceFormatKHR, String> {
    let formats = unsafe { loader.get_physical_device_surface_formats(physical_device, surface) }
        .map_err(|error| error.to_string())?;
    if hdr10_requested
        && let Some(format) = formats.iter().find(|format| {
            matches!(
                format.format,
                vk::Format::A2B10G10R10_UNORM_PACK32 | vk::Format::A2R10G10B10_UNORM_PACK32
            ) && format.color_space == vk::ColorSpaceKHR::HDR10_ST2084_EXT
        })
    {
        return Ok(*format);
    }
    formats
        .iter()
        .find(|format| {
            matches!(
                format.format,
                vk::Format::B8G8R8A8_UNORM | vk::Format::B8G8R8A8_SRGB
            ) && format.color_space == vk::ColorSpaceKHR::SRGB_NONLINEAR
        })
        .or_else(|| formats.first())
        .copied()
        .ok_or_else(|| "Vulkan surface exposes no image format".into())
}

fn create_render_pass(device: &ash::Device, format: vk::Format) -> Result<vk::RenderPass, String> {
    let attachments = [vk::AttachmentDescription::default()
        .format(format)
        .samples(vk::SampleCountFlags::TYPE_1)
        .load_op(vk::AttachmentLoadOp::CLEAR)
        .store_op(vk::AttachmentStoreOp::STORE)
        .initial_layout(vk::ImageLayout::UNDEFINED)
        .final_layout(vk::ImageLayout::PRESENT_SRC_KHR)];
    let color_reference = [vk::AttachmentReference {
        attachment: 0,
        layout: vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL,
    }];
    let subpasses = [vk::SubpassDescription::default()
        .pipeline_bind_point(vk::PipelineBindPoint::GRAPHICS)
        .color_attachments(&color_reference)];
    let dependencies = [vk::SubpassDependency::default()
        .src_subpass(vk::SUBPASS_EXTERNAL)
        .dst_subpass(0)
        .src_stage_mask(vk::PipelineStageFlags::COLOR_ATTACHMENT_OUTPUT)
        .dst_stage_mask(vk::PipelineStageFlags::COLOR_ATTACHMENT_OUTPUT)
        .dst_access_mask(vk::AccessFlags::COLOR_ATTACHMENT_WRITE)];
    let info = vk::RenderPassCreateInfo::default()
        .attachments(&attachments)
        .subpasses(&subpasses)
        .dependencies(&dependencies);
    unsafe { device.create_render_pass(&info, None) }.map_err(|error| error.to_string())
}

fn create_descriptor_set_layout(device: &ash::Device) -> Result<vk::DescriptorSetLayout, String> {
    let bindings = [
        vk::DescriptorSetLayoutBinding::default()
            .binding(0)
            .descriptor_type(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
            .descriptor_count(1)
            .stage_flags(vk::ShaderStageFlags::FRAGMENT),
        vk::DescriptorSetLayoutBinding::default()
            .binding(1)
            .descriptor_type(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
            .descriptor_count(1)
            .stage_flags(vk::ShaderStageFlags::FRAGMENT),
    ];
    let info = vk::DescriptorSetLayoutCreateInfo::default().bindings(&bindings);
    unsafe { device.create_descriptor_set_layout(&info, None) }.map_err(|error| error.to_string())
}

fn create_pipeline_layout(
    device: &ash::Device,
    descriptor_set_layout: vk::DescriptorSetLayout,
) -> Result<vk::PipelineLayout, String> {
    let layouts = [descriptor_set_layout];
    let push_ranges = [vk::PushConstantRange::default()
        .stage_flags(vk::ShaderStageFlags::FRAGMENT)
        .offset(0)
        .size(std::mem::size_of::<VideoColorParameters>() as u32)];
    let info = vk::PipelineLayoutCreateInfo::default()
        .set_layouts(&layouts)
        .push_constant_ranges(&push_ranges);
    unsafe { device.create_pipeline_layout(&info, None) }.map_err(|error| error.to_string())
}

fn create_loading_pipeline_layout(device: &ash::Device) -> Result<vk::PipelineLayout, String> {
    let push_ranges = [vk::PushConstantRange::default()
        .stage_flags(vk::ShaderStageFlags::FRAGMENT)
        .offset(0)
        .size(12)];
    let info = vk::PipelineLayoutCreateInfo::default().push_constant_ranges(&push_ranges);
    unsafe { device.create_pipeline_layout(&info, None) }.map_err(|error| error.to_string())
}

fn create_video_pipeline(
    device: &ash::Device,
    render_pass: vk::RenderPass,
    layout: vk::PipelineLayout,
) -> Result<vk::Pipeline, String> {
    create_pipeline(
        device,
        render_pass,
        layout,
        include_bytes!("shaders/video.frag.spv"),
    )
}

fn create_loading_pipeline(
    device: &ash::Device,
    render_pass: vk::RenderPass,
    layout: vk::PipelineLayout,
) -> Result<vk::Pipeline, String> {
    create_pipeline(
        device,
        render_pass,
        layout,
        include_bytes!("shaders/loading.frag.spv"),
    )
}

fn create_pipeline(
    device: &ash::Device,
    render_pass: vk::RenderPass,
    layout: vk::PipelineLayout,
    fragment_bytes: &[u8],
) -> Result<vk::Pipeline, String> {
    let vertex = create_shader_module(device, include_bytes!("shaders/video.vert.spv"))?;
    let fragment = match create_shader_module(device, fragment_bytes) {
        Ok(fragment) => fragment,
        Err(error) => {
            unsafe { device.destroy_shader_module(vertex, None) };
            return Err(error);
        }
    };
    let stages = [
        vk::PipelineShaderStageCreateInfo::default()
            .stage(vk::ShaderStageFlags::VERTEX)
            .module(vertex)
            .name(c"main"),
        vk::PipelineShaderStageCreateInfo::default()
            .stage(vk::ShaderStageFlags::FRAGMENT)
            .module(fragment)
            .name(c"main"),
    ];
    let vertex_input = vk::PipelineVertexInputStateCreateInfo::default();
    let assembly = vk::PipelineInputAssemblyStateCreateInfo::default()
        .topology(vk::PrimitiveTopology::TRIANGLE_LIST);
    let viewport = vk::PipelineViewportStateCreateInfo::default()
        .viewport_count(1)
        .scissor_count(1);
    let raster = vk::PipelineRasterizationStateCreateInfo::default()
        .polygon_mode(vk::PolygonMode::FILL)
        .cull_mode(vk::CullModeFlags::NONE)
        .front_face(vk::FrontFace::COUNTER_CLOCKWISE)
        .line_width(1.0);
    let multisample = vk::PipelineMultisampleStateCreateInfo::default()
        .rasterization_samples(vk::SampleCountFlags::TYPE_1);
    let blend_attachments = [vk::PipelineColorBlendAttachmentState::default()
        .color_write_mask(vk::ColorComponentFlags::RGBA)];
    let blend = vk::PipelineColorBlendStateCreateInfo::default().attachments(&blend_attachments);
    let dynamic = [vk::DynamicState::VIEWPORT, vk::DynamicState::SCISSOR];
    let dynamic_state = vk::PipelineDynamicStateCreateInfo::default().dynamic_states(&dynamic);
    let info = vk::GraphicsPipelineCreateInfo::default()
        .stages(&stages)
        .vertex_input_state(&vertex_input)
        .input_assembly_state(&assembly)
        .viewport_state(&viewport)
        .rasterization_state(&raster)
        .multisample_state(&multisample)
        .color_blend_state(&blend)
        .dynamic_state(&dynamic_state)
        .layout(layout)
        .render_pass(render_pass)
        .subpass(0);
    let pipeline =
        unsafe { device.create_graphics_pipelines(vk::PipelineCache::null(), &[info], None) }
            .map_err(|(_, error)| error.to_string())?[0];
    unsafe {
        device.destroy_shader_module(fragment, None);
        device.destroy_shader_module(vertex, None);
    }
    Ok(pipeline)
}

fn create_shader_module(device: &ash::Device, bytes: &[u8]) -> Result<vk::ShaderModule, String> {
    let words = ash::util::read_spv(&mut Cursor::new(bytes)).map_err(|error| error.to_string())?;
    let info = vk::ShaderModuleCreateInfo::default().code(&words);
    unsafe { device.create_shader_module(&info, None) }.map_err(|error| error.to_string())
}

fn create_descriptor_pool(device: &ash::Device) -> Result<vk::DescriptorPool, String> {
    let sizes = [vk::DescriptorPoolSize {
        ty: vk::DescriptorType::COMBINED_IMAGE_SAMPLER,
        descriptor_count: (FRAMES_IN_FLIGHT * 2) as u32,
    }];
    let info = vk::DescriptorPoolCreateInfo::default()
        .max_sets(FRAMES_IN_FLIGHT as u32)
        .pool_sizes(&sizes);
    unsafe { device.create_descriptor_pool(&info, None) }.map_err(|error| error.to_string())
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
