use super::core::{Memory, HEIGHT, MAX_REQUEST, WIDTH};
use std::collections::{BTreeMap, BTreeSet};
use std::io::{Read, Write};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};

const INVALID: u32 = 0x1205;
const RESOURCE: u32 = 0x1203;
const CONTEXT: u32 = 0x1204;
const UNSPEC: u32 = 0x1200;
const OOM: u32 = 0x1201;
const LIMIT: usize = 64 * 1024 * 1024;

fn word(data: &[u8], offset: usize) -> Result<u32, u32> {
    Ok(u32::from_le_bytes(
        data.get(offset..offset + 4)
            .ok_or(INVALID)?
            .try_into()
            .unwrap(),
    ))
}

fn wide(data: &[u8], offset: usize) -> Result<u64, u32> {
    Ok(word(data, offset)? as u64 | (word(data, offset + 4)? as u64) << 32)
}

fn words(values: &[u32]) -> Vec<u8> {
    values
        .iter()
        .flat_map(|value| value.to_le_bytes())
        .collect()
}

fn response(kind: u32, payload: &[u8]) -> Vec<u8> {
    let mut result = vec![0; 24];
    result[..4].copy_from_slice(&kind.to_le_bytes());
    result.extend_from_slice(payload);
    result
}

trait Renderer: Send {
    fn call(&mut self, operation: u32, payload: &[u8]) -> Result<Vec<u8>, u32>;
}

struct Worker {
    child: Child,
    input: ChildStdin,
    output: ChildStdout,
    failed: bool,
}

impl Worker {
    fn receive(&mut self) -> Result<Vec<u8>, u32> {
        let mut header = [0; 8];
        self.output.read_exact(&mut header).map_err(|_| UNSPEC)?;
        let status = word(&header, 0)?;
        let size = word(&header, 4)? as usize;
        if size > LIMIT || (status != 0 && size != 0) {
            return Err(UNSPEC);
        }
        let mut data = vec![0; size];
        self.output.read_exact(&mut data).map_err(|_| UNSPEC)?;
        if status != 0 {
            Err(UNSPEC)
        } else {
            Ok(data)
        }
    }
}

impl Renderer for Worker {
    fn call(&mut self, operation: u32, payload: &[u8]) -> Result<Vec<u8>, u32> {
        if self.failed || payload.len() > LIMIT + 60 {
            return Err(UNSPEC);
        }
        let result = (|| {
            self.input
                .write_all(&words(&[operation, payload.len() as u32]))
                .map_err(|_| UNSPEC)?;
            self.input.write_all(payload).map_err(|_| UNSPEC)?;
            self.input.flush().map_err(|_| UNSPEC)?;
            self.receive()
        })();
        if result.is_err() {
            self.failed = true;
            let _ = self.child.kill();
        }
        result
    }
}

impl Drop for Worker {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

struct Capset {
    id: u32,
    version: u32,
    data: Vec<u8>,
}

struct Resource {
    width: u32,
    height: u32,
    depth: u32,
    layers: u32,
    target: u32,
    format: u32,
    level: u32,
    budget: usize,
    backing: Vec<(u64, usize)>,
}

pub struct Gpu3d {
    renderer: Box<dyn Renderer>,
    capsets: Vec<Capset>,
    resources: BTreeMap<u32, Resource>,
    contexts: BTreeMap<u32, BTreeSet<u32>>,
    allocated: usize,
    backing_bytes: usize,
    scanout: Option<(u32, [u32; 4])>,
    pixels: Vec<u8>,
    cursor: Option<Vec<u8>>,
    cursor_position: (u32, u32),
    hotspot: (u32, u32),
    pub generation: u64,
}

impl Gpu3d {
    pub fn connect(path: &str) -> std::io::Result<Self> {
        let mut child = Command::new(path)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()?;
        let input = child.stdin.take().unwrap();
        let output = child.stdout.take().unwrap();
        let mut worker = Worker {
            child,
            input,
            output,
            failed: false,
        };
        let caps = worker
            .receive()
            .map_err(|_| std::io::Error::other("virgl worker initialization failed"))?;
        Self::new(Box::new(worker), &caps)
            .map_err(|_| std::io::Error::other("invalid renderer capsets"))
    }

