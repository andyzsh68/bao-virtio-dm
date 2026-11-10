use super::core::{Gpu, Memory, HEIGHT, MAX_REQUEST, WIDTH};
use super::kms::Kms;
use crate::device::{
    clone_queue, SignalUsedQueue, SingleFdSignalQueue, Subscriber, VirtioDeviceCommon,
    VirtioDeviceT,
};
use api::defines::{BAO_IOEVENTFD_FLAG_DATAMATCH, BAO_IOEVENTFD_FLAG_DEASSIGN};
use api::device_model::BaoDeviceModel;
use api::error::{Error, Result};
use api::types::DeviceConfig;
use event_manager::{
    EventManager, EventOps, Events, MutEventSubscriber, RemoteEndpoint, SubscriberId,
};
use std::borrow::{Borrow, BorrowMut};
use std::os::fd::AsRawFd;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use vhost_user_frontend::GuestMemoryMmap;
use virtio_device::{VirtioConfig, VirtioDeviceActions, VirtioDeviceType, VirtioMmioDevice};
use virtio_queue::{Queue, QueueOwnedT, QueueT};
use vm_device::bus::MmioAddress;
use vm_device::device_manager::{IoManager, MmioManager};
use vm_device::MutDeviceMmio;
use vm_memory::{Bytes, GuestAddress, GuestMemory};
use vmm_sys_util::epoll::EventSet;
use vmm_sys_util::eventfd::EventFd;

impl Memory for GuestMemoryMmap {
    fn contains(&self, address: u64, length: usize) -> bool {
        address.checked_add(length as u64).is_some()
            && self.check_range(GuestAddress(address), length)
    }
    fn read(&self, address: u64, data: &mut [u8]) -> bool {
        self.read_slice(data, GuestAddress(address)).is_ok()
    }
}

struct Handler {
    queues: Vec<Queue>,
    memory: GuestMemoryMmap,
    events: Vec<EventFd>,
    notify: SingleFdSignalQueue,
    gpu: Gpu,
    output: String,
    kms: Arc<Mutex<Option<Kms>>>,
}

impl Handler {
    fn present(&self) -> std::io::Result<()> {
        let frame = self.gpu.frame_rgb();
        if let Some(kms) = self.kms.lock().unwrap().as_mut() {
            kms.present(&frame, self.gpu.generation)?;
        }
        let temporary = format!("{}.tmp", self.output);
        let mut ppm = format!("P6\n{} {}\n255\n", WIDTH, HEIGHT).into_bytes();
        ppm.extend_from_slice(&frame);
        std::fs::write(&temporary, ppm)?;
        std::fs::rename(temporary, &self.output)?;
        println!("GPU_FRAME generation={}", self.gpu.generation);
        Ok(())
    }

    fn drain(&mut self, index: usize) -> std::result::Result<(), String> {
        loop {
            self.queues[index]
                .disable_notification(&self.memory)
                .map_err(|error| error.to_string())?;
            while let Some(chain) = self.queues[index]
                .iter(&self.memory)
                .map_err(|error| error.to_string())?
                .next()
            {
                let mut request = Vec::new();
                let mut outputs = Vec::new();
                let mut invalid = false;
                let mut writable = false;
                let head = chain.head_index();
                let mut descriptors = 0;
                let mut complete = false;
                for descriptor in chain {
                    descriptors += 1;
                    complete = !descriptor.has_next();
                    if descriptors > 256
                        || !self
                            .memory
                            .contains(descriptor.addr().0, descriptor.len() as usize)
                    {
                        invalid = true;
                        break;
                    }
                    if descriptor.is_write_only() {
                        writable = true;
                        outputs.push((descriptor.addr(), descriptor.len() as usize));
                    } else {
                        let start = request.len();
                        if writable || descriptor.len() as usize > MAX_REQUEST - start {
                            invalid = true;
                            break;
                        }
                        request.resize(start + descriptor.len() as usize, 0);
                        if self
                            .memory
                            .read_slice(&mut request[start..], descriptor.addr())
                            .is_err()
                        {
                            invalid = true;
                            break;
                        }
                    }
                }
                let output_size: usize = outputs.iter().map(|output| output.1).sum();
                let mut used = 0;
                if complete && !invalid && (index == 1 || output_size >= 24) {
                    let generation = self.gpu.generation;
                    let mut response = self.gpu.execute(&request, index == 1, &self.memory);
                    if index == 0 && response.len() > output_size {
                        response.truncate(24);
                        response[..4].copy_from_slice(&0x1205u32.to_le_bytes());
                    }
                    let command = request
                        .get(..4)
                        .map(|bytes| u32::from_le_bytes(bytes.try_into().unwrap()))
                        .unwrap_or(0);
                    if generation != self.gpu.generation {
                        if let Err(error) = self.present() {
                            eprintln!("GPU_PRESENT_ERROR {error}");
                            response[..4].copy_from_slice(&0x1200u32.to_le_bytes());
                        }
                    }
                    let status = u32::from_le_bytes(response[..4].try_into().unwrap());
                    println!(
                        "GPU_CMD queue={} type={:#x} response={:#x}",
                        index, command, status
                    );
                    if index == 0 {
                        for (address, length) in outputs {
                            let count = length.min(response.len() - used);
                            self.memory
                                .write_slice(&response[used..used + count], address)
                                .map_err(|error| error.to_string())?;
                            used += count;
                            if used == response.len() {
                                break;
                            }
                        }
                    }
                }
                self.queues[index]
                    .add_used(&self.memory, head, used as u32)
                    .map_err(|error| error.to_string())?;
                if self.queues[index]
                    .needs_notification(&self.memory)
                    .map_err(|error| error.to_string())?
                {
                    self.notify.signal_used_queue(index as u16);
                }
            }
            if !self.queues[index]
                .enable_notification(&self.memory)
                .map_err(|error| error.to_string())?
            {
                break;
            }
        }
        Ok(())
    }
}

