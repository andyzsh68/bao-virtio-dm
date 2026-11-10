use super::*;
use std::cell::RefCell;
use std::sync::{Arc, Mutex};

type Calls = Arc<Mutex<Vec<(u32, Vec<u8>)>>>;

struct Fake {
    calls: Calls,
    fail: Option<u32>,
    shadows: BTreeMap<u32, Vec<u8>>,
}

impl Renderer for Fake {
    fn call(&mut self, operation: u32, payload: &[u8]) -> Result<Vec<u8>, u32> {
        self.calls
            .lock()
            .unwrap()
            .push((operation, payload.to_vec()));
        if self.fail == Some(operation) {
            return Err(UNSPEC);
        }
        match operation {
            6 => {
                for shadow in self.shadows.values_mut() {
                    if !shadow.is_empty() {
                        shadow[0] ^= 0xff;
                    }
                }
                Ok(Vec::new())
            }
            11 => {
                self.shadows
                    .insert(word(payload, 0)?, payload[4..].to_vec());
                Ok(Vec::new())
            }
            12 => self
                .shadows
                .get(&word(payload, 0)?)
                .cloned()
                .ok_or(RESOURCE),
            8 if word(payload, 52)? == 1 => Ok(vec![42; word(payload, 56)? as usize]),
            9 => Ok(vec![
                0;
                word(payload, 12)? as usize
                    * word(payload, 16)? as usize
                    * 4
            ]),
            _ => Ok(Vec::new()),
        }
    }
}

struct Ram(RefCell<Vec<u8>>);

impl Memory for Ram {
    fn contains(&self, address: u64, length: usize) -> bool {
        address
            .checked_add(length as u64)
            .is_some_and(|end| end <= self.0.borrow().len() as u64)
    }
    fn read(&self, address: u64, data: &mut [u8]) -> bool {
        if !self.contains(address, data.len()) {
            return false;
        }
        data.copy_from_slice(&self.0.borrow()[address as usize..address as usize + data.len()]);
        true
    }
    fn write(&self, address: u64, data: &[u8]) -> bool {
        if !self.contains(address, data.len()) {
            return false;
        }
        self.0.borrow_mut()[address as usize..address as usize + data.len()].copy_from_slice(data);
        true
    }
}

fn caps() -> Vec<u8> {
    words(&[1, 1, 4, 123, 2, 2, 4, 456])
}

fn gpu(fail: Option<u32>) -> (Gpu3d, Calls) {
    let calls = Arc::new(Mutex::new(Vec::new()));
    let renderer = Box::new(Fake {
        calls: calls.clone(),
        fail,
        shadows: BTreeMap::new(),
    });
    (Gpu3d::new(renderer, &caps()).unwrap(), calls)
}

fn request(command: u32, context: u32, payload: &[u32]) -> Vec<u8> {
    let mut data = words(&[command, 0, 0, 0, context, 0]);
    data.extend_from_slice(&words(payload));
    data
}

fn run(gpu: &mut Gpu3d, command: u32, context: u32, payload: &[u32], ram: &Ram) -> Vec<u8> {
    gpu.execute(&request(command, context, payload), command >= 0x300, ram)
}

fn create(gpu: &mut Gpu3d, ram: &Ram) {
    assert_eq!(
        word(
            &run(
                gpu,
                0x204,
                0,
                &[1, 2, 2, 2 | 8, 8, 8, 1, 1, 0, 0, 1, 0],
                ram
            ),
            0
        ),
        Ok(0x1100)
    );
}

fn context(gpu: &mut Gpu3d, ram: &Ram) {
    assert_eq!(word(&run(gpu, 0x200, 7, &[0; 18], ram), 0), Ok(0x1100));
}

