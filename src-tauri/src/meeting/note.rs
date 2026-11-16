//! The meeting note: a markdown file with YAML frontmatter, written into the
//! folder the user picked (an Obsidian vault, usually), and the edits made to
//! it afterwards. Edits change only the lines they have to, so anything the
//! user wrote in the note stays.

use std::path::{Path, PathBuf};
use std::sync::LazyLock;

use anyhow::{anyhow, bail, Context, Result};
use chrono::{DateTime, Local, NaiveDate, NaiveTime};
use regex::Regex;
use serde::{Deserialize, Serialize};
use specta::Type;

use super::timeline::MeetingSegment;

pub const DEFAULT_TITLE: &str = "Meeting";
pub const DEFAULT_FILENAME_PATTERN: &str = "{date} {time} {title}";

/// What a note says about its meeting, apart from the transcript.
pub struct NoteDetails {
    pub started: DateTime<Local>,
    pub duration_secs: f64,
    pub title: String,
    /// The audio file's name, when it is kept next to the note.
    pub audio_file: Option<String>,
    pub attendees: Vec<String>,
    pub dealers: Vec<String>,
    pub tickets: Vec<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Type)]
#[serde(rename_all = "lowercase")]
pub enum TagKind {
    Dealer,
    Ticket,
}

/// The whole note.
pub fn render(details: &NoteDetails, transcript: &[MeetingSegment]) -> String {
    let date = details.started.format("%Y-%m-%d");
    let start = details.started.format("%H:%M");
    let mut note = format!(
        "---\ndate: {date}\nstart: {start}\nduration: {}\ntype: meeting\n",
        duration_label(details.duration_secs)
    );
    if let Some(audio) = &details.audio_file {
        note.push_str(&format!("audio: \"[[{audio}]]\"\n"));
    }
    for (key, names) in [
        ("attendees", &details.attendees),
        ("dealers", &details.dealers),
    ] {
        if !names.is_empty() {
            note.push_str(&format!("{key}: {}\n", quoted_list(names)));
        }
    }
    if !details.tickets.is_empty() {
        note.push_str(&format!("tickets: [{}]\n", details.tickets.join(", ")));
    }
    note.push_str(&format!(
        "tags: [{}]\n---\n\n",
        tags(&details.dealers, &details.tickets).join(", ")
    ));
    note.push_str(&format!(
        "{}\n",
        heading(&details.title, &date.to_string(), &start.to_string())
    ));
    for paragraph in paragraphs(transcript) {
        note.push_str(&format!(
            "\n**{}** {}\n{}\n",
            paragraph.name,
            clock(paragraph.start),
            paragraph.text
        ));
    }
    note
}

/// A pause this long, or longer, starts a new paragraph for the same speaker.
const PARAGRAPH_PAUSE_SECS: f64 = 5.0;
/// A paragraph that has run this long is ended at the next segment, so a
/// monologue still gets a timestamp now and then.
const PARAGRAPH_MAX_SECS: f64 = 60.0;

struct Paragraph {
    name: String,
    start: f64,
    end: f64,
    text: String,
}

/// Joins what one speaker said without a real pause or an interruption into
/// one paragraph. The speech model's segments end at every breath, which
/// reads as a list of fragments.
fn paragraphs(transcript: &[MeetingSegment]) -> Vec<Paragraph> {
    let mut joined: Vec<Paragraph> = Vec::new();
    for segment in transcript {
        let text = segment.text.trim();
        if text.is_empty() {
            continue;
        }
        match joined.last_mut() {
            Some(last)
                if last.name == segment.name
                    && segment.start - last.end < PARAGRAPH_PAUSE_SECS
                    && segment.start - last.start < PARAGRAPH_MAX_SECS =>
            {
                last.text.push(' ');
                last.text.push_str(text);
                last.end = last.end.max(segment.end);
            }
            _ => joined.push(Paragraph {
                name: segment.name.clone(),
                start: segment.start,
                end: segment.end,
                text: text.to_string(),
            }),
        }
    }
    joined
}

fn heading(title: &str, date: &str, start: &str) -> String {
    format!("# {title} {date} {start}")
}

/// `34m`, or `1h 4m` from an hour up. Never less than a minute.
fn duration_label(secs: f64) -> String {
    let minutes = ((secs / 60.0).round() as u64).max(1);
    if minutes < 60 {
        format!("{minutes}m")
    } else {
        format!("{}h {}m", minutes / 60, minutes % 60)
    }
}

