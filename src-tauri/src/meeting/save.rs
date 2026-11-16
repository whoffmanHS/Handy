//! Turns a finished recording into what the user keeps: a note, and
//! optionally compressed audio next to it, in the folder they picked.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{anyhow, bail, Context, Result};
use chrono::{DateTime, Local};
use log::{info, warn};

use super::note::{self, NoteDetails};
use super::timeline::MeetingSegment;
use super::transcriber::transcript_path;
use crate::settings::AppSettings;

/// AAC at this rate keeps 16 kHz speech clear at about 30 MB an hour.
const AUDIO_BITRATE: &str = "64000";

pub struct FinishedMeeting {
    /// The working recording: stereo WAV in Handy's own data folder.
    pub recording: PathBuf,
    pub started: DateTime<Local>,
    pub duration_secs: f64,
    /// The title the user gave or the calendar event's, if any.
    pub title: Option<String>,
    /// Who was invited, from the calendar event.
    pub attendees: Vec<String>,
    pub transcript: Vec<MeetingSegment>,
}

pub struct SavedMeeting {
    pub note: PathBuf,
    pub audio: Option<PathBuf>,
    pub dealers: Vec<String>,
    pub tickets: Vec<String>,
}

/// The folder notes go in, if one is set and still there.
pub fn output_folder(settings: &AppSettings) -> Result<PathBuf> {
    let folder = settings
        .meeting_output_folder
        .as_deref()
        .filter(|folder| !folder.trim().is_empty())
        .ok_or_else(|| anyhow!("Choose a folder for meeting notes first"))?;
    let folder = PathBuf::from(folder);
    if !folder.is_dir() {
        bail!("The meeting notes folder is missing: {}", folder.display());
    }
    Ok(folder)
}

/// Writes the note (and the audio, when it is kept), then removes the working
/// files. If the note can't be written, the working files stay.
pub fn save(settings: &AppSettings, meeting: FinishedMeeting) -> Result<SavedMeeting> {
    let folder = output_folder(settings)?;
    let title = meeting
        .title
        .as_deref()
        .map(str::trim)
        .filter(|title| !title.is_empty())
        .unwrap_or(note::DEFAULT_TITLE)
        .to_string();

    let spoken: String = meeting
        .transcript
        .iter()
        .map(|segment| segment.text.as_str())
        .collect::<Vec<_>>()
        .join("\n");
    let searched = format!("{title}\n{spoken}");
    let dealers = note::find_dealers(&settings.meeting_dealers, &searched);
    let tickets = note::find_tickets(&searched);

    let stem = note::file_stem(
        &settings.meeting_filename_pattern,
        meeting.started.date_naive(),
        meeting.started.time(),
        &title,
    );
    let stem = note::free_stem(&folder, &stem, &["md", "m4a", "wav"]);

    let audio = if settings.meeting_keep_audio {
        Some(keep_audio(&meeting.recording, &folder, &stem)?)
    } else {
        None
    };
    let details = NoteDetails {
        started: meeting.started,
        duration_secs: meeting.duration_secs,
        title,
        audio_file: audio
            .as_deref()
            .and_then(Path::file_name)
            .map(|name| name.to_string_lossy().into_owned()),
        attendees: meeting.attendees,
        dealers: dealers.clone(),
        tickets: tickets.clone(),
    };
    let note = folder.join(format!("{stem}.md"));
    std::fs::write(&note, note::render(&details, &meeting.transcript))
        .with_context(|| format!("Failed to write {}", note.display()))?;
    info!("Meeting note saved to {}", note.display());

    for leftover in [
        meeting.recording.clone(),
        transcript_path(&meeting.recording),
    ] {
        if let Err(error) = std::fs::remove_file(&leftover) {
            if leftover.exists() {
                warn!("Couldn't remove {}: {error}", leftover.display());
            }
        }
    }
    Ok(SavedMeeting {
        note,
        audio,
        dealers,
        tickets,
    })
}

/// Puts the recording next to the note as AAC. If the conversion fails, the
/// recording itself is copied there, so the audio is never lost.
fn keep_audio(recording: &Path, folder: &Path, stem: &str) -> Result<PathBuf> {
    let compressed = folder.join(format!("{stem}.m4a"));
    match compress(recording, &compressed) {
        Ok(()) => Ok(compressed),
        Err(error) => {
            warn!("Couldn't compress the meeting audio, keeping it as WAV: {error:#}");
            let _ = std::fs::remove_file(&compressed);
            let copy = folder.join(format!("{stem}.wav"));
            std::fs::copy(recording, &copy)
                .with_context(|| format!("Failed to copy the audio to {}", copy.display()))?;
            Ok(copy)
        }
    }
}

/// `afconvert` ships with macOS.
fn compress(from: &Path, to: &Path) -> Result<()> {
    let output = Command::new("/usr/bin/afconvert")
        .args(["-f", "m4af", "-d", "aac", "-b", AUDIO_BITRATE])
        .arg(from)
        .arg(to)
        .output()
        .context("Failed to run afconvert")?;
    if !output.status.success() {
        bail!(
            "afconvert failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(())
}
