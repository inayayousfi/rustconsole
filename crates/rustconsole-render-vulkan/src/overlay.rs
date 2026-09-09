use ash::vk;
use fontdue::{Font, FontSettings};
use std::collections::HashMap;
use std::io::Cursor;

const FONT_SIZE: f32 = 18.0;
const ATLAS_WIDTH: u32 = 512;
const ATLAS_HEIGHT: u32 = 128;
const FIRST_GLYPH: u8 = 32;
const LAST_GLYPH: u8 = 126;
const TEXT_LEFT: f32 = 16.0;
const FIRST_BASELINE: f32 = 30.0;
const LINE_HEIGHT: f32 = 22.0;
const PANEL_PADDING: f32 = 8.0;

#[derive(Clone, Copy)]
struct AtlasGlyph {
    x: u32,
    y: u32,
    width: u32,
    height: u32,
    xmin: i32,
    ymin: i32,
    advance: f32,
}

pub(crate) struct VulkanOverlay {
    device: ash::Device,
    descriptor_set_layout: vk::DescriptorSetLayout,
    pipeline_layout: vk::PipelineLayout,
    pipeline: vk::Pipeline,
    descriptor_pool: vk::DescriptorPool,
    descriptor_sets: Vec<vk::DescriptorSet>,
    sampler: vk::Sampler,
    image: vk::Image,
    memory: vk::DeviceMemory,
    view: vk::ImageView,
    glyphs: HashMap<char, AtlasGlyph>,
    hdr10_output: bool,
}

