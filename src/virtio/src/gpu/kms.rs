use super::core::{HEIGHT, WIDTH};
use std::fs::{File, OpenOptions};
use std::io::{Error, ErrorKind, Result};
use std::mem::size_of;
use std::os::fd::AsRawFd;

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct Mode {
    clock: u32,
    horizontal: [u16; 5],
    vertical: [u16; 5],
    refresh: u32,
    flags: u32,
    kind: u32,
    name: [u8; 32],
}

#[repr(C)]
#[derive(Default)]
struct Resources {
    framebuffers: u64,
    crtcs: u64,
    connectors: u64,
    encoders: u64,
    framebuffer_count: u32,
    crtc_count: u32,
    connector_count: u32,
    encoder_count: u32,
    min_width: u32,
    max_width: u32,
    min_height: u32,
    max_height: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct Crtc {
    connectors: u64,
    connector_count: u32,
    id: u32,
    framebuffer: u32,
    x: u32,
    y: u32,
    gamma_size: u32,
    mode_valid: u32,
    mode: Mode,
}

#[repr(C)]
#[derive(Default)]
struct Connector {
    encoders: u64,
    modes: u64,
    properties: u64,
    property_values: u64,
    mode_count: u32,
    property_count: u32,
    encoder_count: u32,
    encoder: u32,
    id: u32,
    kind: u32,
    kind_id: u32,
    connection: u32,
    width_mm: u32,
    height_mm: u32,
    subpixel: u32,
    padding: u32,
}

#[repr(C)]
#[derive(Default)]
struct Encoder {
    id: u32,
    kind: u32,
    crtc: u32,
    possible_crtcs: u32,
    possible_clones: u32,
}

#[repr(C)]
#[derive(Default)]
struct DumbBuffer {
    height: u32,
    width: u32,
    bpp: u32,
    flags: u32,
    handle: u32,
    pitch: u32,
    size: u64,
}

#[repr(C)]
#[derive(Default)]
struct Framebuffer {
    id: u32,
    width: u32,
    height: u32,
    pitch: u32,
    bpp: u32,
    depth: u32,
    handle: u32,
}

#[repr(C)]
#[derive(Default)]
struct MapDumb {
    handle: u32,
    padding: u32,
    offset: u64,
}

#[repr(C)]
#[derive(Default)]
struct Dirty {
    framebuffer: u32,
    flags: u32,
    color: u32,
    count: u32,
    clips: u64,
}

fn invalid(message: &str) -> Error {
    Error::new(ErrorKind::InvalidData, message)
}

const fn request<T>(number: u8) -> libc::c_ulong {
    (3 << 30)
        | ((size_of::<T>() as libc::c_ulong) << 16)
        | (b'd' as libc::c_ulong) << 8
        | number as libc::c_ulong
}

fn ioctl<T>(file: &File, number: u8, value: &mut T) -> Result<()> {
    let status = unsafe { libc::ioctl(file.as_raw_fd(), request::<T>(number), value) };
    if status < 0 {
        return Err(Error::new(
            Error::last_os_error().kind(),
            format!("DRM ioctl {number:#x}: {}", Error::last_os_error()),
        ));
    }
    Ok(())
}

fn count(value: u32) -> Result<usize> {
    if value > 4096 {
        return Err(invalid("DRM object count exceeds lab limit"));
    }
    Ok(value as usize)
}

fn display(file: &File, width: u32, height: u32) -> Result<(u32, u32, Mode, Vec<u32>)> {
    let mut resources = Resources::default();
    ioctl(file, 0xa0, &mut resources)?;
    let mut connectors = vec![0u32; count(resources.connector_count)?];
    let mut crtcs = vec![0u32; count(resources.crtc_count)?];
    resources.connectors = connectors.as_mut_ptr() as u64;
    resources.crtcs = crtcs.as_mut_ptr() as u64;
    resources.framebuffer_count = 0;
    resources.encoder_count = 0;
    ioctl(file, 0xa0, &mut resources)?;
    if resources.connector_count as usize > connectors.len()
        || resources.crtc_count as usize > crtcs.len()
    {
        return Err(invalid("DRM resources changed during enumeration"));
    }
    connectors.truncate(resources.connector_count as usize);
    crtcs.truncate(resources.crtc_count as usize);
    let mut selected = None;
    let mut current = Vec::new();
    for id in connectors {
        let mut connector = Connector {
            id,
            ..Default::default()
        };
        ioctl(file, 0xa7, &mut connector)?;
        if connector.encoder != 0 {
            let mut encoder = Encoder {
                id: connector.encoder,
                ..Default::default()
            };
            ioctl(file, 0xa6, &mut encoder)?;
            current.push((id, encoder.crtc));
        }
        if selected.is_some() || connector.connection != 1 {
            continue;
        }
        let mut modes = vec![Mode::default(); count(connector.mode_count)?];
        let mut encoders = vec![0u32; count(connector.encoder_count)?];
        connector.modes = modes.as_mut_ptr() as u64;
        connector.encoders = encoders.as_mut_ptr() as u64;
        connector.property_count = 0;
        ioctl(file, 0xa7, &mut connector)?;
        if connector.mode_count as usize > modes.len()
            || connector.encoder_count as usize > encoders.len()
        {
            return Err(invalid("DRM connector changed during enumeration"));
        }
        let Some(mode) = modes[..connector.mode_count as usize]
            .iter()
            .find(|mode| mode.horizontal[0] as u32 == width && mode.vertical[0] as u32 == height)
        else {
            continue;
        };
        for encoder_id in &encoders[..connector.encoder_count as usize] {
            let mut encoder = Encoder {
                id: *encoder_id,
                ..Default::default()
            };
            ioctl(file, 0xa6, &mut encoder)?;
            if let Some((_, crtc)) = crtcs
                .iter()
                .enumerate()
                .take(32)
                .find(|(index, _)| encoder.possible_crtcs & (1u32 << index) != 0)
            {
                selected = Some((id, *crtc, *mode));
                break;
            }
        }
    }
    let (connector, crtc, mode) =
        selected.ok_or_else(|| invalid("No connected DRM output with the requested mode/CRTC"))?;
    let restore = current
        .into_iter()
        .filter(|(_, assigned)| *assigned == crtc)
        .map(|(id, _)| id)
        .collect();
    Ok((connector, crtc, mode, restore))
}

fn copy_rgb(frame: &[u8], mapping: &mut [u8], pitch: usize, width: u32, height: u32) -> Result<()> {
    if frame.len() != width as usize * height as usize * 3
        || pitch < width as usize * 4
        || pitch
            .checked_mul(height as usize)
            .is_none_or(|size| size > mapping.len())
    {
        return Err(invalid("Invalid RGB frame or DRM buffer layout"));
    }
    for row in 0..height as usize {
        let source = &frame[row * width as usize * 3..][..width as usize * 3];
        let target = &mut mapping[row * pitch..][..width as usize * 4];
        for (rgb, pixel) in source.chunks_exact(3).zip(target.chunks_exact_mut(4)) {
            pixel.copy_from_slice(&[rgb[2], rgb[1], rgb[0], 0]);
        }
    }
    Ok(())
}

pub struct Kms {
    file: File,
    buffer: DumbBuffer,
    framebuffer: Framebuffer,
    address: usize,
    length: usize,
    original: Crtc,
    restore_connectors: Vec<u32>,
    modeset: bool,
}

impl Kms {
    pub fn open(path: &str) -> Result<Self> {
        Self::open_mode(path, WIDTH, HEIGHT)
    }

