//! Cuts one side of the call into speech segments as the audio arrives, each
//! with the time it started.
//!
//! `audio_toolkit::chunking` cuts long audio into ~30 s blocks for accuracy
//! and keeps no timestamps. A conversation needs a cut at every pause, so two
//! speakers' turns can be put in order. The speech scorer and the thresholds
//! are the same as chunking's.

use log::warn;

use crate::audio_toolkit::constants::WHISPER_SAMPLE_RATE;
use crate::audio_toolkit::SpeechScorer;

const RATE: usize = WHISPER_SAMPLE_RATE as usize;
/// Speech starts at this probability and lasts until it falls below
/// `NEG_THRESHOLD`.
const THRESHOLD: f32 = 0.5;
const NEG_THRESHOLD: f32 = THRESHOLD - 0.15;
/// A pause this long ends a segment.
const MIN_SILENCE_SAMPLES: usize = 600 * RATE / 1000;
/// Anything with less voiced audio than this is a click or a cough.
const MIN_SPEECH_SAMPLES: usize = 250 * RATE / 1000;
/// Quiet audio kept on each side of the speech, so words aren't clipped.
const PAD_SAMPLES: usize = 100 * RATE / 1000;
/// Models lose accuracy past 30-40 s in one run, so a monologue is cut here.
const MAX_SEGMENT_SAMPLES: usize = 30 * RATE;

/// A stretch of speech. `start` is in samples from the start of the meeting.
#[derive(Debug, PartialEq)]
pub struct SpeechSegment {
    pub start: usize,
    pub audio: Vec<f32>,
}

impl SpeechSegment {
    pub fn end(&self) -> usize {
        self.start + self.audio.len()
    }
}

struct Open {
    start: usize,
    audio: Vec<f32>,
    voiced: usize,
    /// Samples at the end of `audio` since speech was last heard.
    silence: usize,
}

pub struct Segmenter {
    scorer: Box<dyn SpeechScorer>,
    frame_samples: usize,
    /// Set once scoring fails. Everything after is treated as speech and cut
    /// every `MAX_SEGMENT_SAMPLES`, so the audio is still transcribed.
    unscored: bool,
    /// Audio that doesn't fill a frame yet.
    carry: Vec<f32>,
    /// Samples already taken into frames.
    position: usize,
    /// The last `PAD_SAMPLES` heard while no segment is open.
    lead: Vec<f32>,
    open: Option<Open>,
}

impl Segmenter {
    /// `scorer` must be fresh: scores carry on from what it saw before.
    pub fn new(scorer: Box<dyn SpeechScorer>) -> Self {
        let frame_samples = scorer.frame_samples().max(1);
        Self {
            scorer,
            frame_samples,
            unscored: false,
            carry: Vec::new(),
            position: 0,
            lead: Vec::new(),
            open: None,
        }
    }

    /// When the segment being heard now started, if one is.
    pub fn open_start(&self) -> Option<usize> {
        self.open.as_ref().map(|open| open.start)
    }

    /// Adds audio. Returns the segments this finished, in order.
    pub fn push(&mut self, audio: &[f32]) -> Vec<SpeechSegment> {
        self.carry.extend_from_slice(audio);
        let whole = self.carry.len() / self.frame_samples * self.frame_samples;
        let frames: Vec<f32> = self.carry.drain(..whole).collect();
        let mut finished = Vec::new();
        for frame in frames.chunks(self.frame_samples) {
            let probability = self.score(frame);
            if let Some(segment) = self.step(frame, probability) {
                finished.push(segment);
            }
            self.position += frame.len();
        }
        finished
    }

    /// Ends the audio: returns the segment still open, if it holds speech.
    pub fn finish(&mut self) -> Option<SpeechSegment> {
        let open = self.open.take()?;
        (open.voiced >= MIN_SPEECH_SAMPLES).then_some(SpeechSegment {
            start: open.start,
            audio: open.audio,
        })
    }

    fn score(&mut self, frame: &[f32]) -> f32 {
        if self.unscored {
            return 1.0;
        }
        // Digital silence (padding, or an app that is playing nothing).
        if frame.iter().all(|&sample| sample == 0.0) {
            return 0.0;
        }
        match self.scorer.score(frame) {
            Ok(scores) => scores.first().copied().unwrap_or(0.0),
            Err(error) => {
                warn!("Voice detection failed; cutting the meeting audio by length: {error}");
                self.unscored = true;
                1.0
            }
        }
    }

