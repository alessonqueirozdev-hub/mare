//! The resampler against sines computed exactly.
//!
//! Every output frame says where it is in the input
//! ([`Resampler::position`]), so the right answer for a sine input is known
//! exactly: the sine evaluated there. The error is measured against it, at
//! fixed ratios, at a ratio that changes every frame, and for input pushed in
//! pieces of any size.

use mare_drift::Resampler;

const RATE: f64 = 48_000.0;

/// Pushes a sine of `freq` Hz through at the ratios `ratio(n)` (for output
/// frame n), returns the RMS error and the largest error against the sine at
/// each output's position, skipping the first frames, whose filter reads the
/// silence before the input.
fn error_of(freq: f64, frames: usize, ratio: impl Fn(usize) -> f64) -> (f64, f64) {
    let mut resampler = Resampler::with_defaults(1, 256);
    let omega = 2.0 * std::f64::consts::PI * freq / RATE;
    let mut next_in = 0usize;
    let mut out = [0.0f32];
    let mut sum = 0.0;
    let mut worst = 0.0f64;
    let mut counted = 0;
    for n in 0..frames {
        resampler.set_ratio(ratio(n));
        while resampler.available() == 0 {
            let chunk: Vec<f32> = (next_in..next_in + 64)
                .map(|i| (omega * i as f64).sin() as f32)
                .collect();
            assert_eq!(resampler.push(&chunk), 64);
            next_in += 64;
        }
        let at = resampler.position();
        assert_eq!(resampler.pull(&mut out), 1);
        if n >= 200 {
            let error = (f64::from(out[0]) - (omega * at).sin()).abs();
            sum += error * error;
            worst = worst.max(error);
            counted += 1;
        }
    }
    ((sum / counted as f64).sqrt(), worst)
}

fn db(x: f64) -> f64 {
    20.0 * x.log10()
}

#[test]
fn sines_come_out_where_they_should_at_fixed_ratios() {
    // Measured: −91 to −123 dB RMS, the floor of f32 arithmetic.
    for ratio in [1.0, 1.0001, 0.9997, 1.001, 0.999] {
        for (freq, limit_db) in [
            (100.0, -90.0),
            (1_000.0, -90.0),
            (10_000.0, -90.0),
            (18_000.0, -85.0),
        ] {
            let (rms, worst) = error_of(freq, 20_000, |_| ratio);
            eprintln!(
                "ratio {ratio}, {freq} Hz: rms {:.1} dB, worst {:.1} dB",
                db(rms),
                db(worst)
            );
            assert!(
                db(rms) < limit_db,
                "ratio {ratio}, {freq} Hz: {:.1} dB",
                db(rms)
            );
        }
    }
}

#[test]
fn a_ratio_that_changes_every_frame_stays_exact() {
    // The ratio sweeps ±1000 ppm, a full cycle every 2 000 frames: far faster
    // than drift ever changes.
    let sweep = |n: usize| 1.0 + 1e-3 * (2.0 * std::f64::consts::PI * n as f64 / 2_000.0).sin();
    for freq in [1_000.0, 10_000.0] {
        let (rms, worst) = error_of(freq, 20_000, sweep);
        eprintln!(
            "swept, {freq} Hz: rms {:.1} dB, worst {:.1} dB",
            db(rms),
            db(worst)
        );
        assert!(db(rms) < -90.0, "{freq} Hz: {:.1} dB", db(rms));
    }
}

#[test]
fn input_in_pieces_of_any_size_gives_the_same_output() {
    let input: Vec<f32> = (0..20_000)
        .map(|i| ((i as f32) * 0.013).sin() * 0.5 + ((i as f32) * 0.0007).cos() * 0.25)
        .collect();
    let run = |push_sizes: &[usize], pull_sizes: &[usize]| {
        let mut resampler = Resampler::with_defaults(2, 512);
        resampler.set_ratio(1.000_37);
        let stereo: Vec<f32> = input.iter().flat_map(|&x| [x, -x]).collect();
        let mut out = Vec::new();
        let (mut at, mut p, mut q) = (0usize, 0usize, 0usize);
        while at < input.len() {
            let n = push_sizes[p % push_sizes.len()].min(input.len() - at);
            p += 1;
            assert_eq!(resampler.push(&stereo[at * 2..(at + n) * 2]), n);
            at += n;
            loop {
                let want = pull_sizes[q % pull_sizes.len()];
                q += 1;
                let mut chunk = vec![0.0; want * 2];
                let available = resampler.available();
                let made = resampler.pull(&mut chunk);
                assert!(made >= want.min(available), "pull makes what was available");
                assert!(made <= want);
                out.extend_from_slice(&chunk[..made * 2]);
                if made < want {
                    break;
                }
            }
        }
        out
    };
    let whole = run(&[512], &[1_000_000]);
    for (pushes, pulls) in [
        (&[1usize, 7, 64, 3][..], &[5usize, 1, 128][..]),
        (&[512][..], &[1][..]),
        (&[13][..], &[333, 2][..]),
    ] {
        let pieces = run(pushes, pulls);
        let n = whole.len().min(pieces.len());
        assert!(n > 39_000, "both runs made output");
        assert_eq!(
            &whole[..n],
            &pieces[..n],
            "pushes {pushes:?}, pulls {pulls:?}"
        );
    }
    // The two channels stay each other's negative.
    for frame in whole.chunks(2) {
        assert_eq!(frame[0], -frame[1]);
    }
}

#[test]
fn positions_advance_by_the_ratio_and_latency_is_half_the_taps() {
    let mut resampler = Resampler::with_defaults(1, 256);
    assert_eq!(resampler.latency(), 32);
    assert_eq!(resampler.position(), 0.0);
    // No output until half the taps of input are there.
    resampler.push(&[0.0; 32]);
    assert_eq!(resampler.available(), 0);
    resampler.push(&[0.0; 1]);
    assert_eq!(resampler.available(), 1);
    resampler.set_ratio(1.25);
    resampler.push(&[0.0; 100]);
    let mut out = [0.0f32; 10];
    let made = resampler.pull(&mut out);
    assert_eq!(made, 10);
    assert!((resampler.position() - 12.5).abs() < 1e-12);
    resampler.reset();
    assert_eq!(resampler.position(), 0.0);
    assert_eq!(resampler.available(), 0);
}
