# Reims frame bridge

The direct presentation path connects the QEMU-hosted vGPU to Reims OS Session
without creating a Wayland toplevel or a Vulkan Wayland swapchain. It is a local
Unix `SOCK_SEQPACKET` protocol. Packet fields are little-endian and defined by
`crates/reims-vgpu/src/frame_bridge.rs`.

The compositor listens at `REIMS_VGPU_FRAME_BRIDGE`. The vGPU sends `Hello`,
the compositor answers `Ready` with the XR24 DRM modifiers its wlroots renderer
can import as textures, and each `Frame` carries one DMA-BUF fd per plane via
`SCM_RIGHTS`, followed by an acquire-fence fd when the frame flag says one is
present. A frame is not reusable until the compositor returns `Release` for its
sequence. Either side may send `Goodbye` before closing.

The version 1 `Ready` payload begins with a little-endian `u32` entry count.
Each 16-byte entry is `(drm_format: u32, reserved: u32, modifier: u64)`. The
reserved word must be zero. Implicit `DRM_FORMAT_MOD_INVALID` entries are not
offered because Vulkan image creation needs an explicit layout. The vGPU tests
each offered modifier with `DMA_BUF_EXT` and the exact image usage, passes the
exportable intersection to Vulkan, then reports Vulkan's chosen modifier and
memory-plane layout in `Frame`.

Backpressure is latest-wins. The vGPU may replace a frame which has not yet been
sent, but it must retain every frame whose file descriptors were sent until its
matching release arrives. The compositor may skip rendering an older received
frame, but must still release it. Disconnect releases every transport reference
and the vGPU continues running; the legacy Wayland presenter remains the
bring-up fallback until direct presentation passes the live visual gate.

The Vulkan producer maintains three export slots per display. A slot becomes
busy only after its copy is submitted and stays busy until the publisher drops
the corresponding frame after `Release`; if all slots are busy, drain skips the
new frame instead of waiting. Each copy releases queue-family ownership to
`VK_QUEUE_FAMILY_FOREIGN_EXT` and exports its binary semaphore payload as a
sync FD. The compositor must not return `Release` until that FD signals and it
has finished using the DMA-BUF. Slot reuse acquires ownership back from the
foreign queue family.

Version 1 frame payloads contain display id, dimensions, DRM fourcc, modifier,
one to four `(offset, stride)` plane descriptors, and an acquire-fence flag.
File descriptors are deliberately outside the byte payload, and a packet whose
descriptor count does not match its payload is rejected as a whole.