impl MutEventSubscriber for Handler {
    fn init(&mut self, ops: &mut EventOps) {
        for (index, event) in self.events.iter().enumerate() {
            ops.add(Events::with_data(event, index as u32, EventSet::IN))
                .unwrap();
        }
    }
    fn process(&mut self, events: Events, ops: &mut EventOps) {
        let index = events.data() as usize;
        if index >= self.events.len() || events.event_set() != EventSet::IN {
            return;
        }
        if self.events[index].read().is_ok() {
            if let Err(error) = self.drain(index) {
                eprintln!("GPU_QUEUE_ERROR {}", error);
                for event in &self.events {
                    let _ = ops.remove(Events::empty(event));
                }
            }
        }
    }
}

pub struct VirtioGpu {
    common: VirtioDeviceCommon,
    endpoint: RemoteEndpoint<Subscriber>,
    memory: Option<GuestMemoryMmap>,
    handler: Option<(SubscriberId, Arc<Mutex<Handler>>)>,
    output: String,
    kms: Arc<Mutex<Option<Kms>>>,
}

impl VirtioDeviceT for VirtioGpu {
    fn new(
        config: &DeviceConfig,
        device_manager: Arc<Mutex<IoManager>>,
        event_manager: Option<Arc<Mutex<EventManager<Subscriber>>>>,
        device_model: Arc<Mutex<BaoDeviceModel>>,
    ) -> Result<Arc<Mutex<Self>>> {
        let (features, queues) = Self::initialize(config)?;
        let common = VirtioDeviceCommon::new(
            config,
            device_model,
            VirtioConfig::new(features, queues, Self::config_space(config)?),
        )?;
        let display = std::env::var("BAO_GPU_KMS").unwrap_or_else(|_| "/dev/dri/card0".into());
        let kms = if display == "off" {
            eprintln!("GPU_KMS_DISABLED explicit software-only debug mode");
            None
        } else {
            Some(Kms::open(&display).map_err(Error::EventFdWriteFailed)?)
        };
        let gpu = Arc::new(Mutex::new(Self {
            common,
            endpoint: event_manager.unwrap().lock().unwrap().remote_endpoint(),
            memory: None,
            handler: None,
            kms: Arc::new(Mutex::new(kms)),
            output: config
                .file_path
                .clone()
                .unwrap_or_else(|| "/tmp/virtio-gpu.ppm".into()),
        }));
        let range = gpu.lock().unwrap().common.mmio.range;
        device_manager
            .lock()
            .unwrap()
            .register_mmio(range, gpu.clone())
            .map_err(|_| Error::MmioConfig)?;
        Ok(gpu)
    }
    fn device_features(_config: &DeviceConfig) -> Result<u64> {
        Ok(0)
    }
    fn config_space(_config: &DeviceConfig) -> Result<Vec<u8>> {
        let mut config = vec![0; 16];
        config[8..12].copy_from_slice(&1u32.to_le_bytes());
        Ok(config)
    }
}

