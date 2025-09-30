use core::{
    error::Error,
    iter,
    net::{IpAddr, Ipv4Addr},
    num::NonZeroUsize,
    ptr::NonNull,
};

fn as_bytes<T>(data: &[T]) -> &[u8] {
    // SAFETY: all bit patterns for u8 are valid, references have same lifetime and location
    unsafe { core::slice::from_raw_parts(data.as_ptr().cast(), size_of::<T>() * data.len()) }
}

const PORT: u16 = 6910;
const CHUNK_SIZE_FRAMES: NonZeroUsize = NonZeroUsize::new(1 << 3).unwrap();
const RB_NUM_CHUNKS: NonZeroUsize = NonZeroUsize::new(1 << 8).unwrap();

const DEFAULT_NUM_PORTS: NonZeroUsize = NonZeroUsize::MIN;

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
struct JackBufPtr(NonNull<f32>);

impl JackBufPtr {
    #[inline]
    pub const unsafe fn increment(self) -> Self {
        Self(unsafe { self.0.add(1) })
    }

    #[inline]
    pub const unsafe fn read(self) -> f32 {
        // We're converting to a reference here, instead of using buf.read()
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

fn main() -> Result<(), Box<dyn Error>> {
    let mut args = std::env::args().skip(1);

    let num_ports = args
        .next()
        .as_deref()
        .map(str::parse)
        .unwrap_or(Ok(DEFAULT_NUM_PORTS))?;

    let addr = core::net::SocketAddr::new(
        args.next()
            .as_deref()
            .map(str::parse)
            .unwrap_or(Ok(IpAddr::V4(Ipv4Addr::LOCALHOST)))?,
        PORT,
    );

    let net_chunk_size_spls = CHUNK_SIZE_FRAMES.checked_mul(num_ports).unwrap();
    let rb_size_samples = net_chunk_size_spls.checked_mul(RB_NUM_CHUNKS).unwrap();
    let (mut tx, mut rx) = rtrb::RingBuffer::new(rb_size_samples.get());

    let socket = std::net::UdpSocket::bind("0.0.0.0:0").unwrap();

    let network_thread = std::thread::current();

    let (jack_client, _status) =
        jack::Client::new("CLIENT", jack::ClientOptions::NO_START_SERVER).unwrap();

    let ports = Box::from_iter((0..num_ports.get()).map(|i| {
        jack_client
            .register_port(&format!("input_{i}"), jack::AudioIn::default())
            .unwrap()
    }));

    let mut port_buf_ptrs =
        Box::from_iter(iter::repeat_with(JackBufPtr::dangling).take(num_ports.get()));

    let writer_async_client = jack::contrib::ClosureProcessHandler::new(move |_client, scope| {
        let mut remaining_frames = scope.n_frames() as usize;

        for (port, ptr) in ports.iter().zip(&mut port_buf_ptrs) {
            *ptr = JackBufPtr::from_slice(port.as_slice(scope));
        }

        while let Some(rem) = NonZeroUsize::new(remaining_frames) {
            let frames = CHUNK_SIZE_FRAMES.min(rem);

            let Ok(mut write_chunk) =
                tx.write_chunk_uninit(num_ports.checked_mul(frames).unwrap().get())
            else {
                continue;
            };

            let (start, end) = write_chunk.as_mut_slices();

            let mut rb_chunk_iter = start.iter_mut().chain(end);

            for _i in 0..frames.get() {
                // deinterleave chunk contents

                for buf in &mut port_buf_ptrs {
                    // SAFETY: buf is valid, and within the actual buffer's bounds
                    let sample = unsafe { buf.read() };

                    // SAFETY: this happens at most `frames` times, guaranteeing this stays within
                    // the buffer's bounds
                    *buf = unsafe { buf.increment() };

                    rb_chunk_iter.next().unwrap().write(sample);
                }
            }

            unsafe { write_chunk.commit_all() };

            remaining_frames -= frames.get();

            if rb_size_samples.get() - tx.slots() >= net_chunk_size_spls.get() {
                network_thread.unpark();
            }
        }

        jack::Control::Continue
    });

    let _active_client = jack_client.activate_async((), writer_async_client).unwrap();

    loop {
        let Ok(read_chunk) = rx.read_chunk(net_chunk_size_spls.get()) else {
            std::thread::park();
            continue;
        };

        // The other half is empty since RB_SIZE % CHUNK_SIZE = 0
        let (slice, _) = read_chunk.as_slices();

        // println!("{}", slice.iter().copied().map(f32::abs).max_by(f32::total_cmp).unwrap());
        let slice = as_bytes(slice);

        socket.send_to(slice, &addr).unwrap();

        read_chunk.commit_all();
    }
}
