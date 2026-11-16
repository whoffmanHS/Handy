use std::path::Path;
use std::sync::Arc;

use tauri::{AppHandle, State};

use crate::meeting::note::{self, TagKind};
use crate::meeting::{MeetingManager, MeetingStarted, MeetingStopped};
use crate::settings::{get_settings, write_settings};

/// Starts recording a meeting: the microphone and the call app's audio.
/// `title` names the note; without one it is called "Meeting".
///
/// Runs off the main thread, like `meeting_stop`: both wait on things
/// (permission prompts, the speech engine) that the main thread serves.
#[tauri::command(async)]
#[specta::specta]
pub fn meeting_start(
    manager: State<Arc<MeetingManager>>,
    title: Option<String>,
) -> Result<MeetingStarted, String> {
    manager.start(title)
}

/// Stops the meeting being recorded and saves its note. This transcribes
/// what is left, separates the speakers and compresses the audio, so it can
/// take a while after a long meeting.
#[tauri::command(async)]
#[specta::specta]
pub fn meeting_stop(manager: State<Arc<MeetingManager>>) -> Result<MeetingStopped, String> {
    manager.stop()
}

/// Gives the saved note at `path` a new title. Returns its new path.
#[tauri::command]
#[specta::specta]
pub fn meeting_rename(app: AppHandle, path: String, title: String) -> Result<String, String> {
    let pattern = get_settings(&app).meeting_filename_pattern;
    note::rename(Path::new(&path), &title, &pattern)
        .map(|path| path.to_string_lossy().into_owned())
        .map_err(|error| format!("{error:#}"))
}

/// Takes a wrong dealer or ticket tag off the saved note at `path`.
#[tauri::command]
#[specta::specta]
pub fn meeting_remove_tag(path: String, kind: TagKind, tag: String) -> Result<(), String> {
    note::remove_tag(Path::new(&path), kind, &tag).map_err(|error| format!("{error:#}"))
}

/// Renames a speaker throughout the saved note at `path`, such as
/// "Speaker 1" to "Kyle". Returns how many lines changed.
#[tauri::command]
#[specta::specta]
pub fn meeting_rename_speaker(path: String, from: String, to: String) -> Result<u32, String> {
    note::rename_speaker(Path::new(&path), &from, &to)
        .map(|changed| changed as u32)
        .map_err(|error| format!("{error:#}"))
}

/// Sets whether the other side's voices are told apart in saved notes.
#[tauri::command]
#[specta::specta]
pub fn set_meeting_separate_speakers(app: AppHandle, separate: bool) {
    let mut settings = get_settings(&app);
    settings.meeting_separate_speakers = separate;
    write_settings(&app, settings);
}

/// Sets the folder meeting notes are saved in. `None` clears it.
#[tauri::command]
#[specta::specta]
pub fn set_meeting_output_folder(app: AppHandle, folder: Option<String>) {
    let mut settings = get_settings(&app);
    settings.meeting_output_folder = folder.filter(|folder| !folder.trim().is_empty());
    write_settings(&app, settings);
}

/// Sets the note file name pattern: `{date}`, `{time}` and `{title}`.
#[tauri::command]
#[specta::specta]
pub fn set_meeting_filename_pattern(app: AppHandle, pattern: String) {
    let mut settings = get_settings(&app);
    settings.meeting_filename_pattern = pattern;
    write_settings(&app, settings);
}

/// Sets whether a meeting's audio is kept next to its note.
#[tauri::command]
#[specta::specta]
pub fn set_meeting_keep_audio(app: AppHandle, keep: bool) {
    let mut settings = get_settings(&app);
    settings.meeting_keep_audio = keep;
    write_settings(&app, settings);
}

/// Sets the dealer names notes are tagged with. New names are also added to
/// the custom words, so the speech model spells them right.
#[tauri::command]
#[specta::specta]
pub fn set_meeting_dealers(app: AppHandle, dealers: Vec<String>) {
    let mut settings = get_settings(&app);
    let mut cleaned: Vec<String> = Vec::new();
    for dealer in dealers {
        let dealer = dealer.trim().to_string();
        if !dealer.is_empty() && !cleaned.contains(&dealer) {
            cleaned.push(dealer);
        }
    }
    for dealer in &cleaned {
        if !settings.custom_words.contains(dealer) {
            settings.custom_words.push(dealer.clone());
        }
    }
    settings.meeting_dealers = cleaned;
    write_settings(&app, settings);
}