impl VulkanOverlay {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        device: &ash::Device,
        memory_properties: &vk::PhysicalDeviceMemoryProperties,
        queue: vk::Queue,
        command_pool: vk::CommandPool,
        render_pass: vk::RenderPass,
        frame_count: usize,
        hdr10_output: bool,
    ) -> Result<Self, String> {
        let (pixels, glyphs) = build_atlas()?;
        let (image, memory, view) =
            upload_atlas(device, memory_properties, queue, command_pool, &pixels)?;
        let sampler_info = vk::SamplerCreateInfo::default()
            .mag_filter(vk::Filter::LINEAR)
            .min_filter(vk::Filter::LINEAR)
            .mipmap_mode(vk::SamplerMipmapMode::NEAREST)
            .address_mode_u(vk::SamplerAddressMode::CLAMP_TO_EDGE)
            .address_mode_v(vk::SamplerAddressMode::CLAMP_TO_EDGE)
            .address_mode_w(vk::SamplerAddressMode::CLAMP_TO_EDGE);
        let sampler = unsafe { device.create_sampler(&sampler_info, None) }
            .map_err(|error| error.to_string())?;
        let bindings = [vk::DescriptorSetLayoutBinding::default()
            .binding(0)
            .descriptor_type(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
            .descriptor_count(1)
            .stage_flags(vk::ShaderStageFlags::FRAGMENT)];
        let descriptor_set_layout = unsafe {
            device.create_descriptor_set_layout(
                &vk::DescriptorSetLayoutCreateInfo::default().bindings(&bindings),
                None,
            )
        }
        .map_err(|error| error.to_string())?;
        let push_range = [vk::PushConstantRange::default()
            .stage_flags(vk::ShaderStageFlags::VERTEX | vk::ShaderStageFlags::FRAGMENT)
            .offset(0)
            .size(48)];
        let layouts = [descriptor_set_layout];
        let pipeline_layout = unsafe {
            device.create_pipeline_layout(
                &vk::PipelineLayoutCreateInfo::default()
                    .set_layouts(&layouts)
                    .push_constant_ranges(&push_range),
                None,
            )
        }
        .map_err(|error| error.to_string())?;
        let pipeline = create_pipeline(device, render_pass, pipeline_layout)?;
        let pool_sizes = [vk::DescriptorPoolSize {
            ty: vk::DescriptorType::COMBINED_IMAGE_SAMPLER,
            descriptor_count: frame_count as u32,
        }];
        let descriptor_pool = unsafe {
            device.create_descriptor_pool(
                &vk::DescriptorPoolCreateInfo::default()
                    .max_sets(frame_count as u32)
                    .pool_sizes(&pool_sizes),
                None,
            )
        }
        .map_err(|error| error.to_string())?;
        let descriptor_layouts = vec![descriptor_set_layout; frame_count];
        let descriptor_sets = unsafe {
            device.allocate_descriptor_sets(
                &vk::DescriptorSetAllocateInfo::default()
                    .descriptor_pool(descriptor_pool)
                    .set_layouts(&descriptor_layouts),
            )
        }
        .map_err(|error| error.to_string())?;
        let image_info = [vk::DescriptorImageInfo::default()
            .sampler(sampler)
            .image_view(view)
            .image_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)];
        for descriptor_set in &descriptor_sets {
            let writes = [vk::WriteDescriptorSet::default()
                .dst_set(*descriptor_set)
                .dst_binding(0)
                .descriptor_type(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
                .image_info(&image_info)];
            unsafe { device.update_descriptor_sets(&writes, &[]) };
        }
        Ok(Self {
            device: device.clone(),
            descriptor_set_layout,
            pipeline_layout,
            pipeline,
            descriptor_pool,
            descriptor_sets,
            sampler,
            image,
            memory,
            view,
            glyphs,
            hdr10_output,
        })
    }

    pub(crate) fn record(
        &self,
        command_buffer: vk::CommandBuffer,
        frame_index: usize,
        extent: vk::Extent2D,
        text: &str,
    ) {
        unsafe {
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
                &[self.descriptor_sets[frame_index]],
                &[],
            );
        }
        let (text_width, line_count) = overlay_text_size(text, &self.glyphs);
        self.draw_quad(
            command_buffer,
            extent,
            [
                PANEL_PADDING,
                PANEL_PADDING,
                (TEXT_LEFT + text_width + PANEL_PADDING).min(extent.width as f32),
                (FIRST_BASELINE + (line_count - 1) as f32 * LINE_HEIGHT + PANEL_PADDING)
                    .min(extent.height as f32),
            ],
            [-1.0; 4],
            [0.0, 0.0, 0.0, 0.8],
        );
        let mut x = TEXT_LEFT;
        let mut baseline = FIRST_BASELINE;
        for character in text.chars() {
            if character == '\n' {
                x = TEXT_LEFT;
                baseline += LINE_HEIGHT;
                continue;
            }
            let Some(glyph) = self.glyphs.get(&character) else {
                continue;
            };
            if glyph.width != 0 && glyph.height != 0 {
                let left = x + glyph.xmin as f32;
                let top = baseline - glyph.ymin as f32 - glyph.height as f32;
                self.draw_glyph(
                    command_buffer,
                    extent,
                    glyph,
                    left + 1.0,
                    top + 1.0,
                    [0.0, 0.0, 0.0, 0.85],
                );
                self.draw_glyph(
                    command_buffer,
                    extent,
                    glyph,
                    left,
                    top,
                    [0.95, 0.98, 1.0, 1.0],
                );
            }
            x += glyph.advance;
        }
    }

    fn draw_glyph(
        &self,
        command_buffer: vk::CommandBuffer,
        extent: vk::Extent2D,
        glyph: &AtlasGlyph,
        left: f32,
        top: f32,
        color: [f32; 4],
    ) {
        let right = left + glyph.width as f32;
        let bottom = top + glyph.height as f32;
        self.draw_quad(
            command_buffer,
            extent,
            [left, top, right, bottom],
            [
                glyph.x as f32 / ATLAS_WIDTH as f32,
                glyph.y as f32 / ATLAS_HEIGHT as f32,
                (glyph.x + glyph.width) as f32 / ATLAS_WIDTH as f32,
                (glyph.y + glyph.height) as f32 / ATLAS_HEIGHT as f32,
            ],
            color,
        );
    }

    fn draw_quad(
        &self,
        command_buffer: vk::CommandBuffer,
        extent: vk::Extent2D,
        rectangle: [f32; 4],
        texture_coordinates: [f32; 4],
        color: [f32; 4],
    ) {
        let color = if self.hdr10_output {
            [
                pq_encode(color[0] * 203.0),
                pq_encode(color[1] * 203.0),
                pq_encode(color[2] * 203.0),
                color[3],
            ]
        } else {
            color
        };
        let values = [
            rectangle[0] / extent.width as f32 * 2.0 - 1.0,
            1.0 - rectangle[1] / extent.height as f32 * 2.0,
            rectangle[2] / extent.width as f32 * 2.0 - 1.0,
            1.0 - rectangle[3] / extent.height as f32 * 2.0,
            texture_coordinates[0],
            texture_coordinates[1],
            texture_coordinates[2],
            texture_coordinates[3],
            color[0],
            color[1],
            color[2],
            color[3],
        ];
        let bytes = unsafe {
            std::slice::from_raw_parts(values.as_ptr().cast::<u8>(), size_of_val(&values))
        };
        unsafe {
            self.device.cmd_push_constants(
                command_buffer,
                self.pipeline_layout,
                vk::ShaderStageFlags::VERTEX | vk::ShaderStageFlags::FRAGMENT,
                0,
                bytes,
            );
            self.device.cmd_draw(command_buffer, 6, 1, 0, 0);
        }
    }

    pub(crate) fn destroy(&mut self) {
        unsafe {
            self.device.destroy_pipeline(self.pipeline, None);
            self.device
                .destroy_pipeline_layout(self.pipeline_layout, None);
            self.device
                .destroy_descriptor_pool(self.descriptor_pool, None);
            self.device
                .destroy_descriptor_set_layout(self.descriptor_set_layout, None);
            self.device.destroy_sampler(self.sampler, None);
            self.device.destroy_image_view(self.view, None);
            self.device.destroy_image(self.image, None);
            self.device.free_memory(self.memory, None);
        }
    }
}

