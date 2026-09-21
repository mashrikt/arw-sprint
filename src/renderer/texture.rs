//! Image uploads run on a dedicated worker, including all completion waits.

use crate::image::cache::{DecodedImage, MemoryBudget, MemoryLease};
use std::{
    fmt,
    sync::mpsc::{Receiver, RecvError, RecvTimeoutError},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

/// Wait on an upload worker without holding wgpu's device fence during the wait.
///
/// In wgpu 24, `Maintain::Wait` retains a fence read lock while waiting for Metal;
/// a concurrent UI-thread `Queue::submit` needs that lock exclusively. Polling
/// briefly and sleeping outside wgpu lets rendering submit while a copy runs.
/// The callback still establishes completion before any upload lease is released.
#[doc(hidden)]
pub fn wait_for_upload_completion(
    device: &wgpu::Device,
    completed: &Receiver<()>,
) -> Result<(), RecvError> {
    wait_for_completion(
        || device.poll(wgpu::Maintain::Poll).is_queue_empty(),
        completed,
    )
}

fn wait_for_completion(
    mut poll: impl FnMut() -> bool,
    completed: &Receiver<()>,
) -> Result<(), RecvError> {
    loop {
        let idle = poll();
        match completed.recv_timeout(Duration::from_millis(1)) {
            Ok(()) => return Ok(()),
            Err(RecvTimeoutError::Timeout) => {}
            // A missing callback must not release staging/texture leases while
            // commands are still in flight. Drain first, then report the error.
            Err(RecvTimeoutError::Disconnected) if idle => return Err(RecvError),
            Err(RecvTimeoutError::Disconnected) => {
                std::thread::sleep(Duration::from_millis(1));
            }
        }
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub struct UploadTimings {
    pub submit_ms: f64,
    /// Elapsed from upload start until the GPU acknowledges submitted copies.
    pub completion_ms: f64,
}

pub struct PreparedTexture {
    pub width: u32,
    pub height: u32,
    pub original_width: u32,
    pub original_height: u32,
    pub orientation: u16,
    pub byte_size: usize,
    pub timings: UploadTimings,
    pub(crate) bind_group: wgpu::BindGroup,
    _texture: wgpu::Texture,
    _lease: MemoryLease,
}

#[derive(Debug)]
pub enum UploadError {
    Budget { needed: usize, available: usize },
    Invalid(String),
    Gpu(String),
}

impl fmt::Display for UploadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Budget { needed, available } => {
                write!(f, "GPU upload needs {needed} bytes; {available} available")
            }
            Self::Invalid(error) | Self::Gpu(error) => f.write_str(error),
        }
    }
}
impl std::error::Error for UploadError {}

#[derive(Clone)]
pub struct GpuUploader {
    pub(crate) device: Arc<wgpu::Device>,
    pub(crate) queue: Arc<wgpu::Queue>,
    pub(crate) layout: Arc<wgpu::BindGroupLayout>,
    pub(crate) sampler: Arc<wgpu::Sampler>,
    pub(crate) errors: Arc<Mutex<Option<String>>>,
}

fn allocation_sizes(width: u32, height: u32) -> Result<(usize, usize), UploadError> {
    if width == 0 || height == 0 {
        return Err(UploadError::Invalid("zero-size decoded image".into()));
    }
    let row = (width as usize)
        .checked_mul(4)
        .ok_or_else(|| UploadError::Invalid("GPU row size overflow".into()))?;
    let rgba = row
        .checked_mul(height as usize)
        .ok_or_else(|| UploadError::Invalid("GPU image size overflow".into()))?;
    let padded_row = row
        .checked_add(255)
        .map(|n| n / 256 * 256)
        .ok_or_else(|| UploadError::Invalid("GPU padded row overflow".into()))?;
    let staging = padded_row
        .checked_mul(height as usize)
        .ok_or_else(|| UploadError::Invalid("GPU staging size overflow".into()))?;
    Ok((rgba, staging))
}

fn copy_staging_rows(
    source: &[u8],
    destination: &mut [u8],
    width: u32,
    height: u32,
) -> Result<(), UploadError> {
    let (rgba_bytes, staging_bytes) = allocation_sizes(width, height)?;
    if source.len() != rgba_bytes || destination.len() != staging_bytes {
        return Err(UploadError::Invalid("upload staging size mismatch".into()));
    }
    let row = rgba_bytes / height as usize;
    let padded_row = staging_bytes / height as usize;
    for (source, destination) in source
        .chunks_exact(row)
        .zip(destination.chunks_exact_mut(padded_row))
    {
        destination[..row].copy_from_slice(source);
    }
    Ok(())
}