    fn new(renderer: Box<dyn Renderer>, caps: &[u8]) -> Result<Self, u32> {
        let mut capsets = Vec::new();
        let mut offset = 0;
        for id in 1..=2 {
            if word(caps, offset)? != id {
                return Err(INVALID);
            }
            let version = word(caps, offset + 4)?;
            let size = word(caps, offset + 8)? as usize;
            if version == 0 || size == 0 || size > 65536 {
                return Err(INVALID);
            }
            let data = caps
                .get(offset + 12..offset + 12 + size)
                .ok_or(INVALID)?
                .to_vec();
            capsets.push(Capset { id, version, data });
            offset += size + 12;
        }
        if offset != caps.len() {
            return Err(INVALID);
        }
        Ok(Self {
            renderer,
            capsets,
            resources: BTreeMap::new(),
            contexts: BTreeMap::new(),
            allocated: 0,
            backing_bytes: 0,
            scanout: None,
            pixels: vec![0; WIDTH as usize * HEIGHT as usize * 3],
            cursor: None,
            cursor_position: (0, 0),
            hotspot: (0, 0),
            generation: 0,
        })
    }

    pub fn execute(&mut self, data: &[u8], cursor_queue: bool, memory: &impl Memory) -> Vec<u8> {
        let result = (|| {
            if data.len() < 24 || data.len() > MAX_REQUEST || word(data, 4)? & !1 != 0 {
                return Err(INVALID);
            }
            let command = word(data, 0)?;
            if cursor_queue != matches!(command, 0x300 | 0x301) {
                return Err(INVALID);
            }
            let result = self.command(data, memory)?;
            if word(data, 4)? & 1 != 0 && command != 0x207 {
                self.renderer.call(10, &[])?;
            }
            Ok(result)
        })();
        let mut result = result.unwrap_or_else(|error| response(error, &[]));
        if data.len() >= 24 && word(data, 4).unwrap() & 1 != 0 {
            result[4..20].copy_from_slice(&data[4..20]);
        }
        if data.len() >= 24 && matches!(word(data, 0), Ok(0x200..=0x207)) {
            eprintln!(
                "GPU_VIRGL command={:#x} context={} response={:#x}",
                word(data, 0).unwrap(),
                word(data, 16).unwrap(),
                word(&result, 0).unwrap()
            );
        }
        result
    }

