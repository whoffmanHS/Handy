//! Transcribes the meeting while it records.
//!
//! One thread follows the recording file as it grows, cuts each side into
//! speech segments and runs them through the model Handy has selected. The
//! audio waiting to be transcribed stays on disk, so a slow model on a long
//! call falls behind without using more memory.

use std::collections::VecDeque;
use std::fs::File;
use std::io::{BufWriter, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use log::{info, warn};
use tauri::{AppHandle, Manager};
use tauri_specta::Event;

use super::echo;
use super::segmenter::{Segmenter, SpeechSegment};
use super::timeline::{MeetingSegment, Speaker, Timeline};
use crate::audio_toolkit::constants::WHISPER_SAMPLE_RATE;
use crate::managers::audio::create_detector;
use crate::managers::transcription::{ChunkControl, TranscriptionManager};
use crate::notices::{self, Notice, NoticeCode, NoticeSource};
use crate::settings::get_settings;

const RATE: f64 = WHISPER_SAMPLE_RATE as f64;
/// How much audio is read at a time. Small enough that a reply is shown soon
/// after it is said.
const STEP_FRAMES: u64 = WHISPER_SAMPLE_RATE as u64 * 2;
const IDLE_POLL: Duration = Duration::from_millis(200);
/// How often a waiting segment checks whether dictation has finished.
const DICTATION_POLL: Duration = Duration::from_millis(200);
const BYTES_PER_FRAME: u64 = 4;
/// How much of the call side's audio is kept for comparing with microphone
/// segments: longer than the longest segment, with room to spare.
const CALL_RECENT_SAMPLES: usize = WHISPER_SAMPLE_RATE as usize * 45;

/// What the writer has put on disk so far.
#[derive(Default)]
pub struct Progress {
    /// Stereo frames that are safely in the file.
    pub saved_frames: AtomicU64,
    /// Set once the writer has closed the file. `saved_frames` is final.
    pub finished: AtomicBool,
}

pub struct TranscriberHandle {
    thread: JoinHandle<Vec<MeetingSegment>>,
}

impl TranscriberHandle {
    /// Waits for the rest of the recording to be transcribed and returns the
    /// whole transcript in time order.
    pub fn finish(self) -> Vec<MeetingSegment> {
        self.thread.join().unwrap_or_else(|_| {
            warn!("The meeting transcriber thread panicked");
            Vec::new()
        })
    }
}

pub fn spawn(
    app: &AppHandle,
    recording: &Path,
    progress: Arc<Progress>,
) -> Result<TranscriberHandle> {
    let app = app.clone();
    let recording = recording.to_path_buf();
    let thread = thread::Builder::new()
        .name("meeting-transcriber".into())
        .spawn(move || match run(&app, &recording, &progress) {
            Ok(segments) => segments,
            Err(error) => {
                fail(&app, &format!("{error:#}"));
                Vec::new()
            }
        })
        .context("Failed to start the meeting transcriber thread")?;
    Ok(TranscriberHandle { thread })
}

fn fail(app: &AppHandle, detail: &str) {
    warn!("Meeting transcription failed: {detail}");
    notices::raise(
        app,
        Notice::new(NoticeCode::TranscriptionFailed, NoticeSource::Meeting)
            .with_detail(detail.to_string()),
    );
}

/// Lets dictation go first: a segment waits while push-to-talk is recording
/// or transcribing.
pub struct YieldToDictation<'a> {
    pub app: &'a AppHandle,
}

impl ChunkControl for YieldToDictation<'_> {
    fn proceed(&self) -> bool {
        while crate::tray::dictation_in_progress(self.app) {
            thread::sleep(DICTATION_POLL);
        }
        true
    }

    fn progress(&self, _done: usize, _total: usize) {}
}

struct Transcriber<'a> {
    app: &'a AppHandle,
    manager: Arc<TranscriptionManager>,
    timeline: Timeline,
    /// The call side's recent audio, for comparing with a microphone
    /// segment, and the sample its first entry is at.
    call_recent: VecDeque<f32>,
    call_recent_start: usize,
    /// The transcript on disk, one JSON segment per line as each is final.
    log: BufWriter<File>,
    failed: bool,
}