    pub fn open_mode(path: &str, width: u32, height: u32) -> Result<Self> {
        if width == 0 || height == 0 || width > 4096 || height > 4096 {
            return Err(invalid("DRM mode outside lab limits"));
        }
        let file = OpenOptions::new().read(true).write(true).open(path)?;
        if unsafe { libc::ioctl(file.as_raw_fd(), 0x641e, 0) } < 0 {
            return Err(Error::last_os_error());
        }
        let (connector, crtc, mode, restore_connectors) = display(&file, width, height)?;
        let mut original = Crtc {
            id: crtc,
            ..Default::default()
        };
        ioctl(&file, 0xa1, &mut original)?;
        let mut kms = Self {
            file,
            buffer: DumbBuffer {
                width,
                height,
                bpp: 32,
                ..Default::default()
            },
            framebuffer: Framebuffer::default(),
            address: 0,
            length: 0,
            original,
            restore_connectors,
            modeset: false,
        };
        ioctl(&kms.file, 0xb2, &mut kms.buffer)?;
        kms.length =
            usize::try_from(kms.buffer.size).map_err(|_| invalid("DRM buffer too large"))?;
        if kms.length > 128 * 1024 * 1024
            || kms.buffer.pitch < width * 4
            || kms.length < kms.buffer.pitch as usize * height as usize
        {
            return Err(invalid("Invalid DRM dumb buffer layout"));
        }
        kms.framebuffer = Framebuffer {
            width,
            height,
            pitch: kms.buffer.pitch,
            bpp: 32,
            depth: 24,
            handle: kms.buffer.handle,
            ..Default::default()
        };
        ioctl(&kms.file, 0xae, &mut kms.framebuffer)?;
        let mut map = MapDumb {
            handle: kms.buffer.handle,
            ..Default::default()
        };
        ioctl(&kms.file, 0xb3, &mut map)?;
        let offset =
            libc::off_t::try_from(map.offset).map_err(|_| invalid("Invalid DRM mmap offset"))?;
        let pointer = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                kms.length,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                kms.file.as_raw_fd(),
                offset,
            )
        };
        if pointer == libc::MAP_FAILED {
            return Err(Error::last_os_error());
        }
        kms.address = pointer as usize;
        unsafe { std::ptr::write_bytes(pointer.cast::<u8>(), 0, kms.length) };
        let mut mode_request = Crtc {
            connectors: &connector as *const u32 as u64,
            connector_count: 1,
            id: crtc,
            framebuffer: kms.framebuffer.id,
            mode_valid: 1,
            mode,
            ..Default::default()
        };
        ioctl(&kms.file, 0xa2, &mut mode_request)?;
        kms.modeset = true;
        println!(
            "GPU_KMS_READY device={} connector={} crtc={} framebuffer={} mode={}x{} pitch={}",
            path, connector, crtc, kms.framebuffer.id, width, height, kms.buffer.pitch
        );
        Ok(kms)
    }

    pub fn present(&mut self, frame: &[u8], generation: u64) -> Result<()> {
        let mapping =
            unsafe { std::slice::from_raw_parts_mut(self.address as *mut u8, self.length) };
        copy_rgb(
            frame,
            mapping,
            self.buffer.pitch as usize,
            self.buffer.width,
            self.buffer.height,
        )?;
        let rectangle = [0u16, 0, self.buffer.width as u16, self.buffer.height as u16];
        let mut dirty = Dirty {
            framebuffer: self.framebuffer.id,
            count: 1,
            clips: rectangle.as_ptr() as u64,
            ..Default::default()
        };
        ioctl(&self.file, 0xb1, &mut dirty)?;
        println!(
            "GPU_KMS_FRAME generation={} framebuffer={}",
            generation, self.framebuffer.id
        );
        Ok(())
    }
}

