//! What a capture stream has recorded and the hub has not yet taken.
//!
//! A capture endpoint delivers packets on its own schedule, and the hub
//! takes one period of input each cycle, paced by the output. The
//! [`CaptureFifo`] sits between them: packets are pushed in as they come
//! (converted to float, still interleaved), and each cycle pops one period
//! out into the hub's channels.
//!
//! Its length is bounded both ways, and every correction is counted:
//!
//! - a cycle that finds fewer frames than it needs gets what there is, then
//!   silence (`padded` frames, in `short_cycles` cycles); what arrives late
//!   stays queued, so the queue grows by the shortfall and the next cycles
//!   have it in hand;
//! - when more than `max_backlog` frames are left after a cycle, the oldest
//!   are dropped until one period is left (`dropped`), so that the input's
//!   latency cannot grow without end when the two streams' clocks differ.
//!
//! Packets do not arrive exactly when the output asks for a period, so a
//! queue that starts empty runs short whenever a packet comes a moment late.
//! [`CaptureFifo::prime`] starts it with a cushion of silence (one period is
//! usual), which delays the input by as much and absorbs that jitter.
//!
//! The storage is allocated once, by [`CaptureFifo::new`]; nothing after
//! that allocates.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use mare_hub::AudioBlock;

use crate::format::{SampleFormat, load};

/// Counters of what a capture stream has delivered, and of the corrections.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct CaptureStats {
    /// Frames the stream delivered.
    pub frames_in: u64,
    /// Frames handed to the hub.
    pub frames_out: u64,
    /// Frames of silence handed to the hub because the stream had not
    /// delivered enough.
    pub padded: u64,
    /// Cycles that got fewer frames than they needed.
    pub short_cycles: u64,
    /// Frames dropped, oldest first, to keep the latency bounded.
    pub dropped: u64,
    /// Packets the system flagged as not following on from the one before
    /// (it lost some).
    pub discontinuities: u64,
    /// Packets whose position on the device's clock was not where the one
    /// before ended (for packets pushed with their position).
    pub position_jumps: u64,
    /// Frames of packets that started before the previous one ended: the
    /// same stretch of recording delivered again, and left out.
    pub repeated_frames: u64,
    /// Frames between the end of a packet and the start of the next: a
    /// stretch of recording never delivered, queued as silence.
    pub skipped_frames: u64,
    /// The largest packet, in frames.
    pub largest_packet: u64,
    /// Frames of packets the system flagged as silent.
    pub silent_frames: u64,
    /// Packets not flagged silent whose samples were all exactly zero.
    pub zero_packets: u64,
    /// Packets delivered, of any kind.
    pub packets: u64,
}

/// A bounded queue of recorded frames.
#[derive(Debug)]
pub struct CaptureFifo {
    data: Vec<f32>,
    channels: usize,
    capacity: usize,
    period: usize,
    max_backlog: usize,
    read: usize,
    len: usize,
    next_position: Option<u64>,
    stats: CaptureStats,
    monitor: CaptureMonitor,
}

impl CaptureFifo {
    /// A queue for `channels` channels, cycles of `period` frames, which
    /// keeps at most `max_backlog` frames between cycles.
    ///
    /// # Panics
    ///
    /// If `period` is zero or `max_backlog < period`.
    pub fn new(channels: usize, period: usize, max_backlog: usize) -> Self {
        assert!(period > 0 && max_backlog >= period);
        // Room for the backlog and one more delivery of up to a backlog's
        // worth before the next cycle takes from it.
        let capacity = 2 * max_backlog + period;
        Self {
            data: vec![0.0; capacity * channels],
            channels,
            capacity,
            period,
            max_backlog,
            read: 0,
            len: 0,
            next_position: None,
            stats: CaptureStats::default(),
            monitor: CaptureMonitor::new(),
        }
    }

    /// Frames waiting.
    pub fn len(&self) -> usize {
        self.len
    }

    /// Whether no frames are waiting.
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// The counters so far.
    pub fn stats(&self) -> CaptureStats {
        self.stats
    }

    /// A handle another thread can read the counters through while this
    /// queue is in use; they are published at the end of every
    /// [`CaptureFifo::pop`].
    pub fn monitor(&self) -> CaptureMonitor {
        self.monitor.clone()
    }