impl Transcriber<'_> {
    /// Keeps the call side's latest audio, up to `CALL_RECENT_SAMPLES`.
    fn remember_call(&mut self, audio: &[f32]) {
        self.call_recent.extend(audio);
        let extra = self.call_recent.len().saturating_sub(CALL_RECENT_SAMPLES);
        self.call_recent.drain(..extra);
        self.call_recent_start += extra;
    }

    /// How much of a microphone segment's speech happens while the call is
    /// sounding. 0 when the call's audio for that stretch is no longer kept.
    fn call_coverage(&self, segment: &SpeechSegment) -> f32 {
        let Some(from) = segment.start.checked_sub(self.call_recent_start) else {
            return 0.0;
        };
        let to = from + segment.audio.len();
        if to > self.call_recent.len() {
            return 0.0;
        }
        let call: Vec<f32> = self.call_recent.range(from..to).copied().collect();
        echo::coverage(&segment.audio, &call)
    }

    fn transcribe(&mut self, speaker: Speaker, segment: SpeechSegment) {
        let start = segment.start as f64 / RATE;
        let end = segment.end() as f64 / RATE;
        let call_coverage = match speaker {
            Speaker::Me => self.call_coverage(&segment),
            Speaker::Them => 0.0,
        };
        let control = YieldToDictation { app: self.app };
        let text = match self.manager.transcribe_in_chunks(segment.audio, &control) {
            Ok(Some(transcription)) => transcription.text,
            Ok(None) => return,
            Err(error) => {
                // One notice for the meeting; later segments are still tried.
                if !self.failed {
                    self.failed = true;
                    fail(self.app, &format!("{error:#}"));
                }
                return;
            }
        };
        let text = text.trim();
        if text.is_empty() {
            return;
        }
        let segment = MeetingSegment {
            speaker,
            name: speaker.name().to_string(),
            start,
            end,
            text: text.to_string(),
        };
        if let Some(segment) = self.timeline.add(segment, call_coverage) {
            self.publish(&segment);
        }
    }

    fn settle(&mut self, them_open_since: Option<usize>) {
        let since = them_open_since.map(|samples| samples as f64 / RATE);
        for segment in self.timeline.settle(since) {
            self.publish(&segment);
        }
    }

    fn publish(&mut self, segment: &MeetingSegment) {
        if let Err(error) = segment.emit(self.app) {
            warn!("Failed to emit meeting-segment: {error}");
        }
        let saved = serde_json::to_writer(&mut self.log, segment)
            .map_err(std::io::Error::from)
            .and_then(|()| self.log.write_all(b"\n"))
            .and_then(|()| self.log.flush());
        if let Err(error) = saved {
            warn!("Failed to save a transcript segment: {error}");
        }
    }
}

fn run(app: &AppHandle, recording: &Path, progress: &Progress) -> Result<Vec<MeetingSegment>> {
    let settings = get_settings(app);
    let mut me = Segmenter::new(create_detector(settings.vad_backend, app)?);
    let mut them = Segmenter::new(create_detector(settings.vad_backend, app)?);
    let mut reader = RecordingReader::open(recording)?;
    let log_path = transcript_path(recording);
    let log = File::create(&log_path)
        .with_context(|| format!("Failed to create {}", log_path.display()))?;
    let mut transcriber = Transcriber {
        app,
        manager: Arc::clone(app.state::<Arc<TranscriptionManager>>().inner()),
        timeline: Timeline::new(settings.meeting_echo_filter),
        call_recent: VecDeque::new(),
        call_recent_start: 0,
        log: BufWriter::new(log),
        failed: false,
    };

    loop {
        // Read the flag first: the count is final once it is set.
        let finished = progress.finished.load(Ordering::Acquire);
        let saved = progress.saved_frames.load(Ordering::Acquire);
        let frames = (saved - reader.position).min(STEP_FRAMES);
        if frames == 0 {
            if finished {
                break;
            }
            thread::sleep(IDLE_POLL);
            continue;
        }
        let (mine, theirs) = reader.read(frames)?;
        // Before the microphone's segments: each is compared with the call
        // audio from the same moment.
        transcriber.remember_call(&theirs);
        for segment in me.push(&mine) {
            transcriber.transcribe(Speaker::Me, segment);
        }
        for segment in them.push(&theirs) {
            transcriber.transcribe(Speaker::Them, segment);
        }
        transcriber.settle(them.open_start());
    }

    if let Some(segment) = me.finish() {
        transcriber.transcribe(Speaker::Me, segment);
    }
    if let Some(segment) = them.finish() {
        transcriber.transcribe(Speaker::Them, segment);
    }
    transcriber.settle(None);
    let segments = transcriber.timeline.into_segments();
    info!("Meeting transcribed into {} segments", segments.len());
    Ok(segments)
}

/// The transcript kept next to a recording.
pub fn transcript_path(recording: &Path) -> PathBuf {
    recording.with_extension("jsonl")
}

/// Reads the stereo 16-bit recording while the writer is still adding to it.
struct RecordingReader {
    file: File,
    /// Where the audio starts in the file.
    data_offset: u64,
    /// Frames read so far.
    position: u64,
}

impl RecordingReader {
    fn open(path: &Path) -> Result<Self> {
        let mut file =
            File::open(path).with_context(|| format!("Failed to open {}", path.display()))?;
        // The audio follows the "data" chunk's 8-byte header.
        let mut header = [0u8; 256];
        let read = file.read(&mut header)?;
        let data_offset = header[..read]
            .windows(4)
            .position(|bytes| bytes == b"data")
            .map(|at| at as u64 + 8)
            .ok_or_else(|| anyhow!("The meeting recording has no audio header yet"))?;
        Ok(Self {
            file,
            data_offset,
            position: 0,
        })
    }

    /// The next `frames` frames, as (left, right) samples.
    fn read(&mut self, frames: u64) -> Result<(Vec<f32>, Vec<f32>)> {
        let mut bytes = vec![0u8; (frames * BYTES_PER_FRAME) as usize];
        self.file.seek(SeekFrom::Start(
            self.data_offset + self.position * BYTES_PER_FRAME,
        ))?;
        self.file
            .read_exact(&mut bytes)
            .context("Failed to read the meeting recording")?;
        self.position += frames;
        let sample = |low: u8, high: u8| f32::from(i16::from_le_bytes([low, high])) / 32768.0;
        Ok(bytes
            .as_chunks::<{ BYTES_PER_FRAME as usize }>()
            .0
            .iter()
            .map(|&[l0, l1, r0, r1]| (sample(l0, l1), sample(r0, r1)))
            .unzip())
    }
}