    fn command(&mut self, data: &[u8], memory: &impl Memory) -> Result<Vec<u8>, u32> {
        let command = word(data, 0)?;
        let context = word(data, 16)?;
        if context != 0 && !matches!(command, 0x200..=0x207) {
            return Err(CONTEXT);
        }
        match command {
            0x100 if data.len() == 24 => {
                let mut payload = vec![0; 16 * 24];
                payload[8..20].copy_from_slice(&words(&[WIDTH, HEIGHT, 1]));
                return Ok(response(0x1101, &payload));
            }
            0x108 if data.len() == 32 => {
                let caps = self.capsets.get(word(data, 24)? as usize).ok_or(INVALID)?;
                return Ok(response(
                    0x1102,
                    &words(&[caps.id, caps.version, caps.data.len() as u32, 0]),
                ));
            }
            0x109 if data.len() == 32 => {
                let caps = self
                    .capsets
                    .iter()
                    .find(|caps| caps.id == word(data, 24).unwrap())
                    .ok_or(INVALID)?;
                if word(data, 28)? > caps.version {
                    return Err(INVALID);
                }
                return Ok(response(0x1103, &caps.data));
            }
            0x101 if data.len() == 40 => {
                let format = word(data, 28)?;
                if !valid_format(format) {
                    return Err(INVALID);
                }
                let values = [
                    word(data, 24)?,
                    2,
                    format,
                    (1 << 1) | (1 << 3) | (1 << 18),
                    word(data, 32)?,
                    word(data, 36)?,
                    1,
                    1,
                    0,
                    0,
                    1,
                ];
                self.create(&words(&values))?;
            }
            0x204 if data.len() == 72 => self.create(&data[24..68])?,
            0x102 if data.len() == 32 => {
                let id = word(data, 24)?;
                let resource = self.resources.get(&id).ok_or(RESOURCE)?;
                let budget = resource.budget;
                let backing: usize = resource.backing.iter().map(|entry| entry.1).sum();
                self.renderer.call(2, &words(&[id]))?;
                self.resources.remove(&id);
                self.allocated -= budget;
                self.backing_bytes -= backing;
                for attached in self.contexts.values_mut() {
                    attached.remove(&id);
                }
                if self.scanout.is_some_and(|scanout| scanout.0 == id) {
                    self.scanout = None;
                    self.pixels.fill(0);
                    self.generation = self.generation.wrapping_add(1);
                }
            }
            0x103 if data.len() == 48 => {
                if word(data, 40)? != 0 {
                    return Err(0x1202);
                }
                let id = word(data, 44)?;
                if id == 0 {
                    self.scanout = None;
                    self.pixels.fill(0);
                } else {
                    let rect = rectangle(data, 24)?;
                    let resource = self.resources.get(&id).ok_or(RESOURCE)?;
                    if !fits(rect, resource.width, resource.height)
                        || !valid_format(resource.format)
                        || rect[2] > WIDTH
                        || rect[3] > HEIGHT
                    {
                        return Err(INVALID);
                    }
                    self.scanout = Some((id, rect));
                    self.refresh()?;
                }
                self.generation = self.generation.wrapping_add(1);
            }
            0x104 if data.len() == 48 => {
                let id = word(data, 40)?;
                let resource = self.resources.get(&id).ok_or(RESOURCE)?;
                if !fits(rectangle(data, 24)?, resource.width, resource.height) {
                    return Err(INVALID);
                }
                if self.scanout.is_some_and(|scanout| scanout.0 == id) {
                    self.refresh()?;
                    self.generation = self.generation.wrapping_add(1);
                }
            }
            0x106 => self.attach(data, memory)?,
            0x107 if data.len() == 32 => {
                let id = word(data, 24)?;
                let resource = self.resources.get_mut(&id).ok_or(RESOURCE)?;
                self.renderer.call(7, &words(&[id, 0]))?;
                self.backing_bytes -= resource.backing.iter().map(|entry| entry.1).sum::<usize>();
                resource.backing.clear();
            }
            0x105 if data.len() == 56 => {
                let rect = rectangle(data, 24)?;
                let id = word(data, 48)?;
                let resource = self.resources.get(&id).ok_or(RESOURCE)?;
                if !valid_format(resource.format) || !fits(rect, resource.width, resource.height) {
                    return Err(INVALID);
                }
                let parameters = [
                    id,
                    0,
                    0,
                    resource.width * 4,
                    0,
                    rect[0],
                    rect[1],
                    0,
                    rect[2],
                    rect[3],
                    1,
                ];
                self.transfer(&parameters, wide(data, 40)?, false, memory)?;
            }
            0x200 if data.len() == 96 => {
                let size = word(data, 24)? as usize;
                if context == 0
                    || self.contexts.contains_key(&context)
                    || size > 64
                    || word(data, 28)? != 0
                {
                    return Err(CONTEXT);
                }
                if self.contexts.len() >= 256 {
                    return Err(OOM);
                }
                let mut payload = words(&[context, size as u32]);
                payload.extend_from_slice(&data[32..32 + size]);
                self.renderer.call(3, &payload)?;
                self.contexts.insert(context, BTreeSet::new());
            }
            0x201 if data.len() == 24 => {
                if !self.contexts.contains_key(&context) {
                    return Err(CONTEXT);
                }
                self.renderer.call(4, &words(&[context]))?;
                self.contexts.remove(&context);
            }
            0x202 | 0x203 if data.len() == 32 => {
                let id = word(data, 24)?;
                if !self.resources.contains_key(&id) {
                    return Err(RESOURCE);
                }
                let attached = self.contexts.get_mut(&context).ok_or(CONTEXT)?;
                let attach = command == 0x202;
                self.renderer
                    .call(5, &words(&[context, id, u32::from(attach)]))?;
                if attach {
                    attached.insert(id);
                } else {
                    attached.remove(&id);
                }
            }
            0x205 | 0x206 if data.len() == 72 => {
                if context != 0 && !self.contexts.contains_key(&context) {
                    return Err(CONTEXT);
                }
                let parameters = [
                    word(data, 56)?,
                    context,
                    word(data, 60)?,
                    word(data, 64)?,
                    word(data, 68)?,
                    word(data, 24)?,
                    word(data, 28)?,
                    word(data, 32)?,
                    word(data, 36)?,
                    word(data, 40)?,
                    word(data, 44)?,
                ];
                self.transfer(&parameters, wide(data, 48)?, command == 0x206, memory)?;
            }
            0x207 if data.len() >= 32 => {
                if !self.contexts.contains_key(&context) {
                    return Err(CONTEXT);
                }
                let size = word(data, 24)? as usize;
                if size == 0 || size % 4 != 0 || size != data.len() - 32 {
                    return Err(INVALID);
                }
                let mut payload = words(&[context]);
                payload.extend_from_slice(&data[32..]);
                let snapshots = self.upload_context(context, memory)?;
                self.renderer.call(6, &payload)?;
                self.renderer.call(10, &[])?;
                self.download_context(snapshots, memory)?;
            }
            0x300 | 0x301 if data.len() == 56 => {
                if word(data, 24)? != 0 {
                    return Err(0x1202);
                }
                if command == 0x300 {
                    let id = word(data, 40)?;
                    let hot_x = word(data, 44)?;
                    let hot_y = word(data, 48)?;
                    if hot_x >= 64 || hot_y >= 64 {
                        return Err(INVALID);
                    }
                    self.cursor = if id == 0 {
                        None
                    } else {
                        let resource = self.resources.get(&id).ok_or(RESOURCE)?;
                        if resource.width != 64
                            || resource.height != 64
                            || !valid_format(resource.format)
                        {
                            return Err(INVALID);
                        }
                        let format = resource.format;
                        let pixels = self.renderer.call(9, &words(&[id, 0, 0, 64, 64]))?;
                        if pixels.len() != 64 * 64 * 4 {
                            return Err(UNSPEC);
                        }
                        Some(
                            pixels
                                .chunks_exact(4)
                                .flat_map(|pixel| rgba(format, pixel))
                                .collect(),
                        )
                    };
                    self.hotspot = (hot_x, hot_y);
                }
                self.cursor_position = (word(data, 28)?, word(data, 32)?);
                self.generation = self.generation.wrapping_add(1);
            }
            _ => return Err(INVALID),
        }
        Ok(response(0x1100, &[]))
    }