    fn push_frame(&mut self, mut sample: impl FnMut(usize) -> f32) {
        if self.len == self.capacity {
            // Full: the oldest frame makes room.
            self.read = (self.read + 1) % self.capacity;
            self.len -= 1;
            self.stats.dropped += 1;
        }
        let at = (self.read + self.len) % self.capacity * self.channels;
        for channel in 0..self.channels {
            self.data[at + channel] = sample(channel);
        }
        self.len += 1;
        self.stats.frames_in += 1;
    }

    /// Adds `frames` frames of interleaved samples in `format`.
    ///
    /// # Panics
    ///
    /// If `bytes` is shorter than the frames need.
    pub fn push(&mut self, bytes: &[u8], format: SampleFormat, frames: usize) {
        let size = format.bytes();
        let stride = size * self.channels;
        assert!(bytes.len() >= frames * stride);
        self.stats.packets += 1;
        self.stats.largest_packet = self.stats.largest_packet.max(frames as u64);
        if frames > 0 && bytes[..frames * stride].iter().all(|&b| b == 0) {
            // Zero bytes are a zero sample in every format here.
            self.stats.zero_packets += 1;
        }
        for frame in 0..frames {
            let start = frame * stride;
            self.push_frame(|channel| {
                let at = start + channel * size;
                load(format, &bytes[at..at + size])
            });
        }
    }

    /// Adds `frames` frames of silence (a packet the system flagged silent).
    pub fn push_silence(&mut self, frames: usize) {
        for _ in 0..frames {
            self.push_frame(|_| 0.0);
        }
    }

    /// Puts `frames` frames of silence ahead of everything recorded, as a
    /// cushion against packets that come a moment late. The input is delayed
    /// by as much. These frames are not counted as delivered.
    pub fn prime(&mut self, frames: usize) {
        for _ in 0..frames.min(self.capacity - self.len) {
            let at = (self.read + self.len) % self.capacity * self.channels;
            self.data[at..at + self.channels].fill(0.0);
            self.len += 1;
        }
    }

    /// Places a packet at `position` on the device's clock (its first
    /// frame's, in frames) against where the previous packet ended, and
    /// returns how many of its first frames were delivered already and must
    /// be left out. A stretch never delivered is queued as silence, so that
    /// what follows stays in time. Jumps either way are counted.
    fn align(&mut self, position: u64, frames: usize) -> usize {
        self.stats.largest_packet = self.stats.largest_packet.max(frames as u64);
        let mut repeated = 0;
        if let Some(expected) = self.next_position {
            if position < expected {
                self.stats.position_jumps += 1;
                let overlap = (expected - position).min(frames as u64);
                self.stats.repeated_frames += overlap;
                repeated = overlap as usize;
            } else if position > expected {
                self.stats.position_jumps += 1;
                let gap = position - expected;
                self.stats.skipped_frames += gap;
                // A gap longer than the queue would only be cut again.
                self.push_silence(gap.min(self.capacity as u64) as usize);
            }
        }
        let end = position + frames as u64;
        self.next_position = Some(self.next_position.map_or(end, |next| next.max(end)));
        repeated
    }

    /// Adds a packet of `frames` frames of interleaved samples in `format`,
    /// recorded at `position` on the device's clock: what it repeats of
    /// earlier packets is left out, and what was skipped before it is
    /// silence.
    ///
    /// Positions must count the same frames as the packets. A stream whose
    /// positions count something else (WASAPI in shared mode, where they
    /// count the device's frames before the engine converts the rate) must
    /// use [`CaptureFifo::push`] instead, or every packet loses frames.
    ///
    /// # Panics
    ///
    /// If `bytes` is shorter than the frames need.
    pub fn push_at(&mut self, position: u64, bytes: &[u8], format: SampleFormat, frames: usize) {
        let skip = self.align(position, frames);
        let stride = format.bytes() * self.channels;
        self.push(&bytes[skip * stride..], format, frames - skip);
    }

    /// Adds a packet of `frames` frames the system flagged silent, without a
    /// position.
    pub fn push_silent_packet(&mut self, frames: usize) {
        self.stats.packets += 1;
        self.stats.largest_packet = self.stats.largest_packet.max(frames as u64);
        self.stats.silent_frames += frames as u64;
        self.push_silence(frames);
    }

    /// Adds a packet of `frames` frames of silence recorded at `position`,
    /// placed as [`CaptureFifo::push_at`] places packets.
    pub fn push_silence_at(&mut self, position: u64, frames: usize) {
        let skip = self.align(position, frames);
        self.stats.packets += 1;
        self.stats.largest_packet = self.stats.largest_packet.max(frames as u64);
        self.stats.silent_frames += (frames - skip) as u64;
        self.push_silence(frames - skip);
    }

