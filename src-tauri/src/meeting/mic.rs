//! The "Me" channel: the microphone Handy is set to use, through its own
//! `handy-recorder` stream so dictation's stream is left alone.

use std::sync::mpsc::Sender;

use handy_recorder::{AudioChunk, Channels, ErrorKind, Recorder, RecorderConfig, Sink};
use log::warn;

use crate::settings::AppSettings;

/// Forwards each chunk's real audio to the writer thread.
struct MicSink {
    samples: Sender<Vec<f32>>,
}

impl Sink for MicSink {
    fn process_chunk(&mut self, chunk: AudioChunk<'_>) {
        // The writer hanging up means the meeting is ending.
        let _ = self
            .samples
            .send(chunk.samples[..chunk.valid_frames].to_vec());
    }
}

pub struct MicCapture {
    recorder: Recorder<MicSink>,
}

impl MicCapture {
    /// Opens the selected microphone at 16 kHz mono and starts sending its
    /// audio to `samples`.
    pub fn start(
        settings: &AppSettings,
        samples: Sender<Vec<f32>>,
    ) -> Result<Self, handy_recorder::Error> {
        let device = selected_device(settings);
        let recorder = match open(device.clone(), settings.selected_channel) {
            Err(error) if error.kind() == ErrorKind::InvalidChannel => {
                warn!("{error}; averaging all input channels instead");
                open(device, None)
            }
            result => result,
        }?;
        recorder.start(MicSink { samples })?;
        Ok(Self { recorder })
    }

    pub fn stop(self) {
        match self.recorder.stop() {
            Ok(stopped) if !stopped.is_complete() => warn!(
                "Meeting microphone ended early ({:?}, {} frames dropped)",
                stopped.end_reason, stopped.dropped_frames
            ),
            Ok(_) => {}
            Err(error) => warn!("Stopping the meeting microphone failed: {error}"),
        }
    }
}

fn open(
    device: Option<String>,
    channel: Option<u16>,
) -> Result<Recorder<MicSink>, handy_recorder::Error> {
    Recorder::open_with_failure_handler(
        RecorderConfig {
            device,
            channels: channel.map_or(Channels::MixToMono, Channels::Only),
            ..RecorderConfig::speech()
        },
        |error| warn!("Meeting microphone failed: {error}"),
    )
}

/// The ID of the microphone picked in settings, or `None` for the system
/// default when none is picked or it isn't connected.
fn selected_device(settings: &AppSettings) -> Option<String> {
    let name = settings.selected_microphone.as_ref()?;
    match handy_recorder::list_input_devices() {
        Ok(devices) => devices.into_iter().find(|d| &d.name == name).map(|d| d.id),
        Err(error) => {
            warn!("Listing microphones failed, using the default: {error}");
            None
        }
    }
}