    fn create(&mut self, data: &[u8]) -> Result<(), u32> {
        let id = word(data, 0)?;
        if id == 0 || self.resources.contains_key(&id) {
            return Err(RESOURCE);
        }
        let width = word(data, 16)?;
        let height = word(data, 20)?;
        let depth = word(data, 24)?;
        let layers = word(data, 28)?;
        let level = word(data, 32)?;
        let samples = word(data, 36)?;
        let target = word(data, 4)?;
        let format = word(data, 8)?;
        if width == 0
            || height == 0
            || depth == 0
            || layers == 0
            || width as usize > LIMIT
            || height > 4096
            || depth > 4096
            || layers > 2048
            || level > 12
            || samples > 16
            || target > 9
            || word(data, 40)? & !1 != 0
            || (target == 0
                && (height != 1 || depth != 1 || layers != 1 || level != 0 || samples > 1))
        {
            return Err(INVALID);
        }
        let element_bytes = if target == 0 {
            1
        } else if valid_format(format)
            || [8, 17, 18, 19, 20, 21, 22, 28, 32, 36, 40, 44, 87].contains(&format)
        {
            4
        } else {
            32
        };
        let budget = (width as usize)
            .checked_mul(height as usize)
            .and_then(|size| size.checked_mul(depth as usize))
            .and_then(|size| size.checked_mul(layers as usize))
            .and_then(|size| size.checked_mul(element_bytes))
            .and_then(|size| size.checked_mul(samples.max(1) as usize))
            .and_then(|size| size.checked_mul(if level == 0 { 1 } else { 2 }))
            .ok_or(OOM)?;
        if self.resources.len() >= 256 || budget > LIMIT - self.allocated {
            eprintln!("VIRGL_RESOURCE_BUDGET id={id} target={target} format={format} requested={budget} allocated={} limit={LIMIT}", self.allocated);
            return Err(OOM);
        }
        self.renderer.call(1, data)?;
        self.resources.insert(
            id,
            Resource {
                width,
                height,
                depth,
                layers,
                target,
                format,
                level,
                budget,
                backing: Vec::new(),
            },
        );
        self.allocated += budget;
        println!(
            "VIRGL_RESOURCE id={id} target={target} format={format} budget={budget} allocated={}",
            self.allocated
        );
        Ok(())
    }