#[test]
fn bounded_renderer_capsets() {
    let (mut gpu, _) = gpu(None);
    let ram = Ram(RefCell::new(Vec::new()));
    let info = run(&mut gpu, 0x108, 0, &[1, 0], &ram);
    assert_eq!(&info[24..], &words(&[2, 2, 4, 0]));
    assert_eq!(
        &run(&mut gpu, 0x109, 0, &[2, 0], &ram)[24..],
        &words(&[456])
    );
    assert_eq!(
        word(&run(&mut gpu, 0x108, 0, &[2, 0], &ram), 0),
        Ok(INVALID)
    );
    assert_eq!(
        word(&run(&mut gpu, 0x109, 0, &[2, 3], &ram), 0),
        Ok(INVALID)
    );
    assert!(Gpu3d::new(
        Box::new(Fake {
            calls: Arc::default(),
            fail: None,
            shadows: BTreeMap::new()
        }),
        &caps()[..20]
    )
    .is_err());
}

#[test]
fn context_lifecycle_and_attachment() {
    let (mut gpu, calls) = gpu(None);
    let ram = Ram(RefCell::new(vec![0; 4096]));
    create(&mut gpu, &ram);
    context(&mut gpu, &ram);
    assert_eq!(
        word(&run(&mut gpu, 0x200, 7, &[0; 18], &ram), 0),
        Ok(CONTEXT)
    );
    assert_eq!(word(&run(&mut gpu, 0x202, 7, &[1, 0], &ram), 0), Ok(0x1100));
    assert!(gpu.contexts[&7].contains(&1));
    assert_eq!(word(&run(&mut gpu, 0x203, 7, &[1, 0], &ram), 0), Ok(0x1100));
    assert!(!gpu.contexts[&7].contains(&1));
    assert_eq!(word(&run(&mut gpu, 0x201, 7, &[], &ram), 0), Ok(0x1100));
    assert_eq!(
        word(&run(&mut gpu, 0x202, 7, &[1, 0], &ram), 0),
        Ok(CONTEXT)
    );
    assert_eq!(
        calls
            .lock()
            .unwrap()
            .iter()
            .map(|call| call.0)
            .collect::<Vec<_>>(),
        [1, 3, 5, 5, 4]
    );
}

#[test]
fn submit_is_aligned_bounded_and_fenced_after_renderer() {
    let (mut gpu, calls) = gpu(None);
    let ram = Ram(RefCell::new(Vec::new()));
    context(&mut gpu, &ram);
    let mut data = request(0x207, 7, &[4, 0, 0]);
    data[4..16].copy_from_slice(&words(&[1, 0x89abcdef, 0xfedcba98]));
    let result = gpu.execute(&data, false, &ram);
    assert_eq!(word(&result, 0), Ok(0x1100));
    assert_eq!(&result[4..20], &data[4..20]);
    let calls = calls.lock().unwrap();
    assert_eq!(calls[calls.len() - 2].0, 6);
    assert_eq!(calls[calls.len() - 1].0, 10);
    drop(calls);
    assert_eq!(
        word(&run(&mut gpu, 0x207, 7, &[3, 0, 0], &ram), 0),
        Ok(INVALID)
    );
    assert_eq!(
        word(&run(&mut gpu, 0x207, 8, &[4, 0, 0], &ram), 0),
        Ok(CONTEXT)
    );
}

#[test]
fn renderer_failure_never_reports_successful_fence() {
    let (mut gpu, _) = gpu(Some(10));
    let ram = Ram(RefCell::new(Vec::new()));
    let data = words(&[0x100, 1, 123, 456, 0, 0]);
    let result = gpu.execute(&data, false, &ram);
    assert_eq!(word(&result, 0), Ok(UNSPEC));
    assert_eq!(&result[8..16], &data[8..16]);
}

