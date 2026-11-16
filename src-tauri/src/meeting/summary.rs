//! A short summary, the decisions and the action items for a finished meeting,
//! written by a language model on this Mac.
//!
//! The model's context is small, so a long transcript is read in parts and
//! the notes on each part are then combined.

use anyhow::{anyhow, bail, Result};
use log::warn;
use serde::{Deserialize, Serialize};
use specta::Type;

use super::timeline::MeetingSegment;

/// How much transcript goes to the model at once. Apple's on-device model
/// holds about 4,000 tokens for the instructions, the text and its answer.
const PART_WORDS: usize = 1100;

/// Whatever writes the text. Apple Intelligence is the only one built.
pub trait Summarizer {
    fn generate(&self, instructions: &str, text: &str) -> Result<String>;
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, Type)]
pub struct Summary {
    pub summary: Vec<String>,
    pub decisions: Vec<String>,
    pub action_items: Vec<String>,
}

impl Summary {
    pub fn is_empty(&self) -> bool {
        self.summary.is_empty() && self.decisions.is_empty() && self.action_items.is_empty()
    }

    /// The sections as they go into the note. Empty ones are left out.
    pub fn markdown(&self) -> String {
        let mut sections = Vec::new();
        for (heading, lines, mark) in [
            ("## Summary", &self.summary, "- "),
            ("## Decisions", &self.decisions, "- "),
            ("## Action items", &self.action_items, "- [ ] "),
        ] {
            if !lines.is_empty() {
                let list: Vec<String> = lines.iter().map(|line| format!("{mark}{line}")).collect();
                sections.push(format!("{heading}\n{}", list.join("\n")));
            }
        }
        sections.join("\n\n")
    }

    fn plain(&self) -> String {
        let section = |name: &str, lines: &[String]| {
            let list: Vec<String> = lines.iter().map(|line| format!("- {line}")).collect();
            format!("{name}:\n{}", list.join("\n"))
        };
        [
            section("SUMMARY", &self.summary),
            section("DECISIONS", &self.decisions),
            section("ACTIONS", &self.action_items),
        ]
        .join("\n")
    }
}

const FORMAT: &str = "Answer in exactly this format and nothing else:
SUMMARY:
- the main topics, most important first
DECISIONS:
- one bullet per decision that was made
ACTIONS:
- Name: what that person will do, with the deadline if one was said

Write at most five SUMMARY bullets, each one short sentence. Leave out small \
talk and anything that was only mentioned in passing. Start each ACTIONS \
bullet with the name of the person who will do it, as it appears in the text. \
Only include what the text actually says. If a section has nothing, write a \
single bullet that says None.";

fn part_instructions() -> String {
    format!(
        "You take notes on part of a work meeting transcript. Each line starts \
with the speaker's name.\n\n{FORMAT}"
    )
}

fn combine_instructions() -> String {
    format!(
        "You are given notes taken on consecutive parts of one work meeting. \
Merge them into one set of notes for the whole meeting. Pick the five most \
important SUMMARY points from all the parts. Remove repeats, and keep every \
distinct decision and action.\n\n{FORMAT}"
    )
}

/// What each speaker said, one line per turn.
pub fn turns(transcript: &[MeetingSegment]) -> Vec<String> {
    let mut turns: Vec<(String, String)> = Vec::new();
    for segment in transcript {
        let text = segment.text.trim();
        if text.is_empty() {
            continue;
        }
        match turns.last_mut() {
            Some((name, said)) if *name == segment.name => {
                said.push(' ');
                said.push_str(text);
            }
            _ => turns.push((segment.name.clone(), text.to_string())),
        }
    }
    turns
        .into_iter()
        .map(|(name, said)| format!("{name}: {said}"))
        .collect()
}

/// Groups lines into parts the model can hold. A single line longer than a
/// part is cut at word boundaries.
fn parts(lines: &[String]) -> Vec<String> {
    let mut parts: Vec<String> = Vec::new();
    let mut words_in_part = 0;
    for line in lines {
        let words: Vec<&str> = line.split_whitespace().collect();
        for piece in words.chunks(PART_WORDS) {
            if parts.is_empty() || words_in_part + piece.len() > PART_WORDS {
                parts.push(String::new());
                words_in_part = 0;
            }
            let part = parts.last_mut().expect("a part was just pushed");
            if !part.is_empty() {
                part.push('\n');
            }
            part.push_str(&piece.join(" "));
            words_in_part += piece.len();
        }
    }
    parts
}