impl Drop for Kms {
    fn drop(&mut self) {
        if self.modeset {
            self.original.connectors = self.restore_connectors.as_ptr() as u64;
            self.original.connector_count = self.restore_connectors.len() as u32;
            if let Err(error) = ioctl(&self.file, 0xa2, &mut self.original) {
                eprintln!("GPU_KMS_RESTORE_ERROR {error}");
            }
        }
        if self.address != 0 {
            unsafe { libc::munmap(self.address as *mut libc::c_void, self.length) };
        }
        if self.framebuffer.id != 0 {
            let _ = ioctl(&self.file, 0xaf, &mut self.framebuffer.id);
        }
        if self.buffer.handle != 0 {
            let _ = ioctl(&self.file, 0xb4, &mut self.buffer.handle);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn linux_drm_uapi_layouts() {
        assert_eq!(size_of::<Mode>(), 68);
        assert_eq!(size_of::<Resources>(), 64);
        assert_eq!(size_of::<Crtc>(), 104);
        assert_eq!(size_of::<Connector>(), 80);
        assert_eq!(size_of::<Encoder>(), 20);
        assert_eq!(size_of::<DumbBuffer>(), 32);
        assert_eq!(size_of::<Framebuffer>(), 28);
        assert_eq!(size_of::<MapDumb>(), 16);
        assert_eq!(size_of::<Dirty>(), 24);
        assert_eq!(request::<Crtc>(0xa2), 0xc06864a2);
        assert_eq!(request::<Dirty>(0xb1), 0xc01864b1);
    }

    #[test]
    fn xrgb_conversion_respects_pitch_and_preserves_padding() {
        let pitch = WIDTH as usize * 4 + 16;
        let mut frame = vec![0; WIDTH as usize * HEIGHT as usize * 3];
        frame[..6].copy_from_slice(&[255, 128, 64, 0, 100, 200]);
        let mut mapping = vec![0xa5; pitch * HEIGHT as usize];
        copy_rgb(&frame, &mut mapping, pitch, WIDTH, HEIGHT).unwrap();
        assert_eq!(&mapping[..8], &[64, 128, 255, 0, 200, 100, 0, 0]);
        assert_eq!(&mapping[WIDTH as usize * 4..pitch], &[0xa5; 16]);
        assert_eq!(&mapping[pitch..pitch + 4], &[0, 0, 0, 0]);
    }

    #[test]
    fn invalid_frame_and_buffer_layouts_are_rejected() {
        let frame = vec![0; WIDTH as usize * HEIGHT as usize * 3];
        let mut mapping = vec![0; WIDTH as usize * HEIGHT as usize * 4];
        assert!(copy_rgb(
            &frame[..frame.len() - 1],
            &mut mapping,
            WIDTH as usize * 4,
            WIDTH,
            HEIGHT
        )
        .is_err());
        assert!(copy_rgb(&frame, &mut mapping, WIDTH as usize * 4 - 1, WIDTH, HEIGHT).is_err());
        assert!(copy_rgb(
            &frame,
            &mut mapping[..10],
            WIDTH as usize * 4,
            WIDTH,
            HEIGHT
        )
        .is_err());
        assert!(copy_rgb(&frame, &mut mapping, usize::MAX, WIDTH, HEIGHT).is_err());
        assert!(Kms::open("/nonexistent/bao-gpu-kms-test").is_err());
    }

    #[test]
    fn shared_display_mode_converts_full_width_with_padding() {
        let width = WIDTH * 2;
        let pitch = width as usize * 4 + 16;
        let mut frame = vec![0; width as usize * HEIGHT as usize * 3];
        let right = WIDTH as usize * 3;
        frame[right..right + 3].copy_from_slice(&[21, 43, 65]);
        let mut mapping = vec![0xa5; pitch * HEIGHT as usize];
        copy_rgb(&frame, &mut mapping, pitch, width, HEIGHT).unwrap();
        assert_eq!(&mapping[WIDTH as usize * 4..][..4], &[65, 43, 21, 0]);
        assert_eq!(&mapping[width as usize * 4..pitch], &[0xa5; 16]);
        assert!(Kms::open_mode("/nonexistent", 0, HEIGHT).is_err());
        assert!(Kms::open_mode("/nonexistent", 4097, HEIGHT).is_err());
    }
}
