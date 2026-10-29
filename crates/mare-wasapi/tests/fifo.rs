//! The queue between a capture stream and the hub's cycles.

use mare_hub::AudioBlock;
use mare_wasapi::fifo::CaptureFifo;
use mare_wasapi::format::SampleFormat;

/// Two channels of f32: frame `n` is (n, -n).
fn packet(first: u32, frames: u32) -> Vec<u8> {
    (first..first + frames)
        .flat_map(|n| {
            let x = n as f32;
            [x.to_le_bytes(), (-x).to_le_bytes()]
        })
        .flatten()
        .collect()
}

/// One cycle of `frames`, as (left, right).
fn cycle(fifo: &mut CaptureFifo, frames: usize) -> (Vec<f32>, Vec<f32>) {
    let mut data = vec![7.0; 2 * frames];
    let mut block = AudioBlock::new(&mut data, 2, frames, frames);
    fifo.pop(&mut block);
    (block.channel(0).to_vec(), block.channel(1).to_vec())
}

#[test]
fn frames_come_out_in_order_and_deinterleaved() {
    let mut fifo = CaptureFifo::new(2, 4, 8);
    fifo.push(&packet(0, 4), SampleFormat::F32, 4);
    let (left, right) = cycle(&mut fifo, 4);
    assert_eq!(left, [0.0, 1.0, 2.0, 3.0]);
    assert_eq!(right, [0.0, -1.0, -2.0, -3.0]);
    let stats = fifo.stats();
    assert_eq!(
        (
            stats.frames_in,
            stats.frames_out,
            stats.padded,
            stats.dropped
        ),
        (4, 4, 0, 0)
    );
}

#[test]
fn a_short_delivery_is_padded_with_silence_and_counted() {
    let mut fifo = CaptureFifo::new(2, 4, 8);
    fifo.push(&packet(10, 3), SampleFormat::F32, 3);
    let (left, _) = cycle(&mut fifo, 4);
    assert_eq!(left, [10.0, 11.0, 12.0, 0.0]);
    let (left, _) = cycle(&mut fifo, 4);
    assert_eq!(left, [0.0; 4]);
    assert_eq!(fifo.stats().padded, 1 + 4);
}

#[test]
fn a_backlog_past_the_bound_is_cut_to_one_period_from_the_oldest() {
    let mut fifo = CaptureFifo::new(2, 4, 8);
    // Three periods arrive before the first cycle.
    fifo.push(&packet(0, 12), SampleFormat::F32, 12);
    let (left, _) = cycle(&mut fifo, 4);
    assert_eq!(left, [0.0, 1.0, 2.0, 3.0]);
    // Eight left is within the bound: nothing dropped yet.
    assert_eq!(fifo.len(), 8);
    fifo.push(&packet(12, 4), SampleFormat::F32, 4);
    let (left, _) = cycle(&mut fifo, 4);
    assert_eq!(left, [4.0, 5.0, 6.0, 7.0]);
    // Eight again; one more period over the bound is cut back to one.
    fifo.push(&packet(16, 8), SampleFormat::F32, 8);
    cycle(&mut fifo, 4);
    assert_eq!(fifo.len(), 4);
    let (left, _) = cycle(&mut fifo, 4);
    assert_eq!(left, [20.0, 21.0, 22.0, 23.0], "the newest period is kept");
    assert_eq!(fifo.stats().dropped, 8);
}

#[test]
fn clocks_that_drift_either_way_keep_the_latency_bounded() {
    for (deliver, take) in [(481u32, 480usize), (479, 480)] {
        let mut fifo = CaptureFifo::new(2, take, 2 * take);
        let mut next = 0;
        // One frame of drift a cycle crosses the bound every 480 cycles.
        for _ in 0..2_000 {
            fifo.push(&packet(next, deliver), SampleFormat::F32, deliver as usize);
            next += deliver;
            cycle(&mut fifo, take);
            assert!(fifo.len() <= 2 * take, "{deliver} against {take}");
        }
        let stats = fifo.stats();
        assert_eq!(
            stats.frames_in,
            stats.frames_out + stats.dropped + fifo.len() as u64
        );
        if deliver > take as u32 {
            assert!(stats.dropped > 0 && stats.padded == 0);
        } else {
            assert!(stats.padded > 0 && stats.dropped == 0);
        }
    }
}

#[test]
fn packets_of_any_size_wrap_around_the_storage() {
    let mut fifo = CaptureFifo::new(2, 5, 10);
    let mut next_in = 0;
    let mut next_out = 0.0f32;
    for round in 0..1000u32 {
        let size = [1, 5, 3, 7, 4][round as usize % 5];
        fifo.push(&packet(next_in, size), SampleFormat::F32, size as usize);
        next_in += size;
        if fifo.len() >= 5 {
            let (left, right) = cycle(&mut fifo, 5);
            for (l, r) in left.iter().zip(&right) {
                assert_eq!((*l, *r), (next_out, -next_out));
                next_out += 1.0;
            }
        }
    }
    assert_eq!(fifo.stats().dropped, 0);
}

#[test]
fn silent_packets_and_extra_channels_are_silence() {
    let mut fifo = CaptureFifo::new(1, 3, 6);
    fifo.push_silence(3);
    let mut data = vec![7.0; 3 * 2];
    let mut block = AudioBlock::new(&mut data, 2, 3, 3);
    fifo.pop(&mut block);
    assert_eq!(block.channel(0), [0.0; 3]);
    assert_eq!(block.channel(1), [0.0; 3]);
    fifo.note_discontinuity();
    assert_eq!(fifo.stats().discontinuities, 1);
}
