use super::core::{HEIGHT, WIDTH};
use super::kms::Kms;
use std::fs::{self, DirBuilder};
use std::io::{Error, ErrorKind, Read, Result, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc};
use std::thread;
use std::time::{Duration, Instant};

const MAGIC: &[u8; 8] = b"BAOGPU1\0";
const FRAME_BYTES: usize = WIDTH as usize * HEIGHT as usize * 3;
const IO_TIMEOUT: Duration = Duration::from_secs(2);
const SOCKETS: [&str; 2] = ["local.sock", "backend.sock"];
static STOP: AtomicBool = AtomicBool::new(false);
static REDRAW: AtomicBool = AtomicBool::new(false);

fn invalid(message: &str) -> Error {
    Error::new(ErrorKind::InvalidData, message)
}

fn header(sequence: u64) -> [u8; 24] {
    let mut bytes = [0; 24];
    bytes[..8].copy_from_slice(MAGIC);
    bytes[8..16].copy_from_slice(&sequence.to_le_bytes());
    bytes[16..20].copy_from_slice(&WIDTH.to_le_bytes());
    bytes[20..24].copy_from_slice(&HEIGHT.to_le_bytes());
    bytes
}

fn decode_header(bytes: &[u8; 24], previous: u64) -> Result<u64> {
    let sequence = u64::from_le_bytes(bytes[8..16].try_into().unwrap());
    if &bytes[..8] != MAGIC
        || bytes[16..20] != WIDTH.to_le_bytes()
        || bytes[20..24] != HEIGHT.to_le_bytes()
        || sequence <= previous
    {
        return Err(invalid("Invalid display header, dimensions or sequence"));
    }
    Ok(sequence)
}

pub struct Client {
    path: PathBuf,
    stream: Option<UnixStream>,
    sequence: u64,
}

impl Client {
    pub fn connect(path: &Path) -> Result<Self> {
        let mut client = Self {
            path: path.into(),
            stream: None,
            sequence: 0,
        };
        client.reconnect()?;
        Ok(client)
    }

    fn reconnect(&mut self) -> Result<()> {
        let stream = UnixStream::connect(&self.path)?;
        stream.set_read_timeout(Some(IO_TIMEOUT))?;
        stream.set_write_timeout(Some(IO_TIMEOUT))?;
        self.stream = Some(stream);
        Ok(())
    }

    pub fn present(&mut self, frame: &[u8]) -> Result<()> {
        if frame.len() != FRAME_BYTES {
            return Err(invalid("Invalid display surface length"));
        }
        self.sequence = self
            .sequence
            .checked_add(1)
            .ok_or_else(|| invalid("Display sequence overflow"))?;
        if self.stream.is_none() {
            self.reconnect()?;
        }
        let mut result = self.submit(frame);
        if result.as_ref().is_err_and(|error| {
            matches!(
                error.kind(),
                ErrorKind::BrokenPipe
                    | ErrorKind::ConnectionReset
                    | ErrorKind::UnexpectedEof
                    | ErrorKind::NotConnected
            )
        }) {
            self.stream = None;
            result = self.reconnect().and_then(|_| self.submit(frame));
        }
        if result.is_err() {
            self.stream = None;
        }
        result
    }

    fn submit(&mut self, frame: &[u8]) -> Result<()> {
        let stream = self.stream.as_mut().unwrap();
        stream.write_all(&header(self.sequence))?;
        stream.write_all(frame)?;
        let mut acknowledgement = [0; 16];
        stream.read_exact(&mut acknowledgement)?;
        if &acknowledgement[..8] != MAGIC || acknowledgement[8..] != self.sequence.to_le_bytes() {
            return Err(invalid("Invalid display acknowledgement"));
        }
        Ok(())
    }
}

pub enum Presenter {
    Direct(Kms),
    Shared(Client),
}

impl Presenter {
    pub fn present(&mut self, frame: &[u8], generation: u64) -> Result<()> {
        match self {
            Self::Direct(kms) => kms.present(frame, generation),
            Self::Shared(client) => {
                client.present(frame)?;
                println!("GPU_SHARED_FRAME generation={generation}");
                Ok(())
            }
        }
    }
}

