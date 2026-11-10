use super::*;

struct Ram(Vec<u8>);

impl Memory for Ram {
    fn contains(&self, address: u64, length: usize) -> bool {
        address
            .checked_add(length as u64)
            .is_some_and(|end| end <= self.0.len() as u64)
    }
    fn read(&self, address: u64, data: &mut [u8]) -> bool {
        if !self.contains(address, data.len()) {
            return false;
        }
        data.copy_from_slice(&self.0[address as usize..address as usize + data.len()]);
        true
    }
}

fn request(command: u32, values: &[u32]) -> Vec<u8> {
    let mut data = vec![0; 24];
    put(&mut data, 0, command);
    for value in values {
        data.extend_from_slice(&value.to_le_bytes());
    }
    data
}

fn run(gpu: &mut Gpu, memory: &Ram, command: u32, values: &[u32]) -> u32 {
    word(
        &gpu.execute(&request(command, values), command >= 0x300, memory),
        0,
    )
    .unwrap()
}

fn create(gpu: &mut Gpu, memory: &Ram, id: u32, width: u32, height: u32) {
    assert_eq!(run(gpu, memory, 0x101, &[id, 2, width, height]), 0x1100);
}

#[test]
fn display_and_fence() {
    let mut gpu = Gpu::default();
    let mut data = request(0x100, &[]);
    put(&mut data, 4, 1);
    data[8..16].copy_from_slice(&u64::MAX.to_le_bytes());
    let response = gpu.execute(&data, false, &Ram(vec![]));
    assert_eq!(response.len(), 408);
    assert_eq!(word(&response, 0), Ok(0x1101));
    assert_eq!(&response[4..20], &data[4..20]);
    assert_eq!(word(&response, 32), Ok(WIDTH));
    assert_eq!(word(&response, 36), Ok(HEIGHT));
    assert!(response[48..].iter().all(|byte| *byte == 0));
}

#[test]
fn scatter_transfer_flush_and_reset() {
    let memory = Ram(vec![9, 8, 7, 0, 6, 5, 4, 0, 3, 2, 1, 0, 0, 1, 2, 0]);
    let mut gpu = Gpu::default();
    create(&mut gpu, &memory, 1, 2, 2);
    assert_eq!(
        run(&mut gpu, &memory, 0x106, &[1, 2, 0, 0, 5, 0, 5, 0, 11, 0]),
        0x1100
    );
    assert_eq!(
        run(&mut gpu, &memory, 0x105, &[0, 0, 2, 2, 0, 0, 1, 0]),
        0x1100
    );
    assert_eq!(run(&mut gpu, &memory, 0x103, &[0, 0, 2, 2, 0, 1]), 0x1100);
    assert_eq!(run(&mut gpu, &memory, 0x104, &[0, 0, 2, 2, 1, 0]), 0x1100);
    let frame = gpu.ppm();
    let header = format!("P6\n{} {}\n255\n", WIDTH, HEIGHT).len();
    assert_eq!(&frame[header..header + 6], &[7, 8, 9, 4, 5, 6]);
    assert_eq!(
        &frame[header + WIDTH as usize * 3..][..6],
        &[1, 2, 3, 2, 1, 0]
    );
    assert_eq!(run(&mut gpu, &memory, 0x107, &[1, 0]), 0x1100);
    assert_eq!(
        run(&mut gpu, &memory, 0x105, &[0, 0, 1, 1, 0, 0, 1, 0]),
        INVALID
    );
    gpu.reset();
    assert_eq!(gpu.allocated, 0);
    assert!(gpu.resources.is_empty());
    assert!(gpu.ppm()[header..].iter().all(|byte| *byte == 0));
}