#[test]
fn submitted_backing_is_uploaded_and_changed_bytes_return_after_fence() {
    let (mut gpu, calls) = gpu(None);
    let ram = Ram(RefCell::new(vec![19; 4096]));
    create(&mut gpu, &ram);
    context(&mut gpu, &ram);
    assert_eq!(
        word(&run(&mut gpu, 0x106, 0, &[1, 1, 32, 0, 256, 0], &ram), 0),
        Ok(0x1100)
    );
    assert_eq!(word(&run(&mut gpu, 0x202, 7, &[1, 0], &ram), 0), Ok(0x1100));
    assert_eq!(
        word(&run(&mut gpu, 0x207, 7, &[4, 0, 0], &ram), 0),
        Ok(0x1100)
    );
    assert_eq!(ram.0.borrow()[32], 19 ^ 0xff);
    assert!(ram.0.borrow()[33..288].iter().all(|byte| *byte == 19));
    assert_eq!(ram.0.borrow()[31], 19);
    let operations: Vec<_> = calls.lock().unwrap().iter().map(|call| call.0).collect();
    assert_eq!(&operations[operations.len() - 4..], &[11, 6, 10, 12]);
}

#[test]
fn scatter_gather_round_trip_and_detach() {
    let (mut gpu, calls) = gpu(None);
    let ram = Ram(RefCell::new(vec![19; 4096]));
    create(&mut gpu, &ram);
    assert_eq!(
        word(
            &run(
                &mut gpu,
                0x106,
                0,
                &[1, 2, 32, 0, 128, 0, 512, 0, 128, 0],
                &ram
            ),
            0
        ),
        Ok(0x1100)
    );
    let transfer = [0, 0, 0, 8, 8, 1, 0, 0, 1, 0, 32, 0];
    assert_eq!(
        word(&run(&mut gpu, 0x205, 0, &transfer, &ram), 0),
        Ok(0x1100)
    );
    assert_eq!(
        word(&run(&mut gpu, 0x206, 0, &transfer, &ram), 0),
        Ok(0x1100)
    );
    assert!(ram.0.borrow()[32..160].iter().all(|byte| *byte == 42));
    assert!(ram.0.borrow()[512..640].iter().all(|byte| *byte == 42));
    assert_eq!(ram.0.borrow()[160], 19);
    assert!(calls
        .lock()
        .unwrap()
        .iter()
        .any(|(op, payload)| *op == 8 && payload[60..].iter().all(|byte| *byte == 19)));
    assert_eq!(word(&run(&mut gpu, 0x107, 0, &[1, 0], &ram), 0), Ok(0x1100));
    assert_eq!(gpu.backing_bytes, 0);
    assert_eq!(
        word(&run(&mut gpu, 0x205, 0, &transfer, &ram), 0),
        Ok(INVALID)
    );
}

#[test]
fn invalid_guest_backing_and_resource_budgets_are_rejected() {
    let (mut gpu, _) = gpu(None);
    let ram = Ram(RefCell::new(vec![0; 1024]));
    create(&mut gpu, &ram);
    assert_eq!(
        word(
            &run(&mut gpu, 0x106, 0, &[1, 1, u32::MAX, u32::MAX, 4, 0], &ram),
            0
        ),
        Ok(INVALID)
    );
    assert_eq!(
        word(&run(&mut gpu, 0x106, 0, &[1, 1, 0, 0, 0, 0], &ram), 0),
        Ok(INVALID)
    );
    assert_eq!(
        word(
            &run(
                &mut gpu,
                0x204,
                0,
                &[2, 2, 2, 2, 8192, 4096, 1, 1, 0, 0, 0, 0],
                &ram
            ),
            0
        ),
        Ok(OOM)
    );
    assert_eq!(word(&run(&mut gpu, 0x102, 0, &[1, 0], &ram), 0), Ok(0x1100));
    assert_eq!(gpu.allocated, 0);
}