struct Surfaces {
    frames: [Vec<u8>; 2],
}

impl Surfaces {
    fn new() -> Self {
        Self {
            frames: std::array::from_fn(|_| vec![0; FRAME_BYTES]),
        }
    }

    fn update(&mut self, slot: usize, frame: Vec<u8>) -> Result<()> {
        if slot >= self.frames.len() || frame.len() != FRAME_BYTES {
            return Err(invalid("Invalid compositor slot or frame"));
        }
        self.frames[slot] = frame;
        Ok(())
    }

    fn clear(&mut self, slot: usize) {
        self.frames[slot].fill(0);
    }

    fn compose(&self) -> Vec<u8> {
        let stride = WIDTH as usize * 3;
        let mut composed = vec![0; FRAME_BYTES * 2];
        for row in 0..HEIGHT as usize {
            for slot in 0..2 {
                composed[(row * 2 + slot) * stride..][..stride]
                    .copy_from_slice(&self.frames[slot][row * stride..][..stride]);
            }
        }
        composed
    }
}

enum Event {
    Frame {
        slot: usize,
        sequence: u64,
        frame: Vec<u8>,
        done: mpsc::SyncSender<bool>,
    },
    Closed(usize),
}

fn same_user(stream: &UnixStream) -> Result<()> {
    let mut credentials: libc::ucred = unsafe { std::mem::zeroed() };
    let mut length = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    let status = unsafe {
        libc::getsockopt(
            stream.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            (&mut credentials as *mut libc::ucred).cast(),
            &mut length,
        )
    };
    if status < 0 {
        return Err(Error::last_os_error());
    }
    if credentials.uid != unsafe { libc::geteuid() } {
        return Err(Error::new(
            ErrorKind::PermissionDenied,
            "Display peer UID mismatch",
        ));
    }
    Ok(())
}

fn read_bounded(
    stream: &mut UnixStream,
    bytes: &mut [u8],
    stop: &AtomicBool,
    idle: bool,
) -> Result<()> {
    let mut received = 0;
    let mut deadline = if idle {
        None
    } else {
        Some(Instant::now() + IO_TIMEOUT)
    };
    while received < bytes.len() {
        if stop.load(Ordering::Relaxed) {
            return Err(Error::new(
                ErrorKind::Interrupted,
                "Display service stopping",
            ));
        }
        if deadline.is_some_and(|limit| Instant::now() >= limit) {
            return Err(Error::new(ErrorKind::TimedOut, "Incomplete display packet"));
        }
        match stream.read(&mut bytes[received..]) {
            Ok(0) => {
                return Err(Error::new(
                    ErrorKind::UnexpectedEof,
                    "Display client disconnected",
                ))
            }
            Ok(count) => {
                received += count;
                deadline.get_or_insert_with(|| Instant::now() + IO_TIMEOUT);
            }
            Err(error)
                if matches!(
                    error.kind(),
                    ErrorKind::WouldBlock | ErrorKind::TimedOut | ErrorKind::Interrupted
                ) => {}
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

fn connection(
    mut stream: UnixStream,
    slot: usize,
    events: &mpsc::SyncSender<Event>,
    stop: &AtomicBool,
) -> Result<()> {
    same_user(&stream)?;
    stream.set_read_timeout(Some(Duration::from_millis(100)))?;
    stream.set_write_timeout(Some(IO_TIMEOUT))?;
    let mut previous = 0;
    loop {
        let mut bytes = [0; 24];
        read_bounded(&mut stream, &mut bytes, stop, true)?;
        let sequence = decode_header(&bytes, previous)?;
        let mut frame = vec![0; FRAME_BYTES];
        read_bounded(&mut stream, &mut frame, stop, false)?;
        let (done, completion) = mpsc::sync_channel(1);
        events
            .send(Event::Frame {
                slot,
                sequence,
                frame,
                done,
            })
            .map_err(|_| Error::new(ErrorKind::BrokenPipe, "Compositor stopped"))?;
        if !completion.recv_timeout(IO_TIMEOUT).unwrap_or(false) {
            return Err(Error::new(
                ErrorKind::BrokenPipe,
                "KMS submission not acknowledged",
            ));
        }
        stream.write_all(&bytes[..16])?;
        previous = sequence;
    }
}

fn listen(
    listener: UnixListener,
    slot: usize,
    events: mpsc::SyncSender<Event>,
    stop: Arc<AtomicBool>,
) {
    while !stop.load(Ordering::Relaxed) {
        match listener.accept() {
            Ok((stream, _)) => {
                println!("GPU_DISPLAY_CONNECTED slot={slot}");
                if let Err(error) = connection(stream, slot, &events, &stop) {
                    println!("GPU_DISPLAY_DISCONNECTED slot={slot} reason={error}");
                }
                if events.send(Event::Closed(slot)).is_err() {
                    break;
                }
            }
            Err(error) if error.kind() == ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(20))
            }
            Err(error) => {
                eprintln!("GPU_DISPLAY_LISTENER_ERROR slot={slot} error={error}");
                break;
            }
        }
    }
}

struct SocketPaths(Vec<PathBuf>);

impl Drop for SocketPaths {
    fn drop(&mut self) {
        for path in &self.0 {
            let _ = fs::remove_file(path);
        }
    }
}

fn listeners(directory: &Path) -> Result<(Vec<UnixListener>, SocketPaths)> {
    match DirBuilder::new().mode(0o700).create(directory) {
        Ok(()) => {}
        Err(error) if error.kind() == ErrorKind::AlreadyExists => {}
        Err(error) => return Err(error),
    }
    let metadata = fs::symlink_metadata(directory)?;
    if !metadata.is_dir()
        || metadata.uid() != unsafe { libc::geteuid() }
        || metadata.mode() & 0o077 != 0
    {
        return Err(Error::new(
            ErrorKind::PermissionDenied,
            "Display directory must be owned by this UID and mode 0700",
        ));
    }
    let mut paths = SocketPaths(Vec::new());
    let mut listeners = Vec::new();
    for name in SOCKETS {
        let path = directory.join(name);
        let listener = UnixListener::bind(&path)?;
        paths.0.push(path.clone());
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600))?;
        listener.set_nonblocking(true)?;
        listeners.push(listener);
    }
    Ok((listeners, paths))
}

