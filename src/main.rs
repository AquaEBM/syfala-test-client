mod interleaver;

use core::{
    cell, cmp,
    error::Error,
    iter,
    net::{IpAddr, Ipv4Addr},
    num,
};
use std::io::{self, Read, Write};

const fn as_bytes(data: &[Sample]) -> &[u8] {
    // SAFETY: all bit patterns for u8 are valid, references have same lifetime
    unsafe {
        core::slice::from_raw_parts(
            data.as_ptr().cast(),
            data.len().strict_mul(SAMPLE_SIZE.get()),
        )
    }
}

const fn nz(x: usize) -> num::NonZeroUsize {
    num::NonZeroUsize::new(x).unwrap()
}

const DEFAULT_PORT: u16 = 6910;

type Sample = f32;
const SILENCE: Sample = 0.;

const SAMPLE_SIZE: num::NonZeroUsize = nz(size_of::<Sample>());

const SAMPLE_RATE: f64 = 48000.;

// This has to be big enough to accomodate at least a couple of process cycles
// Needless to say, those can reach very high buffer sizes.
// We choose four seconds here
const RB_SIZE_SECONDS: f64 = 4.;

const RB_SIZE_FRAMES: num::NonZeroUsize = nz((SAMPLE_RATE * RB_SIZE_SECONDS) as usize);

const MAX_DATAGRAM_SIZE: num::NonZeroUsize = nz(1452);

const MAX_SAMPLE_DATA_PER_DATAGRAM: num::NonZeroUsize =
    nz(MAX_DATAGRAM_SIZE.get().strict_sub(size_of::<u64>()));

const MAX_SPLS_PER_DATAGRAM: num::NonZeroUsize =
    nz(MAX_SAMPLE_DATA_PER_DATAGRAM.get() / SAMPLE_SIZE.get());

