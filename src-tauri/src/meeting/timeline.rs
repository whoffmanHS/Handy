//! The meeting's transcript: both sides merged into one list by start time,
//! with the microphone's echo of the call left out.

use serde::{Deserialize, Serialize};
use specta::Type;

use super::echo::ECHO_COVERAGE;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Type)]
pub enum Speaker {
    Me,
    Them,
}

/// One stretch of speech. Sent as the `meeting-segment` event when it is
/// transcribed. Times are seconds from the start of the meeting.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, Type, tauri_specta::Event)]
pub struct MeetingSegment {
    /// Which side of the call it was heard on.
    pub speaker: Speaker,
    /// Who to show it as: "Me", "Them", or "Speaker 1" and so on once the
    /// other side's voices have been told apart.
    pub name: String,
    pub start: f64,
    pub end: f64,
    pub text: String,
}

impl Speaker {
    /// The name a side goes by before its voices are told apart.
    pub fn name(self) -> &'static str {
        match self {
            Self::Me => "Me",
            Self::Them => "Them",
        }
    }
}

/// How far the microphone's copy of the call may sit from the call itself.
const ECHO_SLACK_SECS: f64 = 1.5;
/// The share of a microphone segment's words that must also be in the call,
/// in the same order, for the whole segment to count as echo.
const ECHO_MATCH: f64 = 0.6;
/// The same share for a run of words at the start or end of a segment: the
/// call's last words followed straight away by a reply, or the reverse.
const ECHO_EDGE_MATCH: f64 = 0.75;
/// An edge shorter than this is left alone.
const ECHO_EDGE_MIN_WORDS: usize = 4;
/// Two words this alike are the same word heard twice. The microphone's copy
/// comes through the room, so the model spells it a little differently.
const SAME_WORD: f64 = 0.6;

struct Undecided {
    segment: MeetingSegment,
    /// How much of its speech happens while the call is sounding
    /// ([`echo::coverage`](super::echo::coverage)).
    call_coverage: f32,
}

pub struct Timeline {
    echo_filter: bool,
    /// Sorted by start time.
    segments: Vec<MeetingSegment>,
    /// Microphone segments waiting for the call side to catch up with them.
    undecided: Vec<Undecided>,
}

impl Timeline {
    pub fn new(echo_filter: bool) -> Self {
        Self {
            echo_filter,
            segments: Vec::new(),
            undecided: Vec::new(),
        }
    }

    /// Adds a transcribed segment. A `Them` segment is final at once. A `Me`
    /// segment waits for [`settle`](Self::settle): on speakers the microphone
    /// hears the call too, and the call's own words may not be in yet.
    ///
    /// `call_coverage` is how much of a `Me` segment's speech happens while
    /// the call is sounding; it is ignored for `Them`.
    pub fn add(&mut self, segment: MeetingSegment, call_coverage: f32) -> Option<MeetingSegment> {
        if segment.speaker == Speaker::Me && self.echo_filter {
            self.undecided.push(Undecided {
                segment,
                call_coverage,
            });
            return None;
        }
        self.insert(segment.clone());
        Some(segment)
    }

    /// Decides the waiting `Me` segments the call side has caught up with and
    /// returns the ones kept. `them_open_since` is when the `Them` segment
    /// still being heard started; `None` when there is none, or at the end.
    pub fn settle(&mut self, them_open_since: Option<f64>) -> Vec<MeetingSegment> {
        let (ready, waiting): (Vec<_>, Vec<_>) = std::mem::take(&mut self.undecided)
            .into_iter()
            .partition(|me| {
                them_open_since.is_none_or(|since| since >= me.segment.end + ECHO_SLACK_SECS)
            });
        self.undecided = waiting;
        let kept: Vec<MeetingSegment> = ready
            .into_iter()
            // By sound first: it doesn't depend on how either side was
            // spelled. Then by words, which also catches echo at the edges.
            .filter(|me| me.call_coverage < ECHO_COVERAGE)
            .filter_map(|me| self.without_echo(me.segment))
            .collect();
        for segment in &kept {
            self.insert(segment.clone());
        }
        kept
    }

    pub fn into_segments(self) -> Vec<MeetingSegment> {
        self.segments
    }

    fn insert(&mut self, segment: MeetingSegment) {
        let at = self.segments.partition_point(|s| s.start <= segment.start);
        self.segments.insert(at, segment);
    }

