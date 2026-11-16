//! Meeting mode: records a call with no bot. The microphone and the call
//! app's audio are captured as separate channels of one file.
//!
//! macOS 14.4+ only. On anything else the commands report that meeting mode
//! is unavailable and the rest of the app is unaffected.

#[cfg(target_os = "macos")]
mod calendar;
mod echo;
#[cfg(target_os = "macos")]
mod mic;
pub mod note;
#[cfg(target_os = "macos")]
mod resample;
#[cfg(target_os = "macos")]
mod save;
#[cfg(target_os = "macos")]
mod segmenter;
#[cfg(target_os = "macos")]
mod speakers;
pub mod summary;
#[cfg(target_os = "macos")]
mod tap;
mod timeline;
#[cfg(target_os = "macos")]
mod transcriber;
#[cfg(target_os = "macos")]
mod writer;

use serde::{Deserialize, Serialize};
use specta::Type;
use tauri::AppHandle;

pub use timeline::MeetingSegment;

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
    /// The note that was saved.
    pub note_path: String,
    /// The audio kept next to it, if audio is kept.
    pub audio_path: Option<String>,
    pub duration_secs: f64,
    /// Everything said, in time order.
    pub transcript: Vec<MeetingSegment>,
    /// Who was invited, from the calendar event: names to offer when a
    /// speaker is renamed.
    pub attendees: Vec<String>,
    /// The dealers and Jira tickets the note is tagged with.
    pub dealers: Vec<String>,
    pub tickets: Vec<String>,
}

/// Sent as `meeting-saved` once a meeting's note is written.
#[derive(Clone, Debug, Serialize, Deserialize, Type, tauri_specta::Event)]
pub struct MeetingSaved {
    pub note_path: String,
    pub audio_path: Option<String>,
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
    pub fn start(&self, _title: Option<String>) -> Result<MeetingStarted, String> {
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
    use std::sync::Arc;
    use std::sync::{Mutex, MutexGuard, PoisonError};
    use std::time::Instant;

    use chrono::{DateTime, Local};
    use tauri_specta::Event;

    use log::{error, info};

    use super::calendar::{self, CalendarEvent};
    use super::mic::MicCapture;
    use super::save::{self, FinishedMeeting};
    use super::speakers;
    use super::tap::{ProcessTap, TapError, TapScope};
    use super::transcriber::{self, Progress, TranscriberHandle};
    use super::writer::{self, WriterHandle};
    use super::{CallAudioSource, MeetingManager, MeetingSaved, MeetingStarted, MeetingStopped};
    use crate::managers::audio::recording_notice_code;
    use crate::notices::{self, Notice, NoticeCode, NoticeSource};
    use crate::settings::get_settings;

    pub struct ActiveMeeting {
        mic: MicCapture,
        writer: WriterHandle,
        transcriber: TranscriberHandle,
        path: PathBuf,
        title: Option<String>,
        /// The calendar event happening when the meeting started, once it
        /// has been looked up.
        event: Arc<Mutex<Option<CalendarEvent>>>,
        started: Instant,
        started_at: DateTime<Local>,
    }

    impl MeetingManager {
        fn active(&self) -> MutexGuard<'_, Option<ActiveMeeting>> {
            self.active.lock().unwrap_or_else(PoisonError::into_inner)
        }

        /// Starts recording the microphone and the call app into a new file.
        ///
        /// `title` names the note; without one it is called "Meeting".
        pub fn start(&self, title: Option<String>) -> Result<MeetingStarted, String> {
            let mut active = self.active();
            if active.is_some() {
                return Err("A meeting is already being recorded".into());
            }
            let settings = get_settings(&self.app);
            // Checked before anything is recorded: there must be somewhere
            // to save the note.
            save::output_folder(&settings).map_err(|error| error.to_string())?;
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

            let progress = Arc::new(Progress::default());
            let started = writer::spawn(&path, tap, call, mic_receiver, Arc::clone(&progress))
                .and_then(|writer| {
                    // The writer has put the file's header on disk by now.
                    match transcriber::spawn(&self.app, &path, progress) {
                        Ok(transcriber) => Ok((writer, transcriber)),
                        Err(error) => {
                            let _ = writer.finish();
                            Err(error)
                        }
                    }
                });
            let (writer, transcriber) = match started {
                Ok(started) => started,
                Err(error) => {
                    mic.stop();
                    return Err(self.fail(NoticeCode::RecordingFailed, format!("{error:#}")));
                }
            };

            // Looked up off to the side: the first time, macOS asks the
            // user for calendar access, and recording mustn't wait for that.
            let event = Arc::new(Mutex::new(None));
            let found = Arc::clone(&event);
            let lookup = std::thread::Builder::new()
                .name("meeting-calendar".into())
                .spawn(move || {
                    let event = calendar::current_event();
                    *found.lock().unwrap_or_else(PoisonError::into_inner) = event;
                });
            if let Err(error) = lookup {
                error!("Couldn't start the calendar lookup: {error}");
            }

            if settings.meeting_separate_speakers {
                // Fetched now, if it's missing, so it is there at the end.
                speakers::prepare(&self.app);
            }

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
                transcriber,
                path,
                title,
                event,
                started: Instant::now(),
                started_at: Local::now(),
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
            let recorded = meeting.writer.finish();
            // The transcriber ends once the writer has, failed or not.
            let transcript = meeting.transcriber.finish();
            recorded
                .map_err(|error| self.fail(NoticeCode::RecordingFailed, format!("{error:#}")))?;
            let settings = get_settings(&self.app);
            // While the recording is still there to listen to.
            let transcript = if settings.meeting_separate_speakers {
                speakers::label(&self.app, &meeting.path, transcript)
            } else {
                transcript
            };

            let event = meeting
                .event
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .take();
            let (event_title, attendees) = match event {
                Some(event) => (Some(event.title), event.attendees),
                None => (None, Vec::new()),
            };
            let saved = save::save(
                &settings,
                FinishedMeeting {
                    recording: meeting.path,
                    started: meeting.started_at,
                    duration_secs,
                    // The user's own title wins over the calendar's.
                    title: meeting
                        .title
                        .filter(|title| !title.trim().is_empty())
                        .or(event_title),
                    attendees: attendees.clone(),
                    transcript: transcript.clone(),
                },
            )
            .map_err(|error| self.fail(NoticeCode::RecordingFailed, format!("{error:#}")))?;

            let note_path = saved.note.to_string_lossy().into_owned();
            let audio_path = saved
                .audio
                .map(|audio| audio.to_string_lossy().into_owned());
            let event = MeetingSaved {
                note_path: note_path.clone(),
                audio_path: audio_path.clone(),
            };
            if let Err(error) = event.emit(&self.app) {
                error!("Failed to emit meeting-saved: {error}");
            }
            Ok(MeetingStopped {
                note_path,
                audio_path,
                duration_secs,
                transcript,
                attendees,
                dealers: saved.dealers,
                tickets: saved.tickets,
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