fn compositor(card: &str, directory: &Path) -> Result<()> {
    let (listeners, paths) = listeners(directory)?;
    let mut kms = Kms::open_mode(card, WIDTH * 2, HEIGHT)?;
    let mut surfaces = Surfaces::new();
    kms.present(&surfaces.compose(), 0)?;
    let stop = Arc::new(AtomicBool::new(false));
    let (events, receiver) = mpsc::sync_channel(2);
    let threads: Vec<_> = listeners
        .into_iter()
        .enumerate()
        .map(|(slot, listener)| {
            let events = events.clone();
            let stop = Arc::clone(&stop);
            thread::spawn(move || listen(listener, slot, events, stop))
        })
        .collect();
    drop(events);
    println!(
        "GPU_COMPOSITOR_READY mode={}x{} local=left backend=right directory={}",
        WIDTH * 2,
        HEIGHT,
        directory.display()
    );
    let mut generation = 0u64;
    let result = (|| {
        while !STOP.load(Ordering::Relaxed) {
            let event = match receiver.recv_timeout(Duration::from_millis(50)) {
                Ok(event) => event,
                Err(mpsc::RecvTimeoutError::Timeout) => continue,
                Err(error) => return Err(Error::new(ErrorKind::BrokenPipe, error)),
            };
            generation += 1;
            match event {
                Event::Frame {
                    slot,
                    sequence,
                    frame,
                    done,
                } => {
                    surfaces.update(slot, frame)?;
                    kms.present(&surfaces.compose(), generation)?;
                    println!("GPU_COMPOSITOR_FRAME generation={generation} slot={slot} sequence={sequence}");
                    let _ = done.send(true);
                }
                Event::Closed(slot) => {
                    surfaces.clear(slot);
                    kms.present(&surfaces.compose(), generation)?;
                    println!("GPU_COMPOSITOR_CLEAR generation={generation} slot={slot}");
                }
            }
        }
        Ok(())
    })();
    stop.store(true, Ordering::Relaxed);
    drop(receiver);
    for worker in threads {
        worker
            .join()
            .map_err(|_| Error::other("Display worker panicked"))?;
    }
    result?;
    drop(kms);
    drop(paths);
    println!("GPU_COMPOSITOR_STOPPED");
    Ok(())
}

