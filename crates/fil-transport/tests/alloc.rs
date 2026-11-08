//! How many allocations (and bytes) a data frame costs on its way through the frame path.
//!
//! This file is its own test binary because it installs a counting global allocator. The counters
//! are per thread, so the other tests of the process (there are none in this binary that run
//! beside these on a thread of their own, but tokio's workers are another matter) cannot
//! disturb a measurement: a test measures only what its own thread allocates inside `measure`.
//!
//! What is pinned (C1, "zero-copy frames"):
//!
//! - **Encode:** a 16 KiB data frame written into the writer's reusable buffer allocates nothing;
//!   encoded into a new buffer, once. No `postcard::to_allocvec` for the channel varint.
//! - **Decode on the pipe:** one allocation, of the frame's size, whatever way the stream is
//!   chopped up; no zero-filled buffer; the payload handed up is that buffer.
//! - **Building a data payload from a ring or the output log:** one allocation, of the payload's
//!   size (varint and bytes together), one copy.
//! - **QUIC read** (`quic::chunks::Chunks`, which `read_stream` feeds with quinn's chunks): every
//!   payload, whether it lies inside one chunk or spans several, is one allocation of exactly its
//!   size, filled by one copy and not zero-filled first. It is never a slice of a chunk (that
//!   would pin quinn's receive buffer).

// A global allocator is unsafe by definition; this file is the only place in the workspace that
// writes one, and it only counts and forwards to the system allocator.
#![allow(unsafe_code)]

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

use bytes::{Bytes, BytesMut};
use fil_proto::ChannelId;
use fil_proto::chan::SendChannel;
use fil_proto::frame::{DataSegment, Frame, FrameDecoder};
use fil_proto::limits::{MAX_DATA_PAYLOAD, MAX_FRAME_BODY};
use fil_proto::log::OutputLog;
use fil_transport::quic::chunks::Chunks;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Counts {
    /// Calls to `alloc` and `realloc` (a growth is an allocation).
    allocs: u64,
    /// Calls to `alloc_zeroed`: `vec![0; n]`, `BytesMut::zeroed`. A zero-filled buffer.
    zeroed: u64,
    /// Bytes asked for by all of those.
    bytes: u64,
}

thread_local! {
    static COUNTS: Cell<Counts> = const { Cell::new(Counts { allocs: 0, zeroed: 0, bytes: 0 }) };
}

fn bump(f: impl FnOnce(&mut Counts)) {
    // `try_with`: a thread that is being torn down has no counters, and must still allocate.
    let _ = COUNTS.try_with(|c| {
        let mut v = c.get();
        f(&mut v);
        c.set(v);
    });
}

