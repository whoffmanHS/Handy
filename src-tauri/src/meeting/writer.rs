//! Writes the meeting to a stereo WAV as it records: left = me (microphone),
//! right = them (call audio), both 16 kHz.
//!
//! The file's header is refreshed every couple of seconds, so a crash keeps
//! everything up to the last refresh.

use std::fs::File;
use std::io::BufWriter;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Receiver;
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use anyhow::{anyhow, Context, Result};
use hound::{SampleFormat, WavSpec, WavWriter};
use log::{info, warn};

use super::resample::MonoResampler;
use super::tap::ProcessTap;
use crate::audio_toolkit::constants::WHISPER_SAMPLE_RATE;

const TICK: Duration = Duration::from_millis(20);
const FLUSH_EVERY: Duration = Duration::from_secs(2);
/// How far one channel may run ahead before the other is filled with
/// silence, so a stalled stream can't hold the file back or shift its timing.
const MAX_LEAD: usize = WHISPER_SAMPLE_RATE as usize / 2;

type Wav = WavWriter<BufWriter<File>>;

/// How far behind the meeting clock a stream may fall before the gap is
/// filled with silence. Covers ordinary jitter in when audio arrives.
const MAX_LAG: usize = WHISPER_SAMPLE_RATE as usize / 10;

/// One side of the call, waiting to be paired with the other side.
#[derive(Default)]
struct Channel {
    /// Audio not yet written to the file.
    samples: Vec<f32>,
    /// Every frame this side has had so far, silence included.
    position: usize,
}

impl Channel {
    /// Queues audio that arrived `elapsed` into the meeting. A stream that
    /// starts late or pauses (a Bluetooth mic takes a moment; a call app
    /// sends nothing until it plays something) gets silence for the gap, so
    /// both sides stay on the meeting clock.
    fn push(&mut self, audio: &[f32], elapsed: Duration) {
        if audio.is_empty() {
            return;
        }
        let due = (elapsed.as_secs_f64() * f64::from(WHISPER_SAMPLE_RATE)) as usize;
        let gap = due.saturating_sub(self.position + audio.len());
        if gap > MAX_LAG {
            self.pad(gap);
        }
        self.samples.extend_from_slice(audio);
        self.position += audio.len();
    }

    fn pad(&mut self, frames: usize) {
        self.samples.extend(std::iter::repeat_n(0.0, frames));
        self.position += frames;
    }
}

pub struct WriterHandle {
    stop: Arc<AtomicBool>,
    thread: JoinHandle<Result<()>>,
}

impl WriterHandle {
    /// Writes out what is still queued and closes the file.
    pub fn finish(self) -> Result<()> {
        self.stop.store(true, Ordering::Release);
        self.thread
            .join()
            .map_err(|_| anyhow!("The meeting writer thread panicked"))?
    }
}

/// Starts writing `path`. `call` yields the tap's mono samples at the tap's
/// rate; `mic` yields 16 kHz mono chunks.
pub fn spawn(
    path: &Path,
    tap: ProcessTap,
    call: rtrb::Consumer<f32>,
    mic: Receiver<Vec<f32>>,
) -> Result<WriterHandle> {
    let wav = WavWriter::create(
        path,
        WavSpec {
            channels: 2,
            sample_rate: WHISPER_SAMPLE_RATE,
            bits_per_sample: 16,
            sample_format: SampleFormat::Int,
        },
    )
    .with_context(|| format!("Failed to create {}", path.display()))?;
    let resampler = MonoResampler::new(tap.sample_rate())?;
    let stop = Arc::new(AtomicBool::new(false));
    let thread = thread::Builder::new()
        .name("meeting-writer".into())
        .spawn({
            let stop = Arc::clone(&stop);
            move || run(wav, tap, call, mic, resampler, &stop)
        })
        .context("Failed to start the meeting writer thread")?;
    Ok(WriterHandle { stop, thread })
}