    /// `me` with the call's words taken out: `None` when it is all echo,
    /// shortened when the echo is only at its start or end.
    fn without_echo(&self, me: MeetingSegment) -> Option<MeetingSegment> {
        let mine = words(&me.text);
        if mine.is_empty() {
            return None;
        }
        let from = me.start - ECHO_SLACK_SECS;
        let to = me.end + ECHO_SLACK_SECS;
        // The call's words around the same moment. A word's time is estimated
        // from its place in its segment, so one short reply isn't matched
        // against everything said in a long stretch of the call.
        let theirs: Vec<Word> = self
            .segments
            .iter()
            .filter(|s| s.speaker == Speaker::Them && s.start < to && s.end > from)
            .flat_map(|them| {
                let all = words(&them.text);
                let count = all.len() as f64;
                let (start, length) = (them.start, them.end - them.start);
                all.into_iter().enumerate().filter_map(move |(i, word)| {
                    let at = start + (i as f64 + 0.5) / count * length;
                    (at >= from && at <= to).then_some(word)
                })
            })
            .collect();

        let echoed = echoed_words(&mine, &theirs);
        let share = |flags: &[bool]| {
            flags.iter().filter(|&&echo| echo).count() as f64 / flags.len().max(1) as f64
        };
        // The longest run from each end that is mostly the call's words.
        let edge = |flags: &[bool]| {
            (ECHO_EDGE_MIN_WORDS..=flags.len())
                .rev()
                .find(|&len| flags[len - 1] && share(&flags[..len]) >= ECHO_EDGE_MATCH)
                .unwrap_or(0)
        };
        let leading = edge(&echoed);
        let reversed: Vec<bool> = echoed[leading..].iter().rev().copied().collect();
        let trailing = edge(&reversed);
        let rest = leading..mine.len() - trailing;
        // What is left between the edges is mine unless it, too, is mostly
        // the call's words.
        if rest.is_empty() || share(&echoed[rest.clone()]) >= ECHO_MATCH {
            return None;
        }
        if leading == 0 && trailing == 0 {
            return Some(me);
        }
        let kept = &mine[rest];
        let (first, last) = (kept.first()?, kept.last()?);
        let length = me.end - me.start;
        let count = mine.len() as f64;
        Some(MeetingSegment {
            speaker: me.speaker,
            name: me.name.clone(),
            start: me.start + length * leading as f64 / count,
            end: me.end - length * trailing as f64 / count,
            text: me.text[first.at.start..last.at.end].to_string(),
        })
    }
}

/// A word in lowercase, and where it is in the text it came from.
#[derive(Clone)]
struct Word {
    text: String,
    at: std::ops::Range<usize>,
}

/// The text's words. Punctuation separates them, so "Sub-Zero" and "sub
/// zero" compare equal. A word's place runs to the end of the punctuation
/// right after it.
fn words(text: &str) -> Vec<Word> {
    let mut found: Vec<Word> = Vec::new();
    let mut start = None;
    for (i, c) in text.char_indices() {
        match (c.is_alphanumeric(), start) {
            (true, None) => start = Some(i),
            (false, Some(from)) => {
                found.push(Word {
                    text: text[from..i].to_lowercase(),
                    at: from..i,
                });
                start = None;
            }
            _ => {}
        }
        if !c.is_alphanumeric() && !c.is_whitespace() && start.is_none() {
            if let Some(last) = found.last_mut() {
                if last.at.end == i {
                    last.at.end = i + c.len_utf8();
                }
            }
        }
    }
    if let Some(from) = start {
        found.push(Word {
            text: text[from..].to_lowercase(),
            at: from..text.len(),
        });
    }
    found
}

fn same_word(a: &str, b: &str) -> bool {
    a == b || (a.len().min(b.len()) >= 4 && strsim::normalized_levenshtein(a, b) >= SAME_WORD)
}

/// For each of `mine`, whether it is part of the longest run of words both
/// lists share in order, gaps allowed.
fn echoed_words(mine: &[Word], theirs: &[Word]) -> Vec<bool> {
    let (n, m) = (mine.len(), theirs.len());
    // table[i][j]: the longest shared run of mine[i..] and theirs[j..].
    let mut table = vec![vec![0usize; m + 1]; n + 1];
    for i in (0..n).rev() {
        for j in (0..m).rev() {
            table[i][j] = if same_word(&mine[i].text, &theirs[j].text) {
                table[i + 1][j + 1] + 1
            } else {
                table[i + 1][j].max(table[i][j + 1])
            };
        }
    }
    let mut echoed = vec![false; n];
    let (mut i, mut j) = (0, 0);
    while i < n && j < m {
        if same_word(&mine[i].text, &theirs[j].text) {
            echoed[i] = true;
            i += 1;
            j += 1;
        } else if table[i + 1][j] >= table[i][j + 1] {
            i += 1;
        } else {
            j += 1;
        }
    }
    echoed
}

#[cfg(test)]
mod tests {
    use super::*;

    fn segment(speaker: Speaker, start: f64, end: f64, text: &str) -> MeetingSegment {
        MeetingSegment {
            speaker,
            name: speaker.name().to_string(),
            start,
            end,
            text: text.to_string(),
        }
    }

    #[test]
    fn the_microphones_copy_of_the_call_is_dropped() {
        let mut timeline = Timeline::new(true);
        timeline.add(
            segment(
                Speaker::Them,
                10.0,
                16.0,
                "The Sub-Zero order ships Thursday, and the ticket is ready.",
            ),
            0.0,
        );
        timeline.add(
            segment(Speaker::Me, 10.3, 13.1, "the sub zero order ships Thursday"),
            0.0,
        );
        timeline.add(
            segment(Speaker::Me, 13.4, 16.2, "and the ticket is ready"),
            0.0,
        );
        assert!(timeline.settle(None).is_empty());
        assert_eq!(timeline.into_segments().len(), 1);
    }