struct Counting;

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        bump(|c| {
            c.allocs = c.allocs.wrapping_add(1);
            c.bytes = c.bytes.wrapping_add(layout.size() as u64);
        });
        unsafe { System.alloc(layout) }
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        bump(|c| {
            c.zeroed = c.zeroed.wrapping_add(1);
            c.bytes = c.bytes.wrapping_add(layout.size() as u64);
        });
        unsafe { System.alloc_zeroed(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        bump(|c| {
            c.allocs = c.allocs.wrapping_add(1);
            c.bytes = c.bytes.wrapping_add(new_size as u64);
        });
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static ALLOC: Counting = Counting;

/// What `f` allocated on this thread. The value it returns is handed back (and dropped by the
/// caller, outside the measurement; frees are not counted anyway).
fn measure<R>(f: impl FnOnce() -> R) -> (R, Counts) {
    let before = COUNTS.with(Cell::get);
    let r = f();
    let after = COUNTS.with(Cell::get);
    (
        r,
        Counts {
            allocs: after.allocs - before.allocs,
            zeroed: after.zeroed - before.zeroed,
            bytes: after.bytes - before.bytes,
        },
    )
}

fn data_frame(channel: u32, seq: u64) -> Frame {
    let body: Vec<u8> = (0..MAX_DATA_PAYLOAD).map(|i| (i % 251) as u8).collect();
    Frame::new(ChannelId(channel), DataSegment::encode(seq, &body).unwrap())
}

#[test]
fn encoding_a_16_kib_data_frame_allocates_nothing_into_a_warm_buffer_and_once_into_a_new_one() {
    for channel in [1u32, 16, 300, u32::MAX] {
        let frame = data_frame(channel, 1 << 40);
        // The pipe writer's buffer: reused for every write, so it has grown once already.
        let mut out = BytesMut::with_capacity(64 * 1024);
        let ((), c) = measure(|| frame.encode_into(&mut out).unwrap());
        assert_eq!(
            c.allocs, 0,
            "channel {channel}: encode_into allocated: {c:?}"
        );
        assert_eq!(c.zeroed, 0);
        assert_eq!(out.len(), frame.wire_len());

        let (wire, c) = measure(|| frame.encode().unwrap());
        assert!(c.allocs <= 1, "channel {channel}: encode allocated {c:?}");
        assert_eq!(c.zeroed, 0);
        assert_eq!(
            c.bytes as usize,
            wire.len(),
            "one buffer, exactly the frame"
        );
        assert_eq!(wire, out);
    }
}

#[test]
fn decoding_a_16_kib_data_frame_on_the_pipe_allocates_once_however_the_stream_is_chopped() {
    let frame = data_frame(16, 12345);
    let wire = frame.encode().unwrap();
    for piece in [wire.len(), 16 * 1024, 4096, 1448, 100, 7, 1] {
        let mut dec = FrameDecoder::default();
        let (got, c) = measure(|| {
            let mut got = None;
            for part in wire.chunks(piece) {
                let mut input = part;
                while let Some(f) = dec.push(&mut input).unwrap() {
                    assert!(got.replace(f).is_none());
                }
            }
            got
        });
        let got = got.expect("a frame");
        assert_eq!(got, frame);
        assert_eq!(c.allocs, 1, "reads of {piece}: {c:?}");
        assert_eq!(
            c.zeroed, 0,
            "reads of {piece}: the body buffer was zero-filled"
        );
        assert!(
            c.bytes as usize <= MAX_FRAME_BODY,
            "reads of {piece}: more than one frame's worth was allocated: {c:?}"
        );
    }
}

/// Frames that are decoded and dropped one after another (a bulk transfer) cost one allocation
/// each; nothing accumulates between them.
#[test]
fn a_stream_of_frames_costs_one_allocation_each() {
    let wire = {
        let mut out = BytesMut::new();
        for seq in 0..50u64 {
            data_frame(16, seq * 16 * 1024)
                .encode_into(&mut out)
                .unwrap();
        }
        out.freeze()
    };
    let mut dec = FrameDecoder::default();
    let ((), c) = measure(|| {
        for part in wire.chunks(8192) {
            let mut input = part;
            while let Some(f) = dec.push(&mut input).unwrap() {
                drop(f);
            }
        }
    });
    assert_eq!(c.allocs, 50, "{c:?}");
    assert_eq!(c.zeroed, 0);
}

#[test]
fn building_a_data_payload_from_a_send_channel_is_one_allocation_and_one_copy() {
    let mut ch = SendChannel::new(1 << 20);
    let data: Vec<u8> = (0..MAX_DATA_PAYLOAD * 3).map(|i| (i % 253) as u8).collect();
    assert_eq!(ch.write(&data), data.len());
    for i in 0..3u64 {
        let (payload, c) = measure(|| ch.next_payload().unwrap());
        assert_eq!(c.allocs, 1, "payload {i}: {c:?}");
        assert_eq!(c.zeroed, 0);
        let seg = DataSegment::parse(&payload).unwrap();
        assert_eq!(seg.seq, i * MAX_DATA_PAYLOAD as u64);
        assert_eq!(
            c.bytes as usize,
            payload.len(),
            "exactly the payload, no scratch copy"
        );
        assert_eq!(
            seg.data,
            &data[i as usize * MAX_DATA_PAYLOAD..][..MAX_DATA_PAYLOAD]
        );
    }
}

#[test]
fn building_a_data_payload_from_the_output_log_is_one_allocation_and_one_copy() {
    let mut log = OutputLog::new(1 << 20);
    let data: Vec<u8> = (0..MAX_DATA_PAYLOAD * 2).map(|i| (i % 251) as u8).collect();
    log.append(&data);
    for i in 0..2usize {
        let seq = (i * MAX_DATA_PAYLOAD) as u64;
        let (got, c) = measure(|| log.read_payload(seq, MAX_DATA_PAYLOAD).unwrap());
        let (payload, n) = got;
        assert_eq!(n, MAX_DATA_PAYLOAD);
        assert_eq!(c.allocs, 1, "{c:?}");
        assert_eq!(c.zeroed, 0);
        assert_eq!(c.bytes as usize, payload.len());
        assert_eq!(
            DataSegment::parse(&payload).unwrap().data,
            &data[i * n..][..n]
        );
    }
}

/// A chunk the way quinn hands it over: a slice of a received packet's buffer, which other slices
/// share (a `Bytes` made straight from a `Vec` would allocate its shared header on the first
/// split, which a real chunk has already paid for).
fn datagram_slice(data: &[u8]) -> Bytes {
    let mut packet = BytesMut::with_capacity(data.len() + 100);
    packet.extend_from_slice(data);
    packet.extend_from_slice(&[0; 100]);
    packet.split_to(data.len()).freeze()
}

/// A QUIC payload is always copied into a buffer of its own, even when it lies inside one chunk
/// quinn delivered: one allocation of exactly its size, no zero-fill, and the pointer is not
/// inside the chunk. (A slice of the chunk would pin quinn's receive buffer, up to about 64 KiB
/// for a GRO batch on Linux, for as long as the payload is queued.)
#[test]
fn a_quic_payload_inside_one_chunk_is_one_exact_copy() {
    for len in [100usize, 511, 512, 1200] {
        let mut chunk = vec![0u8; 4 + len + 4];
        chunk[..4].copy_from_slice(&(len as u32).to_be_bytes());
        for (i, b) in chunk[4..4 + len].iter_mut().enumerate() {
            *b = (i % 251) as u8;
        }
        let chunk = datagram_slice(&chunk);
        let (base, end) = (
            chunk.as_ptr() as usize,
            chunk.as_ptr() as usize + chunk.len(),
        );
        let mut chunks = Chunks::default();
        let mut prefix = [0u8; 4];
        let (payload, c) = measure(|| {
            chunks.push(chunk);
            assert_eq!(chunks.copy_to(&mut prefix), 4);
            chunks.start_payload(u32::from_be_bytes(prefix) as usize);
            chunks
                .poll_payload()
                .expect("the whole payload is in the chunk")
        });
        assert_eq!(c.allocs, 1, "len {len}: {c:?}");
        assert_eq!(c.zeroed, 0, "len {len}: zero-filled: {c:?}");
        assert_eq!(c.bytes as usize, len, "len {len}: not an exact buffer");
        let at = payload.as_ptr() as usize;
        assert!(
            at + payload.len() <= base || at >= end,
            "len {len}: a slice of the chunk"
        );
        assert_eq!(payload.len(), len);
        assert_eq!(payload[7 % len], (7 % len) as u8);

        // The reader lets go of the rest of the chunk before it parks: one small allocation,
        // once, and the tail no longer points into quinn's buffer.
        let ((), c) = measure(|| {
            chunks.detach();
            chunks.detach();
        });
        assert_eq!(c.allocs, 1, "len {len}: {c:?}");
        assert_eq!(c.bytes, 4, "len {len}: only the 4 bytes left");
    }
}

/// A payload that spans chunks has to be put together once: one allocation of its size, filled
/// from the chunks, not zeroed first.
#[test]
fn a_quic_payload_across_chunks_is_one_allocation_without_a_zero_fill() {
    let len = MAX_DATA_PAYLOAD;
    let body: Vec<u8> = (0..len).map(|i| (i % 251) as u8).collect();
    let pieces: Vec<Bytes> = body.chunks(1200).map(datagram_slice).collect();
    let mut chunks = Chunks::default();
    let (payload, c) = measure(|| {
        chunks.start_payload(len);
        let mut pieces = pieces.into_iter();
        loop {
            if let Some(p) = chunks.poll_payload() {
                break p;
            }
            assert!(chunks.missing() > 0);
            chunks.push(pieces.next().unwrap());
        }
    });
    assert_eq!(payload[..], body[..]);
    assert_eq!(c.allocs, 1, "{c:?}");
    assert_eq!(c.zeroed, 0, "the payload buffer was zero-filled: {c:?}");
    assert_eq!(c.bytes as usize, len);
}

/// Through a whole `PipeLink` pair (both drivers, both queues, a stream between them), on one
/// thread so the counters see everything: a 16 KiB data frame costs one allocation for the
/// receiver's body buffer and the sender's side none per frame once its buffers have grown. (Measured
/// 1.38 per frame: the rest is the queues' and the runtime's own bookkeeping, not frame data.)
#[tokio::test(flavor = "current_thread")]
async fn a_pipe_link_pair_moves_a_16_kib_frame_for_about_one_allocation() {
    use fil_proto::preamble;
    use fil_transport::link::{Link, LinkParams};
    use fil_transport::pipe::{PipeConfig, PipeLink};
    use tokio::io::{AsyncWriteExt, duplex};

    let (a, mut b) = duplex(1 << 20);
    let cfg = || PipeConfig::new(LinkParams::default());
    let (client, daemon) = tokio::join!(
        async { PipeLink::connect(a, cfg()).await.unwrap() },
        async {
            b.write_all(&preamble::encode(1)).await.unwrap();
            PipeLink::accept(b, cfg()).await.unwrap()
        }
    );
    let channel = ChannelId(16);
    let payloads = |range: std::ops::Range<u64>| -> Vec<Bytes> {
        range
            .map(|i| {
                let seg = DataSegment::encode(i * 16 * 1024, &[5u8; MAX_DATA_PAYLOAD]).unwrap();
                Bytes::from(seg)
            })
            .collect()
    };
    let run = |payloads: Vec<Bytes>| {
        let (client, daemon) = (&client, &daemon);
        async move {
            let n = payloads.len();
            let send = async {
                // (Moved into `send`, not cloned: a `Bytes` made from a `Vec` allocates a shared
                // header on its first clone.)
                for p in payloads {
                    client.send(channel, p).await.unwrap();
                }
            };
            let recv = async {
                for _ in 0..n {
                    let (ch, p) = daemon.recv().await.unwrap();
                    assert_eq!(ch, channel);
                    assert!(p.len() > MAX_DATA_PAYLOAD);
                }
            };
            tokio::join!(send, recv);
        }
    };
    // Warm up: the buffers and queues grow to their working size.
    run(payloads(0..100)).await;
    let measured = payloads(100..300);
    let before = COUNTS.with(Cell::get);
    run(measured).await;
    let after = COUNTS.with(Cell::get);
    let frames = 200u64;
    let allocs = after.allocs - before.allocs;
    let zeroed = after.zeroed - before.zeroed;
    let bytes = after.bytes - before.bytes;
    eprintln!(
        "pipe pair, per 16 KiB frame: {:.2} allocations, {} zero-filled, {} bytes",
        allocs as f64 / frames as f64,
        zeroed,
        bytes / frames
    );
    assert_eq!(zeroed, 0);
    assert!(
        allocs * 2 <= frames * 3,
        "{allocs} allocations for {frames} frames"
    );
    // One body buffer per frame and not a byte more: no second copy of the payload anywhere.
    assert!(
        bytes / frames <= MAX_DATA_PAYLOAD as u64 + 512,
        "{} bytes per frame",
        bytes / frames
    );
}