#[test]
fn byte_buffers_and_desktop_scanouts_use_the_existing_budget() {
    let (mut gpu, _) = gpu(None);
    let ram = Ram(RefCell::new(Vec::new()));
    for identifier in 1..=8 {
        assert_eq!(
            word(
                &run(
                    &mut gpu,
                    0x204,
                    0,
                    &[identifier, 0, 64, 16, 4 * 1024 * 1024, 1, 1, 1, 0, 0, 0, 0],
                    &ram
                ),
                0
            ),
            Ok(0x1100)
        );
    }
    for identifier in 9..=16 {
        assert_eq!(
            word(
                &run(
                    &mut gpu,
                    0x204,
                    0,
                    &[identifier, 2, 2, 2, 640, 480, 1, 1, 0, 0, 0, 0],
                    &ram
                ),
                0
            ),
            Ok(0x1100)
        );
    }
    assert_eq!(gpu.allocated, 32 * 1024 * 1024 + 8 * 640 * 480 * 4);
    assert!(gpu.allocated <= LIMIT);
    for identifier in 1..=16 {
        assert_eq!(
            word(&run(&mut gpu, 0x102, 0, &[identifier, 0], &ram), 0),
            Ok(0x1100)
        );
    }
    assert_eq!(gpu.allocated, 0);
}

#[test]
fn unknown_formats_keep_a_conservative_256_bit_texel_budget() {
    let ram = Ram(RefCell::new(Vec::new()));
    for (format, element_bytes) in [(1, 4), (19, 4), (67, 4), (27, 32), (31, 32), (u32::MAX, 32)] {
        let (mut gpu, _) = gpu(None);
        assert_eq!(
            word(
                &run(
                    &mut gpu,
                    0x204,
                    0,
                    &[1, 2, format, 2, 16, 8, 1, 1, 2, 4, 0, 0],
                    &ram
                ),
                0
            ),
            Ok(0x1100)
        );
        assert_eq!(gpu.allocated, 16 * 8 * element_bytes * 2 * 4);
    }
}

#[test]
fn byte_buffer_dimensions_cannot_bypass_resource_accounting() {
    let ram = Ram(RefCell::new(Vec::new()));
    for (height, depth, layers, level, samples) in [
        (2, 1, 1, 0, 0),
        (1, 2, 1, 0, 0),
        (1, 1, 2, 0, 0),
        (1, 1, 1, 1, 0),
        (1, 1, 1, 0, 2),
    ] {
        let (mut gpu, calls) = gpu(None);
        assert_eq!(
            word(
                &run(
                    &mut gpu,
                    0x204,
                    0,
                    &[1, 0, 64, 16, 1024, height, depth, layers, level, samples, 0, 0],
                    &ram
                ),
                0
            ),
            Ok(INVALID)
        );
        assert_eq!(gpu.allocated, 0);
        assert!(calls.lock().unwrap().is_empty());
    }
}

#[test]
fn exact_byte_budget_limit_is_reclaimed_without_expansion() {
    let (mut gpu, _) = gpu(None);
    let ram = Ram(RefCell::new(Vec::new()));
    assert_eq!(
        word(
            &run(
                &mut gpu,
                0x204,
                0,
                &[1, 0, 64, 16, LIMIT as u32, 1, 1, 1, 0, 0, 0, 0],
                &ram
            ),
            0
        ),
        Ok(0x1100)
    );
    assert_eq!(gpu.allocated, LIMIT);
    assert_eq!(
        word(
            &run(
                &mut gpu,
                0x204,
                0,
                &[2, 0, 64, 16, 1, 1, 1, 1, 0, 0, 0, 0],
                &ram
            ),
            0
        ),
        Ok(OOM)
    );
    assert_eq!(word(&run(&mut gpu, 0x102, 0, &[1, 0], &ram), 0), Ok(0x1100));
    assert_eq!(gpu.allocated, 0);
    assert_eq!(
        word(
            &run(
                &mut gpu,
                0x204,
                0,
                &[2, 0, 64, 16, 1, 1, 1, 1, 0, 0, 0, 0],
                &ram
            ),
            0
        ),
        Ok(0x1100)
    );
    assert_eq!(gpu.allocated, 1);
}