fn pq_encode(nits: f32) -> f32 {
    let m1 = 2610.0 / 16384.0;
    let m2 = 2523.0 / 32.0;
    let c1 = 3424.0 / 4096.0;
    let c2 = 2413.0 / 128.0;
    let c3 = 2392.0 / 128.0;
    let p = (nits.max(0.0) / 10_000.0).powf(m1);
    ((c1 + c2 * p) / (1.0 + c3 * p)).powf(m2)
}

fn overlay_text_size(text: &str, glyphs: &HashMap<char, AtlasGlyph>) -> (f32, usize) {
    let mut maximum_width = 0.0_f32;
    let mut current_width = 0.0_f32;
    let mut line_count = 1;
    for character in text.chars() {
        if character == '\n' {
            maximum_width = maximum_width.max(current_width);
            current_width = 0.0;
            line_count += 1;
        } else if let Some(glyph) = glyphs.get(&character) {
            current_width += glyph.advance;
        }
    }
    (maximum_width.max(current_width), line_count)
}

fn build_atlas() -> Result<(Vec<u8>, HashMap<char, AtlasGlyph>), String> {
    let font = Font::from_bytes(
        include_bytes!("../assets/LiberationMono-Regular.ttf").as_slice(),
        FontSettings::default(),
    )
    .map_err(|error| error.to_string())?;
    let mut pixels = vec![0; (ATLAS_WIDTH * ATLAS_HEIGHT) as usize];
    let mut glyphs = HashMap::new();
    let mut x = 1_u32;
    let mut y = 1_u32;
    let mut row_height = 0_u32;
    for byte in FIRST_GLYPH..=LAST_GLYPH {
        let character = char::from(byte);
        let (metrics, bitmap) = font.rasterize(character, FONT_SIZE);
        let width = metrics.width as u32;
        let height = metrics.height as u32;
        if x + width + 1 > ATLAS_WIDTH {
            x = 1;
            y += row_height + 1;
            row_height = 0;
        }
        if y + height + 1 > ATLAS_HEIGHT {
            return Err("embedded font glyphs do not fit the Vulkan atlas".into());
        }
        for row in 0..height {
            let source = row as usize * width as usize;
            let destination = ((y + row) * ATLAS_WIDTH + x) as usize;
            pixels[destination..destination + width as usize]
                .copy_from_slice(&bitmap[source..source + width as usize]);
        }
        glyphs.insert(
            character,
            AtlasGlyph {
                x,
                y,
                width,
                height,
                xmin: metrics.xmin,
                ymin: metrics.ymin,
                advance: metrics.advance_width,
            },
        );
        x += width + 1;
        row_height = row_height.max(height);
    }
    Ok((pixels, glyphs))
}

