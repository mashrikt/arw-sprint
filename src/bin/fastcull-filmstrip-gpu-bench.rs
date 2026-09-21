//! Experimental cached-filmstrip cost only; not part of the desktop application.
#![deny(unsafe_code)]

#[cfg(not(feature = "desktop"))]
fn main() {
    eprintln!("Build this benchmark with --features desktop.");
    std::process::exit(2);
}

#[cfg(feature = "desktop")]
fn main() {
    if let Err(error) = experiment::run() {
        eprintln!("filmstrip benchmark: {error}");
        std::process::exit(1);
    }
}

#[cfg(feature = "desktop")]
mod experiment {
    use std::time::Instant;
    const FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Bgra8UnormSrgb;
    const THUMB: [u32; 2] = [160, 120];
    const COUNT: usize = 12;

    fn texture(device: &wgpu::Device, width: u32, height: u32) -> wgpu::Texture {
        device.create_texture(&wgpu::TextureDescriptor {
            label: Some("filmstrip experiment texture"),
            size: wgpu::Extent3d {
                width,
                height,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: FORMAT,
            usage: wgpu::TextureUsages::TEXTURE_BINDING
                | wgpu::TextureUsages::COPY_DST
                | wgpu::TextureUsages::RENDER_ATTACHMENT,
            view_formats: &[],
        })
    }

    fn pipeline(
        device: &wgpu::Device,
        shader: &wgpu::ShaderModule,
        vertex_entry: &str,
        fragment_entry: &str,
    ) -> wgpu::RenderPipeline {
        device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("filmstrip experiment pipeline"),
            layout: None,
            vertex: wgpu::VertexState {
                module: shader,
                entry_point: Some(vertex_entry),
                compilation_options: Default::default(),
                buffers: &[],
            },
            fragment: Some(wgpu::FragmentState {
                module: shader,
                entry_point: Some(fragment_entry),
                compilation_options: Default::default(),
                targets: &[Some(wgpu::ColorTargetState {
                    format: FORMAT,
                    blend: None,
                    write_mask: wgpu::ColorWrites::ALL,
                })],
            }),
            primitive: Default::default(),
            depth_stencil: None,
            multisample: Default::default(),
            multiview: None,
            cache: None,
        })
    }

    fn bindings(
        device: &wgpu::Device,
        pipeline: &wgpu::RenderPipeline,
        sampler: &wgpu::Sampler,
        texture: &wgpu::Texture,
        placement: [f32; 4],
    ) -> (wgpu::BindGroup, wgpu::BindGroup) {
        let view = texture.create_view(&Default::default());
        let image = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: None,
            layout: &pipeline.get_bind_group_layout(0),
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::TextureView(&view),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::Sampler(sampler),
                },
            ],
        });
        let buffer = device.create_buffer(&wgpu::BufferDescriptor {
            label: None,
            size: 48,
            usage: wgpu::BufferUsages::UNIFORM,
            mapped_at_creation: true,
        });
        for (slot, value) in buffer
            .slice(..)
            .get_mapped_range_mut()
            .chunks_exact_mut(4)
            .zip(
                placement
                    .into_iter()
                    .chain([1.0, 0.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0]),
            )
        {
            slot.copy_from_slice(&value.to_le_bytes());
        }
        buffer.unmap();
        let transform = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: None,
            layout: &pipeline.get_bind_group_layout(1),
            entries: &[wgpu::BindGroupEntry {
                binding: 0,
                resource: buffer.as_entire_binding(),
            }],
        });
        (image, transform)
    }

    fn report(label: &str, samples: &[f64]) {
        let mut samples = samples.to_vec();
        samples.sort_by(f64::total_cmp);
        let quantile =
            |percent: usize| samples[(samples.len() * percent).div_ceil(100).saturating_sub(1)];
        println!(
            "{label}: p50 {:.3} ms, p95 {:.3} ms, p99 {:.3} ms",
            quantile(50),
            quantile(95),
            quantile(99)
        );
    }

    pub fn run() -> Result<(), String> {
        let args: Vec<_> = std::env::args().skip(1).collect();
        if args.first().is_some_and(|s| s == "--help" || s == "-h") {
            println!("fastcull-filmstrip-gpu-bench [--pairs 80] [--width 2560] [--height 1600]\nBounds: 10..240 pairs; dimensions 640..4096. Metal offscreen timing; no photos or XMP accessed.");
            return Ok(());
        }
        let (mut pairs, mut width, mut height) = (80u32, 2560u32, 1600u32);
        for option in args.chunks(2) {
            let [key, value] = option else {
                return Err("each option requires an integer value".into());
            };
            let value = value.parse::<u32>().map_err(|_| "invalid integer")?;
            match key.as_str() {
                "--pairs" => pairs = value,
                "--width" => width = value,
                "--height" => height = value,
                _ => return Err(format!("unknown option: {key}")),
            }
        }
        if !(10..=240).contains(&pairs)
            || !(640..=4096).contains(&width)
            || !(640..=4096).contains(&height)
        {
            return Err("require 10..240 pairs and 640..4096 pixel dimensions".into());
        }
        let pairs = pairs as usize;
        let instance = wgpu::Instance::new(&wgpu::InstanceDescriptor {
            backends: wgpu::Backends::METAL,
            ..Default::default()
        });
        let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::LowPower,
            compatible_surface: None,
            force_fallback_adapter: false,
        }))
        .ok_or("no Metal adapter")?;
        if adapter.limits().max_texture_dimension_2d < 7008 {
            return Err("GPU does not support the 7008-pixel source texture".into());
        }
        let (device, queue) = pollster::block_on(adapter.request_device(
            &wgpu::DeviceDescriptor {
                label: Some("cached filmstrip experiment"),
                required_features: wgpu::Features::empty(),
                required_limits:
                    wgpu::Limits::downlevel_defaults().using_resolution(adapter.limits()),
                memory_hints: wgpu::MemoryHints::MemoryUsage,
            },
            None,
        ))
        .map_err(|e| e.to_string())?;
        let shader_source = format!(
            "{}\n{}",
            include_str!("../renderer/image.wgsl"),
            r#"
@fragment
fn fs_pattern(@builtin(position) position: vec4<f32>) -> @location(0) vec4<f32> {
    var hash = (u32(position.x) * 1664525u) ^ (u32(position.y) * 1013904223u);
    hash = hash ^ (hash >> 16u);
    return vec4<f32>(f32(hash & 255u), f32((hash >> 8u) & 255u),
                     f32((hash >> 16u) & 255u), 255.0) / 255.0;
}
"#
        );
        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: None,
            source: wgpu::ShaderSource::Wgsl(shader_source.into()),
        });
        let photo_pipeline = pipeline(&device, &shader, "vs_main", "fs_main");
        let background_pipeline = pipeline(&device, &shader, "vs_background", "fs_background");
        let pattern_pipeline = pipeline(&device, &shader, "vs_background", "fs_pattern");
        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            ..Default::default()
        });
        let output = texture(&device, width, height);
        let output_view = output.create_view(&Default::default());
        let photo = texture(&device, 7008, 4672);
        let photo_view = photo.create_view(&Default::default());
        let mut encoder = device.create_command_encoder(&Default::default());
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &photo_view,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color::BLACK),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                ..Default::default()
            });
            // Materialize varied source texels once; don't benchmark sampling a
            // constant texture represented only by compressed fast-clear state.
            pass.set_pipeline(&pattern_pipeline);
            pass.draw(0..3, 0..1);
        }
        let _ = device.poll(wgpu::Maintain::WaitForSubmissionIndex(
            queue.submit([encoder.finish()]),
        ));
        let fit = (width as f32 / 7008.0).min(height as f32 / 4672.0);
        let photo_binding = bindings(
            &device,
            &photo_pipeline,
            &sampler,
            &photo,
            [
                7008.0 * fit / width as f32,
                4672.0 * fit / height as f32,
                0.0,
                0.0,
            ],
        );
        let thumbnails: Vec<_> = (0..COUNT)
            .map(|_| texture(&device, THUMB[0], THUMB[1]))
            .collect();
        let mut pixels = vec![255u8; (THUMB[0] * THUMB[1] * 4) as usize];
        for (i, pixel) in pixels.chunks_exact_mut(4).enumerate() {
            pixel[..3].copy_from_slice(&[
                (i % 251) as u8,
                ((i / THUMB[0] as usize) % 239) as u8,
                ((i / 7) % 233) as u8,
            ]);
        }
        let upload_start = Instant::now();
        for thumbnail in &thumbnails {
            queue.write_texture(
                wgpu::TexelCopyTextureInfo {
                    texture: thumbnail,
                    mip_level: 0,
                    origin: wgpu::Origin3d::ZERO,
                    aspect: wgpu::TextureAspect::All,
                },
                &pixels,
                wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(THUMB[0] * 4),
                    rows_per_image: Some(THUMB[1]),
                },
                thumbnail.size(),
            );
        }
        let _ = device.poll(wgpu::Maintain::WaitForSubmissionIndex(queue.submit([])));
        let upload_ms = upload_start.elapsed().as_secs_f64() * 1000.0;
        let cell = width as f32 / COUNT as f32;
        let thumb_width = (cell - 8.0).min(320.0);
        let thumb_height = thumb_width * THUMB[1] as f32 / THUMB[0] as f32;
        let thumb_bindings: Vec<_> = thumbnails
            .iter()
            .enumerate()
            .map(|(i, texture)| {
                bindings(
                    &device,
                    &photo_pipeline,
                    &sampler,
                    texture,
                    [
                        thumb_width / width as f32,
                        thumb_height / height as f32,
                        (i as f32 + 0.5) * cell * 2.0 / width as f32 - 1.0,
                        -1.0 + (thumb_height + 8.0) / height as f32,
                    ],
                )
            })
            .collect();
        let render = |strip: bool| {
            let start = Instant::now();
            let mut encoder = device.create_command_encoder(&Default::default());
            {
                let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                    color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                        view: &output_view,
                        resolve_target: None,
                        ops: wgpu::Operations {
                            load: wgpu::LoadOp::Clear(wgpu::Color::BLACK),
                            store: wgpu::StoreOp::Store,
                        },
                    })],
                    ..Default::default()
                });
                pass.set_pipeline(&background_pipeline);
                pass.draw(0..3, 0..1);
                pass.set_pipeline(&photo_pipeline);
                pass.set_bind_group(0, &photo_binding.0, &[]);
                pass.set_bind_group(1, &photo_binding.1, &[]);
                pass.draw(0..6, 0..1);
                if strip {
                    for (image, transform) in &thumb_bindings {
                        pass.set_bind_group(0, image, &[]);
                        pass.set_bind_group(1, transform, &[]);
                        pass.draw(0..6, 0..1);
                    }
                }
            }
            let _ = device.poll(wgpu::Maintain::WaitForSubmissionIndex(
                queue.submit([encoder.finish()]),
            ));
            start.elapsed().as_secs_f64() * 1000.0
        };
        for _ in 0..12 {
            render(false);
            render(true);
        }
        let (mut baseline, mut filmstrip, mut delta) = (
            Vec::with_capacity(pairs),
            Vec::with_capacity(pairs),
            Vec::with_capacity(pairs),
        );
        for pair in 0..pairs {
            let (base, strip) = if pair % 2 == 0 {
                (render(false), render(true))
            } else {
                let strip = render(true);
                (render(false), strip)
            };
            baseline.push(base);
            filmstrip.push(strip);
            delta.push(strip - base);
        }
        println!("Adapter: {}\nOffscreen target: {width}x{height}; pairs: {pairs}; 12 warmup pairs; alternating AB/BA order.", adapter.get_info().name);
        println!("CPU wall time from encoding through GPU completion. Not GPU timestamps, GUI/event latency, or display scanout.");
        println!("Synthetic photo texture: 7008x4672 deterministic GPU-generated noise; cached thumbnails: {COUNT} x {}x{}; no decoding, presentation, or per-frame uploads.", THUMB[0], THUMB[1]);
        println!("Same photo placement in both variants; strip overlays {COUNT} quads of {thumb_width:.1}x{thumb_height:.1} pixels.");
        println!("Thumbnail batch write/submit/completion: {upload_ms:.3} ms; texture bytes: {} ({:.2} MiB).", COUNT * pixels.len(), COUNT as f64 * pixels.len() as f64 / 1048576.0);
        report("Baseline", &baseline);
        report("With cached filmstrip", &filmstrip);
        report("Paired additional cost", &delta);
        println!("pair\torder\tbaseline_ms\tfilmstrip_ms\tdelta_ms");
        for pair in 0..pairs {
            println!(
                "{}\t{}\t{:.6}\t{:.6}\t{:.6}",
                pair + 1,
                if pair % 2 == 0 { "AB" } else { "BA" },
                baseline[pair],
                filmstrip[pair],
                delta[pair]
            );
        }
        Ok(())
    }
}
