//! Renderer that makes it easy to draw textured 2d rectangles
//!
//! Implementation details:
//! We assume the standard usecase are individual textured rectangles.
//! Since we're not allowed to bind many textures at once (no widespread bindless support!),
//! we are forced to have individual bind groups per rectangle and thus a draw call per rectangle.

use std::num::NonZeroU64;

use smallvec::smallvec;

use crate::{
    include_file,
    renderer::utils::next_multiple_of,
    resource_managers::{ResourceManagerError, Texture2DHandle},
    view_builder::ViewBuilder,
    wgpu_resources::{
        BindGroupDesc, BindGroupEntry, BindGroupLayoutDesc, BufferDesc, GpuBindGroupHandleStrong,
        GpuBindGroupLayoutHandle, GpuRenderPipelineHandle, PipelineLayoutDesc, RenderPipelineDesc,
        SamplerDesc, ShaderModuleDesc,
    },
};

use super::*;

mod gpu_data {
    use crate::wgpu_buffer_types;

    // Keep in sync with mirror in rectangle.wgsl
    #[repr(C)]
    #[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
    pub struct UniformBuffer {
        pub top_left_corner_position: wgpu_buffer_types::Vec3,
        pub extent_u: wgpu_buffer_types::Vec3,
        pub extent_v: wgpu_buffer_types::Vec3,
    }
}

/// Texture filter setting for magnification (a texel covers several pixels).
#[derive(Debug)]
pub enum TextureFilterMag {
    Linear,
    Nearest,
    // TODO(andreas): Offer advanced (shader implemented) filters like cubic?
}

/// Texture filter setting for minification (several texels fall to one pixel).
#[derive(Debug)]
pub enum TextureFilterMin {
    Linear,
    Nearest,
    // TODO(andreas): Offer mipmapping here?
}

pub struct Rectangle {
    /// Top left corner position in world space.
    pub top_left_corner_position: glam::Vec3,
    /// Vector that spans up the rectangle from its top left corner along the u axis of the texture.
    pub extent_u: glam::Vec3,
    /// Vector that spans up the rectangle from its top left corner along the v axis of the texture.
    pub extent_v: glam::Vec3,

    /// Texture that fills the rectangle
    pub texture: Texture2DHandle,

    pub texture_filter_magnification: TextureFilterMag,
    pub texture_filter_minification: TextureFilterMin,
    // TODO(andreas): additional options like color map, tinting etc.
}

#[derive(Clone)]
pub struct RectangleDrawData {
    bind_groups: Vec<GpuBindGroupHandleStrong>,
}

impl Drawable for RectangleDrawData {
    type Renderer = RectangleRenderer;
}