    fn attach(&mut self, data: &[u8], memory: &impl Memory) -> Result<(), u32> {
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
        let mut backing = Vec::with_capacity(count);
        let mut total = 0usize;
        for index in 0..count {
            let address = wide(data, 32 + index * 16)?;
            let length = word(data, 40 + index * 16)? as usize;
            total = total.checked_add(length).ok_or(INVALID)?;
            if length == 0
                || total > LIMIT - self.backing_bytes
                || !memory.contains(address, length)
            {
                return Err(INVALID);
            }
            backing.push((address, length));
        }
        self.renderer.call(7, &words(&[id, total as u32]))?;
        resource.backing = backing;
        self.backing_bytes += total;
        Ok(())
    }

    fn transfer(
        &mut self,
        parameters: &[u32; 11],
        offset: u64,
        read: bool,
        memory: &impl Memory,
    ) -> Result<(), u32> {
        let resource = self.resources.get(&parameters[0]).ok_or(RESOURCE)?;
        let level = parameters[2];
        let rect = [parameters[5], parameters[6], parameters[8], parameters[9]];
        let size: usize = resource.backing.iter().map(|entry| entry.1).sum();
        let depth = match resource.target {
            3 => (resource.depth >> level).max(1),
            4 | 6 | 7 | 8 => resource.layers,
            _ => 1,
        };
        if level > resource.level
            || !fits(
                rect,
                (resource.width >> level).max(1),
                (resource.height >> level).max(1),
            )
            || parameters[10] == 0
            || parameters[7]
                .checked_add(parameters[10])
                .is_none_or(|end| end > depth)
            || size == 0
            || offset >= size as u64
        {
            return Err(INVALID);
        }
        let mut payload = words(parameters);
        payload.extend_from_slice(&offset.to_le_bytes());
        payload.extend_from_slice(&words(&[u32::from(read), size as u32]));
        for &(address, length) in &resource.backing {
            let start = payload.len();
            payload.resize(start + length, 0);
            if !memory.read(address, &mut payload[start..]) {
                return Err(INVALID);
            }
        }
        let pixels = self.renderer.call(8, &payload)?;
        if read {
            if pixels.len() != size {
                return Err(UNSPEC);
            }
            let mut start = 0;
            for &(address, length) in &resource.backing {
                if !memory.write(address, &pixels[start..start + length]) {
                    return Err(INVALID);
                }
                start += length;
            }
        } else if !pixels.is_empty() {
            return Err(UNSPEC);
        }
        Ok(())
    }