impl Borrow<VirtioConfig<Queue>> for VirtioGpu {
    fn borrow(&self) -> &VirtioConfig<Queue> {
        &self.common.config
    }
}
impl BorrowMut<VirtioConfig<Queue>> for VirtioGpu {
    fn borrow_mut(&mut self) -> &mut VirtioConfig<Queue> {
        &mut self.common.config
    }
}
impl VirtioDeviceType for VirtioGpu {
    fn device_type(&self) -> u32 {
        16
    }
}
impl VirtioDeviceActions for VirtioGpu {
    type E = Error;
    fn activate(&mut self) -> Result<()> {
        if self.memory.is_none() {
            self.memory = Some(self.common.mem());
        }
        let memory = self.memory.as_ref().unwrap().clone();
        if !self
            .common
            .config
            .queues
            .iter()
            .all(|queue| queue.is_valid(&memory))
        {
            return Err(Error::BaoBusInvalidState);
        }
        let events = self.common.prepare_activate()?;
        let handler = Arc::new(Mutex::new(Handler {
            queues: self.common.config.queues.iter().map(clone_queue).collect(),
            memory,
            events,
            notify: SingleFdSignalQueue {
                irqfd: self
                    .common
                    .irqfd
                    .try_clone()
                    .map_err(Error::EventFdWriteFailed)?,
                interrupt_status: self.common.config.interrupt_status.clone(),
            },
            gpu: Gpu::default(),
            output: self.output.clone(),
            kms: self.kms.clone(),
        }));
        let subscriber = handler.clone();
        let id = self
            .endpoint
            .call_blocking(move |manager| -> event_manager::Result<SubscriberId> {
                Ok(manager.add_subscriber(subscriber))
            })
            .map_err(|_| Error::HandleIoEventFailed)?;
        self.handler = Some((id, handler));
        self.common.config.device_activated = true;
        println!(
            "GPU_ACTIVATED features={:#x}",
            self.common.config.driver_features
        );
        Ok(())
    }
    fn reset(&mut self) -> Result<()> {
        if let Some((id, handler)) = self.handler.take() {
            self.endpoint
                .call_blocking(move |manager| manager.remove_subscriber(id))
                .map_err(|_| Error::HandleIoEventFailed)?;
            let mut handler = handler.lock().unwrap();
            for (index, event) in handler.events.iter().enumerate() {
                self.common
                    .device_model
                    .lock()
                    .unwrap()
                    .register_ioeventfd(
                        event.as_raw_fd() as u32,
                        BAO_IOEVENTFD_FLAG_DATAMATCH | BAO_IOEVENTFD_FLAG_DEASSIGN,
                        self.common.mmio.range.base().0 + 0x50,
                        index as u64,
                    )?;
            }
            handler.gpu.reset();
            handler.present().map_err(Error::EventFdWriteFailed)?;
        }
        let features = self.common.config.device_features;
        self.common
            .config
            .interrupt_status
            .store(0, Ordering::SeqCst);
        self.common.config = VirtioConfig::new(
            features,
            vec![Queue::new(256).unwrap(), Queue::new(256).unwrap()],
            vec![0, 0, 0, 0, 0, 0, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0],
        );
        println!("GPU_RESET");
        Ok(())
    }
    fn write_config(&mut self, _offset: usize, _data: &[u8]) {}
}
impl VirtioMmioDevice for VirtioGpu {
    fn queue_notify(&mut self, _value: u32) {}
}
impl MutDeviceMmio for VirtioGpu {
    fn mmio_read(&mut self, _base: MmioAddress, offset: u64, data: &mut [u8]) {
        if matches!(offset, 0xb0 | 0xb4) && data.len() == 4 {
            data.copy_from_slice(&u32::MAX.to_le_bytes());
        } else {
            self.read(offset, data);
        }
    }
    fn mmio_write(&mut self, _base: MmioAddress, offset: u64, data: &[u8]) {
        self.write(offset, data);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicU8;
    use vmm_sys_util::eventfd::EFD_NONBLOCK;

    fn handler() -> Handler {
        let memory = GuestMemoryMmap::from_ranges(&[(GuestAddress(0), 0x10000)]).unwrap();
        let mut queue = Queue::new(256).unwrap();
        queue.set_size(256);
        queue.set_ready(true);
        queue
            .try_set_desc_table_address(GuestAddress(0x1000))
            .unwrap();
        queue
            .try_set_avail_ring_address(GuestAddress(0x2000))
            .unwrap();
        queue
            .try_set_used_ring_address(GuestAddress(0x3000))
            .unwrap();
        memory
            .write_slice(&1u16.to_le_bytes(), GuestAddress(0x2002))
            .unwrap();
        memory
            .write_slice(&0x100u32.to_le_bytes(), GuestAddress(0x4000))
            .unwrap();
        Handler {
            queues: vec![queue],
            memory,
            events: vec![EventFd::new(EFD_NONBLOCK).unwrap()],
            notify: SingleFdSignalQueue {
                irqfd: EventFd::new(EFD_NONBLOCK).unwrap(),
                interrupt_status: Arc::new(AtomicU8::new(0)),
            },
            gpu: Gpu::default(),
            output: "/tmp/unused-gpu-test.ppm".into(),
            kms: Arc::new(Mutex::new(None)),
        }
    }

    fn descriptor(handler: &Handler, index: u16, address: u64, length: u32, flags: u16, next: u16) {
        let mut data = Vec::new();
        data.extend_from_slice(&address.to_le_bytes());
        data.extend_from_slice(&length.to_le_bytes());
        data.extend_from_slice(&flags.to_le_bytes());
        data.extend_from_slice(&next.to_le_bytes());
        handler
            .memory
            .write_slice(&data, GuestAddress(0x1000 + index as u64 * 16))
            .unwrap();
    }

    fn used(handler: &Handler) -> (u16, u32) {
        let mut count = [0; 2];
        let mut length = [0; 4];
        handler
            .memory
            .read_slice(&mut count, GuestAddress(0x3002))
            .unwrap();
        handler
            .memory
            .read_slice(&mut length, GuestAddress(0x3008))
            .unwrap();
        (u16::from_le_bytes(count), u32::from_le_bytes(length))
    }

    #[test]
    fn presentation_failure_completes_request_with_error_and_fence() {
        let mut handler = handler();
        handler.output = "/proc/bao-gpu-nonexistent/frame.ppm".into();
        let mut request = vec![0; 48];
        request[..4].copy_from_slice(&0x103u32.to_le_bytes());
        request[4..8].copy_from_slice(&1u32.to_le_bytes());
        request[8..16].copy_from_slice(&0x123456789abcdef0u64.to_le_bytes());
        handler
            .memory
            .write_slice(&request, GuestAddress(0x4000))
            .unwrap();
        descriptor(&handler, 0, 0x4000, 48, 1, 1);
        descriptor(&handler, 1, 0x5000, 24, 2, 0);
        handler.drain(0).unwrap();
        assert_eq!(used(&handler), (1, 24));
        let mut response = [0; 24];
        handler
            .memory
            .read_slice(&mut response, GuestAddress(0x5000))
            .unwrap();
        assert_eq!(
            u32::from_le_bytes(response[..4].try_into().unwrap()),
            0x1200
        );
        assert_eq!(&response[4..16], &request[4..16]);
        assert_eq!(handler.notify.irqfd.read().unwrap(), 1);
    }

    #[test]
    fn display_chain_response_and_irq() {
        let mut handler = handler();
        descriptor(&handler, 0, 0x4000, 24, 1, 1);
        descriptor(&handler, 1, 0x5000, 408, 2, 0);
        handler.drain(0).unwrap();
        assert_eq!(used(&handler), (1, 408));
        assert_eq!(handler.notify.irqfd.read().unwrap(), 1);
        let mut response = [0; 4];
        handler
            .memory
            .read_slice(&mut response, GuestAddress(0x5000))
            .unwrap();
        assert_eq!(u32::from_le_bytes(response), 0x1101);
    }

    #[test]
    fn split_command_and_response_descriptors() {
        let mut handler = handler();
        descriptor(&handler, 0, 0x4000, 12, 1, 1);
        descriptor(&handler, 1, 0x4100, 12, 1, 2);
        descriptor(&handler, 2, 0x5000, 12, 3, 3);
        descriptor(&handler, 3, 0x5100, 396, 2, 0);
        handler.drain(0).unwrap();
        assert_eq!(used(&handler), (1, 408));
        let mut width = [0; 4];
        handler
            .memory
            .read_slice(&mut width, GuestAddress(0x5100 + 20))
            .unwrap();
        assert_eq!(u32::from_le_bytes(width), 640);
    }

    #[test]
    fn malformed_chain_cannot_mutate_gpu() {
        for scenario in 0..5 {
            let mut handler = handler();
            match scenario {
                0 => {
                    descriptor(&handler, 0, 0x20000, 24, 1, 1);
                    descriptor(&handler, 1, 0x5000, 408, 2, 0);
                }
                1 => {
                    descriptor(&handler, 0, 0x4000, 24, 1, 1);
                    descriptor(&handler, 1, 0x5000, 8, 2, 0);
                }
                2 => {
                    descriptor(&handler, 0, 0x5000, 408, 3, 1);
                    descriptor(&handler, 1, 0x4000, 24, 0, 0);
                }
                3 => {
                    descriptor(&handler, 0, 0x4000, 24, 1, 1);
                    descriptor(&handler, 1, 0x5000, u32::MAX, 2, 0);
                }
                _ => {
                    descriptor(&handler, 0, 0x4000, 24, 1, 1);
                    descriptor(&handler, 1, 0x5000, 408, 3, 300);
                }
            }
            handler.drain(0).unwrap();
            assert_eq!(used(&handler), (1, 0));
            assert_eq!(handler.gpu.generation, 0);
        }
    }
}