#[test]
fn partial_transfer_uses_offset_stride_and_destination() {
    let memory = Ram((0..64).collect());
    let mut gpu = Gpu::default();
    create(&mut gpu, &memory, 1, 4, 4);
    assert_eq!(run(&mut gpu, &memory, 0x106, &[1, 1, 0, 0, 64, 0]), 0x1100);
    assert_eq!(
        run(&mut gpu, &memory, 0x105, &[1, 1, 2, 2, 20, 0, 1, 0]),
        0x1100
    );
    let resource = &gpu.resources[&1];
    assert_eq!(&resource.pixels[20..28], &memory.0[20..28]);
    assert_eq!(&resource.pixels[36..44], &memory.0[36..44]);
    assert!(resource.pixels[..20].iter().all(|byte| *byte == 0));
}

#[test]
fn rejects_bad_resources_and_bounds_without_mutation() {
    let memory = Ram(vec![0; 16]);
    let mut gpu = Gpu::default();
    assert_eq!(run(&mut gpu, &memory, 0x101, &[0, 2, 1, 1]), RESOURCE);
    assert_eq!(run(&mut gpu, &memory, 0x101, &[1, 900, 1, 1]), INVALID);
    assert_eq!(run(&mut gpu, &memory, 0x101, &[1, 2, u32::MAX, 1]), INVALID);
    create(&mut gpu, &memory, 1, 2, 2);
    assert_eq!(run(&mut gpu, &memory, 0x101, &[1, 2, 2, 2]), RESOURCE);
    assert_eq!(run(&mut gpu, &memory, 0x102, &[2, 0]), RESOURCE);
    assert_eq!(
        run(
            &mut gpu,
            &memory,
            0x106,
            &[1, 1, 0xfffffff0, u32::MAX, 32, 0]
        ),
        INVALID
    );
    assert!(gpu.resources[&1].backing.is_empty());
    assert_eq!(run(&mut gpu, &memory, 0x106, &[1, 1, 0, 0, 16, 0]), 0x1100);
    assert_eq!(
        run(
            &mut gpu,
            &memory,
            0x105,
            &[0, 0, 2, 2, u32::MAX, u32::MAX, 1, 0]
        ),
        INVALID
    );
    assert_eq!(
        run(&mut gpu, &memory, 0x105, &[u32::MAX, 0, 2, 2, 0, 0, 1, 0]),
        INVALID
    );
    assert_eq!(run(&mut gpu, &memory, 0x103, &[0, 0, 2, 2, 1, 1]), SCANOUT);
    assert_eq!(run(&mut gpu, &memory, 0x106, &[1, 1, 0, 0, 16, 0]), INVALID);
    assert!(gpu.resources[&1].pixels.iter().all(|byte| *byte == 0));
}

#[test]
fn truncation_unknown_commands_and_queue_selection() {
    let memory = Ram(vec![]);
    let mut gpu = Gpu::default();
    for (command, minimum) in [
        (0x100, 24),
        (0x101, 40),
        (0x102, 32),
        (0x103, 48),
        (0x104, 48),
        (0x105, 56),
        (0x106, 32),
        (0x107, 32),
        (0x300, 56),
        (0x301, 56),
    ] {
        let full = request(command, &[0; 10]);
        for length in 0..minimum {
            assert_eq!(
                word(&gpu.execute(&full[..length], command >= 0x300, &memory), 0),
                Ok(INVALID)
            );
        }
    }
    for command in [0x108, 0x109, 0x10a, 0x10c, 0x200, u32::MAX] {
        assert_eq!(
            word(&gpu.execute(&request(command, &[]), false, &memory), 0),
            Ok(UNSPEC)
        );
    }
    assert_eq!(
        word(&gpu.execute(&request(0x100, &[]), true, &memory), 0),
        Ok(INVALID)
    );
    let oversized = vec![0; MAX_REQUEST + 1];
    assert_eq!(
        word(&gpu.execute(&oversized, false, &memory), 0),
        Ok(INVALID)
    );
}

