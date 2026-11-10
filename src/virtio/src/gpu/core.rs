use std::collections::BTreeMap;

pub const WIDTH: u32 = 640;
pub const HEIGHT: u32 = 480;
pub const MAX_REQUEST: usize = 32 + 16384 * 16;
const MAX_BYTES: usize = 128 * 1024 * 1024;
const INVALID: u32 = 0x1205;
const RESOURCE: u32 = 0x1203;
const SCANOUT: u32 = 0x1202;
const OOM: u32 = 0x1201;
const UNSPEC: u32 = 0x1200;

pub trait Memory {
    fn contains(&self, address: u64, length: usize) -> bool;
    fn read(&self, address: u64, data: &mut [u8]) -> bool;
    fn write(&self, _address: u64, _data: &[u8]) -> bool {
        false
    }
}

#[derive(Clone, Copy, Default, Debug, PartialEq)]
struct Rect {
    x: u32,
    y: u32,
    width: u32,
    height: u32,
}

impl Rect {
    fn parse(data: &[u8], offset: usize) -> Result<Self, u32> {
        Ok(Self {
            x: word(data, offset)?,
            y: word(data, offset + 4)?,
            width: word(data, offset + 8)?,
            height: word(data, offset + 12)?,
        })
    }

    fn fits(self, width: u32, height: u32) -> bool {
        self.width > 0
            && self.height > 0
            && self
                .x
                .checked_add(self.width)
                .is_some_and(|end| end <= width)
            && self
                .y
                .checked_add(self.height)
                .is_some_and(|end| end <= height)
    }
}

struct Resource {
    width: u32,
    height: u32,
    format: u32,
    pixels: Vec<u8>,
    backing: Vec<(u64, usize)>,
}

impl Resource {
    fn rgb(&self, offset: usize) -> [u8; 4] {
        let pixel = &self.pixels[offset..offset + 4];
        match self.format {
            1 => [pixel[2], pixel[1], pixel[0], pixel[3]],
            2 => [pixel[2], pixel[1], pixel[0], 255],
            3 => [pixel[1], pixel[2], pixel[3], pixel[0]],
            4 => [pixel[1], pixel[2], pixel[3], 255],
            67 => [pixel[0], pixel[1], pixel[2], pixel[3]],
            68 => [pixel[3], pixel[2], pixel[1], 255],
            121 => [pixel[3], pixel[2], pixel[1], pixel[0]],
            134 => [pixel[0], pixel[1], pixel[2], 255],
            _ => unreachable!(),
        }
    }

    fn read_backing(&self, memory: &impl Memory, mut offset: usize, data: &mut [u8]) -> bool {
        let mut copied = 0;
        for &(address, length) in &self.backing {
            if offset >= length {
                offset -= length;
                continue;
            }
            let count = (length - offset).min(data.len() - copied);
            if !memory.read(address + offset as u64, &mut data[copied..copied + count]) {
                return false;
            }
            copied += count;
            offset = 0;
            if copied == data.len() {
                return true;
            }
        }
        false
    }
}

#[derive(Default)]
pub struct Gpu {
    renderer: Option<super::virgl::Gpu3d>,
    resources: BTreeMap<u32, Resource>,
    scanout: Option<(u32, Rect)>,
    cursor: Option<Vec<u8>>,
    cursor_position: (u32, u32),
    hotspot: (u32, u32),
    allocated: usize,
    pub generation: u64,
}

fn word(data: &[u8], offset: usize) -> Result<u32, u32> {
    let bytes = data.get(offset..offset + 4).ok_or(INVALID)?;
    Ok(u32::from_le_bytes(bytes.try_into().unwrap()))
}

fn wide(data: &[u8], offset: usize) -> Result<u64, u32> {
    Ok(word(data, offset)? as u64 | (word(data, offset + 4)? as u64) << 32)
}

