//! A link between two clocks: audio made on one, taken on the other.
//!
//! The producer side runs on the first clock (a device's capture thread, or
//! the hub's cycle for playback) and pushes interleaved frames into a
//! lock-free single-producer single-consumer queue, noting when each packet
//! arrived. The consumer side runs on the second clock and pulls frames at
//! its own pace through a [`Resampler`] whose ratio a [`RatioController`]
//! sets from the queue's level, measured finer than a packet: the frames
//! waiting, plus those the producer has made since its last packet, which the
//! time since that packet says (`docs/adr/0001`).
//!
//! Time is given by the caller, in nanoseconds of a clock both sides share
//! (the system's monotonic clock in a hub, a simulated one in tests). Nothing
//! after [`link`] allocates, locks or makes a system call.

use std::cell::UnsafeCell;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, AtomicU64, AtomicUsize, Ordering};

use crate::{ControllerSettings, RatioController, Resampler};

struct Shared {
    channels: usize,
    /// Capacity in frames, a power of two.
    capacity: usize,
    data: UnsafeCell<Box<[f32]>>,
    /// Frames written, and read, since the link was made (wrapping).
    head: AtomicUsize,
    tail: AtomicUsize,
    /// When the last packet arrived, and its size.
    last_push_ns: AtomicU64,
    last_push_frames: AtomicU32,
    /// Frames the producer could not push for want of room.
    overflowed: AtomicU64,
}

// SAFETY: `data` is written only by the producer, in frames between `tail`
// and `head + capacity`, and read only by the consumer, in frames between
// `tail` and `head`; the indices are published with release and read with
// acquire, so the two never touch the same frame at once.
unsafe impl Sync for Shared {}

impl Shared {
    fn len(&self) -> usize {
        self.head
            .load(Ordering::Acquire)
            .wrapping_sub(self.tail.load(Ordering::Acquire))
    }
}

/// How a [`link`] is made.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct LinkSettings {
    /// Channels of the audio.
    pub channels: usize,
    /// The producer's nominal frames per second.
    pub producer_rate: u32,
    /// The consumer's nominal frames per second. It must equal the
    /// producer's: the resampler corrects drift, not a change of rate.
    pub consumer_rate: u32,
    /// The largest packet the producer pushes, in frames.
    pub max_packet: usize,
    /// The frames the consumer pulls each time.
    pub consumer_period: usize,
    /// The level the queue is held at, in producer frames. At least the
    /// largest packet plus the consumer's period plus the resampler's
    /// latency, with room for the loop's transient.
    pub target: f64,
}

impl LinkSettings {
    /// A link whose target is the largest packet, twice the consumer's
    /// period and the resampler's latency, with half a packet to spare.
    pub fn new(channels: usize, rate: u32, max_packet: usize, consumer_period: usize) -> Self {
        Self {
            channels,
            producer_rate: rate,
            consumer_rate: rate,
            max_packet,
            consumer_period,
            target: (max_packet + max_packet / 2 + 2 * consumer_period + 32) as f64,
        }
    }
}

/// Makes a link and returns its two sides.
///
/// # Panics
///
/// If the two rates differ, or a size is zero.
pub fn link(settings: LinkSettings) -> (LinkProducer, LinkConsumer) {
    assert_eq!(
        settings.producer_rate, settings.consumer_rate,
        "a link corrects drift, not a change of rate"
    );
    assert!(settings.channels > 0 && settings.max_packet > 0 && settings.consumer_period > 0);
    // Room for the target, a packet arriving on top of it and the loop's
    // transient.
    let needed = settings.target as usize + 4 * settings.max_packet + 4 * settings.consumer_period;
    let capacity = needed.next_power_of_two();
    let shared = Arc::new(Shared {
        channels: settings.channels,
        capacity,
        data: UnsafeCell::new(vec![0.0; capacity * settings.channels].into_boxed_slice()),
        head: AtomicUsize::new(0),
        tail: AtomicUsize::new(0),
        last_push_ns: AtomicU64::new(0),
        last_push_frames: AtomicU32::new(0),
        overflowed: AtomicU64::new(0),
    });
    let controller = RatioController::new(ControllerSettings::new(
        settings.target,
        settings.consumer_period,
        settings.consumer_rate,
    ));
    let consumer = LinkConsumer {
        shared: Arc::clone(&shared),
        resampler: Resampler::with_defaults(settings.channels, capacity),
        controller,
        settings,
        primed: false,
        stats: LinkStats::default(),
    };
    (LinkProducer { shared }, consumer)
}

/// The side of a link that makes audio.
pub struct LinkProducer {
    shared: Arc<Shared>,
}