#[test]
fn all_formats_convert_to_rgb() {
    let memory = Ram(vec![10, 20, 30, 40]);
    let expected = [
        (1, [30, 20, 10, 40]),
        (2, [30, 20, 10, 255]),
        (3, [20, 30, 40, 10]),
        (4, [20, 30, 40, 255]),
        (67, [10, 20, 30, 40]),
        (68, [40, 30, 20, 255]),
        (121, [40, 30, 20, 10]),
        (134, [10, 20, 30, 255]),
    ];
    for (format, rgba) in expected {
        let mut gpu = Gpu::default();
        assert_eq!(run(&mut gpu, &memory, 0x101, &[1, format, 1, 1]), 0x1100);
        assert_eq!(run(&mut gpu, &memory, 0x106, &[1, 1, 0, 0, 4, 0]), 0x1100);
        assert_eq!(
            run(&mut gpu, &memory, 0x105, &[0, 0, 1, 1, 0, 0, 1, 0]),
            0x1100
        );
        assert_eq!(gpu.resources[&1].rgb(0), rgba);
    }
}

#[test]
fn cursor_update_move_hide_and_clipping() {
    let mut memory = Ram(vec![0; 64 * 64 * 4]);
    for pixel in memory.0.chunks_exact_mut(4) {
        pixel.copy_from_slice(&[0, 0, 255, 255]);
    }
    let mut gpu = Gpu::default();
    create(&mut gpu, &memory, 1, 64, 64);
    assert_eq!(
        run(&mut gpu, &memory, 0x106, &[1, 1, 0, 0, 16384, 0]),
        0x1100
    );
    assert_eq!(
        run(&mut gpu, &memory, 0x105, &[0, 0, 64, 64, 0, 0, 1, 0]),
        0x1100
    );
    assert_eq!(
        run(&mut gpu, &memory, 0x300, &[0, 0, 0, 0, 1, 1, 1, 0]),
        0x1100
    );
    let header = format!("P6\n{} {}\n255\n", WIDTH, HEIGHT).len();
    assert_eq!(&gpu.ppm()[header..header + 3], &[255, 0, 0]);
    assert_eq!(
        run(
            &mut gpu,
            &memory,
            0x301,
            &[0, u32::MAX, u32::MAX, 0, 0, 0, 0, 0]
        ),
        0x1100
    );
    assert!(gpu.ppm()[header..].iter().all(|byte| *byte == 0));
    assert_eq!(
        run(&mut gpu, &memory, 0x300, &[0, 0, 0, 0, 1, 64, 0, 0]),
        INVALID
    );
    assert_eq!(
        run(&mut gpu, &memory, 0x300, &[0, 0, 0, 0, 0, 0, 0, 0]),
        0x1100
    );
    assert!(gpu.cursor.is_none());
}

#[test]
fn aggregate_resource_budget_and_unref() {
    let memory = Ram(vec![]);
    let mut gpu = Gpu::default();
    create(&mut gpu, &memory, 1, 4096, 4096);
    create(&mut gpu, &memory, 2, 4096, 4096);
    assert_eq!(run(&mut gpu, &memory, 0x101, &[3, 2, 1, 1]), OOM);
    assert_eq!(run(&mut gpu, &memory, 0x102, &[1, 0]), 0x1100);
    create(&mut gpu, &memory, 3, 1, 1);
}

#[test]
fn deterministic_malformed_packets_never_panic() {
    let memory = Ram(vec![0; 4096]);
    let mut gpu = Gpu::default();
    let mut seed = 0x7d93_5a12u32;
    let commands = [
        0x100u32, 0x101, 0x102, 0x103, 0x104, 0x105, 0x106, 0x107, 0x300, 0x301,
    ];
    for iteration in 0..10000 {
        let length = iteration % 128;
        let mut data = vec![0; length];
        for byte in &mut data {
            seed ^= seed << 13;
            seed ^= seed >> 17;
            seed ^= seed << 5;
            *byte = seed as u8;
        }
        let command = commands[iteration % commands.len()];
        if length >= 24 {
            put(&mut data, 0, command);
            put(&mut data, 4, (iteration % 2) as u32);
            put(&mut data, 16, 0);
        }
        let response = gpu.execute(&data, command >= 0x300, &memory);
        assert!(response.len() >= 24 && response.len() <= 408);
        if iteration % 100 == 0 {
            gpu.reset();
        }
    }
}