fn upload_atlas(
    device: &ash::Device,
    memory_properties: &vk::PhysicalDeviceMemoryProperties,
    queue: vk::Queue,
    command_pool: vk::CommandPool,
    pixels: &[u8],
) -> Result<(vk::Image, vk::DeviceMemory, vk::ImageView), String> {
    let buffer = unsafe {
        device.create_buffer(
            &vk::BufferCreateInfo::default()
                .size(pixels.len() as u64)
                .usage(vk::BufferUsageFlags::TRANSFER_SRC)
                .sharing_mode(vk::SharingMode::EXCLUSIVE),
            None,
        )
    }
    .map_err(|error| error.to_string())?;
    let buffer_requirements = unsafe { device.get_buffer_memory_requirements(buffer) };
    let buffer_type = memory_type(
        memory_properties,
        buffer_requirements.memory_type_bits,
        vk::MemoryPropertyFlags::HOST_VISIBLE | vk::MemoryPropertyFlags::HOST_COHERENT,
    )?;
    let buffer_memory = unsafe {
        device.allocate_memory(
            &vk::MemoryAllocateInfo::default()
                .allocation_size(buffer_requirements.size)
                .memory_type_index(buffer_type),
            None,
        )
    }
    .map_err(|error| error.to_string())?;
    unsafe {
        device
            .bind_buffer_memory(buffer, buffer_memory, 0)
            .map_err(|error| error.to_string())?;
        let mapped = device
            .map_memory(
                buffer_memory,
                0,
                pixels.len() as u64,
                vk::MemoryMapFlags::empty(),
            )
            .map_err(|error| error.to_string())?;
        std::ptr::copy_nonoverlapping(pixels.as_ptr(), mapped.cast(), pixels.len());
        device.unmap_memory(buffer_memory);
    }
    let image = unsafe {
        device.create_image(
            &vk::ImageCreateInfo::default()
                .image_type(vk::ImageType::TYPE_2D)
                .format(vk::Format::R8_UNORM)
                .extent(vk::Extent3D {
                    width: ATLAS_WIDTH,
                    height: ATLAS_HEIGHT,
                    depth: 1,
                })
                .mip_levels(1)
                .array_layers(1)
                .samples(vk::SampleCountFlags::TYPE_1)
                .tiling(vk::ImageTiling::OPTIMAL)
                .usage(vk::ImageUsageFlags::TRANSFER_DST | vk::ImageUsageFlags::SAMPLED)
                .sharing_mode(vk::SharingMode::EXCLUSIVE)
                .initial_layout(vk::ImageLayout::UNDEFINED),
            None,
        )
    }
    .map_err(|error| error.to_string())?;
    let image_requirements = unsafe { device.get_image_memory_requirements(image) };
    let image_type = memory_type(
        memory_properties,
        image_requirements.memory_type_bits,
        vk::MemoryPropertyFlags::DEVICE_LOCAL,
    )?;
    let image_memory = unsafe {
        device.allocate_memory(
            &vk::MemoryAllocateInfo::default()
                .allocation_size(image_requirements.size)
                .memory_type_index(image_type),
            None,
        )
    }
    .map_err(|error| error.to_string())?;
    unsafe {
        device
            .bind_image_memory(image, image_memory, 0)
            .map_err(|error| error.to_string())?;
    }
    let command_buffer = unsafe {
        device.allocate_command_buffers(
            &vk::CommandBufferAllocateInfo::default()
                .command_pool(command_pool)
                .level(vk::CommandBufferLevel::PRIMARY)
                .command_buffer_count(1),
        )
    }
    .map_err(|error| error.to_string())?[0];
    unsafe {
        device
            .begin_command_buffer(
                command_buffer,
                &vk::CommandBufferBeginInfo::default()
                    .flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT),
            )
            .map_err(|error| error.to_string())?;
        let to_transfer = [vk::ImageMemoryBarrier::default()
            .dst_access_mask(vk::AccessFlags::TRANSFER_WRITE)
            .old_layout(vk::ImageLayout::UNDEFINED)
            .new_layout(vk::ImageLayout::TRANSFER_DST_OPTIMAL)
            .image(image)
            .subresource_range(color_subresource_range())];
        device.cmd_pipeline_barrier(
            command_buffer,
            vk::PipelineStageFlags::TOP_OF_PIPE,
            vk::PipelineStageFlags::TRANSFER,
            vk::DependencyFlags::empty(),
            &[],
            &[],
            &to_transfer,
        );
        let regions = [vk::BufferImageCopy::default()
            .image_subresource(vk::ImageSubresourceLayers {
                aspect_mask: vk::ImageAspectFlags::COLOR,
                mip_level: 0,
                base_array_layer: 0,
                layer_count: 1,
            })
            .image_extent(vk::Extent3D {
                width: ATLAS_WIDTH,
                height: ATLAS_HEIGHT,
                depth: 1,
            })];
        device.cmd_copy_buffer_to_image(
            command_buffer,
            buffer,
            image,
            vk::ImageLayout::TRANSFER_DST_OPTIMAL,
            &regions,
        );
        let to_sample = [vk::ImageMemoryBarrier::default()
            .src_access_mask(vk::AccessFlags::TRANSFER_WRITE)
            .dst_access_mask(vk::AccessFlags::SHADER_READ)
            .old_layout(vk::ImageLayout::TRANSFER_DST_OPTIMAL)
            .new_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)
            .image(image)
            .subresource_range(color_subresource_range())];
        device.cmd_pipeline_barrier(
            command_buffer,
            vk::PipelineStageFlags::TRANSFER,
            vk::PipelineStageFlags::FRAGMENT_SHADER,
            vk::DependencyFlags::empty(),
            &[],
            &[],
            &to_sample,
        );
        device
            .end_command_buffer(command_buffer)
            .map_err(|error| error.to_string())?;
        let command_buffers = [command_buffer];
        device
            .queue_submit(
                queue,
                &[vk::SubmitInfo::default().command_buffers(&command_buffers)],
                vk::Fence::null(),
            )
            .map_err(|error| error.to_string())?;
        device
            .queue_wait_idle(queue)
            .map_err(|error| error.to_string())?;
        device.free_command_buffers(command_pool, &[command_buffer]);
        device.destroy_buffer(buffer, None);
        device.free_memory(buffer_memory, None);
    }
    let view = unsafe {
        device.create_image_view(
            &vk::ImageViewCreateInfo::default()
                .image(image)
                .view_type(vk::ImageViewType::TYPE_2D)
                .format(vk::Format::R8_UNORM)
                .subresource_range(color_subresource_range()),
            None,
        )
    }
    .map_err(|error| error.to_string())?;
    Ok((image, image_memory, view))
}