    #[test]
    fn talking_over_the_call_is_kept() {
        let mut timeline = Timeline::new(true);
        timeline.add(
            segment(
                Speaker::Them,
                10.0,
                16.0,
                "The order ships Thursday and the ticket is ready.",
            ),
            0.0,
        );
        timeline.add(
            segment(Speaker::Me, 12.0, 14.0, "Wait, which dealer is that for?"),
            0.0,
        );
        assert_eq!(timeline.settle(None).len(), 1);
    }

    #[test]
    fn a_short_reply_is_not_matched_against_a_whole_monologue() {
        let mut timeline = Timeline::new(true);
        let mut monologue = "okay ".to_string();
        monologue.push_str(&"we reviewed every banner size in the set ".repeat(8));
        timeline.add(segment(Speaker::Them, 0.0, 30.0, &monologue), 0.0);
        timeline.add(segment(Speaker::Me, 25.0, 25.6, "Okay."), 0.0);
        assert_eq!(timeline.settle(None).len(), 1);
    }

    #[test]
    fn a_reply_waits_until_the_call_side_has_caught_up() {
        let mut timeline = Timeline::new(true);
        timeline.add(segment(Speaker::Me, 5.0, 7.0, "ships Thursday"), 0.0);
        // The other side has been talking since 4.0 and isn't transcribed yet.
        assert!(timeline.settle(Some(4.0)).is_empty());
        timeline.add(segment(Speaker::Them, 4.0, 8.0, "It ships Thursday."), 0.0);
        assert!(timeline.settle(None).is_empty());
    }

    #[test]
    fn echo_the_model_spelled_differently_is_still_dropped() {
        let mut timeline = Timeline::new(true);
        timeline.add(segment(
            Speaker::Them,
            16.7,
            22.05,
            "Hi Will, this is the simulated caller, the sub-zero order for Klechner Ships Thursday",
        ), 0.0);
        timeline.add(
            segment(
                Speaker::Me,
                17.53,
                22.12,
                "This is the simulated call, the South Euro order for Pleckner Ship's Thursday.",
            ),
            0.0,
        );
        assert!(timeline.settle(None).is_empty());
    }

    #[test]
    fn echo_running_into_a_reply_is_cut_off_the_front() {
        let mut timeline = Timeline::new(true);
        timeline.add(segment(
            Speaker::Them,
            29.95,
            35.75,
            "Great, can you also send the thermiter banner sizes, and the Fisher and Pagel rebate details?",
        ), 0.0);
        timeline.add(segment(
            Speaker::Me,
            30.04,
            42.56,
            "Great, can you also send the thermiter banner sizes, and the Fisher and Paper rebate details I will send those by end of day. Anything else for the Black Friday set?",
        ), 0.0);
        let kept = timeline.settle(None);
        assert_eq!(kept.len(), 1);
        assert_eq!(
            kept[0].text,
            "I will send those by end of day. Anything else for the Black Friday set?"
        );
        assert!(kept[0].start > 35.0 && kept[0].start < 38.0);
        assert_eq!(kept[0].end, 42.56);
    }

    #[test]
    fn a_reply_running_into_echo_is_cut_off_the_back() {
        let mut timeline = Timeline::new(true);
        timeline.add(
            segment(
                Speaker::Them,
                12.0,
                15.0,
                "Perfect, I will send the proofs over tonight.",
            ),
            0.0,
        );
        timeline.add(
            segment(
                Speaker::Me,
                9.0,
                15.1,
                "That works for me. Perfect, I will send the proofs over tonight.",
            ),
            0.0,
        );
        let kept = timeline.settle(None);
        assert_eq!(kept[0].text, "That works for me.");
        assert_eq!(kept[0].start, 9.0);
    }

    #[test]
    fn echo_is_dropped_by_sound_even_when_the_words_differ() {
        let mut timeline = Timeline::new(true);
        timeline.add(
            segment(Speaker::Them, 80.0, 88.0, "but you can stillink."),
            0.0,
        );
        timeline.add(
            segment(Speaker::Me, 87.0, 88.4, "talk about the loom link."),
            0.97,
        );
        timeline.add(
            segment(Speaker::Me, 100.0, 102.0, "I'd agree. That fits."),
            0.0,
        );
        let kept = timeline.settle(None);
        assert_eq!(kept.len(), 1);
        assert_eq!(kept[0].text, "I'd agree. That fits.");
    }

    #[test]
    fn with_the_filter_off_everything_is_kept_in_time_order() {
        let mut timeline = Timeline::new(false);
        assert!(timeline
            .add(segment(Speaker::Me, 10.3, 13.0, "ships Thursday"), 0.0)
            .is_some());
        timeline.add(
            segment(Speaker::Them, 10.0, 13.0, "It ships Thursday."),
            0.0,
        );
        let segments = timeline.into_segments();
        assert_eq!(segments[0].speaker, Speaker::Them);
        assert_eq!(segments.len(), 2);
    }
}
