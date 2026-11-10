use std::sync::Arc;

use tauri::State;

use crate::meeting::{MeetingManager, MeetingStarted, MeetingStopped};

/// Starts recording a meeting: the microphone and the call app's audio.
#[tauri::command]
#[specta::specta]
pub fn meeting_start(manager: State<Arc<MeetingManager>>) -> Result<MeetingStarted, String> {
    manager.start()
}

/// Stops the meeting being recorded and returns where its audio is.
#[tauri::command]
#[specta::specta]
pub fn meeting_stop(manager: State<Arc<MeetingManager>>) -> Result<MeetingStopped, String> {
    manager.stop()
}
