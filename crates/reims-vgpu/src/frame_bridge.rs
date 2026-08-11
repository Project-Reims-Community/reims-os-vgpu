//! Wire contract between the vGPU publisher and Reims OS Session.
//!
//! The transport is a local Unix `SOCK_SEQPACKET` socket.  Every packet starts
//! with [`Header`]; DMA-BUF plane descriptors follow a [`Frame`] packet and the
//! corresponding file descriptors are carried in the same packet with
//! `SCM_RIGHTS`.  An optional acquire-fence fd is last.  Keeping the byte
//! contract here, outside either Vulkan presentation or compositor policy,
//! lets the current Wayland-window presenter remain a fallback while the direct
//! path is brought up.

pub const MAGIC: u32 = u32::from_le_bytes(*b"RFB1");
pub const VERSION: u16 = 1;
pub const MAX_PLANES: u8 = 4;
pub const HEADER_LEN: usize = 16;
pub const FRAME_LEN: usize = 64;
pub const READY_ENTRY_LEN: usize = 16;
pub const READY_MAX_ENTRIES: usize = 64;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u16)]
pub enum Kind {
    Hello = 1,
    Ready = 2,
    Frame = 3,
    Release = 4,
    Goodbye = 5,
}

impl Kind {
    fn decode(raw: u16) -> Option<Self> {
        Some(match raw {
            1 => Self::Hello,
            2 => Self::Ready,
            3 => Self::Frame,
            4 => Self::Release,
            5 => Self::Goodbye,
            _ => return None,
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Header {
    pub kind: Kind,
    pub payload_len: u32,
    pub sequence: u32,
}

impl Header {
    pub fn encode(self) -> [u8; HEADER_LEN] {
        let mut out = [0; HEADER_LEN];
        out[0..4].copy_from_slice(&MAGIC.to_le_bytes());
        out[4..6].copy_from_slice(&VERSION.to_le_bytes());
        out[6..8].copy_from_slice(&(self.kind as u16).to_le_bytes());
        out[8..12].copy_from_slice(&self.payload_len.to_le_bytes());
        out[12..16].copy_from_slice(&self.sequence.to_le_bytes());
        out
    }

    pub fn decode(bytes: &[u8]) -> Option<Self> {
        if bytes.len() < HEADER_LEN
            || u32::from_le_bytes(bytes[0..4].try_into().ok()?) != MAGIC
            || u16::from_le_bytes(bytes[4..6].try_into().ok()?) != VERSION
        {
            return None;
        }
        Some(Self {
            kind: Kind::decode(u16::from_le_bytes(bytes[6..8].try_into().ok()?))?,
            payload_len: u32::from_le_bytes(bytes[8..12].try_into().ok()?),
            sequence: u32::from_le_bytes(bytes[12..16].try_into().ok()?),
        })
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Plane {
    pub offset: u32,
    pub stride: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Frame {
    pub display_id: u32,
    pub width: u32,
    pub height: u32,
    pub drm_format: u32,
    pub modifier: u64,
    pub plane_count: u8,
    pub has_acquire_fence: bool,
    pub slot_id: u16,
    pub planes: [Plane; MAX_PLANES as usize],
}

#[cfg(unix)]
pub struct Connection {
    fd: std::os::fd::OwnedFd,
    modifiers: Vec<u64>,
}

/// One exported image submitted to the compositor. Descriptor ownership moves
/// into the publisher and lasts until the matching `Release` packet arrives.
#[cfg(unix)]
pub struct PublishedFrame {
    pub frame: Frame,
    pub plane_fds: Vec<std::os::fd::OwnedFd>,
    pub acquire_fence: Option<std::os::fd::OwnedFd>,
    released: Option<Box<dyn FnOnce() + Send>>,
}

#[cfg(unix)]
impl PublishedFrame {
    pub fn new(
        frame: Frame,
        plane_fds: Vec<std::os::fd::OwnedFd>,
        acquire_fence: Option<std::os::fd::OwnedFd>,
        released: impl FnOnce() + Send + 'static,
    ) -> Self {
        Self {
            frame,
            plane_fds,
            acquire_fence,
            released: Some(Box::new(released)),
        }
    }
}

#[cfg(unix)]
impl Drop for PublishedFrame {
    fn drop(&mut self) {
        if let Some(released) = self.released.take() {
            released();
        }
    }
}

#[cfg(unix)]
struct PublisherState {
    pending: Option<PublishedFrame>,
    stopped: bool,
    next_sequence: u32,
}

/// Latest-frame-wins bridge worker.
///
/// `Connection::send_frame` deliberately waits for compositor ownership to
/// end. Keeping that wait on this thread prevents a slow compositor from ever
/// stalling the guest drain worker. If several frames arrive while one is
/// displayed, replacing `pending` also closes the superseded descriptors.
#[cfg(unix)]
pub struct Publisher {
    shared: std::sync::Arc<(std::sync::Mutex<PublisherState>, std::sync::Condvar)>,
    socket_fd: std::os::fd::RawFd,
    worker: Option<std::thread::JoinHandle<()>>,
}

#[cfg(unix)]
impl Publisher {
    pub fn new(connection: Connection) -> std::io::Result<Self> {
        let socket_fd = connection.raw_fd();
        let shared = std::sync::Arc::new((
            std::sync::Mutex::new(PublisherState {
                pending: None,
                stopped: false,
                next_sequence: 1,
            }),
            std::sync::Condvar::new(),
        ));
        let worker_shared = std::sync::Arc::clone(&shared);
        let worker = std::thread::Builder::new()
            .name("reims-frame-bridge".into())
            .spawn(move || publisher_main(connection, &worker_shared))?;
        Ok(Self {
            shared,
            socket_fd,
            worker: Some(worker),
        })
    }

    pub fn publish(&self, frame: PublishedFrame) {
        let (lock, wake) = &*self.shared;
        let mut state = lock.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        if !state.stopped {
            state.pending = Some(frame);
            wake.notify_one();
        }
    }
}

#[cfg(unix)]
fn publisher_main(
    connection: Connection,
    shared: &std::sync::Arc<(std::sync::Mutex<PublisherState>, std::sync::Condvar)>,
) {
    use std::os::fd::AsRawFd;

    loop {
        let (sequence, published) = {
            let (lock, wake) = &**shared;
            let mut state = lock.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
            while state.pending.is_none() && !state.stopped {
                state = wake
                    .wait(state)
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
            }
            if state.stopped {
                return;
            }
            let sequence = state.next_sequence;
            state.next_sequence = state.next_sequence.wrapping_add(1);
            (
                sequence,
                state.pending.take().expect("pending checked above"),
            )
        };
        let plane_fds: Vec<_> = published.plane_fds.iter().map(AsRawFd::as_raw_fd).collect();
        let fence_fd = published.acquire_fence.as_ref().map(AsRawFd::as_raw_fd);
        if let Err(error) = connection.send_frame(sequence, published.frame, &plane_fds, fence_fd) {
            let stopped = shared
                .0
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .stopped;
            if !stopped {
                crate::observe::fail(format!("frame_bridge state=disconnected error={error}"));
            }
            return;
        }
    }
}

#[cfg(unix)]
impl Drop for Publisher {
    fn drop(&mut self) {
        {
            let (lock, wake) = &*self.shared;
            let mut state = lock.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
            state.stopped = true;
            state.pending = None;
            wake.notify_one();
        }
        // Wake a worker blocked waiting for Release. The worker exclusively
        // owns and closes the descriptor after recv returns.
        unsafe {
            libc::shutdown(self.socket_fd, libc::SHUT_RDWR);
        }
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

#[cfg(unix)]
impl Connection {
    pub fn connect(path: &std::path::Path, sequence: u32) -> std::io::Result<Self> {
        use std::os::fd::{FromRawFd, OwnedFd};
        use std::os::unix::ffi::OsStrExt;

        let bytes = path.as_os_str().as_bytes();
        let address = unsafe {
            let mut address: libc::sockaddr_un = std::mem::zeroed();
            address.sun_family = libc::AF_UNIX as libc::sa_family_t;
            if bytes.is_empty() || bytes.len() >= address.sun_path.len() {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "frame bridge socket path is empty or too long",
                ));
            }
            for (dst, src) in address.sun_path.iter_mut().zip(bytes) {
                *dst = *src as libc::c_char;
            }
            address
        };
        let raw =
            unsafe { libc::socket(libc::AF_UNIX, libc::SOCK_SEQPACKET | libc::SOCK_CLOEXEC, 0) };
        if raw < 0 {
            return Err(std::io::Error::last_os_error());
        }
        let fd = unsafe { OwnedFd::from_raw_fd(raw) };
        let address_len = (std::mem::offset_of!(libc::sockaddr_un, sun_path) + bytes.len() + 1)
            as libc::socklen_t;
        let status = unsafe {
            libc::connect(
                raw,
                (&address as *const libc::sockaddr_un).cast(),
                address_len,
            )
        };
        if status < 0 {
            return Err(std::io::Error::last_os_error());
        }

        let hello = Header {
            kind: Kind::Hello,
            payload_len: 0,
            sequence,
        }
        .encode();
        if unsafe { libc::send(raw, hello.as_ptr().cast(), hello.len(), libc::MSG_NOSIGNAL) }
            != hello.len() as isize
        {
            return Err(std::io::Error::last_os_error());
        }
        let mut reply = [0; HEADER_LEN + 4 + READY_ENTRY_LEN * READY_MAX_ENTRIES];
        let received = unsafe { libc::recv(raw, reply.as_mut_ptr().cast(), reply.len(), 0) };
        if received < HEADER_LEN as isize {
            return Err(std::io::Error::last_os_error());
        }
        let received = received as usize;
        let header = Header::decode(&reply[..received]).ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "invalid bridge Ready packet",
            )
        })?;
        if header.kind != Kind::Ready
            || header.sequence != sequence
            || received != HEADER_LEN + header.payload_len as usize
        {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "bridge did not acknowledge Hello",
            ));
        }
        let payload = &reply[HEADER_LEN..received];
        if payload.len() < 4 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "bridge Ready packet has no modifier count",
            ));
        }
        let count = u32::from_le_bytes(payload[0..4].try_into().unwrap()) as usize;
        if count > READY_MAX_ENTRIES || payload.len() != 4 + count * READY_ENTRY_LEN {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "bridge Ready modifier list is malformed",
            ));
        }
        let xr24 = u32::from_le_bytes(*b"XR24");
        let mut modifiers = Vec::with_capacity(count);
        for entry in payload[4..].chunks_exact(READY_ENTRY_LEN) {
            let format = u32::from_le_bytes(entry[0..4].try_into().unwrap());
            let reserved = u32::from_le_bytes(entry[4..8].try_into().unwrap());
            let modifier = u64::from_le_bytes(entry[8..16].try_into().unwrap());
            if format != xr24 || reserved != 0 {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "bridge Ready contains an unsupported format entry",
                ));
            }
            if !modifiers.contains(&modifier) {
                modifiers.push(modifier);
            }
        }
        Ok(Self { fd, modifiers })
    }

    pub fn raw_fd(&self) -> std::os::fd::RawFd {
        use std::os::fd::AsRawFd;
        self.fd.as_raw_fd()
    }

    pub fn modifiers(&self) -> &[u64] {
        &self.modifiers
    }

    /// Send one frame and wait for its release acknowledgement.
    ///
    /// This blocking primitive is intentionally below publication policy. The
    /// product publisher calls it from its bridge thread, never from the guest
    /// drain worker; keeping the acknowledgement here makes it impossible for
    /// a pool slot to be reused before the compositor has dropped its fds.
    pub fn send_frame(
        &self,
        sequence: u32,
        frame: Frame,
        plane_fds: &[std::os::fd::RawFd],
        acquire_fence: Option<std::os::fd::RawFd>,
    ) -> std::io::Result<()> {
        use std::os::fd::AsRawFd;

        if plane_fds.len() != frame.plane_count as usize
            || acquire_fence.is_some() != frame.has_acquire_fence
        {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "frame metadata and descriptor count disagree",
            ));
        }
        let payload = frame.encode().ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::InvalidInput, "invalid frame metadata")
        })?;
        let header = Header {
            kind: Kind::Frame,
            payload_len: FRAME_LEN as u32,
            sequence,
        }
        .encode();
        let mut descriptors = plane_fds.to_vec();
        descriptors.extend(acquire_fence);
        let control_len = unsafe {
            libc::CMSG_SPACE((descriptors.len() * std::mem::size_of::<libc::c_int>()) as u32)
                as usize
        };
        let mut control = vec![0u8; control_len];
        let mut iov = [
            libc::iovec {
                iov_base: header.as_ptr().cast_mut().cast(),
                iov_len: header.len(),
            },
            libc::iovec {
                iov_base: payload.as_ptr().cast_mut().cast(),
                iov_len: payload.len(),
            },
        ];
        let mut message: libc::msghdr = unsafe { std::mem::zeroed() };
        message.msg_iov = iov.as_mut_ptr();
        message.msg_iovlen = iov.len();
        message.msg_control = control.as_mut_ptr().cast();
        message.msg_controllen = control.len();
        unsafe {
            let cmsg = libc::CMSG_FIRSTHDR(&message);
            (*cmsg).cmsg_level = libc::SOL_SOCKET;
            (*cmsg).cmsg_type = libc::SCM_RIGHTS;
            (*cmsg).cmsg_len =
                libc::CMSG_LEN((descriptors.len() * std::mem::size_of::<libc::c_int>()) as u32)
                    as usize;
            std::ptr::copy_nonoverlapping(
                descriptors.as_ptr(),
                libc::CMSG_DATA(cmsg).cast::<libc::c_int>(),
                descriptors.len(),
            );
        }
        let sent = unsafe { libc::sendmsg(self.fd.as_raw_fd(), &message, libc::MSG_NOSIGNAL) };
        if sent != (HEADER_LEN + FRAME_LEN) as isize {
            return Err(std::io::Error::last_os_error());
        }
        let mut reply = [0; HEADER_LEN];
        if unsafe {
            libc::recv(
                self.fd.as_raw_fd(),
                reply.as_mut_ptr().cast(),
                reply.len(),
                0,
            )
        } != reply.len() as isize
        {
            return Err(std::io::Error::last_os_error());
        }
        let release = Header::decode(&reply).ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "invalid bridge Release packet",
            )
        })?;
        if release.kind != Kind::Release || release.payload_len != 0 || release.sequence != sequence
        {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "bridge released the wrong frame",
            ));
        }
        Ok(())
    }
}

