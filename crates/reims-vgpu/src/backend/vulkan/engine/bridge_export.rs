//! Vulkan resources shared with the direct compositor frame bridge.
//!
//! Extension presence is not enough: external-memory compatibility is a
//! property of a concrete format, usage, tiling and handle type. This module
//! asks that exact question before creating an image, then keeps all handles in
//! one explicitly-destroyed object so a declined bridge cannot perturb the
//! ordinary window presenter.

use ash::vk;
use std::os::fd::FromRawFd;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use super::context::DeviceContext;

const HANDLE: vk::ExternalMemoryHandleTypeFlags = vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT;

pub(crate) struct ExportSlot {
    pub slot_id: u16,
    pub image: vk::Image,
    pub memory: vk::DeviceMemory,
    pub semaphore: vk::Semaphore,
    pub command_pool: vk::CommandPool,
    pub command_buffer: vk::CommandBuffer,
    pub width: u32,
    pub height: u32,
    pub modifier: u64,
    pub offset: u32,
    pub stride: u32,
    busy: Arc<AtomicBool>,
    transitioned: bool,
}

impl ExportSlot {
    pub(crate) unsafe fn create(
        ctx: &DeviceContext,
        width: u32,
        height: u32,
        offered_modifiers: &[u64],
        slot_id: u16,
    ) -> Result<Self, String> {
        let memory_fd = ctx
            .bridge_external_memory_fd
            .as_ref()
            .ok_or_else(|| "VK_KHR_external_memory_fd unavailable".to_owned())?;
        let _semaphore_fd = ctx
            .bridge_external_semaphore_fd
            .as_ref()
            .ok_or_else(|| "VK_KHR_external_semaphore_fd unavailable".to_owned())?;

        let mut compatible = Vec::new();
        for &modifier in offered_modifiers {
            let mut drm_info = vk::PhysicalDeviceImageDrmFormatModifierInfoEXT::default()
                .drm_format_modifier(modifier)
                .sharing_mode(vk::SharingMode::EXCLUSIVE);
            let mut external_info = vk::PhysicalDeviceExternalImageFormatInfo::default()
                .handle_type(HANDLE);
            let format_info = vk::PhysicalDeviceImageFormatInfo2::default()
                .format(vk::Format::B8G8R8A8_UNORM)
                .ty(vk::ImageType::TYPE_2D)
                .tiling(vk::ImageTiling::DRM_FORMAT_MODIFIER_EXT)
                .usage(vk::ImageUsageFlags::TRANSFER_DST | vk::ImageUsageFlags::SAMPLED)
                .flags(vk::ImageCreateFlags::empty())
                .push_next(&mut drm_info)
                .push_next(&mut external_info);
            let mut external = vk::ExternalImageFormatProperties::default();
            let mut properties = vk::ImageFormatProperties2::default().push_next(&mut external);
            if ctx.instance.get_physical_device_image_format_properties2(
                    ctx.pd, &format_info, &mut properties).is_ok()
                && external.external_memory_properties.external_memory_features
                    .contains(vk::ExternalMemoryFeatureFlags::EXPORTABLE)
                && external.external_memory_properties.compatible_handle_types.contains(HANDLE)
            {
                compatible.push(modifier);
            }
        }
        if compatible.is_empty() {
            return Err(format!(
                "no exportable B8G8R8A8 DMA-BUF modifier overlaps compositor offers ({})",
                offered_modifiers.len()
            ));
        }
        let mut modifier_list = vk::ImageDrmFormatModifierListCreateInfoEXT::default()
            .drm_format_modifiers(&compatible);
        let mut external_image = vk::ExternalMemoryImageCreateInfo::default().handle_types(HANDLE);
        let image_info = vk::ImageCreateInfo::default()
            .image_type(vk::ImageType::TYPE_2D)
            .format(vk::Format::B8G8R8A8_UNORM)
            .extent(vk::Extent3D {
                width,
                height,
                depth: 1,
            })
            .mip_levels(1)
            .array_layers(1)
            .samples(vk::SampleCountFlags::TYPE_1)
            .tiling(vk::ImageTiling::DRM_FORMAT_MODIFIER_EXT)
            .usage(vk::ImageUsageFlags::TRANSFER_DST | vk::ImageUsageFlags::SAMPLED)
            .sharing_mode(vk::SharingMode::EXCLUSIVE)
            .initial_layout(vk::ImageLayout::UNDEFINED)
            .push_next(&mut external_image)
            .push_next(&mut modifier_list);
        let image = ctx
            .device
            .create_image(&image_info, None)
            .map_err(|error| format!("create export image failed: {error:?}"))?;
        let modifier_loader =
            ash::ext::image_drm_format_modifier::Device::new(&ctx.instance, &ctx.device);
        let mut modifier_properties = vk::ImageDrmFormatModifierPropertiesEXT::default();
        if let Err(error) = modifier_loader
            .get_image_drm_format_modifier_properties(image, &mut modifier_properties)
        {
            ctx.device.destroy_image(image, None);
            return Err(format!("query chosen DRM modifier failed: {error:?}"));
        }
        let modifier = modifier_properties.drm_format_modifier;

        let result = (|| {
            let requirements = ctx.device.get_image_memory_requirements(image);
            let memory_type_index = ctx
                .memory_type_for(
                    requirements.memory_type_bits,
                    crate::backend::vulkan::caps::MemoryClass::DeviceLocal,
                )
                .ok_or_else(|| "no device-local memory type for export image".to_owned())?;
            let mut export = vk::ExportMemoryAllocateInfo::default().handle_types(HANDLE);
            let mut dedicated = vk::MemoryDedicatedAllocateInfo::default().image(image);
            let allocation = vk::MemoryAllocateInfo::default()
                .allocation_size(requirements.size)
                .memory_type_index(memory_type_index)
                .push_next(&mut export)
                .push_next(&mut dedicated);
            let memory = ctx
                .device
                .allocate_memory(&allocation, None)
                .map_err(|error| format!("allocate export memory failed: {error:?}"))?;
            if let Err(error) = ctx.device.bind_image_memory(image, memory, 0) {
                ctx.device.free_memory(memory, None);
                return Err(format!("bind export image failed: {error:?}"));
            }

            let layout = ctx.device.get_image_subresource_layout(
                image,
                vk::ImageSubresource::default()
                    .aspect_mask(vk::ImageAspectFlags::MEMORY_PLANE_0_EXT),
            );
            let offset = u32::try_from(layout.offset)
                .map_err(|_| "export image plane offset exceeds protocol".to_owned())?;
            let stride = u32::try_from(layout.row_pitch)
                .map_err(|_| "export image stride exceeds protocol".to_owned())?;

            let mut export_semaphore = vk::ExportSemaphoreCreateInfo::default()
                .handle_types(vk::ExternalSemaphoreHandleTypeFlags::SYNC_FD);
            let semaphore = ctx
                .device
                .create_semaphore(
                    &vk::SemaphoreCreateInfo::default().push_next(&mut export_semaphore),
                    None,
                )
                .map_err(|error| format!("create export semaphore failed: {error:?}"))?;
            let command_pool = match ctx.device.create_command_pool(
                &vk::CommandPoolCreateInfo::default()
                    .queue_family_index(ctx.gq)
                    .flags(vk::CommandPoolCreateFlags::RESET_COMMAND_BUFFER),
                None,
            ) {
                Ok(pool) => pool,
                Err(error) => {
                    ctx.device.destroy_semaphore(semaphore, None);
                    ctx.device.free_memory(memory, None);
                    return Err(format!("create export command pool failed: {error:?}"));
                }
            };
            let command_buffer = match ctx.device.allocate_command_buffers(
                &vk::CommandBufferAllocateInfo::default()
                    .command_pool(command_pool)
                    .level(vk::CommandBufferLevel::PRIMARY)
                    .command_buffer_count(1),
            ) {
                Ok(buffers) => buffers[0],
                Err(error) => {
                    ctx.device.destroy_command_pool(command_pool, None);
                    ctx.device.destroy_semaphore(semaphore, None);
                    ctx.device.free_memory(memory, None);
                    return Err(format!("allocate export command buffer failed: {error:?}"));
                }
            };
            // Prove the memory handle can actually be materialized. The probe
            // fd is disposable; each published frame obtains a fresh duplicate.
            let probe_fd = memory_fd
                .get_memory_fd(
                    &vk::MemoryGetFdInfoKHR::default()
                        .memory(memory)
                        .handle_type(HANDLE),
                )
                .map_err(|error| format!("export DMA-BUF fd failed: {error:?}"))?;
            libc::close(probe_fd);
            Ok(Self {
                slot_id,
                image,
                memory,
                semaphore,
                command_pool,
                command_buffer,
                width,
                height,
                modifier,
                offset,
                stride,
                busy: Arc::new(AtomicBool::new(false)),
                transitioned: false,
            })
        })();
        if result.is_err() {
            ctx.device.destroy_image(image, None);
        }
        result
    }