pub fn summarize(model: &dyn Summarizer, lines: &[String]) -> Result<Summary> {
    let parts = parts(lines);
    if parts.is_empty() {
        bail!("There is nothing to summarize");
    }
    let mut notes: Vec<Summary> = Vec::new();
    for (at, part) in parts.iter().enumerate() {
        match model.generate(&part_instructions(), part) {
            Ok(answer) => {
                let summary = parse(&answer);
                if !summary.is_empty() {
                    notes.push(summary);
                }
            }
            // One part the model won't read shouldn't cost the whole summary.
            Err(error) => warn!(
                "Couldn't summarize part {} of {}: {error:#}",
                at + 1,
                parts.len()
            ),
        }
    }
    while notes.len() > 1 {
        let texts: Vec<String> = notes.iter().map(Summary::plain).collect();
        let groups = self::parts(&texts);
        if groups.len() >= notes.len() {
            // The notes won't shrink any further; keep them all as they are.
            return Ok(joined(notes));
        }
        let mut merged = Vec::new();
        for group in &groups {
            match model.generate(&combine_instructions(), group) {
                Ok(answer) => merged.push(parse(&answer)),
                Err(error) => {
                    warn!("Couldn't combine the meeting notes: {error:#}");
                    return Ok(joined(notes));
                }
            }
        }
        merged.retain(|summary| !summary.is_empty());
        if merged.is_empty() {
            return Ok(joined(notes));
        }
        notes = merged;
    }
    notes
        .pop()
        .ok_or_else(|| anyhow!("The model gave no summary"))
}

fn joined(notes: Vec<Summary>) -> Summary {
    let mut all = Summary::default();
    for note in notes {
        all.summary.extend(note.summary);
        all.decisions.extend(note.decisions);
        all.action_items.extend(note.action_items);
    }
    all
}

/// Reads the model's answer leniently: it adds markdown, numbers its lists
/// and renames headings when it feels like it.
fn parse(answer: &str) -> Summary {
    #[derive(Clone, Copy)]
    enum Section {
        Summary,
        Decisions,
        Actions,
    }
    let mut summary = Summary::default();
    let mut section = None;
    for line in answer.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let bare = line
            .trim_matches(|c: char| c == '#' || c == '*' || c == ':' || c.is_whitespace())
            .to_lowercase();
        let heading = if bare == "summary" {
            Some(Section::Summary)
        } else if bare == "decisions" {
            Some(Section::Decisions)
        } else if bare == "actions" || bare == "action items" {
            Some(Section::Actions)
        } else {
            None
        };
        if heading.is_some() {
            section = heading;
            continue;
        }
        let item = item(line);
        if item.is_empty() || is_nothing(&item) {
            continue;
        }
        match section {
            Some(Section::Summary) => summary.summary.push(item),
            Some(Section::Decisions) => summary.decisions.push(item),
            Some(Section::Actions) => summary.action_items.push(item),
            None => {}
        }
    }
    summary
}

/// A list line without its bullet, number or checkbox.
fn item(line: &str) -> String {
    let mut rest = line.trim_start_matches(['-', '*', '•', ' ']);
    let digits = rest.chars().take_while(char::is_ascii_digit).count();
    if digits > 0 {
        if let Some(after) = rest[digits..].strip_prefix(['.', ')']) {
            rest = after;
        }
    }
    let rest = rest.trim();
    let rest = rest
        .strip_prefix("[ ]")
        .or_else(|| rest.strip_prefix("[x]"))
        .unwrap_or(rest);
    let rest = rest.trim().replace("**", "");
    // The model sometimes copies the word from the format instead of a name.
    match rest.strip_prefix("Owner:") {
        Some(named) => named
            .trim()
            .replacen(" – ", ": ", 1)
            .replacen(" - ", ": ", 1),
        None => rest,
    }
}

