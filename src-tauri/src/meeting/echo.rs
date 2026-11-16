//! Tells the microphone's copy of the call from the user's own voice by
//! comparing the two channels' audio.
//!
//! On speakers the microphone hears the call. That copy only ever sounds
//! while the call does; the user's own voice mostly sounds while the call is
//! quiet. So the share of a microphone segment's loud moments that coincide
//! with sound on the call separates the two, and it doesn't depend on how the
//! speech model spelled anything.

use crate::audio_toolkit::constants::WHISPER_SAMPLE_RATE;

/// 10 ms.
const FRAME: usize = WHISPER_SAMPLE_RATE as usize / 100;
/// The call counts as sounding at a moment if it does within this many
/// frames either side: the room adds a short delay and a tail.
const SLACK_FRAMES: usize = 5;
/// The quietest a microphone frame can be and still count as speech.
const MIC_MIN_LEVEL: f32 = 0.0075;
/// A microphone frame is speech when it is this many times louder than the
/// segment's own quiet moments.
const MIC_OVER_FLOOR: f32 = 3.0;
/// The call is sounding above this level.
const CALL_LEVEL: f32 = 0.006;
/// Too little speech to judge.
const MIN_SPEECH_FRAMES: usize = 10;

/// A microphone segment at or above this is the call played back.
pub const ECHO_COVERAGE: f32 = 0.85;

fn levels(audio: &[f32]) -> Vec<f32> {
    audio
        .as_chunks::<FRAME>()
        .0
        .iter()
        .map(|frame| (frame.iter().map(|s| s * s).sum::<f32>() / FRAME as f32).sqrt())
        .collect()
}

/// The share (0 to 1) of the microphone's speech in `mic` that happens while
/// the call is sounding in `call`. Both cover the same stretch of time. A
/// segment with too little speech to judge scores 0.
pub fn coverage(mic: &[f32], call: &[f32]) -> f32 {
    let mic = levels(mic);
    let call = levels(call);
    let frames = mic.len().min(call.len());
    if frames == 0 {
        return 0.0;
    }
    let mut sorted = mic[..frames].to_vec();
    sorted.sort_by(f32::total_cmp);
    let floor = sorted[frames / 10];
    let speech_level = (floor * MIC_OVER_FLOOR).max(MIC_MIN_LEVEL);

    let mut speech = 0usize;
    let mut covered = 0usize;
    for (at, &level) in mic[..frames].iter().enumerate() {
        if level <= speech_level {
            continue;
        }
        speech += 1;
        let from = at.saturating_sub(SLACK_FRAMES);
        let to = (at + SLACK_FRAMES + 1).min(frames);
        if call[from..to].iter().any(|&level| level > CALL_LEVEL) {
            covered += 1;
        }
    }
    if speech < MIN_SPEECH_FRAMES {
        return 0.0;
    }
    covered as f32 / speech as f32
}

#[cfg(test)]
mod tests {
    use super::*;

    const RATE: usize = WHISPER_SAMPLE_RATE as usize;

    /// `seconds` of a tone at `level`, or of faint room noise at level 0.
    fn sound(seconds: f32, level: f32) -> Vec<f32> {
        (0..(seconds * RATE as f32) as usize)
            .map(|i| {
                let wave = (i as f32 * 0.2).sin();
                if level == 0.0 {
                    wave * 0.001
                } else {
                    wave * level
                }
            })
            .collect()
    }

    fn join(parts: &[Vec<f32>]) -> Vec<f32> {
        parts.concat()
    }

    #[test]
    fn the_call_played_back_is_fully_covered() {
        // The room delays the copy by 20 ms and makes it quieter.
        let call = join(&[sound(0.5, 0.0), sound(2.0, 0.3), sound(0.5, 0.0)]);
        let mic = join(&[sound(0.52, 0.0), sound(2.0, 0.05), sound(0.48, 0.0)]);
        assert!(coverage(&mic, &call) >= ECHO_COVERAGE);
    }

    #[test]
    fn talking_while_the_call_is_quiet_is_not_covered() {
        let call = sound(3.0, 0.0);
        let mic = join(&[sound(0.5, 0.0), sound(2.0, 0.2), sound(0.5, 0.0)]);
        assert_eq!(coverage(&mic, &call), 0.0);
    }

    #[test]
    fn a_reply_that_starts_as_they_finish_is_kept() {
        // They stop a third of the way into the reply.
        let call = join(&[sound(1.0, 0.3), sound(2.0, 0.0)]);
        let mic = join(&[sound(0.4, 0.0), sound(2.2, 0.2), sound(0.4, 0.0)]);
        let covered = coverage(&mic, &call);
        assert!(covered > 0.1 && covered < ECHO_COVERAGE, "{covered}");
    }

    #[test]
    fn a_segment_with_almost_no_speech_is_not_judged() {
        let call = sound(2.0, 0.3);
        let mic = join(&[sound(1.0, 0.0), sound(0.05, 0.2), sound(0.95, 0.0)]);
        assert_eq!(coverage(&mic, &call), 0.0);
    }
}