#[test]
fn scanout_readback_and_resource_teardown() {
    let (mut gpu, calls) = gpu(None);
    let ram = Ram(RefCell::new(Vec::new()));
    create(&mut gpu, &ram);
    assert_eq!(
        word(&run(&mut gpu, 0x103, 0, &[0, 0, 8, 8, 0, 1], &ram), 0),
        Ok(0x1100)
    );
    assert_eq!(gpu.generation, 1);
    assert!(calls.lock().unwrap().iter().any(|(op, _)| *op == 9));
    assert_eq!(
        word(&run(&mut gpu, 0x104, 0, &[0, 0, 8, 8, 1, 0], &ram), 0),
        Ok(0x1100)
    );
    assert_eq!(gpu.generation, 2);
    assert_eq!(word(&run(&mut gpu, 0x102, 0, &[1, 0], &ram), 0), Ok(0x1100));
    assert!(gpu.scanout.is_none());
    assert!(gpu.frame_rgb().iter().all(|byte| *byte == 0));
}

#[test]
fn queue_and_header_validation() {
    let (mut gpu, calls) = gpu(None);
    let ram = Ram(RefCell::new(Vec::new()));
    for data in [
        vec![],
        vec![0; 23],
        words(&[0x100, 2, 0, 0, 0, 0]),
        request(0x100, 1, &[]),
    ] {
        assert_ne!(word(&gpu.execute(&data, false, &ram), 0), Ok(0x1100));
    }
    assert_eq!(
        word(&gpu.execute(&request(0x100, 0, &[]), true, &ram), 0),
        Ok(INVALID)
    );
    assert!(calls.lock().unwrap().is_empty());
}

#[test]
#[ignore = "requires GPU_VIRGL_WORKER pointing to an executable real renderer"]
fn real_renderer_clear_fence_and_readback() {
    let path = std::env::var("GPU_VIRGL_WORKER").unwrap();
    let mut gpu = Gpu3d::connect(&path).unwrap();
    let ram = Ram(RefCell::new(vec![0; 4096]));
    create(&mut gpu, &ram);
    context(&mut gpu, &ram);
    assert_eq!(word(&run(&mut gpu, 0x202, 7, &[1, 0], &ram), 0), Ok(0x1100));
    assert_eq!(
        word(&run(&mut gpu, 0x106, 0, &[1, 1, 0, 0, 256, 0], &ram), 0),
        Ok(0x1100)
    );
    let commands = [
        5 << 16 | 8 << 8 | 1,
        1,
        1,
        2,
        0,
        0,
        3 << 16 | 5,
        1,
        0,
        1,
        8 << 16 | 7,
        4,
        1.0f32.to_bits(),
        0,
        0,
        1.0f32.to_bits(),
        0,
        0,
        0,
    ];
    let mut data = request(0x207, 7, &[commands.len() as u32 * 4, 0]);
    data.extend_from_slice(&words(&commands));
    data[4..8].copy_from_slice(&1u32.to_le_bytes());
    assert_eq!(word(&gpu.execute(&data, false, &ram), 0), Ok(0x1100));
    let transfer = [13 << 16 | 43, 1, 0, 0, 32, 0, 0, 0, 0, 8, 8, 1, 0, 2];
    let mut encoded_read = request(0x207, 7, &[transfer.len() as u32 * 4, 0]);
    encoded_read.extend_from_slice(&words(&transfer));
    assert_eq!(
        word(&gpu.execute(&encoded_read, false, &ram), 0),
        Ok(0x1100)
    );
    assert_eq!(&ram.0.borrow()[..3], &[0, 0, 255]);
    assert_eq!(
        word(&run(&mut gpu, 0x103, 0, &[0, 0, 8, 8, 0, 1], &ram), 0),
        Ok(0x1100)
    );
    let pixels = gpu.frame_rgb();
    for row in 0..8usize {
        for column in 0..8usize {
            let offset = (row * WIDTH as usize + column) * 3;
            assert_eq!(&pixels[offset..offset + 3], &[255, 0, 0]);
        }
    }
    assert_eq!(word(&run(&mut gpu, 0x201, 7, &[], &ram), 0), Ok(0x1100));
    assert_eq!(word(&run(&mut gpu, 0x102, 0, &[1, 0], &ram), 0), Ok(0x1100));
}