fn local_frame(pattern: bool) -> Vec<u8> {
    let colors = [[32, 160, 64], [32, 64, 192]];
    let mut frame = Vec::with_capacity(FRAME_BYTES);
    for row in 0..HEIGHT {
        for column in 0..WIDTH {
            let index = ((row / 80 + column / 80) as usize + usize::from(pattern)) % 2;
            frame.extend_from_slice(&colors[index]);
        }
    }
    frame
}

fn local_app(directory: &Path, animate: bool) -> Result<()> {
    let mut client = Client::connect(&directory.join(SOCKETS[0]))?;
    let mut pattern = false;
    client.present(&local_frame(pattern))?;
    println!("GPU_LOCAL_FRAME pattern=0 sequence={}", client.sequence);
    let mut next = Instant::now() + Duration::from_millis(200);
    while !STOP.load(Ordering::Relaxed) {
        if REDRAW.swap(false, Ordering::Relaxed) || (animate && Instant::now() >= next) {
            pattern = !pattern;
            client.present(&local_frame(pattern))?;
            println!(
                "GPU_LOCAL_FRAME pattern={} sequence={}",
                u8::from(pattern),
                client.sequence
            );
            next = Instant::now() + Duration::from_millis(200);
        }
        thread::sleep(Duration::from_millis(10));
    }
    println!("GPU_LOCAL_STOPPED");
    Ok(())
}

extern "C" fn signal(number: libc::c_int) {
    if number == libc::SIGUSR1 {
        REDRAW.store(true, Ordering::Relaxed);
    } else {
        STOP.store(true, Ordering::Relaxed);
    }
}