    fn step(&mut self, frame: &[f32], probability: f32) -> Option<SpeechSegment> {
        let Some(open) = self.open.as_mut() else {
            if probability >= THRESHOLD {
                let mut audio = std::mem::take(&mut self.lead);
                let start = self.position - audio.len();
                audio.extend_from_slice(frame);
                self.open = Some(Open {
                    start,
                    audio,
                    voiced: frame.len(),
                    silence: 0,
                });
            } else {
                self.lead.extend_from_slice(frame);
                let extra = self.lead.len().saturating_sub(PAD_SAMPLES);
                self.lead.drain(..extra);
            }
            return None;
        };

        open.audio.extend_from_slice(frame);
        if probability >= NEG_THRESHOLD {
            open.voiced += frame.len();
            open.silence = 0;
        } else {
            open.silence += frame.len();
        }

        let paused = open.silence >= MIN_SILENCE_SAMPLES;
        if !paused && open.audio.len() < MAX_SEGMENT_SAMPLES {
            return None;
        }
        let mut open = self.open.take()?;
        if paused {
            // Keep a little of the pause; the rest leads into the next segment.
            let keep = open.audio.len() - open.silence + PAD_SAMPLES.min(open.silence);
            let tail = open.audio.split_off(keep);
            let lead_from = tail.len().saturating_sub(PAD_SAMPLES);
            self.lead = tail[lead_from..].to_vec();
        }
        (open.voiced >= MIN_SPEECH_SAMPLES).then_some(SpeechSegment {
            start: open.start,
            audio: open.audio,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio_toolkit::vad::{VadFrame, VoiceActivityDetector};
    use anyhow::Result;

    const FRAME: usize = 480;

    /// Calls anything louder than 0.1 speech.
    struct LoudIsSpeech;

    impl VoiceActivityDetector for LoudIsSpeech {
        fn push_frame<'a>(&'a mut self, _frame: &'a [f32]) -> Result<VadFrame<'a>> {
            Ok(VadFrame::Noise)
        }
        fn frame_samples(&self) -> usize {
            FRAME
        }
    }

    impl SpeechScorer for LoudIsSpeech {
        fn score(&mut self, frames: &[f32]) -> Result<Vec<f32>> {
            Ok(frames
                .chunks(FRAME)
                .map(|frame| if frame[0].abs() > 0.1 { 1.0 } else { 0.0 })
                .collect())
        }
    }

    fn segmenter() -> Segmenter {
        Segmenter::new(Box::new(LoudIsSpeech))
    }

    fn quiet(seconds: f32) -> Vec<f32> {
        vec![0.01; (seconds * RATE as f32) as usize]
    }

    fn speech(seconds: f32) -> Vec<f32> {
        vec![0.5; (seconds * RATE as f32) as usize]
    }

    #[test]
    fn a_turn_is_cut_at_the_pause_with_its_start_time() {
        let mut segmenter = segmenter();
        // Whole frames, so the speech starts exactly on a frame.
        let before = 60 * FRAME;
        assert!(segmenter.push(&vec![0.01; before]).is_empty());
        assert!(segmenter.push(&speech(3.0)).is_empty());
        assert_eq!(segmenter.open_start(), Some(before - PAD_SAMPLES));
        let segments = segmenter.push(&quiet(1.0));
        assert_eq!(segments.len(), 1);
        assert_eq!(segments[0].start, before - PAD_SAMPLES);
        assert_eq!(segments[0].audio.len(), 3 * RATE + 2 * PAD_SAMPLES);
        assert_eq!(segmenter.open_start(), None);
    }

    #[test]
    fn a_short_pause_does_not_split_a_turn() {
        let mut segmenter = segmenter();
        segmenter.push(&speech(1.0));
        segmenter.push(&quiet(0.3));
        segmenter.push(&speech(1.0));
        let segments = segmenter.push(&quiet(1.0));
        assert_eq!(segments.len(), 1);
        assert_eq!(segments[0].start, 0);
    }

    #[test]
    fn two_turns_come_out_as_two_segments() {
        let mut segmenter = segmenter();
        let mut audio = speech(1.0);
        audio.extend(quiet(2.0));
        audio.extend(speech(1.0));
        audio.extend(quiet(1.0));
        let segments = segmenter.push(&audio);
        assert_eq!(segments.len(), 2);
        assert_eq!(segments[0].start, 0);
        assert!(segments[1].start > 2 * RATE && segments[1].start < 3 * RATE);
    }

    #[test]
    fn a_click_is_dropped() {
        let mut segmenter = segmenter();
        segmenter.push(&speech(0.06));
        assert!(segmenter.push(&quiet(1.0)).is_empty());
    }

    #[test]
    fn a_monologue_is_cut_at_the_limit() {
        let mut segmenter = segmenter();
        let segments = segmenter.push(&speech(70.0));
        assert_eq!(segments.len(), 2);
        assert_eq!(segments[0].audio.len(), MAX_SEGMENT_SAMPLES);
        assert_eq!(segments[1].start, MAX_SEGMENT_SAMPLES);
        assert!(segmenter.finish().is_some());
    }

    #[test]
    fn digital_silence_is_never_speech() {
        let mut segmenter = segmenter();
        assert!(segmenter.push(&vec![0.0; 10 * RATE]).is_empty());
        assert!(segmenter.finish().is_none());
    }
}