fn pop_upload_scopes(device: &wgpu::Device) -> Option<wgpu::Error> {
    let validation = pollster::block_on(device.pop_error_scope());
    let allocation = pollster::block_on(device.pop_error_scope());
    validation.or(allocation)
}

impl GpuUploader {
    pub(crate) fn check_error(&self) -> Result<(), String> {
        let error = self
            .errors
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        match &*error {
            Some(error) => Err(error.clone()),
            None => Ok(()),
        }
    }

    /// Additional headroom required while the CPU image is already accounted for.
    pub fn bytes_needed(width: u32, height: u32) -> Result<usize, UploadError> {
        let (rgba, staging) = allocation_sizes(width, height)?;
        rgba.checked_add(staging)
            .ok_or_else(|| UploadError::Invalid("GPU reservation overflow".into()))
    }

    pub fn upload(
        &self,
        image: Arc<DecodedImage>,
        orientation: u16,
        budget: Arc<MemoryBudget>,
    ) -> Result<Arc<PreparedTexture>, UploadError> {
        // Retired frame callbacks can own texture leases after the UI goes idle.
        // Reap completed work before measuring upload headroom or retrying it.
        let _ = self.device.poll(wgpu::Maintain::Poll);
        self.check_error().map_err(UploadError::Gpu)?;
        let (rgba_bytes, staging_bytes) = allocation_sizes(image.width, image.height)?;
        if image.pixels.len() != rgba_bytes {
            return Err(UploadError::Invalid(
                "decoded RGBA buffer size mismatch".into(),
            ));
        }
        let limit = self.device.limits().max_texture_dimension_2d;
        if image.width > limit || image.height > limit {
            return Err(UploadError::Invalid(format!(
                "image {}x{} exceeds this GPU's {limit}-pixel texture limit",
                image.width, image.height
            )));
        }
        if staging_bytes as u64 > self.device.limits().max_buffer_size {
            return Err(UploadError::Invalid(
                "image upload staging exceeds this GPU's buffer limit".into(),
            ));
        }
        let padded_row = u32::try_from(staging_bytes / image.height as usize)
            .map_err(|_| UploadError::Invalid("GPU row pitch exceeds u32".into()))?;
        let needed = Self::bytes_needed(image.width, image.height)?;
        let available = budget.available();
        if available < needed {
            return Err(UploadError::Budget { needed, available });
        }
        let gpu_lease = budget.try_reserve(rgba_bytes).ok_or(UploadError::Budget {
            needed,
            available: budget.available(),
        })?;
        let staging_lease = budget
            .try_reserve(staging_bytes)
            .ok_or(UploadError::Budget {
                needed,
                available: budget.available(),
            })?;
        let start = Instant::now();
        self.device.push_error_scope(wgpu::ErrorFilter::OutOfMemory);
        self.device.push_error_scope(wgpu::ErrorFilter::Validation);
        let texture = self.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("ARW embedded JPEG"),
            size: wgpu::Extent3d {
                width: image.width,
                height: image.height,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8UnormSrgb,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
        // Queue::write_texture holds wgpu's pending-writes lock while allocating
        // and copying the entire image. A mapped buffer moves that CPU copy
        // outside the shared queue lock, keeping foreground submissions short.
        // MAP_WRITE is essential: mapped_at_creation without it creates another
        // hidden staging buffer in wgpu 24. Metal uses one shared write-combined
        // allocation here, already covered by the padded staging reservation.
        let staging = self.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("JPEG upload staging"),
            size: staging_bytes as u64,
            usage: wgpu::BufferUsages::MAP_WRITE | wgpu::BufferUsages::COPY_SRC,
            mapped_at_creation: true,
        });
        let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
        let bind_group = image_binding(&self.device, &self.layout, &self.sampler, &view);
        // Detect allocation/validation failures before accessing a mapped view.
        // No copy has been submitted, so these resources may safely be released.
        if let Some(error) = pop_upload_scopes(&self.device) {
            return Err(UploadError::Gpu(error.to_string()));
        }
        {
            let mut mapped = staging.slice(..).get_mapped_range_mut();
            copy_staging_rows(&image.pixels, &mut mapped, image.width, image.height)?;
        }
        staging.unmap();
        self.device.push_error_scope(wgpu::ErrorFilter::OutOfMemory);
        self.device.push_error_scope(wgpu::ErrorFilter::Validation);
        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("JPEG upload copy"),
            });
        encoder.copy_buffer_to_texture(
            wgpu::TexelCopyBufferInfo {
                buffer: &staging,
                layout: wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(padded_row),
                    rows_per_image: Some(image.height),
                },
            },
            wgpu::TexelCopyTextureInfo {
                texture: &texture,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            texture.size(),
        );
        self.queue.submit([encoder.finish()]);
        let submit_ms = start.elapsed().as_secs_f64() * 1000.0;
        let (done_tx, done_rx) = std::sync::mpsc::sync_channel(1);
        self.queue.on_submitted_work_done(move || {
            let _ = done_tx.send(());
        });
        // Keep the GPU completion wait outside wgpu's shared queue/device locks.
        let completion = wait_for_upload_completion(&self.device, &done_rx)
            .map_err(|e| UploadError::Gpu(e.to_string()));
        let error = pop_upload_scopes(&self.device);
        // Restore the device-global scope stack even if completion delivery fails.
        completion?;
        if let Some(error) = error {
            return Err(UploadError::Gpu(error.to_string()));
        }
        self.check_error().map_err(UploadError::Gpu)?;
        let timings = UploadTimings {
            submit_ms,
            completion_ms: start.elapsed().as_secs_f64() * 1000.0,
        };
        drop(staging);
        drop(staging_lease);
        Ok(Arc::new(PreparedTexture {
            width: image.width,
            height: image.height,
            original_width: image.original_width,
            original_height: image.original_height,
            orientation: if (1..=8).contains(&orientation) {
                orientation
            } else {
                1
            },
            byte_size: rgba_bytes,
            timings,
            bind_group,
            _texture: texture,
            _lease: gpu_lease,
        }))
    }
}