fn is_nothing(item: &str) -> bool {
    let bare = item
        .trim_matches(|c: char| !c.is_alphanumeric())
        .to_lowercase();
    bare == "none"
        || bare == "n/a"
        || bare.starts_with("none ")
        || bare.starts_with("no decisions")
        || bare.starts_with("no action")
        || bare.starts_with("no further")
}

#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
pub struct AppleIntelligence;

#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
impl Summarizer for AppleIntelligence {
    fn generate(&self, instructions: &str, text: &str) -> Result<String> {
        crate::apple_intelligence::generate_text(instructions, text).map_err(|error| anyhow!(error))
    }
}

/// The model summaries are written with, when this Mac has one.
pub fn local_model() -> Option<Box<dyn Summarizer>> {
    #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
    if crate::apple_intelligence::check_apple_intelligence_availability() {
        return Some(Box::new(AppleIntelligence));
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Canned(Vec<&'static str>, std::cell::Cell<usize>);

    impl Summarizer for Canned {
        fn generate(&self, _: &str, _: &str) -> Result<String> {
            let at = self.1.get();
            self.1.set(at + 1);
            Ok(self.0[at].to_string())
        }
    }

    #[test]
    fn reads_a_messy_answer() {
        let summary = parse(
            "**Summary:**\n1. Banners went out\n\n## Decisions\n- None\n\nAction Items:\n- [ ] **Jess**: send the copy by noon\n* Kyle: update the invite",
        );
        assert_eq!(summary.summary, ["Banners went out"]);
        assert!(summary.decisions.is_empty());
        assert_eq!(
            summary.action_items,
            ["Jess: send the copy by noon", "Kyle: update the invite"]
        );
    }

    #[test]
    fn a_short_meeting_is_one_request() {
        let model = Canned(vec!["SUMMARY:\n- Short call"], Default::default());
        let summary = summarize(&model, &["Me: hello there".to_string()]).unwrap();
        assert_eq!(summary.summary, ["Short call"]);
        assert_eq!(model.1.get(), 1);
    }

    #[test]
    fn a_long_meeting_is_read_in_parts_then_combined() {
        let line = format!("Me: {}", "word ".repeat(700));
        let model = Canned(
            vec![
                "SUMMARY:\n- First half\nACTIONS:\n- Me: one",
                "SUMMARY:\n- Second half\nACTIONS:\n- Me: two",
                "SUMMARY:\n- Whole call\nACTIONS:\n- Me: one\n- Me: two",
            ],
            Default::default(),
        );
        let summary = summarize(&model, &[line.clone(), line]).unwrap();
        assert_eq!(summary.summary, ["Whole call"]);
        assert_eq!(summary.action_items, ["Me: one", "Me: two"]);
        assert_eq!(model.1.get(), 3);
    }

    #[test]
    fn one_very_long_turn_is_cut() {
        let line = "word ".repeat(PART_WORDS * 2 + 10);
        assert_eq!(parts(&[line]).len(), 3);
    }

    #[test]
    fn empty_sections_are_left_out_of_the_note() {
        let summary = Summary {
            summary: vec!["Talked".into()],
            decisions: vec![],
            action_items: vec!["Me: send it".into()],
        };
        assert_eq!(
            summary.markdown(),
            "## Summary\n- Talked\n\n## Action items\n- [ ] Me: send it"
        );
    }

    /// Run by hand against a saved note to hear what the real model says:
    /// `HANDY_SUMMARY_NOTE=/path/to/note.md cargo test real_model -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn real_model() {
        let path = std::env::var("HANDY_SUMMARY_NOTE").expect("set HANDY_SUMMARY_NOTE");
        let note = std::fs::read_to_string(path).unwrap();
        let mut lines: Vec<String> = Vec::new();
        let mut name = None;
        for line in note.lines() {
            if let Some(rest) = line.strip_prefix("**") {
                name = rest.split_once("**").map(|(name, _)| name.to_string());
            } else if let Some(speaker) = name.take() {
                lines.push(format!("{speaker}: {line}"));
            }
        }
        let model = local_model().expect("Apple Intelligence isn't available");
        let started = std::time::Instant::now();
        let summary = summarize(model.as_ref(), &lines).unwrap();
        println!(
            "{} parts, {:.1}s\n\n{}",
            parts(&lines).len(),
            started.elapsed().as_secs_f64(),
            summary.markdown()
        );
    }
}