fn clock(secs: f64) -> String {
    let secs = secs.max(0.0) as u64;
    format!("{:02}:{:02}:{:02}", secs / 3600, secs / 60 % 60, secs % 60)
}

fn quoted_list(names: &[String]) -> String {
    let quoted: Vec<String> = names
        .iter()
        .map(|name| format!("\"{}\"", name.replace('\\', "\\\\").replace('"', "\\\"")))
        .collect();
    format!("[{}]", quoted.join(", "))
}

/// The note's Obsidian tags: `meeting`, each dealer, each ticket.
fn tags(dealers: &[String], tickets: &[String]) -> Vec<String> {
    let mut tags = vec!["meeting".to_string()];
    tags.extend(dealers.iter().map(|dealer| tag_for(dealer)));
    tags.extend(tickets.iter().cloned());
    tags
}

/// A name as an Obsidian tag: tags can't hold spaces or most punctuation.
fn tag_for(name: &str) -> String {
    let mut tag = String::new();
    for c in name.chars() {
        if c.is_alphanumeric() {
            tag.extend(c.to_lowercase());
        } else if !tag.ends_with('-') && !tag.is_empty() {
            tag.push('-');
        }
    }
    tag.trim_end_matches('-').to_string()
}

/// The file name, without extension, for a meeting: `pattern` with `{date}`
/// (2026-11-10), `{time}` (1402) and `{title}` filled in, and anything a file
/// name can't hold removed.
pub fn file_stem(pattern: &str, date: NaiveDate, time: NaiveTime, title: &str) -> String {
    let pattern = if pattern.trim().is_empty() {
        DEFAULT_FILENAME_PATTERN
    } else {
        pattern
    };
    let stem = pattern
        .replace("{date}", &date.format("%Y-%m-%d").to_string())
        .replace("{time}", &time.format("%H%M").to_string())
        .replace("{title}", title);
    let cleaned: String = stem
        .chars()
        .map(|c| match c {
            '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|' | '#' | '^' | '[' | ']' => ' ',
            c if c.is_control() => ' ',
            c => c,
        })
        .collect();
    let stem = cleaned.split_whitespace().collect::<Vec<_>>().join(" ");
    let stem = stem.trim_matches('.').trim();
    if stem.is_empty() {
        DEFAULT_TITLE.to_string()
    } else {
        stem.to_string()
    }
}

/// A path in `folder` for `stem` that no file has yet, whatever extension it
/// would get: ` 2`, ` 3`... is added when the name is taken.
pub fn free_stem(folder: &Path, stem: &str, extensions: &[&str]) -> String {
    let taken = |stem: &str| {
        extensions
            .iter()
            .any(|ext| folder.join(format!("{stem}.{ext}")).exists())
    };
    if !taken(stem) {
        return stem.to_string();
    }
    (2..)
        .map(|n| format!("{stem} {n}"))
        .find(|candidate| !taken(candidate))
        .unwrap_or_else(|| stem.to_string())
}

/// The dealers from `known` that are named in `text`, in the order given.
pub fn find_dealers(known: &[String], text: &str) -> Vec<String> {
    let haystack = format!(" {} ", plain_words(text));
    let mut found: Vec<String> = Vec::new();
    for dealer in known {
        let needle = plain_words(dealer);
        if needle.is_empty() || found.contains(dealer) {
            continue;
        }
        if haystack.contains(&format!(" {needle} ")) {
            found.push(dealer.clone());
        }
    }
    found
}

/// Lowercase words separated by single spaces, punctuation dropped.
fn plain_words(text: &str) -> String {
    text.split(|c: char| !c.is_alphanumeric())
        .filter(|word| !word.is_empty())
        .map(str::to_lowercase)
        .collect::<Vec<_>>()
        .join(" ")
}

/// Jira keys in `text`, as `KEY-123`, in the order first seen. A speech model
/// writes a key it hears as `PMNAMC 72` or `PMNAMC72`, so those count too.
pub fn find_tickets(text: &str) -> Vec<String> {
    static KEY: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"\b(?:([A-Z][A-Z0-9]+)-|([A-Z]{3,}[0-9]?) |([A-Z]{3,}))([0-9]{1,6})\b")
            .expect("valid regex")
    });
    let mut found: Vec<String> = Vec::new();
    for captures in KEY.captures_iter(text) {
        let project = (1..=3).find_map(|group| captures.get(group));
        let (Some(project), Some(number)) = (project, captures.get(4)) else {
            continue;
        };
        let key = format!("{}-{}", project.as_str(), number.as_str());
        if !found.contains(&key) {
            found.push(key);
        }
    }
    found
}