fn create_pipeline(
    device: &ash::Device,
    render_pass: vk::RenderPass,
    layout: vk::PipelineLayout,
) -> Result<vk::Pipeline, String> {
    let vertex = create_shader_module(device, include_bytes!("shaders/overlay.vert.spv"))?;
    let fragment = create_shader_module(device, include_bytes!("shaders/overlay.frag.spv"))?;
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
    let blend_attachment = [vk::PipelineColorBlendAttachmentState::default()
        .blend_enable(true)
        .src_color_blend_factor(vk::BlendFactor::SRC_ALPHA)
        .dst_color_blend_factor(vk::BlendFactor::ONE_MINUS_SRC_ALPHA)
        .color_blend_op(vk::BlendOp::ADD)
        .src_alpha_blend_factor(vk::BlendFactor::ONE)
        .dst_alpha_blend_factor(vk::BlendFactor::ONE_MINUS_SRC_ALPHA)
        .alpha_blend_op(vk::BlendOp::ADD)
        .color_write_mask(vk::ColorComponentFlags::RGBA)];
    let dynamic = [vk::DynamicState::VIEWPORT, vk::DynamicState::SCISSOR];
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
    let blend = vk::PipelineColorBlendStateCreateInfo::default().attachments(&blend_attachment);
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
    unsafe {
        device.create_shader_module(&vk::ShaderModuleCreateInfo::default().code(&words), None)
    }
    .map_err(|error| error.to_string())
}

fn memory_type(
    properties: &vk::PhysicalDeviceMemoryProperties,
    allowed: u32,
    required: vk::MemoryPropertyFlags,
) -> Result<u32, String> {
    (0..properties.memory_type_count)
        .find(|index| {
            allowed & (1 << index) != 0
                && properties.memory_types[*index as usize]
                    .property_flags
                    .contains(required)
        })
        .ok_or_else(|| "Vulkan found no suitable overlay memory type".into())
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn panel_measurement_uses_longest_line_and_all_lines() {
        let (_, glyphs) = build_atlas().unwrap();
        let (single_width, single_lines) = overlay_text_size("Target bitrate", &glyphs);
        let (multi_width, multi_lines) =
            overlay_text_size("Target bitrate\nMaximum bitrate 100 Mbit/s", &glyphs);

        assert_eq!(single_lines, 1);
        assert_eq!(multi_lines, 2);
        assert!(multi_width > single_width);
    }
}
