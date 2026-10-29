//! Conversion between the hub's floats and the formats devices take.

use mare_hub::AudioBlock;
use mare_wasapi::format::{SampleFormat, deinterleave, interleave};

const ALL: [SampleFormat; 5] = [
    SampleFormat::F32,
    SampleFormat::I16,
    SampleFormat::I24,
    SampleFormat::I24In32,
    SampleFormat::I32,
];

fn convert(format: SampleFormat, samples: &[f32]) -> Vec<u8> {
    let mut data = samples.to_vec();
    let frames = data.len();
    let block = AudioBlock::new(&mut data, 1, frames, frames);
    let mut out = vec![0xAA; frames * format.bytes()];
    interleave(&block, format, 1, &mut out);
    out
}

#[test]
fn full_scale_and_silence_have_the_values_the_formats_define() {
    assert_eq!(
        convert(SampleFormat::I16, &[1.0, -1.0, 0.0]),
        [0xFF, 0x7F, 0x01, 0x80, 0, 0]
    );
    assert_eq!(
        convert(SampleFormat::I24, &[1.0, -1.0, 0.0]),
        [0xFF, 0xFF, 0x7F, 0x01, 0x00, 0x80, 0, 0, 0]
    );
    // 24 bits in the top three bytes of four.
    assert_eq!(
        convert(SampleFormat::I24In32, &[1.0]),
        [0x00, 0xFF, 0xFF, 0x7F]
    );
    assert_eq!(
        convert(SampleFormat::I32, &[-1.0]),
        [0x01, 0x00, 0x00, 0x80]
    );
    assert_eq!(convert(SampleFormat::F32, &[0.5]), 0.5f32.to_le_bytes());
    for format in ALL {
        assert!(
            convert(format, &[0.0]).iter().all(|&b| b == 0),
            "{format:?}"
        );
    }
}

#[test]
fn out_of_range_samples_are_clamped_and_nan_is_silence() {
    for format in [
        SampleFormat::I16,
        SampleFormat::I24,
        SampleFormat::I24In32,
        SampleFormat::I32,
    ] {
        assert_eq!(
            convert(format, &[7.5]),
            convert(format, &[1.0]),
            "{format:?}"
        );
        assert_eq!(
            convert(format, &[-9.0]),
            convert(format, &[-1.0]),
            "{format:?}"
        );
        assert_eq!(
            convert(format, &[f32::NAN]),
            convert(format, &[0.0]),
            "{format:?}"
        );
    }
}

#[test]
fn a_round_trip_changes_a_sample_by_at_most_half_a_step() {
    let samples: Vec<f32> = (0..2001).map(|i| (i as f32 - 1000.0) / 1000.0).collect();
    for format in ALL {
        let bytes = convert(format, &samples);
        let mut back = vec![9.0; samples.len()];
        let frames = back.len();
        let mut block = AudioBlock::new(&mut back, 1, frames, frames);
        deinterleave(&bytes, format, 1, &mut block);
        let step = match format.valid_bits() {
            32 if format == SampleFormat::F32 => 0.0,
            bits => 1.0 / ((1u64 << (bits - 1)) - 1) as f64,
        };
        for (x, y) in samples.iter().zip(&back) {
            let error = (f64::from(*x) - f64::from(*y)).abs();
            // A float's own rounding adds a little on 32-bit integers.
            assert!(error <= step / 2.0 + 1e-7, "{format:?}: {x} became {y}");
        }
    }
}

#[test]
fn channels_interleave_and_extra_device_channels_are_silent() {
    // Two hub channels into a four-channel device.
    let mut data = vec![0.0; 2 * 8];
    let mut block = AudioBlock::new(&mut data, 2, 8, 3);
    block.channel_mut(0).copy_from_slice(&[0.25, 0.5, 0.75]);
    block.channel_mut(1).copy_from_slice(&[-0.25, -0.5, -0.75]);
    let mut out = vec![0xAA; 3 * 4 * 4];
    interleave(&block, SampleFormat::F32, 4, &mut out);
    let samples: Vec<f32> = out
        .chunks(4)
        .map(|b| f32::from_le_bytes(b.try_into().unwrap()))
        .collect();
    assert_eq!(
        samples,
        [
            0.25, -0.25, 0.0, 0.0, 0.5, -0.5, 0.0, 0.0, 0.75, -0.75, 0.0, 0.0
        ]
    );

    // And back, into a block with a channel more than the device has.
    let mut data = vec![9.0; 5 * 3];
    let mut block = AudioBlock::new(&mut data, 5, 3, 3);
    deinterleave(&out, SampleFormat::F32, 4, &mut block);
    assert_eq!(block.channel(0), [0.25, 0.5, 0.75]);
    assert_eq!(block.channel(1), [-0.25, -0.5, -0.75]);
    for channel in 2..5 {
        assert_eq!(block.channel(channel), [0.0, 0.0, 0.0]);
    }
}