pub(crate) fn image_binding(
    device: &wgpu::Device,
    layout: &wgpu::BindGroupLayout,
    sampler: &wgpu::Sampler,
    view: &wgpu::TextureView,
) -> wgpu::BindGroup {
    device.create_bind_group(&wgpu::BindGroupDescriptor {
        label: Some("image texture binding"),
        layout,
        entries: &[
            wgpu::BindGroupEntry {
                binding: 0,
                resource: wgpu::BindingResource::TextureView(view),
            },
            wgpu::BindGroupEntry {
                binding: 1,
                resource: wgpu::BindingResource::Sampler(sampler),
            },
        ],
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn upload_budget_includes_padded_staging_and_texture() {
        assert_eq!(GpuUploader::bytes_needed(1, 2).unwrap(), 8 + 512);
        assert_eq!(GpuUploader::bytes_needed(64, 2).unwrap(), 512 + 512);
        assert!(GpuUploader::bytes_needed(0, 1).is_err());
        assert!(GpuUploader::bytes_needed(u32::MAX, u32::MAX).is_err());
    }

    #[test]
    fn staging_preserves_rows_and_padding_for_aligned_and_unaligned_widths() {
        for width in [1, 63, 64, 65, 160] {
            let (rgba_bytes, staging_bytes) = allocation_sizes(width, 3).unwrap();
            let source: Vec<_> = (0..rgba_bytes).map(|index| (index % 251) as u8).collect();
            let mut staging = vec![0xaa; staging_bytes];
            copy_staging_rows(&source, &mut staging, width, 3).unwrap();
            let row = width as usize * 4;
            let padded_row = staging_bytes / 3;
            for (index, destination) in staging.chunks_exact(padded_row).enumerate() {
                assert_eq!(&destination[..row], &source[index * row..(index + 1) * row]);
                assert!(destination[row..].iter().all(|&byte| byte == 0xaa));
            }
        }
    }

    #[test]
    fn staging_rejects_invalid_sizes_before_writing_any_bytes() {
        let mut staging = vec![0xaa; 512];
        assert!(copy_staging_rows(&[0; 7], &mut staging, 1, 2).is_err());
        assert!(staging.iter().all(|&byte| byte == 0xaa));
        assert!(copy_staging_rows(&[0; 8], &mut staging[..511], 1, 2).is_err());
        assert!(staging.iter().all(|&byte| byte == 0xaa));
        assert!(copy_staging_rows(&[], &mut [], 0, 1).is_err());
        assert!(copy_staging_rows(&[], &mut [], u32::MAX, u32::MAX).is_err());
    }

    #[test]
    fn completion_does_not_require_unrelated_submissions_to_finish() {
        let (sender, completed) = std::sync::mpsc::sync_channel(1);
        let mut polls = 0;
        wait_for_completion(
            || {
                polls += 1;
                if polls == 2 {
                    sender.send(()).unwrap();
                }
                false // Other queue work can remain after this upload finishes.
            },
            &completed,
        )
        .unwrap();
        assert_eq!(polls, 2);
    }

    #[test]
    fn disconnected_completion_drains_before_releasing_upload_ownership() {
        let (sender, completed) = std::sync::mpsc::sync_channel(1);
        drop(sender);
        let mut polls = 0;
        assert!(wait_for_completion(
            || {
                polls += 1;
                polls == 3
            },
            &completed,
        )
        .is_err());
        assert_eq!(polls, 3);
    }
}
