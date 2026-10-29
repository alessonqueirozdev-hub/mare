//! The event rings: order, wrap-around, a full ring, a corrupt one, and a
//! producer and a consumer on two threads.

use std::ptr::NonNull;
use std::sync::atomic::Ordering;

use mare_protocol::ring::{
    RingConsumer, RingCorrupt, RingProducer, RingView, UmpEvent, ring_bytes, valid_capacity,
};
use mare_protocol::ump::packet_words;

/// 64 bytes, 64-byte aligned, like a block of a mapping.
#[repr(C, align(64))]
#[derive(Clone, Copy)]
struct Block([u8; 64]);

struct Memory(Vec<Block>);

impl Memory {
    fn ring(capacity: u32) -> (Self, RingView) {
        let mut memory = Self(vec![
            Block([0; 64]);
            ring_bytes(capacity).div_ceil(64) as usize
        ]);
        let base = NonNull::new(memory.0.as_mut_ptr().cast()).unwrap();
        // SAFETY: the memory is aligned, large enough, and kept by the caller
        // for as long as the view.
        let view = unsafe { RingView::from_raw(base, capacity) };
        (memory, view)
    }
}

fn event(n: u32) -> UmpEvent {
    UmpEvent {
        time: u64::from(n) * 10,
        words: [n, 0, 0, 0],
    }
}

#[test]
fn events_come_out_in_order_across_the_wrap() {
    let (_memory, ring) = Memory::ring(4);
    // SAFETY: one producer and one consumer, on this thread.
    let (mut tx, mut rx) = unsafe { (RingProducer::new(ring), RingConsumer::new(ring)) };
    // Start near the end of the index space, so the indices wrap too.
    ring.header().head.store(u32::MAX - 1, Ordering::Relaxed);
    ring.header().tail.store(u32::MAX - 1, Ordering::Relaxed);
    let mut next_out = 0;
    for n in 0..20 {
        tx.push(event(n)).unwrap();
        if n % 3 == 2 {
            while let Some(e) = rx.pop().unwrap() {
                assert_eq!(e, event(next_out));
                next_out += 1;
            }
        }
    }
    while let Some(e) = rx.pop().unwrap() {
        assert_eq!(e, event(next_out));
        next_out += 1;
    }
    assert_eq!(next_out, 20);
}

#[test]
fn a_full_ring_hands_the_event_back() {
    let (_memory, ring) = Memory::ring(2);
    // SAFETY: one producer and one consumer, on this thread.
    let (mut tx, mut rx) = unsafe { (RingProducer::new(ring), RingConsumer::new(ring)) };
    tx.push(event(1)).unwrap();
    tx.push(event(2)).unwrap();
    assert_eq!(tx.free(), 0);
    assert_eq!(tx.push(event(3)), Err(event(3)));
    assert_eq!(rx.pop(), Ok(Some(event(1))));
    tx.push(event(3)).unwrap();
    assert_eq!(rx.pop(), Ok(Some(event(2))));
    assert_eq!(rx.pop(), Ok(Some(event(3))));
    assert_eq!(rx.pop(), Ok(None));
}

#[test]
fn indices_further_apart_than_the_capacity_are_corruption_not_reads() {
    let (_memory, ring) = Memory::ring(8);
    // SAFETY: one producer and one consumer, on this thread.
    let (mut tx, mut rx) = unsafe { (RingProducer::new(ring), RingConsumer::new(ring)) };
    // A hostile producer claims a million events.
    ring.header().head.store(1_000_000, Ordering::Release);
    assert_eq!(rx.pop(), Err(RingCorrupt));
    assert_eq!(ring.len(), Err(RingCorrupt));
    // A producer facing a corrupted tail refuses rather than overwriting.
    assert!(tx.push(event(0)).is_err());
    rx.discard_all();
    assert_eq!(rx.pop(), Ok(None));
    tx.push(event(9)).unwrap();
    assert_eq!(rx.pop(), Ok(Some(event(9))));
}

#[test]
fn a_producer_and_a_consumer_on_two_threads_lose_nothing() {
    let (memory, ring) = Memory::ring(64);
    const COUNT: u32 = 200_000;
    // SAFETY: the producer moves to the other thread; the consumer stays.
    let (mut tx, mut rx) = unsafe { (RingProducer::new(ring), RingConsumer::new(ring)) };
    let producer = std::thread::spawn(move || {
        let mut n = 0;
        while n < COUNT {
            if tx.push(event(n)).is_ok() {
                n += 1;
            } else {
                std::hint::spin_loop();
            }
        }
    });
    let mut expected = 0;
    while expected < COUNT {
        if let Some(e) = rx.pop().unwrap() {
            assert_eq!(e, event(expected));
            expected += 1;
        } else {
            std::hint::spin_loop();
        }
    }
    producer.join().unwrap();
    drop(memory);
}

#[test]
fn capacities_are_powers_of_two_up_to_the_limit() {
    assert!(valid_capacity(0));
    assert!(valid_capacity(1));
    assert!(valid_capacity(1 << 16));
    assert!(!valid_capacity(3));
    assert!(!valid_capacity(1 << 17));
}

#[test]
fn packet_lengths_follow_the_message_type() {
    let words = |mt: u32| packet_words(mt << 28);
    let expected = [1, 1, 1, 2, 2, 4, 1, 1, 2, 2, 2, 3, 3, 4, 4, 4];
    for (mt, len) in expected.into_iter().enumerate() {
        assert_eq!(words(mt as u32), len, "message type {mt:X}");
    }
    // A MIDI 2.0 note on is two words; a MIDI 1.0 one in UMP is one.
    assert_eq!(packet_words(0x4090_3C00), 2);
    assert_eq!(packet_words(0x2090_3C64), 1);
}