impl Frame {
    pub fn encode(self) -> Option<[u8; FRAME_LEN]> {
        if self.plane_count == 0 || self.plane_count > MAX_PLANES {
            return None;
        }
        let mut out = [0; FRAME_LEN];
        out[0..4].copy_from_slice(&self.display_id.to_le_bytes());
        out[4..8].copy_from_slice(&self.width.to_le_bytes());
        out[8..12].copy_from_slice(&self.height.to_le_bytes());
        out[12..16].copy_from_slice(&self.drm_format.to_le_bytes());
        out[16..24].copy_from_slice(&self.modifier.to_le_bytes());
        out[24] = self.plane_count;
        out[25] = u8::from(self.has_acquire_fence);
        out[26..28].copy_from_slice(&self.slot_id.to_le_bytes());
        for (index, plane) in self.planes.iter().enumerate() {
            let at = 24 + 8 + index * 8;
            out[at..at + 4].copy_from_slice(&plane.offset.to_le_bytes());
            out[at + 4..at + 8].copy_from_slice(&plane.stride.to_le_bytes());
        }
        Some(out)
    }

    pub fn decode(bytes: &[u8]) -> Option<Self> {
        if bytes.len() != FRAME_LEN || bytes[24] == 0 || bytes[24] > MAX_PLANES || bytes[25] > 1 {
            return None;
        }
        let mut planes = [Plane::default(); MAX_PLANES as usize];
        for (index, plane) in planes.iter_mut().enumerate() {
            let at = 24 + 8 + index * 8;
            *plane = Plane {
                offset: u32::from_le_bytes(bytes[at..at + 4].try_into().ok()?),
                stride: u32::from_le_bytes(bytes[at + 4..at + 8].try_into().ok()?),
            };
        }
        Some(Self {
            display_id: u32::from_le_bytes(bytes[0..4].try_into().ok()?),
            width: u32::from_le_bytes(bytes[4..8].try_into().ok()?),
            height: u32::from_le_bytes(bytes[8..12].try_into().ok()?),
            drm_format: u32::from_le_bytes(bytes[12..16].try_into().ok()?),
            modifier: u64::from_le_bytes(bytes[16..24].try_into().ok()?),
            plane_count: bytes[24],
            has_acquire_fence: bytes[25] != 0,
            slot_id: u16::from_le_bytes(bytes[26..28].try_into().ok()?),
            planes,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_rejects_wrong_magic_version_and_kind() {
        let encoded = Header {
            kind: Kind::Frame,
            payload_len: FRAME_LEN as u32,
            sequence: 27,
        }
        .encode();
        assert_eq!(Header::decode(&encoded).unwrap().sequence, 27);
        for at in [0, 4, 6] {
            let mut corrupt = encoded;
            corrupt[at] ^= 0xff;
            assert!(Header::decode(&corrupt).is_none());
        }
    }

    #[test]
    fn frame_round_trip_preserves_every_plane_and_sync_bit() {
        let frame = Frame {
            display_id: 3,
            width: 1920,
            height: 1080,
            drm_format: u32::from_le_bytes(*b"XR24"),
            modifier: 0x0300_0000_0060_6014,
            plane_count: 2,
            has_acquire_fence: true,
            slot_id: 2,
            planes: [
                Plane {
                    offset: 11,
                    stride: 7680,
                },
                Plane {
                    offset: 99,
                    stride: 3840,
                },
                Plane::default(),
                Plane::default(),
            ],
        };
        assert_eq!(Frame::decode(&frame.encode().unwrap()).unwrap(), frame);
    }

    #[test]
    fn frame_rejects_impossible_plane_and_fence_counts() {
        let mut bytes = [0; FRAME_LEN];
        bytes[24] = 1;
        assert!(Frame::decode(&bytes).is_some());
        bytes[24] = 0;
        assert!(Frame::decode(&bytes).is_none());
        bytes[24] = MAX_PLANES + 1;
        assert!(Frame::decode(&bytes).is_none());
        bytes[24] = 1;
        bytes[25] = 2;
        assert!(Frame::decode(&bytes).is_none());
    }
}