/// A saved note split into its frontmatter lines and everything after them.
struct SavedNote {
    frontmatter: Vec<String>,
    body: String,
}

impl SavedNote {
    fn read(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("Failed to read {}", path.display()))?;
        let rest = text
            .strip_prefix("---\n")
            .ok_or_else(|| anyhow!("{} has no frontmatter", path.display()))?;
        let (frontmatter, body) = rest
            .split_once("\n---\n")
            .ok_or_else(|| anyhow!("{} has no frontmatter", path.display()))?;
        let note = Self {
            frontmatter: frontmatter.lines().map(str::to_string).collect(),
            body: body.to_string(),
        };
        if note.value("type") != Some("meeting") {
            bail!("{} is not a meeting note", path.display());
        }
        Ok(note)
    }

    fn write(&self, path: &Path) -> Result<()> {
        let text = format!("---\n{}\n---\n{}", self.frontmatter.join("\n"), self.body);
        std::fs::write(path, text).with_context(|| format!("Failed to write {}", path.display()))
    }

    fn line(&self, key: &str) -> Option<usize> {
        let prefix = format!("{key}:");
        self.frontmatter
            .iter()
            .position(|line| line.starts_with(&prefix))
    }

    fn value(&self, key: &str) -> Option<&str> {
        let line = &self.frontmatter[self.line(key)?];
        Some(line[key.len() + 1..].trim())
    }

    /// The items of an inline list such as `[a, "b c"]`.
    fn list(&self, key: &str) -> Vec<String> {
        let Some(value) = self.value(key) else {
            return Vec::new();
        };
        let inner = value.trim_start_matches('[').trim_end_matches(']');
        let mut items = Vec::new();
        let mut item = String::new();
        let mut quoted = false;
        let mut chars = inner.chars();
        while let Some(c) = chars.next() {
            match c {
                '\\' if quoted => item.extend(chars.next()),
                '"' => quoted = !quoted,
                ',' if !quoted => items.push(std::mem::take(&mut item)),
                c => item.push(c),
            }
        }
        items.push(item);
        items
            .into_iter()
            .map(|item| item.trim().to_string())
            .filter(|item| !item.is_empty())
            .collect()
    }

    /// Sets `key`'s line, or removes it when `value` is `None`. A new key
    /// goes right before `tags`.
    fn set(&mut self, key: &str, value: Option<String>) {
        let line = value.map(|value| format!("{key}: {value}"));
        match (self.line(key), line) {
            (Some(at), Some(line)) => self.frontmatter[at] = line,
            (Some(at), None) => {
                self.frontmatter.remove(at);
            }
            (None, Some(line)) => {
                let at = self.line("tags").unwrap_or(self.frontmatter.len());
                self.frontmatter.insert(at, line);
            }
            (None, None) => {}
        }
    }

    fn audio_file(&self) -> Option<String> {
        let link = self.value("audio")?.trim_matches('"');
        Some(link.strip_prefix("[[")?.strip_suffix("]]")?.to_string())
    }
}

/// Gives a saved note a new title: its file name, its heading, and its audio
/// file's name and link. Returns the note's new path.
pub fn rename(path: &Path, title: &str, pattern: &str) -> Result<PathBuf> {
    let title = title.trim();
    if title.is_empty() {
        bail!("The title is empty");
    }
    let mut note = SavedNote::read(path)?;
    let folder = path
        .parent()
        .ok_or_else(|| anyhow!("{} has no folder", path.display()))?;
    let date = note.value("date").unwrap_or_default().to_string();
    let start = note.value("start").unwrap_or_default().to_string();
    let day = NaiveDate::parse_from_str(&date, "%Y-%m-%d")
        .with_context(|| format!("{} has no usable date", path.display()))?;
    let time = NaiveTime::parse_from_str(&start, "%H:%M")
        .with_context(|| format!("{} has no usable start time", path.display()))?;

    let wanted = file_stem(pattern, day, time, title);
    let current = path.file_stem().and_then(|stem| stem.to_str());
    let audio = note.audio_file();
    let audio_extension = audio
        .as_deref()
        .and_then(|name| Path::new(name).extension()?.to_str())
        .map(str::to_string);
    let stem = if current == Some(wanted.as_str()) {
        wanted
    } else {
        let mut extensions = vec!["md"];
        extensions.extend(audio_extension.as_deref());
        free_stem(folder, &wanted, &extensions)
    };

    // The audio first: if it can't move, the note still points at it.
    if let (Some(old), Some(extension)) = (&audio, &audio_extension) {
        let new = format!("{stem}.{extension}");
        if *old != new {
            let from = folder.join(old);
            if from.exists() {
                std::fs::rename(&from, folder.join(&new))
                    .with_context(|| format!("Failed to rename {}", from.display()))?;
            }
            note.set("audio", Some(format!("\"[[{new}]]\"")));
        }
    }

    // Only the first heading: the one this module wrote.
    let new_heading = heading(title, &date, &start);
    let mut replaced = false;
    note.body = note
        .body
        .split('\n')
        .map(|line| {
            if !replaced && line.starts_with("# ") {
                replaced = true;
                new_heading.as_str()
            } else {
                line
            }
        })
        .collect::<Vec<_>>()
        .join("\n");

    let new_path = folder.join(format!("{stem}.md"));
    note.write(path)?;
    if new_path != path {
        std::fs::rename(path, &new_path)
            .with_context(|| format!("Failed to rename {}", path.display()))?;
    }
    Ok(new_path)
}