    pub(crate) fn available(&self) -> bool {
        !self.busy.load(Ordering::Acquire)
    }

    pub(crate) unsafe fn publish(
        &mut self,
        ctx: &DeviceContext,
        display_id: u32,
        source: vk::Image,
        source_access: super::pools::ResidentAccess,
    ) -> Result<crate::frame_bridge::PublishedFrame, String> {
        if !self.available() {
            return Err("export slot is still owned by compositor".to_owned());
        }
        ctx.device
            .reset_command_buffer(self.command_buffer, vk::CommandBufferResetFlags::empty())
            .map_err(|error| format!("reset export command buffer failed: {error:?}"))?;
        ctx.device
            .begin_command_buffer(
                self.command_buffer,
                &vk::CommandBufferBeginInfo::default()
                    .flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT),
            )
            .map_err(|error| format!("begin export command buffer failed: {error:?}"))?;
        let range = vk::ImageSubresourceRange::default()
            .aspect_mask(vk::ImageAspectFlags::COLOR)
            .level_count(1)
            .layer_count(1);
        let (source_stage, source_flags) = source_access.source_scope();
        let source_barrier = vk::ImageMemoryBarrier::default()
            .src_access_mask(source_flags)
            .dst_access_mask(vk::AccessFlags::TRANSFER_READ)
            .old_layout(source_access.layout())
            .new_layout(vk::ImageLayout::TRANSFER_SRC_OPTIMAL)
            .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
            .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
            .image(source)
            .subresource_range(range);
        let destination_barrier = vk::ImageMemoryBarrier::default()
            .src_access_mask(if self.transitioned {
                vk::AccessFlags::MEMORY_READ
            } else {
                vk::AccessFlags::empty()
            })
            .dst_access_mask(vk::AccessFlags::TRANSFER_WRITE)
            .old_layout(if self.transitioned {
                vk::ImageLayout::GENERAL
            } else {
                vk::ImageLayout::UNDEFINED
            })
            .new_layout(vk::ImageLayout::TRANSFER_DST_OPTIMAL)
            .src_queue_family_index(if self.transitioned {
                vk::QUEUE_FAMILY_FOREIGN_EXT
            } else {
                vk::QUEUE_FAMILY_IGNORED
            })
            .dst_queue_family_index(if self.transitioned {
                ctx.gq
            } else {
                vk::QUEUE_FAMILY_IGNORED
            })
            .image(self.image)
            .subresource_range(range);
        ctx.device.cmd_pipeline_barrier(
            self.command_buffer,
            source_stage | vk::PipelineStageFlags::ALL_COMMANDS,
            vk::PipelineStageFlags::TRANSFER,
            vk::DependencyFlags::empty(),
            &[],
            &[],
            &[source_barrier, destination_barrier],
        );
        let layers = vk::ImageSubresourceLayers::default()
            .aspect_mask(vk::ImageAspectFlags::COLOR)
            .layer_count(1);
        ctx.device.cmd_copy_image(
            self.command_buffer,
            source,
            vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
            self.image,
            vk::ImageLayout::TRANSFER_DST_OPTIMAL,
            &[vk::ImageCopy::default()
                .src_subresource(layers)
                .dst_subresource(layers)
                .extent(vk::Extent3D {
                    width: self.width,
                    height: self.height,
                    depth: 1,
                })],
        );
        let external_barrier = vk::ImageMemoryBarrier::default()
            .src_access_mask(vk::AccessFlags::TRANSFER_WRITE)
            .dst_access_mask(vk::AccessFlags::MEMORY_READ)
            .old_layout(vk::ImageLayout::TRANSFER_DST_OPTIMAL)
            .new_layout(vk::ImageLayout::GENERAL)
            .src_queue_family_index(ctx.gq)
            .dst_queue_family_index(vk::QUEUE_FAMILY_FOREIGN_EXT)
            .image(self.image)
            .subresource_range(range);
        ctx.device.cmd_pipeline_barrier(
            self.command_buffer,
            vk::PipelineStageFlags::TRANSFER,
            vk::PipelineStageFlags::ALL_COMMANDS,
            vk::DependencyFlags::empty(),
            &[],
            &[],
            &[external_barrier],
        );
        ctx.device
            .end_command_buffer(self.command_buffer)
            .map_err(|error| format!("end export command buffer failed: {error:?}"))?;
        let commands = [self.command_buffer];
        let signals = [self.semaphore];
        ctx.device
            .queue_submit(
                ctx.queue(),
                &[vk::SubmitInfo::default()
                    .command_buffers(&commands)
                    .signal_semaphores(&signals)],
                vk::Fence::null(),
            )
            .map_err(|error| format!("submit export copy failed: {error:?}"))?;
        self.transitioned = true;