impl LinkProducer {
    /// Pushes interleaved frames that arrived at `now_ns`. Returns the frames
    /// taken; any that do not fit are dropped and counted.
    pub fn push(&mut self, input: &[f32], now_ns: u64) -> usize {
        let s = &*self.shared;
        let c = s.channels;
        let frames = input.len() / c;
        let head = s.head.load(Ordering::Relaxed);
        let free = s.capacity - head.wrapping_sub(s.tail.load(Ordering::Acquire));
        let taken = frames.min(free);
        // SAFETY: the producer alone writes, and only the free frames, which
        // the consumer does not read until `head` is published below.
        let data = unsafe { &mut *s.data.get() };
        for frame in 0..taken {
            let at = (head.wrapping_add(frame) & (s.capacity - 1)) * c;
            data[at..at + c].copy_from_slice(&input[frame * c..(frame + 1) * c]);
        }
        s.head.store(head.wrapping_add(taken), Ordering::Release);
        s.last_push_frames.store(taken as u32, Ordering::Relaxed);
        s.last_push_ns.store(now_ns, Ordering::Release);
        if taken < frames {
            s.overflowed
                .fetch_add((frames - taken) as u64, Ordering::Relaxed);
        }
        taken
    }
}

/// Counters of a link's consumer.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
#[non_exhaustive]
pub struct LinkStats {
    /// Frames handed out.
    pub frames_out: u64,
    /// Frames of silence handed out while the queue first filled.
    pub priming: u64,
    /// Frames of silence handed out because the queue ran dry after it had
    /// filled.
    pub starved: u64,
    /// Frames the producer dropped for want of room.
    pub overflowed: u64,
    /// The estimated drift of the producer's clock against the consumer's,
    /// as a fraction.
    pub drift: f64,
    /// The level last measured, in producer frames.
    pub level: f64,
}

/// The side of a link that takes audio, at its own pace.
pub struct LinkConsumer {
    shared: Arc<Shared>,
    resampler: Resampler,
    controller: RatioController,
    settings: LinkSettings,
    primed: bool,
    stats: LinkStats,
}

impl LinkConsumer {
    /// Moves what the producer has pushed into the resampler.
    fn drain(&mut self) {
        let s = &*self.shared;
        let c = s.channels;
        let tail = s.tail.load(Ordering::Relaxed);
        let available = s.head.load(Ordering::Acquire).wrapping_sub(tail);
        let frames = available.min(self.resampler.room());
        // SAFETY: the consumer alone reads, and only frames the producer has
        // published; they stay until `tail` moves past them below.
        let data = unsafe { &*s.data.get() };
        let mut moved = 0;
        while moved < frames {
            // Up to the end of the storage at a time.
            let start = tail.wrapping_add(moved) & (s.capacity - 1);
            let run = (frames - moved).min(s.capacity - start);
            self.resampler.push(&data[start * c..(start + run) * c]);
            moved += run;
        }
        s.tail.store(tail.wrapping_add(moved), Ordering::Release);
    }

    /// The level, in producer frames: what waits in the queue and the
    /// resampler, plus what the producer has made since its last packet.
    fn level(&self, now_ns: u64) -> f64 {
        let s = &*self.shared;
        let since = now_ns.saturating_sub(s.last_push_ns.load(Ordering::Acquire));
        let made = (since as f64 * 1e-9 * f64::from(self.settings.producer_rate))
            .min(f64::from(s.last_push_frames.load(Ordering::Relaxed)));
        s.len() as f64 + self.resampler.buffered() + made
    }

    /// Fills `output` (interleaved, `channels` samples a frame) at `now_ns`.
    /// Until the queue first reaches its target, and whenever it runs dry,
    /// the frames are silence, counted.
    pub fn pull(&mut self, output: &mut [f32], now_ns: u64) {
        let c = self.shared.channels;
        let frames = output.len() / c;
        self.drain();
        let level = self.level(now_ns);
        self.stats.level = level;
        self.stats.overflowed = self.shared.overflowed.load(Ordering::Relaxed);
        if !self.primed {
            if level < self.settings.target {
                output.fill(0.0);
                self.stats.priming += frames as u64;
                return;
            }
            self.primed = true;
        }
        let ratio = self.controller.update(level);
        self.resampler.set_ratio(ratio);
        self.stats.drift = self.controller.drift();
        let made = self.resampler.pull(output);
        output[made * c..].fill(0.0);
        self.stats.frames_out += made as u64;
        self.stats.starved += (frames - made) as u64;
    }

    /// The counters so far.
    pub fn stats(&self) -> LinkStats {
        self.stats
    }

    /// The position of the next frame handed out, in producer frames since
    /// the first pushed. For tests that check where audio came from.
    pub fn position(&self) -> f64 {
        self.resampler.position()
    }

    /// The latency the link adds, in producer frames: the queue's target and
    /// the resampler's delay.
    pub fn latency(&self) -> f64 {
        self.settings.target + self.resampler.latency() as f64
    }
}