fn run(
    mut wav: Wav,
    mut tap: ProcessTap,
    mut call: rtrb::Consumer<f32>,
    mic: Receiver<Vec<f32>>,
    mut resampler: MonoResampler,
    stop: &AtomicBool,
) -> Result<()> {
    let started = Instant::now();
    let mut last_flush = started;
    let mut me = Channel::default();
    let mut them = Channel::default();
    let mut call_audio = Vec::new();

    loop {
        // Read the flag first: audio that arrives before it is set is still
        // collected below.
        let stopping = stop.load(Ordering::Acquire);
        let elapsed = started.elapsed();

        let waiting = call.slots();
        if waiting > 0 {
            if let Ok(chunk) = call.read_chunk(waiting) {
                let (first, second) = chunk.as_slices();
                call_audio.clear();
                resampler.push(first, &mut call_audio)?;
                resampler.push(second, &mut call_audio)?;
                chunk.commit_all();
                them.push(&call_audio, elapsed);
            }
        }
        while let Ok(samples) = mic.try_recv() {
            me.push(&samples, elapsed);
        }

        // A side that is too far ahead (or, at the end, ahead at all) is
        // written against silence instead of waiting for the other.
        let limit = if stopping { 0 } else { MAX_LEAD };
        let (me_len, them_len) = (me.samples.len(), them.samples.len());
        if me_len > them_len + limit {
            them.pad(me_len - them_len);
        } else if them_len > me_len + limit {
            me.pad(them_len - me_len);
        }

        let paired = me.samples.len().min(them.samples.len());
        write_frames(&mut wav, &me.samples[..paired], &them.samples[..paired])?;
        me.samples.drain(..paired);
        them.samples.drain(..paired);

        if stopping {
            break;
        }
        if last_flush.elapsed() >= FLUSH_EVERY {
            last_flush = Instant::now();
            wav.flush().context("Failed to save the meeting audio")?;
            if let Err(error) = tap.refresh() {
                warn!("Couldn't follow the call app's audio processes: {error}");
            }
        }
        thread::sleep(TICK);
    }

    let dropped = tap.dropped();
    drop(tap);
    wav.finalize()
        .context("Failed to finish the meeting audio")?;
    if dropped > 0 {
        warn!("Meeting recording lost {dropped} call audio samples");
    }
    info!("Meeting audio saved after {:?}", started.elapsed());
    Ok(())
}

/// Writes interleaved frames from two sides of equal length.
fn write_frames(wav: &mut Wav, me: &[f32], them: &[f32]) -> Result<()> {
    for (&me, &them) in me.iter().zip(them) {
        for sample in [me, them] {
            wav.write_sample((sample.clamp(-1.0, 1.0) * f32::from(i16::MAX)) as i16)
                .context("Failed to write the meeting audio")?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_late_stream_gets_silence_in_front() {
        let mut channel = Channel::default();
        channel.push(&[1.0; 480], Duration::from_millis(250));
        assert_eq!(channel.samples.len(), 4_000);
        assert_eq!(channel.samples[3_519], 0.0);
        assert_eq!(channel.samples[3_520], 1.0);
    }

    #[test]
    fn steady_audio_is_left_alone() {
        let mut channel = Channel::default();
        for chunk in 1..=100u64 {
            // 30 ms chunks arriving up to 50 ms late.
            channel.push(&[1.0; 480], Duration::from_millis(chunk * 30 + 50));
        }
        assert_eq!(channel.samples, vec![1.0; 48_000]);
    }

    #[test]
    fn a_stream_that_pauses_resumes_on_the_clock() {
        let mut channel = Channel::default();
        channel.push(&[1.0; 16_000], Duration::from_secs(1));
        // Silence already written for it while it was paused.
        channel.pad(16_000 * 10);
        channel.push(&[1.0; 16_000], Duration::from_secs(40));
        assert_eq!(channel.position, 16_000 * 40);
        assert_eq!(channel.samples[16_000 * 39 - 1], 0.0);
        assert_eq!(channel.samples[16_000 * 39], 1.0);
    }
}