    fn refresh(&mut self) -> Result<(), u32> {
        if let Some((id, rect)) = self.scanout {
            let format = self.resources.get(&id).ok_or(RESOURCE)?.format;
            let pixels = self
                .renderer
                .call(9, &words(&[id, rect[0], rect[1], rect[2], rect[3]]))?;
            if pixels.len() != rect[2] as usize * rect[3] as usize * 4 {
                return Err(UNSPEC);
            }
            self.pixels.fill(0);
            for row in 0..rect[3] as usize {
                for column in 0..rect[2] as usize {
                    let source = (row * rect[2] as usize + column) * 4;
                    let target = (row * WIDTH as usize + column) * 3;
                    self.pixels[target..target + 3]
                        .copy_from_slice(&rgba(format, &pixels[source..source + 4])[..3]);
                }
            }
        }
        Ok(())
    }

    fn upload_context(
        &mut self,
        context: u32,
        memory: &impl Memory,
    ) -> Result<Vec<(u32, Vec<u8>)>, u32> {
        let mut snapshots = Vec::new();
        for id in self.contexts.get(&context).ok_or(CONTEXT)? {
            let resource = self.resources.get(id).ok_or(RESOURCE)?;
            if resource.backing.is_empty() {
                continue;
            }
            let mut payload = words(&[*id]);
            for &(address, length) in &resource.backing {
                let start = payload.len();
                payload.resize(start + length, 0);
                if !memory.read(address, &mut payload[start..]) {
                    return Err(INVALID);
                }
            }
            self.renderer.call(11, &payload)?;
            snapshots.push((*id, payload[4..].to_vec()));
        }
        Ok(snapshots)
    }

    fn download_context(
        &mut self,
        snapshots: Vec<(u32, Vec<u8>)>,
        memory: &impl Memory,
    ) -> Result<(), u32> {
        for (id, before) in snapshots {
            let after = self.renderer.call(12, &words(&[id]))?;
            if after.len() != before.len() {
                return Err(UNSPEC);
            }
            let resource = self.resources.get(&id).ok_or(RESOURCE)?;
            let mut start = 0;
            for &(address, length) in &resource.backing {
                let mut offset = 0;
                while offset < length {
                    if before[start + offset] == after[start + offset] {
                        offset += 1;
                        continue;
                    }
                    let changed = offset;
                    while offset < length && before[start + offset] != after[start + offset] {
                        offset += 1;
                    }
                    if !memory.write(
                        address + changed as u64,
                        &after[start + changed..start + offset],
                    ) {
                        return Err(INVALID);
                    }
                }
                start += length;
            }
        }
        Ok(())
    }

    pub fn frame_rgb(&self) -> Vec<u8> {
        let mut pixels = self.pixels.clone();
        if let Some(cursor) = &self.cursor {
            for row in 0..64usize {
                for column in 0..64usize {
                    let target_x =
                        self.cursor_position.0 as i64 - self.hotspot.0 as i64 + column as i64;
                    let target_y =
                        self.cursor_position.1 as i64 - self.hotspot.1 as i64 + row as i64;
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
                        pixels[target + channel] = ((cursor[source + channel] as u32 * alpha
                            + pixels[target + channel] as u32 * (255 - alpha)
                            + 127)
                            / 255) as u8;
                    }
                }
            }
        }
        pixels
    }
}

fn rectangle(data: &[u8], offset: usize) -> Result<[u32; 4], u32> {
    Ok([
        word(data, offset)?,
        word(data, offset + 4)?,
        word(data, offset + 8)?,
        word(data, offset + 12)?,
    ])
}

fn fits(rect: [u32; 4], width: u32, height: u32) -> bool {
    rect[2] != 0
        && rect[3] != 0
        && rect[0].checked_add(rect[2]).is_some_and(|end| end <= width)
        && rect[1]
            .checked_add(rect[3])
            .is_some_and(|end| end <= height)
}

fn valid_format(format: u32) -> bool {
    [1, 2, 3, 4, 67, 68, 121, 134].contains(&format)
}

fn rgba(format: u32, pixel: &[u8]) -> [u8; 4] {
    match format {
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

#[cfg(test)]
#[path = "virgl-tests.rs"]
mod tests;
