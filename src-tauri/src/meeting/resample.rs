//! Streaming mono resampler to 16 kHz for the call audio.

use anyhow::{anyhow, Result};
use rubato::audioadapter_buffers::direct::InterleavedSlice;
use rubato::{Fft, FixedSync, Indexing, Resampler};

use crate::audio_toolkit::constants::WHISPER_SAMPLE_RATE;

const CHUNK_FRAMES: usize = 1024;

pub struct MonoResampler {
    /// `None` when the input is already at 16 kHz.
    stage: Option<Stage>,
}

struct Stage {
    resampler: Fft<f32>,
    /// Input not yet resampled: always less than one step after `push`.
    pending: Vec<f32>,
    scratch: Vec<f32>,
    /// Output frames of the resampler's delay still to drop.
    delay_left: usize,
}

impl MonoResampler {
    pub fn new(rate: u32) -> Result<Self> {
        if rate == WHISPER_SAMPLE_RATE {
            return Ok(Self { stage: None });
        }
        let resampler = Fft::<f32>::new(
            rate as usize,
            WHISPER_SAMPLE_RATE as usize,
            CHUNK_FRAMES,
            1,
            FixedSync::Input,
        )
        .map_err(|e| anyhow!("Failed to set up resampling from {rate} Hz: {e}"))?;
        Ok(Self {
            stage: Some(Stage {
                pending: Vec::with_capacity(2 * resampler.input_frames_max()),
                scratch: vec![0.0; resampler.output_frames_max()],
                delay_left: resampler.output_delay(),
                resampler,
            }),
        })
    }

    /// Appends the 16 kHz version of `samples` to `output`, holding back
    /// whatever doesn't fill a resampling step yet.
    pub fn push(&mut self, samples: &[f32], output: &mut Vec<f32>) -> Result<()> {
        let Some(stage) = self.stage.as_mut() else {
            output.extend_from_slice(samples);
            return Ok(());
        };
        stage.pending.extend_from_slice(samples);
        let mut taken = 0;
        while stage.pending.len() - taken >= stage.resampler.input_frames_next() {
            taken += stage.step(taken, output)?;
        }
        stage.pending.drain(..taken);
        Ok(())
    }
}

impl Stage {
    fn step(&mut self, offset: usize, output: &mut Vec<f32>) -> Result<usize> {
        let input = InterleavedSlice::new(&self.pending[..], 1, self.pending.len())
            .map_err(|e| anyhow!("Failed to resample: {e}"))?;
        let scratch_len = self.scratch.len();
        let mut out = InterleavedSlice::new_mut(&mut self.scratch[..], 1, scratch_len)
            .map_err(|e| anyhow!("Failed to resample: {e}"))?;
        let indexing = Indexing::new().input_offset(offset);
        let (taken, produced) = self
            .resampler
            .process_into_buffer(&input, &mut out, Some(&indexing))
            .map_err(|e| anyhow!("Failed to resample: {e}"))?;
        let dropped = self.delay_left.min(produced);
        self.delay_left -= dropped;
        output.extend_from_slice(&self.scratch[dropped..produced]);
        Ok(taken)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn three_seconds_at_48k_become_three_seconds_at_16k() {
        let mut resampler = MonoResampler::new(48_000).unwrap();
        let mut output = Vec::new();
        for _ in 0..300 {
            resampler.push(&[0.25; 480], &mut output).unwrap();
        }
        // Short only by what is still waiting for a full step.
        assert!(
            (47_000..=48_000).contains(&output.len()),
            "{}",
            output.len()
        );
    }

    #[test]
    fn audio_at_16k_passes_through() {
        let mut resampler = MonoResampler::new(16_000).unwrap();
        let mut output = Vec::new();
        resampler.push(&[0.5; 100], &mut output).unwrap();
        assert_eq!(output, vec![0.5; 100]);
    }
}