const DEFAULT_N_PORTS: num::NonZeroUsize = num::NonZeroUsize::MIN; // AKA 1

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
        DEFAULT_PORT,
    );

    let n_ports_request = args
        .next()
        .as_deref()
        .map(str::parse)
        .unwrap_or(Ok(DEFAULT_N_PORTS))?
        .get();

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

    let rb_size_spls = RB_SIZE_FRAMES.checked_mul(n_ports).unwrap().get();

    println!("Allocating Ring Buffer ({rb_size_spls} samples)");

    let (mut tx, mut rx) = rtrb::RingBuffer::<Sample>::new(rb_size_spls);

    let network_thread = std::thread::current();

    println!("Creating JACK client...");
    let (jack_client, _status) = jack::Client::new(
        &format!("SyFaLa\n{}\n{}", addr.ip(), addr.port()),
        jack::ClientOptions::NO_START_SERVER,
    )?;

    let ports = Box::from_iter((1..=n_ports.get()).map(|i| {
        jack_client
            .register_port(&format!("input_{i}"), jack::AudioIn::default())
            .unwrap()
    }));

    let mut interleaver = interleaver::Interleaver::<Sample>::new(n_ports.get());

    // lol
    let mut frame_counter_cell = cell::OnceCell::new();

    let async_client = jack::contrib::ClosureProcessHandler::new(move |_client, scope| {
        let Some(current_cycle_frames) = num::NonZeroUsize::new(scope.n_frames() as usize) else {
            return jack::Control::Continue;
        };

        let last_frame_time = scope.last_frame_time();

        // NIGHTLY: #[feature(once_cell_mut)], use get_or_init_mut
        let _ = frame_counter_cell.get_or_init(|| last_frame_time);
        let frame_counter = frame_counter_cell.get_mut().unwrap();

        // Wrapping arithmetic is fine here if it is guaranteed that last_frame_time
        // increases monotonically While the docs do, such isn't always the case
        let n_missed_frames = last_frame_time.wrapping_sub(*frame_counter);

        let n_missed_frames = n_missed_frames as usize;

        if n_missed_frames > 1000 {
            println!(
                "hehehee {n_missed_frames}, last: {last_frame_time}, now {frame_counter}, frames: {current_cycle_frames}"
            );
        }

        // Number of frames we want this cycle
        // The first frames_missed frames will be filled with zeros
        let n_requested_frames = current_cycle_frames.checked_add(n_missed_frames).unwrap();
        // We must always send whole frames (hence the floor division)
        let n_available_frames = tx.slots() / n_ports;

        // Number of frames available in the ring buffer
        // This should always be equal to requested_frames. If it isn't,
        // our ring buffer is too small, or our network thread is stalling...
        let n_read_frames = n_available_frames.min(n_requested_frames.get());

        // Number of frames that will actually be filled with zeros
        let n_zero_filled_frames = n_missed_frames.min(n_read_frames);
        let n_zero_filled_samples = n_zero_filled_frames.strict_mul(n_ports.get());

        let n_written_frames = n_read_frames.strict_sub(n_zero_filled_frames);

        let mut write_chunk = tx
            .write_chunk_uninit(n_read_frames.strict_mul(n_ports.get()))
            .unwrap();

        let (start, end) = write_chunk.as_mut_slices();

        // Split the ring buffer slices into two parts, the first one will be filled with
        // zeros, we will write into the rest as much of this cycle's data as we can

        let start_zero_filled_len = start.len().min(n_zero_filled_samples);
        let end_zero_filled_len = n_zero_filled_samples.strict_sub(start_zero_filled_len);

        let (start_zero_filled, start_samples) = start.split_at_mut(start_zero_filled_len);
        let (end_zero_filled, end_samples) = end.split_at_mut(end_zero_filled_len);

        // NIGHTLY: #[feature(maybe_uninit_fill)], use write_filled
        for s in iter::chain(start_zero_filled, end_zero_filled) {
            s.write(SILENCE);
        }

        if let Err(_e) = interleaver.interleave(
            n_written_frames,
            ports.iter().map(|p| p.as_slice(scope)),
            [start_samples, end_samples],
        ) {
            // reaching here is an irrecoverable bug! exit...
            return jack::Control::Quit;
        }

        unsafe { write_chunk.commit_all() };

        *frame_counter = frame_counter.wrapping_add(n_read_frames as u32);

        // Is this necessary?
        network_thread.unpark();

        jack::Control::Continue
    });

    println!("Starting JACK client...\n");
    let _async_client = jack_client.activate_async((), async_client)?;

    println!("Starting UDP client...");
    let socket = std::net::UdpSocket::bind(stream.local_addr()?)?;
    socket.connect(&addr)?;

    // Keep track of the number of samples sent, samples, not frames, we allow sending
    // incomplete frames in a packet to account for very high channel count scenarios
    let mut current_sample_idx = 0u64;

    let chunk_size_spls = n_ports.checked_mul(chunk_size_frames).unwrap();

    // Here is where we'll write the packets sent over UDP
    let mut scratch_buf = arrayvec::ArrayVec::<u8, { MAX_DATAGRAM_SIZE.get() }>::new_const();

    loop {
        scratch_buf.clear();
        scratch_buf.extend(current_sample_idx.to_be_bytes());

        // Number of samples left to fill the next server chunk
        let n_remaining_chunk_spls = nz(chunk_size_spls
            .get()
            .strict_sub(current_sample_idx as usize % chunk_size_spls));

        // If it's greater than the allowed size of a datagram, split the samples
        let n_samples_to_send = n_remaining_chunk_spls.min(MAX_SPLS_PER_DATAGRAM);

        if rx.slots() < n_samples_to_send.get() {
            std::thread::park();
            continue;
        }

        let rb_chunk = rx.read_chunk(n_samples_to_send.get()).unwrap();

        let (start, end) = rb_chunk.as_slices();
        scratch_buf.try_extend_from_slice(as_bytes(start)).unwrap();
        scratch_buf.try_extend_from_slice(as_bytes(end)).unwrap();
        rb_chunk.commit_all();

        let bytes_sent = socket.send(scratch_buf.as_slice())?;

        if bytes_sent < scratch_buf.len() {
            eprintln!("WARNING: partially dropped datagram");
        }

        current_sample_idx = current_sample_idx.wrapping_add(n_samples_to_send.get() as u64);
    }
}
