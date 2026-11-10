//! Meeting mode: records a call with no bot. The microphone and the call
//! app's audio are captured as separate channels of one file.
//!
//! macOS 14.4+ only. On anything else the commands report that meeting mode
//! is unavailable and the rest of the app is unaffected.

#[cfg(target_os = "macos")]
mod mic;
#[cfg(target_os = "macos")]
mod resample;
#[cfg(target_os = "macos")]
mod tap;
#[cfg(target_os = "macos")]
mod writer;

use serde::Serialize;
use specta::Type;
use tauri::AppHandle;

/// Where the call audio is coming from.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Type)]
#[serde(rename_all = "lowercase")]
pub enum CallAudioSource {
    /// Only the meeting app set in settings.
    App,
    /// Everything the Mac plays, because the meeting app wasn't running.
    System,
}

#[derive(Clone, Debug, Serialize, Type)]
pub struct MeetingStarted {
    /// The recording in progress: stereo WAV, left = me, right = them.
    pub path: String,
    pub call_audio: CallAudioSource,
}

#[derive(Clone, Debug, Serialize, Type)]
pub struct MeetingStopped {
    pub path: String,
    pub duration_secs: f64,
}

pub struct MeetingManager {
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    app: AppHandle,
    #[cfg(target_os = "macos")]
    active: std::sync::Mutex<Option<macos::ActiveMeeting>>,
}

impl MeetingManager {
    pub fn new(app: &AppHandle) -> Self {
        Self {
            app: app.clone(),
            #[cfg(target_os = "macos")]
            active: std::sync::Mutex::new(None),
        }
    }

    #[cfg(not(target_os = "macos"))]
    pub fn start(&self) -> Result<MeetingStarted, String> {
        Err("Meeting mode needs macOS 14.4 or later".into())
    }

    #[cfg(not(target_os = "macos"))]
    pub fn stop(&self) -> Result<MeetingStopped, String> {
        Err("No meeting is being recorded".into())
    }
}

#[cfg(target_os = "macos")]
mod macos {
    use std::path::PathBuf;
    use std::sync::mpsc;
    use std::sync::{MutexGuard, PoisonError};
    use std::time::Instant;

    use log::{error, info};

    use super::mic::MicCapture;
    use super::tap::{ProcessTap, TapError, TapScope};
    use super::writer::{self, WriterHandle};
    use super::{CallAudioSource, MeetingManager, MeetingStarted, MeetingStopped};
    use crate::managers::audio::recording_notice_code;
    use crate::notices::{self, Notice, NoticeCode, NoticeSource};
    use crate::settings::get_settings;

    pub struct ActiveMeeting {
        mic: MicCapture,
        writer: WriterHandle,
        path: PathBuf,
        started: Instant,
    }

    impl MeetingManager {
        fn active(&self) -> MutexGuard<'_, Option<ActiveMeeting>> {
            self.active.lock().unwrap_or_else(PoisonError::into_inner)
        }

        /// Starts recording the microphone and the call app into a new file.
        pub fn start(&self) -> Result<MeetingStarted, String> {
            let mut active = self.active();
            if active.is_some() {
                return Err("A meeting is already being recorded".into());
            }
            let settings = get_settings(&self.app);
            let path = self.new_recording_path()?;

            let (tap, call) = ProcessTap::start(&settings.meeting_target_app)
                .map_err(|error| self.fail(tap_notice_code(&error), error.to_string()))?;
            let call_audio = match tap.scope() {
                TapScope::App => CallAudioSource::App,
                TapScope::System => CallAudioSource::System,
            };

            let (mic_samples, mic_receiver) = mpsc::channel();
            let mic = MicCapture::start(&settings, mic_samples).map_err(|error| {
                let error = anyhow::Error::new(error);
                self.fail(recording_notice_code(&error), error.to_string())
            })?;

            let writer = match writer::spawn(&path, tap, call, mic_receiver) {
                Ok(writer) => writer,
                Err(error) => {
                    mic.stop();
                    return Err(self.fail(NoticeCode::RecordingFailed, format!("{error:#}")));
                }
            };

            notices::clear(&self.app, NoticeSource::Meeting);
            info!(
                "Meeting recording started ({call_audio:?} audio) at {}",
                path.display()
            );
            let started = MeetingStarted {
                path: path.to_string_lossy().into_owned(),
                call_audio,
            };
            *active = Some(ActiveMeeting {
                mic,
                writer,
                path,
                started: Instant::now(),
            });
            Ok(started)
        }

        /// Stops the recording and closes its file.
        pub fn stop(&self) -> Result<MeetingStopped, String> {
            let meeting = self
                .active()
                .take()
                .ok_or_else(|| "No meeting is being recorded".to_string())?;
            let duration_secs = meeting.started.elapsed().as_secs_f64();
            // The microphone first, so its last audio is queued before the
            // writer does its final pass.
            meeting.mic.stop();
            meeting
                .writer
                .finish()
                .map_err(|error| self.fail(NoticeCode::RecordingFailed, format!("{error:#}")))?;
            Ok(MeetingStopped {
                path: meeting.path.to_string_lossy().into_owned(),
                duration_secs,
            })
        }

        fn new_recording_path(&self) -> Result<PathBuf, String> {
            let dir = crate::portable::app_data_dir(&self.app)
                .map_err(|error| error.to_string())?
                .join("meetings");
            std::fs::create_dir_all(&dir)
                .map_err(|error| format!("Failed to create {}: {error}", dir.display()))?;
            let name = chrono::Local::now().format("%Y-%m-%d %H%M%S Meeting.wav");
            Ok(dir.join(name.to_string()))
        }

        /// Reports a failure to the user and returns it for the command.
        fn fail(&self, code: NoticeCode, detail: String) -> String {
            error!("Meeting recording failed: {detail}");
            notices::raise(
                &self.app,
                Notice::new(code, NoticeSource::Meeting).with_detail(detail.clone()),
            );
            detail
        }
    }

    fn tap_notice_code(error: &TapError) -> NoticeCode {
        match error {
            TapError::PermissionDenied => NoticeCode::MeetingAudioPermissionDenied,
            _ => NoticeCode::RecordingFailed,
        }
    }
}