fn put(data: &mut [u8], offset: usize, value: u32) {
    data[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
}

impl Gpu {
    pub fn virgl(path: &str) -> std::io::Result<Self> {
        Ok(Self { renderer: Some(super::virgl::Gpu3d::connect(path)?), ..Self::default() })
    }

    pub fn reset(&mut self) {
        let generation = self.generation.wrapping_add(1);
        *self = Self::default();
        self.generation = generation;
    }

    pub fn execute(&mut self, data: &[u8], cursor_queue: bool, memory: &impl Memory) -> Vec<u8> {
        if let Some(renderer) = self.renderer.as_mut() {
            let response = renderer.execute(data, cursor_queue, memory);
            self.generation = renderer.generation;
            return response;
        }
        let result = if data.len() < 24 || data.len() > MAX_REQUEST {
            Err(INVALID)
        } else if word(data, 4).unwrap() & !1 != 0 {
            Err(INVALID)
        } else if word(data, 16).unwrap() != 0 {
            Err(0x1204)
        } else {
            self.command(data, cursor_queue, memory)
        };
        let mut response = match result {
            Ok(response) => response,
            Err(error) => {
                let mut response = vec![0; 24];
                put(&mut response, 0, error);
                response
            }
        };
        if data.len() >= 24 && word(data, 4).unwrap() & 1 != 0 {
            put(&mut response, 4, 1);
            response[8..16].copy_from_slice(&data[8..16]);
            response[16..20].copy_from_slice(&data[16..20]);
        }
        response
    }

    fn command(
        &mut self,
        data: &[u8],
        cursor_queue: bool,
        memory: &impl Memory,
    ) -> Result<Vec<u8>, u32> {
        let command = word(data, 0)?;
        if cursor_queue != matches!(command, 0x300 | 0x301) {
            return Err(INVALID);
        }
        let minimum = match command {
            0x100 => 24,
            0x101 => 40,
            0x102 | 0x106 | 0x107 => 32,
            0x103 | 0x104 => 48,
            0x105 | 0x300 | 0x301 => 56,
            _ => return Err(UNSPEC),
        };
        if data.len() < minimum {
            return Err(INVALID);
        }
        match command {
            0x100 => {
                let mut response = vec![0; 24 + 16 * 24];
                put(&mut response, 0, 0x1101);
                put(&mut response, 32, WIDTH);
                put(&mut response, 36, HEIGHT);
                put(&mut response, 40, 1);
                return Ok(response);
            }
            0x101 => {
                let id = word(data, 24)?;
                let format = word(data, 28)?;
                let width = word(data, 32)?;
                let height = word(data, 36)?;
                if id == 0 || self.resources.contains_key(&id) {
                    return Err(RESOURCE);
                }
                if width == 0
                    || height == 0
                    || width > 4096
                    || height > 4096
                    || ![1, 2, 3, 4, 67, 68, 121, 134].contains(&format)
                {
                    return Err(INVALID);
                }
                let size = width as usize * height as usize * 4;
                if self.resources.len() >= 256 || size > MAX_BYTES - self.allocated {
                    return Err(OOM);
                }
                let mut pixels = Vec::new();
                pixels.try_reserve_exact(size).map_err(|_| OOM)?;
                pixels.resize(size, 0);
                self.resources.insert(
                    id,
                    Resource {
                        width,
                        height,
                        format,
                        pixels,
                        backing: Vec::new(),
                    },
                );
                self.allocated += size;
            }
            0x102 => {
                let id = word(data, 24)?;
                let resource = self.resources.remove(&id).ok_or(RESOURCE)?;
                self.allocated -= resource.pixels.len();
                if self.scanout.is_some_and(|scanout| scanout.0 == id) {
                    self.scanout = None;
                    self.generation = self.generation.wrapping_add(1);
                }
            }
            0x103 => {
                let rect = Rect::parse(data, 24)?;
                if word(data, 40)? != 0 {
                    return Err(SCANOUT);
                }
                let id = word(data, 44)?;
                if id == 0 {
                    self.scanout = None;
                } else {
                    let resource = self.resources.get(&id).ok_or(RESOURCE)?;
                    if !rect.fits(resource.width, resource.height)
                        || rect.width > WIDTH
                        || rect.height > HEIGHT
                    {
                        return Err(INVALID);
                    }
                    self.scanout = Some((id, rect));
                }
                self.generation = self.generation.wrapping_add(1);
            }
            0x104 => {
                let rect = Rect::parse(data, 24)?;
                let id = word(data, 40)?;
                let resource = self.resources.get(&id).ok_or(RESOURCE)?;
                if !rect.fits(resource.width, resource.height) {
                    return Err(INVALID);
                }
                if self.scanout.is_some_and(|scanout| scanout.0 == id) {
                    self.generation = self.generation.wrapping_add(1);
                }
            }
            0x105 => {
                let rect = Rect::parse(data, 24)?;
                let offset = usize::try_from(wide(data, 40)?).map_err(|_| INVALID)?;
                let id = word(data, 48)?;
                let resource = self.resources.get_mut(&id).ok_or(RESOURCE)?;
                if !rect.fits(resource.width, resource.height) || resource.backing.is_empty() {
                    return Err(INVALID);
                }
                let stride = resource.width as usize * 4;
                let row_bytes = rect.width as usize * 4;
                let last = offset
                    .checked_add((rect.height as usize - 1) * stride)
                    .and_then(|end| end.checked_add(row_bytes))
                    .ok_or(INVALID)?;
                let backing_size: usize = resource.backing.iter().map(|entry| entry.1).sum();
                if last > backing_size {
                    return Err(INVALID);
                }
                let mut temporary = vec![0; row_bytes * rect.height as usize];
                for row in 0..rect.height as usize {
                    if !resource.read_backing(
                        memory,
                        offset + row * stride,
                        &mut temporary[row * row_bytes..(row + 1) * row_bytes],
                    ) {
                        return Err(INVALID);
                    }
                }
                for row in 0..rect.height as usize {
                    let destination = (rect.y as usize + row) * stride + rect.x as usize * 4;
                    resource.pixels[destination..destination + row_bytes]
                        .copy_from_slice(&temporary[row * row_bytes..(row + 1) * row_bytes]);
                }
            }
            0x106 => {
                let id = word(data, 24)?;
                let count = word(data, 28)? as usize;
                let resource = self.resources.get_mut(&id).ok_or(RESOURCE)?;
                if count == 0
                    || count > 16384
                    || data.len() != 32 + count * 16
                    || !resource.backing.is_empty()
                {
                    return Err(INVALID);
                }
                let mut entries = Vec::with_capacity(count);
                let mut total = 0usize;
                for index in 0..count {
                    let address = wide(data, 32 + index * 16)?;
                    let length = word(data, 40 + index * 16)? as usize;
                    total = total.checked_add(length).ok_or(INVALID)?;
                    if length == 0 || total > MAX_BYTES || !memory.contains(address, length) {
                        return Err(INVALID);
                    }
                    entries.push((address, length));
                }
                resource.backing = entries;
            }
            0x107 => {
                let resource = self.resources.get_mut(&word(data, 24)?).ok_or(RESOURCE)?;
                resource.backing.clear();
            }
            0x300 | 0x301 => {
                if data.len() < 56 {
                    return Err(INVALID);
                }
                if word(data, 24)? != 0 {
                    return Err(SCANOUT);
                }
                if command == 0x300 {
                    let id = word(data, 40)?;
                    let hot_x = word(data, 44)?;
                    let hot_y = word(data, 48)?;
                    if id == 0 {
                        self.cursor = None;
                    } else {
                        let resource = self.resources.get(&id).ok_or(RESOURCE)?;
                        if resource.width != 64
                            || resource.height != 64
                            || hot_x >= 64
                            || hot_y >= 64
                        {
                            return Err(INVALID);
                        }
                        let mut pixels = Vec::with_capacity(64 * 64 * 4);
                        for offset in (0..resource.pixels.len()).step_by(4) {
                            pixels.extend_from_slice(&resource.rgb(offset));
                        }
                        self.cursor = Some(pixels);
                    }
                    self.hotspot = (hot_x, hot_y);
                }
                self.cursor_position = (word(data, 28)?, word(data, 32)?);
                self.generation = self.generation.wrapping_add(1);
            }
            _ => return Err(INVALID),
        }
        let mut response = vec![0; 24];
        put(&mut response, 0, 0x1100);
        Ok(response)
    }

    pub fn frame_rgb(&self) -> Vec<u8> {
        if let Some(renderer) = &self.renderer {
            return renderer.frame_rgb();
        }
        let mut frame = vec![0u8; WIDTH as usize * HEIGHT as usize * 3];
        if let Some((id, rect)) = self.scanout {
            if let Some(resource) = self.resources.get(&id) {
                for row in 0..rect.height as usize {
                    for column in 0..rect.width as usize {
                        let offset = ((rect.y as usize + row) * resource.width as usize
                            + rect.x as usize
                            + column)
                            * 4;
                        let target = (row * WIDTH as usize + column) * 3;
                        frame[target..target + 3].copy_from_slice(&resource.rgb(offset)[..3]);
                    }
                }
            }
        }
        if let Some(cursor) = &self.cursor {
            let origin_x = self.cursor_position.0 as i64 - self.hotspot.0 as i64;
            let origin_y = self.cursor_position.1 as i64 - self.hotspot.1 as i64;
            for row in 0..64usize {
                for column in 0..64usize {
                    let target_x = origin_x + column as i64;
                    let target_y = origin_y + row as i64;
                    if target_x < 0
                        || target_y < 0
                        || target_x >= WIDTH as i64
                        || target_y >= HEIGHT as i64
                    {
                        continue;
                    }
                    let source = (row * 64 + column) * 4;
                    let target = (target_y as usize * WIDTH as usize + target_x as usize) * 3;
                    let alpha = cursor[source + 3] as u32;
                    for channel in 0..3 {
                        frame[target + channel] = ((cursor[source + channel] as u32 * alpha
                            + frame[target + channel] as u32 * (255 - alpha)
                            + 127)
                            / 255) as u8;
                    }
                }
            }
        }
        frame
    }

    pub fn ppm(&self) -> Vec<u8> {
        let mut ppm = format!("P6\n{} {}\n255\n", WIDTH, HEIGHT).into_bytes();
        ppm.extend_from_slice(&self.frame_rgb());
        ppm
    }
}

#[cfg(test)]
#[path = "tests.rs"]
mod tests;