#[test]
#[ignore = "requires GPU_VIRGL_WORKER pointing to an executable real renderer"]
fn real_renderer_sg_upload_then_context_and_download() {
    let path = std::env::var("GPU_VIRGL_WORKER").unwrap();
    let mut gpu = Gpu3d::connect(&path).unwrap();
    let ram = Ram(RefCell::new(vec![0; 4096]));
    for pixel in ram.0.borrow_mut()[..256].chunks_exact_mut(4) {
        pixel.copy_from_slice(&[0, 0, 255, 255]);
    }
    assert_eq!(
        word(&run(&mut gpu, 0x101, 0, &[1, 2, 8, 8], &ram), 0),
        Ok(0x1100)
    );
    assert_eq!(
        word(&run(&mut gpu, 0x106, 0, &[1, 1, 0, 0, 256, 0], &ram), 0),
        Ok(0x1100)
    );
    assert_eq!(
        word(&run(&mut gpu, 0x105, 0, &[0, 0, 8, 8, 0, 0, 1, 0], &ram), 0),
        Ok(0x1100)
    );
    let mut scanout = request(0x103, 0, &[0, 0, 8, 8, 0, 1]);
    scanout[4..8].copy_from_slice(&1u32.to_le_bytes());
    assert_eq!(word(&gpu.execute(&scanout, false, &ram), 0), Ok(0x1100));
    assert_eq!(&gpu.frame_rgb()[..3], &[255, 0, 0]);
    context(&mut gpu, &ram);
    ram.0.borrow_mut()[..256].fill(0);
    assert_eq!(
        word(
            &run(
                &mut gpu,
                0x206,
                0,
                &[0, 0, 0, 8, 8, 1, 0, 0, 1, 0, 32, 0],
                &ram
            ),
            0
        ),
        Ok(0x1100)
    );
    assert_eq!(&ram.0.borrow()[..3], &[0, 0, 255]);
}

#[test]
fn array_layer_transfers_are_not_mip_depth() {
    let (mut gpu, _) = gpu(None);
    let ram = Ram(RefCell::new(vec![0; 4096]));
    assert_eq!(
        word(
            &run(
                &mut gpu,
                0x204,
                0,
                &[1, 7, 2, 8, 8, 8, 1, 4, 1, 0, 0, 0],
                &ram
            ),
            0
        ),
        Ok(0x1100)
    );
    assert_eq!(
        word(&run(&mut gpu, 0x106, 0, &[1, 1, 0, 0, 1024, 0], &ram), 0),
        Ok(0x1100)
    );
    assert_eq!(
        word(
            &run(
                &mut gpu,
                0x205,
                0,
                &[0, 0, 3, 4, 4, 1, 0, 0, 1, 1, 16, 64],
                &ram
            ),
            0
        ),
        Ok(0x1100)
    );
    assert_eq!(
        word(
            &run(
                &mut gpu,
                0x205,
                0,
                &[0, 0, 4, 4, 4, 1, 0, 0, 1, 1, 16, 64],
                &ram
            ),
            0
        ),
        Ok(INVALID)
    );
}

#[test]
fn malformed_packets_cannot_panic_or_exceed_accounting() {
    let (mut gpu, _) = gpu(None);
    let ram = Ram(RefCell::new(vec![0; 4096]));
    let mut state = 0x1234abcd_u32;
    for iteration in 0..4096 {
        let length = iteration % 128;
        let mut data = vec![0; length];
        for byte in &mut data {
            state = state.wrapping_mul(1664525).wrapping_add(1013904223);
            *byte = (state >> 24) as u8;
        }
        if length >= 24 {
            data[..4].copy_from_slice(&(0x100u32 + (iteration % 0x210) as u32).to_le_bytes());
            data[4..24].fill(0);
        }
        let result = gpu.execute(&data, iteration % 2 == 0, &ram);
        assert!(result.len() >= 24);
        assert!(gpu.allocated <= LIMIT && gpu.backing_bytes <= LIMIT);
    }
}
