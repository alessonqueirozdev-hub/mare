//! A resampler whose ratio may change at every output frame.
//!
//! The filter is a windowed sinc (Kaiser window), stored as a table of
//! `phases + 1` sets of `taps` coefficients for fractional positions
//! 0, 1/phases, …, 1; an output frame at a position between two table entries
//! uses coefficients interpolated linearly between them. Each set is
//! normalised to a gain of exactly 1 at DC.
//!
//! Positions are counted in input frames. The output frame at input position
//! *p* is the band-limited input evaluated at *p*; consecutive output frames
//! are `ratio` input frames apart, so a ratio above 1 consumes input faster
//! than it produces output. An output frame needs `taps / 2` input frames
//! after its position, which is the resampler's latency.

/// A variable-ratio resampler of interleaved audio.
#[derive(Debug, Clone)]
pub struct Resampler {
    channels: usize,
    taps: usize,
    phases: usize,
    table: Vec<f32>,
    scratch: Vec<f32>,
    /// Interleaved input not yet consumed, `capacity` frames of room.
    buffer: Vec<f32>,
    capacity: usize,
    len: usize,
    /// The next output frame's position, in frames from the buffer's start:
    /// a whole frame and a fraction, kept apart so that moving the buffer
    /// (which changes the whole part) never rounds the fraction.
    index: usize,
    frac: f64,
    /// Input frames discarded from the buffer's start so far.
    discarded: u64,
    ratio: f64,
}

/// The zeroth-order modified Bessel function of the first kind, by its
/// series, for the Kaiser window.
fn bessel_i0(x: f64) -> f64 {
    let mut sum = 1.0;
    let mut term = 1.0;
    let half = x / 2.0;
    for k in 1..64 {
        term *= half / k as f64;
        let add = term * term;
        sum += add;
        if add < sum * 1e-17 {
            break;
        }
    }
    sum
}

impl Resampler {
    /// A resampler for `channels` channels, with a filter of `taps`
    /// coefficients (even, 8 or more) in `phases` steps, that accepts up to
    /// `max_push` frames at a time.
    ///
    /// The filter passes up to `cutoff` (a fraction of the input's Nyquist
    /// frequency, below 1) and its Kaiser window has shape `beta`; 64 taps,
    /// cutoff 0.9 and beta 8.6 keep aliasing near −86 dB.
    ///
    /// # Panics
    ///
    /// If `taps` is odd or below 8, `phases` or `channels` is zero, or
    /// `cutoff` is not between 0 and 1.
    pub fn new(
        channels: usize,
        taps: usize,
        phases: usize,
        cutoff: f64,
        beta: f64,
        max_push: usize,
    ) -> Self {
        assert!(
            taps >= 8 && taps % 2 == 0,
            "taps must be even and at least 8"
        );
        assert!(phases > 0 && channels > 0);
        assert!(cutoff > 0.0 && cutoff < 1.0);
        let half = taps / 2;
        let mut table = vec![0.0f32; (phases + 1) * taps];
        let norm = bessel_i0(beta);
        for phase in 0..=phases {
            let frac = phase as f64 / phases as f64;
            let row = &mut table[phase * taps..(phase + 1) * taps];
            let mut values = [0.0f64; 512];
            assert!(taps <= values.len(), "at most 512 taps");
            let mut sum = 0.0;
            for (k, value) in values.iter_mut().take(taps).enumerate() {
                // Distance from this tap's input frame to the output position.
                let t = (k as f64 - (half as f64 - 1.0)) - frac;
                let x = std::f64::consts::PI * cutoff * t;
                let sinc = if t.abs() < 1e-12 { 1.0 } else { x.sin() / x };
                let w = t / half as f64;
                let window = if w.abs() >= 1.0 {
                    0.0
                } else {
                    bessel_i0(beta * (1.0 - w * w).sqrt()) / norm
                };
                *value = sinc * window;
                sum += *value;
            }
            for (out, value) in row.iter_mut().zip(values.iter()) {
                *out = (*value / sum) as f32;
            }
        }
        let capacity = max_push + taps + 2;
        let mut resampler = Self {
            channels,
            taps,
            phases,
            table,
            scratch: vec![0.0; taps],
            buffer: vec![0.0; capacity * channels],
            capacity,
            len: 0,
            index: 0,
            frac: 0.0,
            discarded: 0,
            ratio: 1.0,
        };
        resampler.reset();
        resampler
    }

    /// A resampler with the usual filter: 64 taps, 256 phases, cutoff 0.9,
    /// beta 8.6.
    pub fn with_defaults(channels: usize, max_push: usize) -> Self {
        Self::new(channels, 64, 256, 0.9, 8.6, max_push)
    }

