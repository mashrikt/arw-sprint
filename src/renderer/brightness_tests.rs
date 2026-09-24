use super::*;

#[test]
fn brightness_is_bounded_and_neutral_for_invalid_values() {
    for (input, expected) in [(-9.0, -3.0), (9.0, 3.0), (0.0, 0.0), (1.0, 1.0)] {
        assert_eq!(bounded_brightness_stops(input), expected);
    }
    for input in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
        assert_eq!(bounded_brightness_stops(input), 0.0);
    }
}

#[test]
fn brightness_uses_only_transform_padding_and_leaves_ui_neutral() {
    for orientation in 1..=8 {
        let neutral = transform_bytes([0.2, 0.3, -0.4, 0.5], orientation);
        assert_eq!(&neutral[28..32], &1.0f32.to_le_bytes());
        let bright = photo_transform_bytes([0.2, 0.3, -0.4, 0.5], orientation, 8.0);
        assert_eq!(&neutral[..28], &bright[..28]);
        assert_eq!(&neutral[32..], &bright[32..]);
        assert_eq!(&bright[28..32], &8.0f32.to_le_bytes());
    }
}

/// Run the actual photo shader on Metal, including hardware sRGB decoding and
/// encoding. Reuse the same source texture for every adjustment; only uniforms
/// change. A neighboring UI quad stays neutral and uncovered pixels stay dark.
#[test]
fn metal_brightness_adjusts_linear_rgb_preserves_alpha_ui_and_background() {
    let instance = wgpu::Instance::new(&wgpu::InstanceDescriptor {
        backends: wgpu::Backends::METAL,
        ..Default::default()
    });
    let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
        power_preference: wgpu::PowerPreference::LowPower,
        compatible_surface: None,
        force_fallback_adapter: false,
    }))
    .expect("brightness GPU test needs a Metal adapter");
    let (device, queue) = pollster::block_on(adapter.request_device(
        &wgpu::DeviceDescriptor {
            label: Some("display brightness regression"),
            required_features: wgpu::Features::empty(),
            required_limits: wgpu::Limits::downlevel_defaults(),
            memory_hints: wgpu::MemoryHints::MemoryUsage,
        },
        None,
    ))
    .unwrap();
    let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
        label: Some("production photo shader"),
        source: wgpu::ShaderSource::Wgsl(include_str!("image.wgsl").into()),
    });
    let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
        label: Some("brightness regression pipeline"),
        layout: None,
        vertex: wgpu::VertexState {
            module: &shader,
            entry_point: Some("vs_main"),
            compilation_options: Default::default(),
            buffers: &[],
        },
        fragment: Some(wgpu::FragmentState {
            module: &shader,
            entry_point: Some("fs_main"),
            compilation_options: Default::default(),
            targets: &[Some(wgpu::ColorTargetState {
                format: wgpu::TextureFormat::Rgba8UnormSrgb,
                blend: None,
                write_mask: wgpu::ColorWrites::ALL,
            })],
        }),
        primitive: wgpu::PrimitiveState::default(),
        depth_stencil: None,
        multisample: wgpu::MultisampleState::default(),
        multiview: None,
        cache: None,
    });
    let source = device.create_texture(&wgpu::TextureDescriptor {
        label: Some("known sRGB pixels"),
        size: wgpu::Extent3d {
            width: 1,
            height: 1,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: wgpu::TextureFormat::Rgba8UnormSrgb,
        usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
        view_formats: &[],
    });
    let pixel = [64, 96, 128, 173];
    queue.write_texture(
        source.as_image_copy(),
        &pixel,
        wgpu::TexelCopyBufferLayout {
            offset: 0,
            bytes_per_row: Some(4),
            rows_per_image: Some(1),
        },
        source.size(),
    );
    let source_view = source.create_view(&Default::default());
    let sampler = device.create_sampler(&Default::default());
    let image_binding = texture::image_binding(
        &device,
        &pipeline.get_bind_group_layout(0),
        &sampler,
        &source_view,
    );
    let layout = pipeline.get_bind_group_layout(1);
    let (photo_uniform, photo_binding) = transform_binding(&device, &layout);
    let (ui_uniform, ui_binding) = transform_binding(&device, &layout);
    queue.write_buffer(
        &ui_uniform,
        0,
        &transform_bytes([1.0 / 3.0, 1.0, 0.0, 0.0], 1),
    );
    let output = device.create_texture(&wgpu::TextureDescriptor {
        label: Some("brightness output"),
        size: wgpu::Extent3d {
            width: 3,
            height: 1,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: wgpu::TextureFormat::Rgba8UnormSrgb,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
        view_formats: &[],
    });
    let view = output.create_view(&Default::default());
    let readback = device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("brightness readback"),
        size: 256,
        usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
        mapped_at_creation: false,
    });
    for stops in [0.0f32, 1.0, -1.0, 3.0, -3.0, 0.0] {
        let multiplier = bounded_brightness_stops(stops).exp2();
        queue.write_buffer(
            &photo_uniform,
            0,
            &photo_transform_bytes([1.0 / 3.0, 1.0, -2.0 / 3.0, 0.0], 1, multiplier),
        );
        let mut encoder = device.create_command_encoder(&Default::default());
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("brightness verification"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color::BLACK),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
            });
            pass.set_pipeline(&pipeline);
            pass.set_bind_group(0, &image_binding, &[]);
            for binding in [&photo_binding, &ui_binding] {
                pass.set_bind_group(1, binding, &[]);
                pass.draw(0..6, 0..1);
            }
        }
        encoder.copy_texture_to_buffer(
            output.as_image_copy(),
            wgpu::TexelCopyBufferInfo {
                buffer: &readback,
                layout: wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(256),
                    rows_per_image: Some(1),
                },
            },
            output.size(),
        );
        queue.submit([encoder.finish()]);
        let (sender, receiver) = std::sync::mpsc::sync_channel(1);
        readback
            .slice(..)
            .map_async(wgpu::MapMode::Read, move |result| {
                sender.send(result).unwrap();
            });
        let _ = device.poll(wgpu::Maintain::Wait);
        receiver.recv().unwrap().unwrap();
        let mapped = readback.slice(..).get_mapped_range();
        for channel in 0..3 {
            let srgb = f32::from(pixel[channel]) / 255.0;
            let linear = ((srgb + 0.055) / 1.055).powf(2.4) * multiplier;
            let adjusted = if linear <= 0.003_130_8 {
                12.92 * linear
            } else {
                1.055 * linear.powf(1.0 / 2.4) - 0.055
            };
            let expected = (adjusted.clamp(0.0, 1.0) * 255.0).round() as u8;
            assert!(
                mapped[channel].abs_diff(expected) <= 1,
                "{stops} stops, channel {channel}: {} vs {expected}",
                mapped[channel],
            );
            assert!(mapped[4 + channel].abs_diff(pixel[channel]) <= 1);
        }
        assert_eq!(mapped[3], pixel[3], "photo alpha changed");
        assert_eq!(mapped[7], pixel[3], "UI alpha changed");
        assert_eq!(&mapped[8..12], &[0, 0, 0, 255], "background changed");
        drop(mapped);
        readback.unmap();
    }
}