impl RectangleDrawData {
    pub fn new(
        ctx: &mut RenderContext,
        rectangles: &[Rectangle],
    ) -> Result<Self, ResourceManagerError> {
        crate::profile_function!();

        let rectangle_renderer = ctx.renderers.get_or_create::<_, RectangleRenderer>(
            &ctx.shared_renderer_data,
            &mut ctx.resource_pools,
            &ctx.device,
            &mut ctx.resolver,
        );

        if rectangles.is_empty() {
            return Ok(RectangleDrawData {
                bind_groups: Vec::new(),
            });
        }

        let uniform_buffer_size = std::mem::size_of::<gpu_data::UniformBuffer>();
        let allocation_size_per_uniform_buffer = next_multiple_of(
            uniform_buffer_size as u32,
            ctx.device.limits().min_uniform_buffer_offset_alignment,
        ) as u64;
        let combined_buffers_size = allocation_size_per_uniform_buffer * rectangles.len() as u64;

        // Allocate all constant buffers at once.
        // TODO(andreas): This should come from a per-frame allocator!
        let uniform_buffer = ctx.resource_pools.buffers.alloc(
            &ctx.device,
            &BufferDesc {
                label: "rectangle uniform buffers".into(),
                size: allocation_size_per_uniform_buffer * rectangles.len() as u64,
                usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::UNIFORM,
            },
        );

        // Fill staging buffer in a separate loop to avoid borrow checker issues
        {
            // TODO(andreas): This should come from a staging buffer.
            let mut staging_buffer = ctx.queue.write_buffer_with(
                ctx.resource_pools
                    .buffers
                    .get_resource(&uniform_buffer)
                    .unwrap(),
                0,
                NonZeroU64::new(combined_buffers_size).unwrap(),
            );

            for (i, rectangle) in rectangles.iter().enumerate() {
                let offset = i * allocation_size_per_uniform_buffer as usize;

                // CAREFUL: Memory from `write_buffer_with` may not be aligned, causing bytemuck to fail at runtime if we use it to cast the memory to a slice!
                // I.e. this will crash randomly:
                //
                // let target_buffer = bytemuck::from_bytes_mut::<gpu_data::UniformBuffer>(
                //     &mut staging_buffer[offset..(offset + uniform_buffer_size)],
                // );
                //
                // TODO(andreas): with our own staging buffers we could fix this very easily

                staging_buffer[offset..(offset + uniform_buffer_size)].copy_from_slice(
                    bytemuck::bytes_of(&gpu_data::UniformBuffer {
                        top_left_corner_position: rectangle.top_left_corner_position.into(),
                        extent_u: rectangle.extent_u.into(),
                        extent_v: rectangle.extent_v.into(),
                    }),
                );
            }
        }

        let mut bind_groups = Vec::with_capacity(rectangles.len());
        for (i, rectangle) in rectangles.iter().enumerate() {
            let texture = ctx.texture_manager_2d.get_or_create_gpu_resource(
                &mut ctx.resource_pools,
                &ctx.device,
                &ctx.queue,
                rectangle.texture,
            )?;

            let sampler = ctx.resource_pools.samplers.get_or_create(
                &ctx.device,
                &SamplerDesc {
                    label: format!(
                        "rectangle sampler mag {:?} min {:?}",
                        rectangle.texture_filter_magnification,
                        rectangle.texture_filter_minification
                    )
                    .into(),
                    mag_filter: match rectangle.texture_filter_magnification {
                        TextureFilterMag::Linear => wgpu::FilterMode::Linear,
                        TextureFilterMag::Nearest => wgpu::FilterMode::Nearest,
                    },
                    min_filter: match rectangle.texture_filter_minification {
                        TextureFilterMin::Linear => wgpu::FilterMode::Linear,
                        TextureFilterMin::Nearest => wgpu::FilterMode::Nearest,
                    },
                    mipmap_filter: wgpu::FilterMode::Nearest,
                    ..Default::default()
                },
            );

            bind_groups.push(ctx.resource_pools.bind_groups.alloc(
                &ctx.device,
                &BindGroupDesc {
                    label: "rectangle".into(),
                    entries: smallvec![
                        BindGroupEntry::Buffer {
                            handle: *uniform_buffer,
                            offset: i as u64 * allocation_size_per_uniform_buffer,
                            size: NonZeroU64::new(uniform_buffer_size as u64),
                        },
                        BindGroupEntry::DefaultTextureView(*texture),
                        BindGroupEntry::Sampler(sampler)
                    ],
                    layout: rectangle_renderer.bind_group_layout,
                },
                &ctx.resource_pools.bind_group_layouts,
                &ctx.resource_pools.textures,
                &ctx.resource_pools.buffers,
                &ctx.resource_pools.samplers,
            ));
        }

        Ok(RectangleDrawData { bind_groups })
    }
}

pub struct RectangleRenderer {
    render_pipeline: GpuRenderPipelineHandle,
    bind_group_layout: GpuBindGroupLayoutHandle,
}

impl Renderer for RectangleRenderer {
    type DrawData = RectangleDrawData;