    /// Forgets all input, as if just made; the first output frame will be the
    /// first input frame pushed after this.
    pub fn reset(&mut self) {
        // Silence before the first frame, so the first output frame has the
        // history its filter reads.
        let lead = self.taps / 2 - 1;
        self.buffer[..lead * self.channels].fill(0.0);
        self.len = lead;
        self.index = lead;
        self.frac = 0.0;
        self.discarded = 0;
    }

    /// Channels.
    pub fn channels(&self) -> usize {
        self.channels
    }

    /// Input frames consumed per output frame.
    pub fn ratio(&self) -> f64 {
        self.ratio
    }

    /// Sets the input frames consumed per output frame, from the next output
    /// frame on.
    pub fn set_ratio(&mut self, ratio: f64) {
        assert!(ratio > 0.0 && ratio.is_finite());
        self.ratio = ratio;
    }

    /// The delay the filter adds, in input frames.
    pub fn latency(&self) -> usize {
        self.taps / 2
    }

    /// The next output frame's position, in input frames since the first
    /// frame pushed after [`Resampler::reset`].
    pub fn position(&self) -> f64 {
        (self.discarded + (self.index - (self.taps / 2 - 1)) as u64) as f64 + self.frac
    }

    /// Input frames held and not yet consumed past the next output's
    /// position.
    pub fn buffered(&self) -> f64 {
        (self.len - self.index) as f64 - self.frac
    }

    /// Frames of room for input.
    pub fn room(&self) -> usize {
        // Room after the buffer is compacted.
        let keep_from = self.index - (self.taps / 2 - 1);
        self.capacity - (self.len - keep_from)
    }

    fn compact(&mut self) {
        let keep_from = self.index - (self.taps / 2 - 1);
        if keep_from == 0 {
            return;
        }
        let c = self.channels;
        self.buffer.copy_within(keep_from * c..self.len * c, 0);
        self.len -= keep_from;
        self.index -= keep_from;
        self.discarded += keep_from as u64;
    }

    /// Adds interleaved input, `channels` samples a frame. Returns how many
    /// frames were taken: all of them, unless there was not room.
    pub fn push(&mut self, input: &[f32]) -> usize {
        self.compact();
        let c = self.channels;
        let frames = (input.len() / c).min(self.capacity - self.len);
        self.buffer[self.len * c..(self.len + frames) * c].copy_from_slice(&input[..frames * c]);
        self.len += frames;
        frames
    }

    /// Output frames that can surely be made now, at the current ratio:
    /// [`Resampler::pull`] makes at least this many. (It may make one more,
    /// where the rounding of positions summed frame by frame falls on the
    /// right side of the last input frame.)
    pub fn available(&self) -> usize {
        let half = self.taps / 2;
        // An output at position p needs input up to floor(p) + taps/2.
        if self.index + half >= self.len {
            return 0;
        }
        let room = (self.len - 1 - half - self.index) as f64 - self.frac;
        // The next frame always fits; the rest are counted a hair short, so
        // rounding never promises one too many.
        ((room / self.ratio) - 1e-9).floor().max(0.0) as usize + 1
    }

    /// Writes up to `output.len() / channels` interleaved output frames and
    /// returns how many: fewer if the input runs out.
    pub fn pull(&mut self, output: &mut [f32]) -> usize {
        let c = self.channels;
        let half = self.taps / 2;
        let wanted = output.len() / c;
        let mut made = 0;
        while made < wanted {
            let i0 = self.index;
            if i0 + half >= self.len {
                break;
            }
            let phase = self.frac * self.phases as f64;
            let j = (phase.floor() as usize).min(self.phases - 1);
            let t = (phase - j as f64) as f32;
            let a = &self.table[j * self.taps..(j + 1) * self.taps];
            let b = &self.table[(j + 1) * self.taps..(j + 2) * self.taps];
            for ((s, &x), &y) in self.scratch.iter_mut().zip(a).zip(b) {
                *s = x + t * (y - x);
            }
            let first = i0 + 1 - half;
            for ch in 0..c {
                let mut acc = 0.0f32;
                for (k, &h) in self.scratch.iter().enumerate() {
                    acc += h * self.buffer[(first + k) * c + ch];
                }
                output[made * c + ch] = acc;
            }
            self.frac += self.ratio;
            let whole = self.frac.floor();
            self.index += whole as usize;
            self.frac -= whole;
            made += 1;
        }
        made
    }
}