/// Renames a speaker throughout a saved note: every `**Speaker 1** 00:01:02`
/// line becomes `**Kyle** 00:01:02`. Returns how many lines changed.
pub fn rename_speaker(path: &Path, from: &str, to: &str) -> Result<usize> {
    let to = to.trim();
    if to.is_empty() || to.contains(['*', '\n', '\r']) {
        bail!("That isn't a usable speaker name");
    }
    let mut note = SavedNote::read(path)?;
    let old = format!("**{from}** ");
    let mut changed = 0;
    note.body = note
        .body
        .split('\n')
        .map(|line| match line.strip_prefix(&old) {
            Some(time) if is_clock(time) => {
                changed += 1;
                format!("**{to}** {time}")
            }
            _ => line.to_string(),
        })
        .collect::<Vec<_>>()
        .join("\n");
    if changed > 0 {
        note.write(path)?;
    }
    Ok(changed)
}

/// `00:01:02`.
fn is_clock(text: &str) -> bool {
    let bytes = text.as_bytes();
    bytes.len() == 8
        && bytes.iter().enumerate().all(|(at, byte)| {
            if at == 2 || at == 5 {
                *byte == b':'
            } else {
                byte.is_ascii_digit()
            }
        })
}

/// Takes a wrong dealer or ticket off a saved note: out of its list and out
/// of its tags.
pub fn remove_tag(path: &Path, kind: TagKind, tag: &str) -> Result<()> {
    let mut note = SavedNote::read(path)?;
    let (key, as_tag) = match kind {
        TagKind::Dealer => ("dealers", tag_for(tag)),
        TagKind::Ticket => ("tickets", tag.to_string()),
    };
    let remaining: Vec<String> = note
        .list(key)
        .into_iter()
        .filter(|item| item != tag)
        .collect();
    let value = match kind {
        _ if remaining.is_empty() => None,
        TagKind::Dealer => Some(quoted_list(&remaining)),
        TagKind::Ticket => Some(format!("[{}]", remaining.join(", "))),
    };
    note.set(key, value);
    let tags: Vec<String> = note
        .list("tags")
        .into_iter()
        .filter(|item| *item != as_tag)
        .collect();
    note.set("tags", Some(format!("[{}]", tags.join(", "))));
    note.write(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::meeting::timeline::Speaker;
    use chrono::TimeZone;

    fn details() -> NoteDetails {
        NoteDetails {
            started: Local.with_ymd_and_hms(2026, 11, 10, 14, 2, 0).unwrap(),
            duration_secs: 34.0 * 60.0,
            title: DEFAULT_TITLE.to_string(),
            audio_file: Some("2026-11-10 1402 Meeting.m4a".to_string()),
            attendees: Vec::new(),
            dealers: Vec::new(),
            tickets: Vec::new(),
        }
    }

    fn transcript() -> Vec<MeetingSegment> {
        vec![
            MeetingSegment {
                speaker: Speaker::Me,
                name: "Me".to_string(),
                start: 4.2,
                end: 6.0,
                text: "Text...".to_string(),
            },
            MeetingSegment {
                speaker: Speaker::Them,
                name: "Them".to_string(),
                start: 11.0,
                end: 13.0,
                text: "Text...".to_string(),
            },
        ]
    }

    #[test]
    fn a_plain_note_matches_the_planned_format() {
        assert_eq!(
            render(&details(), &transcript()),
            "---\n\
             date: 2026-11-10\n\
             start: 14:02\n\
             duration: 34m\n\
             type: meeting\n\
             audio: \"[[2026-11-10 1402 Meeting.m4a]]\"\n\
             tags: [meeting]\n\
             ---\n\
             \n\
             # Meeting 2026-11-10 14:02\n\
             \n\
             **Me** 00:00:04\n\
             Text...\n\
             \n\
             **Them** 00:00:11\n\
             Text...\n"
        );
    }

    #[test]
    fn one_speakers_run_of_segments_reads_as_a_paragraph() {
        let said = |speaker: Speaker, start: f64, end: f64, text: &str| MeetingSegment {
            speaker,
            name: speaker.name().to_string(),
            start,
            end,
            text: text.to_string(),
        };
        let note = render(
            &details(),
            &[
                said(Speaker::Them, 10.0, 14.0, "So each dealer gets a link."),
                said(Speaker::Them, 14.8, 17.0, "I mean, technically"),
                said(Speaker::Them, 17.5, 19.0, "that works."),
                said(Speaker::Me, 19.5, 20.5, "Agreed."),
                said(Speaker::Them, 21.0, 23.0, "Great."),
                // A real pause: a new paragraph for the same speaker.
                said(Speaker::Them, 40.0, 42.0, "Next topic."),
            ],
        );
        let body = note.split("2026-11-10 14:02\n").nth(1).unwrap();
        assert_eq!(
            body,
            "\n**Them** 00:00:10\nSo each dealer gets a link. I mean, technically that works.\n\
             \n**Me** 00:00:19\nAgreed.\n\
             \n**Them** 00:00:21\nGreat.\n\
             \n**Them** 00:00:40\nNext topic.\n"
        );
    }

    #[test]
    fn a_monologue_still_gets_a_timestamp_every_minute() {
        let transcript: Vec<MeetingSegment> = (0..10)
            .map(|i| MeetingSegment {
                speaker: Speaker::Them,
                name: "Them".to_string(),
                start: f64::from(i) * 20.0,
                end: f64::from(i) * 20.0 + 19.0,
                text: "And another thing.".to_string(),
            })
            .collect();
        let note = render(&details(), &transcript);
        assert_eq!(note.matches("**Them**").count(), 4);
    }

    #[test]
    fn dealers_and_tickets_go_in_frontmatter_and_tags() {
        let note = render(
            &NoteDetails {
                dealers: vec!["Kleckner".to_string(), "Fisher & Paykel".to_string()],
                tickets: vec!["PMNAMC-72".to_string()],
                audio_file: None,
                ..details()
            },
            &[],
        );
        assert!(note.contains("dealers: [\"Kleckner\", \"Fisher & Paykel\"]\n"));
        assert!(note.contains("tickets: [PMNAMC-72]\n"));
        assert!(note.contains("tags: [meeting, kleckner, fisher-paykel, PMNAMC-72]\n"));
        assert!(!note.contains("audio:"));
    }

    #[test]
    fn long_and_short_durations() {
        assert_eq!(duration_label(20.0), "1m");
        assert_eq!(duration_label(64.0 * 60.0), "1h 4m");
    }

    #[test]
    fn file_names_follow_the_pattern_and_stay_legal() {
        let date = NaiveDate::from_ymd_opt(2026, 11, 10).unwrap();
        let time = NaiveTime::from_hms_opt(14, 2, 0).unwrap();
        assert_eq!(
            file_stem(DEFAULT_FILENAME_PATTERN, date, time, "Meeting"),
            "2026-11-10 1402 Meeting"
        );
        assert_eq!(
            file_stem(
                "{title} ({date})",
                date,
                time,
                "Kleckner: Q4 / Black Friday?"
            ),
            "Kleckner Q4 Black Friday (2026-11-10)"
        );
        assert_eq!(file_stem("", date, time, "A"), "2026-11-10 1402 A");
    }

    #[test]
    fn dealers_are_found_as_whole_names() {
        let known = vec![
            "Kleckner".to_string(),
            "Fisher & Paykel".to_string(),
            "Reds".to_string(),
        ];
        assert_eq!(
            find_dealers(
                &known,
                "the Fisher and Paykel... no, fisher & paykel rebate for KLECKNER."
            ),
            vec!["Kleckner".to_string(), "Fisher & Paykel".to_string()]
        );
        assert!(find_dealers(&known, "hundreds of banners").is_empty());
    }

    #[test]
    fn tickets_are_found_however_the_model_wrote_them() {
        assert_eq!(
            find_tickets("update PMNAMC 72 and PREM-2561, then PMNAMC72 and PMKS2 119 at 3 PM"),
            vec!["PMNAMC-72", "PREM-2561", "PMKS2-119"]
        );
        assert!(find_tickets("We meet at 10 AM on Friday the 13th in Q4 2026.").is_empty());
    }

    #[test]
    fn renaming_moves_the_note_and_audio_and_keeps_the_users_text() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("2026-11-10 1402 Meeting.md");
        let audio = dir.path().join("2026-11-10 1402 Meeting.m4a");
        let mut note = render(&details(), &transcript());
        note.push_str("\nMy own notes.\n");
        std::fs::write(&path, &note).unwrap();
        std::fs::write(&audio, b"audio").unwrap();

        let renamed = rename(&path, "Kleckner sync", DEFAULT_FILENAME_PATTERN).unwrap();

        assert_eq!(renamed, dir.path().join("2026-11-10 1402 Kleckner sync.md"));
        assert!(!path.exists() && !audio.exists());
        assert!(dir
            .path()
            .join("2026-11-10 1402 Kleckner sync.m4a")
            .exists());
        let text = std::fs::read_to_string(&renamed).unwrap();
        assert!(text.contains("audio: \"[[2026-11-10 1402 Kleckner sync.m4a]]\"\n"));
        assert!(text.contains("\n# Kleckner sync 2026-11-10 14:02\n"));
        assert!(text.ends_with("\nMy own notes.\n"));
    }

    #[test]
    fn renaming_does_not_overwrite_another_note() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("2026-11-10 1402 Meeting.md");
        std::fs::write(
            &path,
            render(
                &NoteDetails {
                    audio_file: None,
                    ..details()
                },
                &[],
            ),
        )
        .unwrap();
        std::fs::write(dir.path().join("2026-11-10 1402 Sync.md"), "other").unwrap();
        let renamed = rename(&path, "Sync", DEFAULT_FILENAME_PATTERN).unwrap();
        assert_eq!(renamed, dir.path().join("2026-11-10 1402 Sync 2.md"));
    }

    #[test]
    fn removing_a_tag_takes_it_out_of_its_list_and_the_tags() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("note.md");
        let note = render(
            &NoteDetails {
                dealers: vec!["Kleckner".to_string(), "Fisher & Paykel".to_string()],
                tickets: vec!["PMNAMC-72".to_string()],
                ..details()
            },
            &transcript(),
        );
        std::fs::write(&path, note).unwrap();

        remove_tag(&path, TagKind::Dealer, "Fisher & Paykel").unwrap();
        remove_tag(&path, TagKind::Ticket, "PMNAMC-72").unwrap();

        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("dealers: [\"Kleckner\"]\n"));
        assert!(!text.contains("tickets:"));
        assert!(text.contains("tags: [meeting, kleckner]\n"));
        assert!(text.contains("**Them** 00:00:11"));
    }

    #[test]
    fn renaming_a_speaker_changes_their_lines_and_nothing_else() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("note.md");
        let said = |name: &str, start: f64, text: &str| MeetingSegment {
            speaker: Speaker::Them,
            name: name.to_string(),
            start,
            end: start + 1.0,
            text: text.to_string(),
        };
        let note = render(
            &details(),
            &[
                said("Speaker 1", 1.0, "Ask **Speaker 2** about it."),
                said("Speaker 2", 10.0, "Sure."),
                said("Speaker 1", 20.0, "Thanks."),
            ],
        );
        std::fs::write(&path, note).unwrap();

        assert_eq!(rename_speaker(&path, "Speaker 2", "Kyle").unwrap(), 1);
        assert_eq!(rename_speaker(&path, "Speaker 1", "Jess").unwrap(), 2);
        assert!(rename_speaker(&path, "Jess", " ").is_err());

        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("**Jess** 00:00:01\nAsk **Speaker 2** about it.\n"));
        assert!(text.contains("**Kyle** 00:00:10\nSure.\n"));
        assert!(text.contains("**Jess** 00:00:20\nThanks.\n"));
    }

    #[test]
    fn a_file_that_is_not_a_meeting_note_is_left_alone() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("other.md");
        std::fs::write(&path, "---\ntype: project\n---\n# Mine\n").unwrap();
        assert!(rename(&path, "New", DEFAULT_FILENAME_PATTERN).is_err());
        assert!(remove_tag(&path, TagKind::Ticket, "A-1").is_err());
    }
}