    fn create_renderer<Fs: FileSystem>(
        shared_data: &SharedRendererData,
        pools: &mut WgpuResourcePools,
        device: &wgpu::Device,
        resolver: &mut FileResolver<Fs>,
    ) -> Self {
        crate::profile_function!();

        let bind_group_layout = pools.bind_group_layouts.get_or_create(
            device,
            &BindGroupLayoutDesc {
                label: "rectangles".into(),
                entries: vec![
                    wgpu::BindGroupLayoutEntry {
                        binding: 0,
                        visibility: wgpu::ShaderStages::VERTEX,
                        ty: wgpu::BindingType::Buffer {
                            ty: wgpu::BufferBindingType::Uniform,
                            // We could use dynamic offset here into a single large buffer.
                            // But we have to set a new texture anyways and its doubtful that splitting the bind group is of any use.
                            has_dynamic_offset: false,
                            min_binding_size: (std::mem::size_of::<gpu_data::UniformBuffer>()
                                as u64)
                                .try_into()
                                .ok(),
                        },
                        count: None,
                    },
                    wgpu::BindGroupLayoutEntry {
                        binding: 1,
                        visibility: wgpu::ShaderStages::FRAGMENT,
                        ty: wgpu::BindingType::Texture {
                            sample_type: wgpu::TextureSampleType::Float { filterable: true },
                            view_dimension: wgpu::TextureViewDimension::D2,
                            multisampled: false,
                        },
                        count: None,
                    },
                    wgpu::BindGroupLayoutEntry {
                        binding: 2,
                        visibility: wgpu::ShaderStages::FRAGMENT,
                        ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                        count: None,
                    },
                ],
            },
        );

        let pipeline_layout = pools.pipeline_layouts.get_or_create(
            device,
            &PipelineLayoutDesc {
                label: "rectangle".into(),
                entries: vec![shared_data.global_bindings.layout, bind_group_layout],
            },
            &pools.bind_group_layouts,
        );

        let shader_module = pools.shader_modules.get_or_create(
            device,
            resolver,
            &ShaderModuleDesc {
                label: "rectangle".into(),
                source: include_file!("../../shader/rectangle.wgsl"),
            },
        );

        let render_pipeline = pools.render_pipelines.get_or_create(
            device,
            &RenderPipelineDesc {
                label: "rectangle".into(),
                pipeline_layout,
                vertex_entrypoint: "vs_main".into(),
                vertex_handle: shader_module,
                fragment_entrypoint: "fs_main".into(),
                fragment_handle: shader_module,
                vertex_buffers: smallvec![],
                render_targets: smallvec![Some(ViewBuilder::MAIN_TARGET_COLOR_FORMAT.into())],
                primitive: wgpu::PrimitiveState {
                    topology: wgpu::PrimitiveTopology::TriangleStrip,
                    cull_mode: None,
                    ..Default::default()
                },
                depth_stencil: ViewBuilder::MAIN_TARGET_DEFAULT_DEPTH_STATE,
                multisample: ViewBuilder::MAIN_TARGET_DEFAULT_MSAA_STATE,
            },
            &pools.pipeline_layouts,
            &pools.shader_modules,
        );

        RectangleRenderer {
            render_pipeline,
            bind_group_layout,
        }
    }

    fn draw<'a>(
        &self,
        pools: &'a WgpuResourcePools,
        pass: &mut wgpu::RenderPass<'a>,
        draw_data: &Self::DrawData,
    ) -> anyhow::Result<()> {
        crate::profile_function!();
        if draw_data.bind_groups.is_empty() {
            return Ok(());
        }

        let pipeline = pools.render_pipelines.get_resource(self.render_pipeline)?;
        pass.set_pipeline(pipeline);

        for bind_group in &draw_data.bind_groups {
            let bind_group = pools.bind_groups.get_resource(bind_group)?;
            pass.set_bind_group(1, bind_group, &[]);
            pass.draw(0..4, 0..1);
        }

        Ok(())
    }
}