        let memory_fd = ctx
            .bridge_external_memory_fd
            .as_ref()
            .ok_or_else(|| "DMA-BUF export loader disappeared".to_owned())?
            .get_memory_fd(
                &vk::MemoryGetFdInfoKHR::default()
                    .memory(self.memory)
                    .handle_type(HANDLE),
            )
            .map_err(|error| format!("export frame DMA-BUF failed: {error:?}"))?;
        let fence_fd = ctx
            .bridge_external_semaphore_fd
            .as_ref()
            .ok_or_else(|| "sync-fd export loader disappeared".to_owned())?
            .get_semaphore_fd(
                &vk::SemaphoreGetFdInfoKHR::default()
                    .semaphore(self.semaphore)
                    .handle_type(vk::ExternalSemaphoreHandleTypeFlags::SYNC_FD),
            )
            .map_err(|error| format!("export acquire sync-fd failed: {error:?}"))?;
        let busy = Arc::clone(&self.busy);
        busy.store(true, Ordering::Release);
        Ok(crate::frame_bridge::PublishedFrame::new(
            crate::frame_bridge::Frame {
                display_id,
                width: self.width,
                height: self.height,
                drm_format: u32::from_le_bytes(*b"XR24"),
                modifier: self.modifier,
                plane_count: 1,
                has_acquire_fence: true,
                slot_id: self.slot_id,
                planes: [
                    crate::frame_bridge::Plane {
                        offset: self.offset,
                        stride: self.stride,
                    },
                    crate::frame_bridge::Plane::default(),
                    crate::frame_bridge::Plane::default(),
                    crate::frame_bridge::Plane::default(),
                ],
            },
            vec![std::os::fd::OwnedFd::from_raw_fd(memory_fd)],
            Some(std::os::fd::OwnedFd::from_raw_fd(fence_fd)),
            move || busy.store(false, Ordering::Release),
        ))
    }

    pub(crate) unsafe fn destroy(self, ctx: &DeviceContext) {
        ctx.device.destroy_command_pool(self.command_pool, None);
        ctx.device.destroy_semaphore(self.semaphore, None);
        ctx.device.destroy_image(self.image, None);
        ctx.device.free_memory(self.memory, None);
    }
}
