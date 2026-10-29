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
    stats: CaptureStats,
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
            stats: CaptureStats::default(),
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
    }
}