pub fn run() -> Result<()> {
    let arguments: Vec<_> = std::env::args().collect();
    unsafe {
        for number in [libc::SIGINT, libc::SIGTERM, libc::SIGUSR1] {
            if libc::signal(number, signal as *const () as libc::sighandler_t) == libc::SIG_ERR {
                return Err(Error::last_os_error());
            }
        }
    }
    match arguments.get(1).map(String::as_str) {
        Some("compositor") if arguments.len() == 4 => compositor(&arguments[2], Path::new(&arguments[3])),
        Some("local-app") if arguments.len() == 3 || (arguments.len() == 4 && arguments[3] == "--animate") => {
            local_app(Path::new(&arguments[2]), arguments.len() == 4)
        }
        _ => Err(Error::new(ErrorKind::InvalidInput,
            "Usage: bao-gpu-display compositor CARD PRIVATE_DIRECTORY | local-app PRIVATE_DIRECTORY [--animate]")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_rejects_bad_magic_dimensions_and_stale_sequences() {
        assert_eq!(decode_header(&header(2), 1).unwrap(), 2);
        assert!(decode_header(&header(1), 1).is_err());
        assert!(decode_header(&header(0), 0).is_err());
        for offset in [0, 16, 20] {
            let mut packet = header(2);
            packet[offset] ^= 1;
            assert!(decode_header(&packet, 1).is_err());
        }
    }

    #[test]
    fn independent_surfaces_preserve_rows_and_disconnect_isolation() {
        let mut surfaces = Surfaces::new();
        let left = local_frame(false);
        let right = local_frame(true);
        surfaces.update(0, left.clone()).unwrap();
        surfaces.update(1, right.clone()).unwrap();
        let stride = WIDTH as usize * 3;
        let composed = surfaces.compose();
        for row in 0..HEIGHT as usize {
            assert_eq!(
                &composed[row * stride * 2..][..stride],
                &left[row * stride..][..stride]
            );
            assert_eq!(
                &composed[row * stride * 2 + stride..][..stride],
                &right[row * stride..][..stride]
            );
        }
        surfaces.clear(1);
        let cleared = surfaces.compose();
        for row in 0..HEIGHT as usize {
            assert_eq!(
                &cleared[row * stride * 2..][..stride],
                &left[row * stride..][..stride]
            );
            assert!(cleared[row * stride * 2 + stride..][..stride]
                .iter()
                .all(|byte| *byte == 0));
        }
        assert!(surfaces.update(2, right.clone()).is_err());
        assert!(surfaces.update(0, right[..10].to_vec()).is_err());
    }

    #[test]
    fn socket_client_waits_for_matching_acknowledgement() {
        let (stream, mut peer) = UnixStream::pair().unwrap();
        let mut client = Client {
            path: PathBuf::new(),
            stream: Some(stream),
            sequence: 0,
        };
        let worker = thread::spawn(move || {
            let mut bytes = [0; 24];
            peer.read_exact(&mut bytes).unwrap();
            assert_eq!(decode_header(&bytes, 0).unwrap(), 1);
            let mut frame = vec![0; FRAME_BYTES];
            peer.read_exact(&mut frame).unwrap();
            assert_eq!(frame, local_frame(false));
            peer.write_all(&bytes[..16]).unwrap();
            peer.read_exact(&mut bytes).unwrap();
            peer.read_exact(&mut frame).unwrap();
            peer.write_all(&header(99)[..16]).unwrap();
        });
        client.present(&local_frame(false)).unwrap();
        assert!(client.present(&local_frame(true)).is_err());
        assert!(client.stream.is_none());
        assert!(client.present(&[0; 3]).is_err());
        worker.join().unwrap();
    }

    #[test]
    fn partial_packet_and_shutdown_do_not_block_forever() {
        let (mut stream, mut peer) = UnixStream::pair().unwrap();
        stream
            .set_read_timeout(Some(Duration::from_millis(10)))
            .unwrap();
        peer.write_all(&[1]).unwrap();
        let stop = AtomicBool::new(false);
        assert_eq!(
            read_bounded(&mut stream, &mut [0; 2], &stop, false)
                .unwrap_err()
                .kind(),
            ErrorKind::TimedOut
        );
        stop.store(true, Ordering::Relaxed);
        assert_eq!(
            read_bounded(&mut stream, &mut [0], &stop, true)
                .unwrap_err()
                .kind(),
            ErrorKind::Interrupted
        );
    }

    #[test]
    fn two_connection_workers_only_acknowledge_presented_frames() {
        let stop = Arc::new(AtomicBool::new(false));
        let (events, receiver) = mpsc::sync_channel(2);
        let mut clients = Vec::new();
        let mut workers = Vec::new();
        for slot in 0..2 {
            let (mut client, stream) = UnixStream::pair().unwrap();
            client.set_read_timeout(Some(IO_TIMEOUT)).unwrap();
            let events = events.clone();
            let stop = Arc::clone(&stop);
            workers.push(thread::spawn(move || {
                connection(stream, slot, &events, &stop)
            }));
            client.write_all(&header(1)).unwrap();
            client.write_all(&local_frame(slot == 1)).unwrap();
            clients.push(client);
        }
        let mut surfaces = Surfaces::new();
        for _ in 0..2 {
            let Event::Frame {
                slot,
                sequence,
                frame,
                done,
            } = receiver.recv_timeout(IO_TIMEOUT).unwrap()
            else {
                panic!("expected a frame")
            };
            assert_eq!(sequence, 1);
            let mut probe = [0; 16];
            clients[slot].set_nonblocking(true).unwrap();
            assert_eq!(
                clients[slot].read(&mut probe).unwrap_err().kind(),
                ErrorKind::WouldBlock
            );
            clients[slot].set_nonblocking(false).unwrap();
            surfaces.update(slot, frame).unwrap();
            done.send(true).unwrap();
            clients[slot].read_exact(&mut probe).unwrap();
            assert_eq!(probe, header(1)[..16]);
        }
        assert_eq!(surfaces.frames[0], local_frame(false));
        assert_eq!(surfaces.frames[1], local_frame(true));
        stop.store(true, Ordering::Relaxed);
        drop(clients);
        for worker in workers {
            assert!(worker.join().unwrap().is_err());
        }
    }
}