    /// Counts a packet that does not follow on from the one before.
    pub fn note_discontinuity(&mut self) {
        self.stats.discontinuities += 1;
    }

    /// Fills `block` with the oldest frames, then silence if there are not
    /// enough; channels past the stream's are silent. Then trims the backlog.
    pub fn pop(&mut self, block: &mut AudioBlock<'_>) {
        let frames = block.frames();
        let available = self.len.min(frames);
        for channel in 0..block.channels() {
            let out = block.channel_mut(channel);
            for (i, sample) in out.iter_mut().enumerate().take(available) {
                *sample = if channel < self.channels {
                    self.data[(self.read + i) % self.capacity * self.channels + channel]
                } else {
                    0.0
                };
            }
            out[available..].fill(0.0);
        }
        self.read = (self.read + available) % self.capacity;
        self.len -= available;
        self.stats.frames_out += available as u64;
        self.stats.padded += (frames - available) as u64;
        if available < frames {
            self.stats.short_cycles += 1;
        }
        if self.len > self.max_backlog {
            let extra = self.len - self.period;
            self.read = (self.read + extra) % self.capacity;
            self.len -= extra;
            self.stats.dropped += extra as u64;
        }
        self.monitor.publish(&self.stats, self.len);
    }
}

/// A queue's counters, readable from another thread while it runs.
///
/// Each field is published on its own with relaxed ordering, so a reading
/// taken while the queue publishes may mix two cycles' values; each value is
/// one the queue really had. Publishing is a handful of atomic stores: no
/// allocation, no lock.
#[derive(Clone, Debug, Default)]
pub struct CaptureMonitor {
    cells: Arc<Cells>,
}

#[derive(Debug, Default)]
struct Cells {
    frames_in: AtomicU64,
    frames_out: AtomicU64,
    padded: AtomicU64,
    short_cycles: AtomicU64,
    dropped: AtomicU64,
    discontinuities: AtomicU64,
    position_jumps: AtomicU64,
    repeated_frames: AtomicU64,
    skipped_frames: AtomicU64,
    largest_packet: AtomicU64,
    silent_frames: AtomicU64,
    zero_packets: AtomicU64,
    packets: AtomicU64,
    backlog: AtomicU64,
    cycles: AtomicU64,
}

impl CaptureMonitor {
    /// A monitor nothing publishes to yet.
    pub fn new() -> Self {
        Self::default()
    }

    fn publish(&self, stats: &CaptureStats, backlog: usize) {
        let c = &self.cells;
        let set = |cell: &AtomicU64, value: u64| cell.store(value, Ordering::Relaxed);
        set(&c.frames_in, stats.frames_in);
        set(&c.frames_out, stats.frames_out);
        set(&c.padded, stats.padded);
        set(&c.short_cycles, stats.short_cycles);
        set(&c.dropped, stats.dropped);
        set(&c.discontinuities, stats.discontinuities);
        set(&c.position_jumps, stats.position_jumps);
        set(&c.repeated_frames, stats.repeated_frames);
        set(&c.skipped_frames, stats.skipped_frames);
        set(&c.largest_packet, stats.largest_packet);
        set(&c.silent_frames, stats.silent_frames);
        set(&c.zero_packets, stats.zero_packets);
        set(&c.packets, stats.packets);
        set(&c.backlog, backlog as u64);
        c.cycles.fetch_add(1, Ordering::Relaxed);
    }

    /// The counters as last published.
    pub fn stats(&self) -> CaptureStats {
        let c = &self.cells;
        let get = |cell: &AtomicU64| cell.load(Ordering::Relaxed);
        CaptureStats {
            frames_in: get(&c.frames_in),
            frames_out: get(&c.frames_out),
            padded: get(&c.padded),
            short_cycles: get(&c.short_cycles),
            dropped: get(&c.dropped),
            discontinuities: get(&c.discontinuities),
            position_jumps: get(&c.position_jumps),
            repeated_frames: get(&c.repeated_frames),
            skipped_frames: get(&c.skipped_frames),
            largest_packet: get(&c.largest_packet),
            silent_frames: get(&c.silent_frames),
            zero_packets: get(&c.zero_packets),
            packets: get(&c.packets),
        }
    }

    /// Frames queued after the last cycle.
    pub fn backlog(&self) -> u64 {
        self.cells.backlog.load(Ordering::Relaxed)
    }

    /// Cycles published so far.
    pub fn cycles(&self) -> u64 {
        self.cells.cycles.load(Ordering::Relaxed)
    }
}
