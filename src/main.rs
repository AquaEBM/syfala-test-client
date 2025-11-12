use core::{
    cmp,
    error::Error,
    iter,
    net::{IpAddr, Ipv4Addr},
    num,
    ptr::NonNull,
};
use std::io::{self, Read, Write};

fn as_bytes<T>(data: &[T]) -> &[u8] {
    // SAFETY: all bit patterns for u8 are valid, references have same lifetime and location
    unsafe { core::slice::from_raw_parts(data.as_ptr().cast(), size_of::<T>() * data.len()) }
}

const PORT: u16 = 6910;

// This has to be big enough to accomodate at least a couple of process cycles
// Needless to say, those can have very high buffer sizes. One second is usually enough
const RB_SIZE_FRAMES: num::NonZeroUsize = num::NonZeroUsize::new(1 << 16).unwrap();

const MAX_SPLS_PER_DATAGRAM: num::NonZeroUsize = num::NonZeroUsize::new(362).unwrap();

// 1
const DEFAULT_N_PORTS: num::NonZeroUsize = num::NonZeroUsize::MIN;

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
struct JackBufPtr(NonNull<f32>);

impl JackBufPtr {
    #[inline]
    pub const unsafe fn increment(&mut self) {
        *self = Self(unsafe { self.0.add(1) })
    }

    #[inline]
    pub const unsafe fn read(&self) -> f32 {
        // We're converting to a reference here, instead of using self.0.read()
        // to make it clear to the optimizer that we have read-only access
        *unsafe { self.0.as_ref() }
    }

    #[inline]
    pub const fn from_slice(ptr: &[f32]) -> Self {
        Self(NonNull::new(ptr.as_ptr().cast_mut()).unwrap())
    }

    #[inline]
    pub const fn dangling() -> Self {
        Self(NonNull::dangling())
    }
}

unsafe impl Send for JackBufPtr {}
unsafe impl Sync for JackBufPtr {}

fn read_exact_array<const N: usize>(reader: &mut (impl Read + ?Sized)) -> io::Result<[u8; N]> {
    let mut buf = [0; N];
    reader.read_exact(&mut buf).map(|()| buf)
}

fn main() -> Result<(), Box<dyn Error>> {
    let mut args = std::env::args().skip(1);

    let n_ports_request = args
        .next()
        .as_deref()
        .map(|s| s.parse().unwrap())
        .unwrap_or(DEFAULT_N_PORTS.get());

    let addr = core::net::SocketAddr::new(
        args.next()
            .as_deref()
            .map(str::parse)
            .unwrap_or(Ok(IpAddr::V4(Ipv4Addr::LOCALHOST)))?,
        PORT,
    );

    println!("Attempting to connect to SyFaLa Server at {addr}...");
    let mut stream = std::net::TcpStream::connect(&addr)?;
    stream.set_nodelay(true)?;

    println!("Success!\n");

    println!("Requesting {n_ports_request} ports...");
    stream.write_all(&n_ports_request.to_be_bytes())?;

    let n_ports = usize::from_be_bytes(read_exact_array(&mut stream)?);

    match n_ports.cmp(&n_ports_request) {
        cmp::Ordering::Less => println!("WARNING: Found {n_ports} ports instead\n"),
        cmp::Ordering::Equal => println!("Accepted!\n"),
        cmp::Ordering::Greater => {
            unreachable!("INTERNAL ERROR: server returned invalid port count")
        }
    }

    let chunk_size_frames =
        num::NonZeroUsize::new(usize::from_be_bytes(read_exact_array(&mut stream)?))
            .expect("INTERNAL ERROR: Server returned a chunk size of 0 frames");

    println!("Server suggests {chunk_size_frames} frames of buffering\n");

    let Some(n_ports) = num::NonZeroUsize::new(n_ports) else {
        return Err("Attempted to create a client with 0 ports".into());
    };

    println!("Creating UDP Socket...");
    let socket = std::net::UdpSocket::bind(stream.local_addr().unwrap()).unwrap();
    socket.connect(&addr)?;
    println!("Success!\n");

    let chunk_size_spls = chunk_size_frames
        .checked_mul(n_ports)
        .unwrap()
        .min(MAX_SPLS_PER_DATAGRAM);

    let rb_size_spls = RB_SIZE_FRAMES
        .checked_mul(n_ports)
        .unwrap()
        .get()
        .checked_next_multiple_of(chunk_size_spls.get())
        .unwrap();

    println!("Allocating Ring Buffer. Size = {rb_size_spls} samples");

    let (mut tx, mut rx) = rtrb::RingBuffer::new(rb_size_spls);

    let network_thread = std::thread::current();

    let (jack_client, _status) = jack::Client::new(
        &format!("SyFaLa {} | {}", addr.ip(), addr.port()),
        jack::ClientOptions::NO_START_SERVER,
    )
    .unwrap();

    let ports = Box::from_iter((1..=n_ports.get()).map(|i| {
        jack_client
            .register_port(&format!("input_{i}"), jack::AudioIn::default())
            .unwrap()
    }));

    let mut port_buf_ptrs =
        Box::from_iter(iter::repeat_with(JackBufPtr::dangling).take(n_ports.get()));

    let writer_async_client = jack::contrib::ClosureProcessHandler::new(move |_client, scope| {
        let Some(frames) = num::NonZeroUsize::new(scope.n_frames() as usize) else {
            return jack::Control::Continue;
        };

        for (port, ptr) in ports.iter().zip(&mut port_buf_ptrs) {
            *ptr = JackBufPtr::from_slice(port.as_slice(scope));
        }

        let Ok(mut write_chunk) = tx.write_chunk_uninit(n_ports.checked_mul(frames).unwrap().get())
        else {
            return jack::Control::Continue;
        };

        let (start, end) = write_chunk.as_mut_slices();

        let mut ptrs_iter = port_buf_ptrs.iter_mut();

        for sample in iter::chain(start, end) {
            let ptr = if let Some(ptr) = ptrs_iter.next() {
                ptr
            } else {
                ptrs_iter = port_buf_ptrs.iter_mut();
                ptrs_iter.next().unwrap()
            };

            // SAFETY: buf is valid, and within the actual buffer's bounds
            sample.write(unsafe { ptr.read() });

            // SAFETY: this happens at most `frames` times for this pointer, guaranteeing
            // this stays within the buffer's bounds
            unsafe { ptr.increment() };
        }

        unsafe { write_chunk.commit_all() };

        if rb_size_spls - tx.slots() >= chunk_size_spls.get() {
            network_thread.unpark();
        }

        jack::Control::Continue
    });

    let _async_client = jack_client.activate_async((), writer_async_client).unwrap();

    loop {
        let Ok(read_chunk) = rx.read_chunk(chunk_size_spls.get()) else {
            std::thread::park();
            continue;
        };

        // the ring buffer's size is a multiple of chunk_size_spls, so the second slice is empty
        let (slice, _) = read_chunk.as_slices();

        socket.send(as_bytes(slice)).unwrap();

        read_chunk.commit_all();
    }
}
