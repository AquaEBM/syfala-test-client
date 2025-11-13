mod interleaver;

use core::{
    cmp,
    error::Error,
    net::{IpAddr, Ipv4Addr},
    num,
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

fn read_exact_array<const N: usize>(reader: &mut (impl Read + ?Sized)) -> io::Result<[u8; N]> {
    let mut buf = [0; N];
    reader.read_exact(&mut buf).map(|()| buf)
}

fn main() -> Result<(), Box<dyn Error>> {
    let mut args = std::env::args().skip(1);

    let addr = core::net::SocketAddr::new(
        args.next()
            .as_deref()
            .map(str::parse)
            .unwrap_or(Ok(IpAddr::V4(Ipv4Addr::LOCALHOST)))?,
        PORT,
    );

    let n_ports_request = args
        .next()
        .as_deref()
        .map(|s| s.parse().unwrap())
        .unwrap_or(DEFAULT_N_PORTS.get());

    println!("Connecting to SyFaLa Server at {addr}...");
    let mut stream = std::net::TcpStream::connect(&addr)?;
    stream.set_nodelay(true)?;

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

    let chunk_size_spls = chunk_size_frames
        .checked_mul(n_ports)
        .unwrap()
        .min(MAX_SPLS_PER_DATAGRAM);

    let rb_size_spls = RB_SIZE_FRAMES
        .checked_mul(n_ports)
        .unwrap()
        .get();

    println!("Allocating Ring Buffer ({rb_size_spls} samples)");

    let (mut tx, mut rx) = rtrb::RingBuffer::new(rb_size_spls);

    let network_thread = std::thread::current();

    println!("Creating JACK client...");
    let (jack_client, _status) = jack::Client::new(
        &format!("SyFaLa\n{}\n{}", addr.ip(), addr.port()),
        jack::ClientOptions::NO_START_SERVER,
    )
    .unwrap();

    let ports = Box::from_iter((1..=n_ports.get()).map(|i| {
        jack_client
            .register_port(&format!("input_{i}"), jack::AudioIn::default())
            .unwrap()
    }));

    let mut interleaver = interleaver::Interleaver::new(n_ports.get());

    let async_client = jack::contrib::ClosureProcessHandler::new(move |_client, scope| {
        let Some(frames) = num::NonZeroUsize::new(scope.n_frames() as usize) else {
            return jack::Control::Continue;
        };

        let slots = tx.slots().min(n_ports.checked_mul(frames).unwrap().get());
        let mut write_chunk = tx.write_chunk_uninit(slots).unwrap();

        if let Err(_e) = interleaver.interleaver(
            frames.get(),
            ports.iter().map(|p| p.as_slice(scope)),
            write_chunk.as_mut_slices().into()
        ) {
            return jack::Control::Quit;
        }

        unsafe { write_chunk.commit_all() };

        network_thread.unpark();

        jack::Control::Continue
    });

    println!("Starting JACK client...\n");
    let _async_client = jack_client.activate_async((), async_client).unwrap();

    println!("Creating UDP Socket...");
    let socket = std::net::UdpSocket::bind(stream.local_addr().unwrap()).unwrap();
    socket.connect(&addr)?;

    let mut packet_idx: u32 = 0;
    
    // index of the last sent sample within the current server chunk
    let mut chunk_sample_idx: usize = 0;

    let mut scratch_buffer = Vec::with_capacity(MAX_SPLS_PER_DATAGRAM.get() + 1);

    println!("Starting UDP client...\n");

    loop {

        let slots = rx.slots().min(MAX_SPLS_PER_DATAGRAM.get());

        // the number of samples left to fill the current server chunk
        let required_samples = chunk_size_spls.get() - chunk_sample_idx;

        if slots < required_samples {
            println!("waiting for samples...");
            std::thread::park();
            continue;
        }

        // Reinterpreted back as a u32 by the server.
        scratch_buffer.push(f32::from_bits(packet_idx));

        let rb_chunk = rx.read_chunk(slots).unwrap();
        let (start, end) = rb_chunk.as_slices();
        scratch_buffer.extend_from_slice(start);
        scratch_buffer.extend_from_slice(end);
        rb_chunk.commit_all();

        socket.send(as_bytes(scratch_buffer.as_slice()))?;
        scratch_buffer.clear();
        packet_idx = packet_idx.wrapping_add(1);

        chunk_sample_idx += slots;
        chunk_sample_idx %= chunk_size_spls;
    }
}
