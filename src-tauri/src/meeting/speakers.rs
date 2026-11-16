//! Tells the people on the other side of the call apart, once it is over.
//!
//! A diarization model (who spoke when) runs over the call channel. Each
//! `Them` segment then gets its speaker, and a segment two people share is
//! split where the speaker changes and transcribed again piece by piece.
//!
//! The model is Streaming Sortformer from Handy's own catalog, run by the
//! speech library Handy already ships. It tells up to four voices apart.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use anyhow::{anyhow, bail, Context, Result};
use futures_util::StreamExt;
use log::{info, warn};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use tauri::{AppHandle, Manager};
use transcribe_cpp::{
    DiarizeExtension, DiarizeOptions, Model, SortformerDiarizeOptions, SortformerPreset,
};

use super::timeline::{MeetingSegment, Speaker};
use super::transcriber::YieldToDictation;
use crate::audio_toolkit::constants::WHISPER_SAMPLE_RATE;
use crate::managers::transcription::TranscriptionManager;

const RATE: f64 = WHISPER_SAMPLE_RATE as f64;
/// A voice with less than this share of the talking is laughter, crosstalk
/// or a moment the model split off, not another person.
const MIN_SHARE: f64 = 0.08;
/// A piece of a segment shorter than this joins its neighbour: too short to
/// transcribe by itself, and usually a blip in the model's answer.
const MIN_PIECE_SECS: f64 = 1.0;

/// One stretch of one voice, in seconds from the start of the meeting.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Turn {
    pub start: f64,
    pub end: f64,
    /// 1 is whoever talked most.
    pub voice: u32,
}

/// The name a voice goes by until the user renames it.
pub fn voice_name(voice: u32) -> String {
    format!("Speaker {voice}")
}

// ---- The model file ----

#[derive(Deserialize)]
struct Catalog {
    models: Vec<CatalogModel>,
}

#[derive(Deserialize)]
struct CatalogModel {
    role: String,
    repository: String,
    revision: String,
    quant: String,
    files: BTreeMap<String, CatalogFile>,
}

#[derive(Deserialize)]
struct CatalogFile {
    filename: String,
    size: u64,
    sha256: String,
}

struct ModelFile {
    url: String,
    filename: String,
    size: u64,
    sha256: String,
}

/// The diarization model Handy's catalog lists.
fn catalog_model() -> Result<ModelFile> {
    let catalog: Catalog = serde_json::from_str(include_str!("../catalog/catalog.json"))
        .context("Failed to read the model catalog")?;
    let model = catalog
        .models
        .into_iter()
        .find(|model| model.role == "diarize")
        .ok_or_else(|| anyhow!("The model catalog has no speaker model"))?;
    let file = model
        .files
        .get(&model.quant)
        .ok_or_else(|| anyhow!("The speaker model has no default file"))?;
    Ok(ModelFile {
        url: format!(
            "https://huggingface.co/{}/resolve/{}/{}",
            model.repository, model.revision, file.filename
        ),
        filename: file.filename.clone(),
        size: file.size,
        sha256: file.sha256.clone(),
    })
}

fn model_path(app: &AppHandle, file: &ModelFile) -> Result<PathBuf> {
    Ok(crate::portable::app_data_dir(app)?
        .join("meetings")
        .join("models")
        .join(&file.filename))
}

/// The speaker model on disk, if it has been downloaded.
fn installed_model(app: &AppHandle) -> Option<PathBuf> {
    let file = catalog_model().ok()?;
    let path = model_path(app, &file).ok()?;
    let complete = std::fs::metadata(&path).is_ok_and(|meta| meta.len() == file.size);
    complete.then_some(path)
}

/// Fetches the speaker model in the background if it isn't there yet, so it
/// is ready by the time a meeting ends. One download at a time.
pub fn prepare(app: &AppHandle) {
    static DOWNLOADING: AtomicBool = AtomicBool::new(false);
    if installed_model(app).is_some() || DOWNLOADING.swap(true, Ordering::AcqRel) {
        return;
    }
    let app = app.clone();
    let spawned = std::thread::Builder::new()
        .name("meeting-speaker-model".into())
        .spawn(move || {
            match tauri::async_runtime::block_on(download(&app)) {
                Ok(path) => info!("Speaker model downloaded to {}", path.display()),
                Err(error) => warn!("Couldn't download the speaker model: {error:#}"),
            }
            DOWNLOADING.store(false, Ordering::Release);
        });
    if spawned.is_err() {
        DOWNLOADING.store(false, Ordering::Release);
    }
}

async fn download(app: &AppHandle) -> Result<PathBuf> {
    let file = catalog_model()?;
    let path = model_path(app, &file)?;
    let folder = path.parent().ok_or_else(|| anyhow!("No models folder"))?;
    std::fs::create_dir_all(folder)?;
    let partial = path.with_extension("partial");

    let response = reqwest::get(&file.url).await?.error_for_status()?;
    let mut body = response.bytes_stream();
    let mut out = std::fs::File::create(&partial)?;
    let mut hasher = Sha256::new();
    while let Some(chunk) = body.next().await {
        let chunk = chunk?;
        hasher.update(&chunk);
        std::io::Write::write_all(&mut out, &chunk)?;
    }
    drop(out);
    let digest = hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    if digest != file.sha256 {
        let _ = std::fs::remove_file(&partial);
        bail!("The speaker model download doesn't match its checksum");
    }
    std::fs::rename(&partial, &path)?;
    Ok(path)
}

// ---- Who spoke when ----

/// Runs the model over the call channel.
fn diarize(model: &Path, call: &[f32]) -> Result<Vec<Turn>> {
    let model = Model::load(model).map_err(|e| anyhow!("Failed to load the speaker model: {e}"))?;
    let mut session = model
        .diarize_session()
        .map_err(|e| anyhow!("Failed to start the speaker model: {e}"))?;
    // The offline operating point: the recording is finished, so the model
    // can look far ahead, which is markedly more accurate than real time.
    let options = DiarizeOptions {
        family: Some(DiarizeExtension::Sortformer(SortformerDiarizeOptions {
            preset: Some(SortformerPreset::VeryHighLatency),
        })),
    };
    let segments = session
        .run(call, &options)
        .map_err(|e| anyhow!("The speaker model failed: {e}"))?;
    Ok(segments
        .into_iter()
        .filter(|segment| segment.t1_ms > segment.t0_ms)
        .map(|segment| Turn {
            start: segment.t0_ms as f64 / 1000.0,
            end: segment.t1_ms as f64 / 1000.0,
            voice: segment.speaker_id.max(0) as u32,
        })
        .collect())
}

/// Keeps the voices that did a real share of the talking, numbered from the
/// one who talked most, and gives every other turn to whichever of them was
/// speaking nearest in time.
fn fold(mut turns: Vec<Turn>) -> Vec<Turn> {
    turns.sort_by(|a, b| a.start.total_cmp(&b.start));
    let mut talked: BTreeMap<u32, f64> = BTreeMap::new();
    for turn in &turns {
        *talked.entry(turn.voice).or_default() += turn.end - turn.start;
    }
    let total: f64 = talked.values().sum();
    let mut main: Vec<(u32, f64)> = talked
        .into_iter()
        .filter(|(_, secs)| total > 0.0 && secs / total >= MIN_SHARE)
        .collect();
    main.sort_by(|a, b| b.1.total_cmp(&a.1));
    let number = |voice: u32| {
        main.iter()
            .position(|(kept, _)| *kept == voice)
            .map(|at| at as u32 + 1)
    };

    let kept: Vec<Turn> = turns
        .iter()
        .filter_map(|turn| {
            Some(Turn {
                voice: number(turn.voice)?,
                ..*turn
            })
        })
        .collect();
    let nearest = |turn: &Turn| {
        kept.iter()
            .min_by(|a, b| gap(a, turn).total_cmp(&gap(b, turn)))
            .map(|near| near.voice)
    };
    let mut folded: Vec<Turn> = turns
        .iter()
        .filter_map(|turn| {
            let voice = number(turn.voice).or_else(|| nearest(turn))?;
            Some(Turn { voice, ..*turn })
        })
        .collect();
    folded.sort_by(|a, b| a.start.total_cmp(&b.start));
    folded
}

/// The silence between two turns; 0 when they touch or overlap.
fn gap(a: &Turn, b: &Turn) -> f64 {
    (a.start - b.end).max(b.start - a.end).max(0.0)
}

/// Cuts `start..end` where the voice changes. Returns each piece with its
/// voice; one piece when a single voice has it, none when no voice was heard
/// anywhere near it.
fn pieces(start: f64, end: f64, turns: &[Turn]) -> Vec<Turn> {
    // Each voice's turns inside the segment, in time order.
    let mut inside: Vec<Turn> = turns
        .iter()
        .filter(|turn| turn.end > start && turn.start < end)
        .map(|turn| Turn {
            start: turn.start.max(start),
            end: turn.end.min(end),
            voice: turn.voice,
        })
        .collect();
    if inside.is_empty() {
        // Speech the model didn't mark: the nearest voice in time has it.
        let whole = Turn {
            start,
            end,
            voice: 0,
        };
        return turns
            .iter()
            .min_by(|a, b| gap(a, &whole).total_cmp(&gap(b, &whole)))
            .map(|near| {
                vec![Turn {
                    voice: near.voice,
                    ..whole
                }]
            })
            .unwrap_or_default();
    }
    inside.sort_by(|a, b| a.start.total_cmp(&b.start));

    // Runs of one voice. A run reaches to where the next voice starts.
    let mut runs: Vec<Turn> = Vec::new();
    for turn in inside {
        match runs.last_mut() {
            Some(last) if last.voice == turn.voice => last.end = last.end.max(turn.end),
            Some(last) => {
                last.end = turn.start.max(last.start);
                runs.push(turn);
            }
            None => runs.push(turn),
        }
    }
    // A run too short to stand alone joins the one before it (or after).
    let mut merged: Vec<Turn> = Vec::new();
    for run in runs {
        match merged.last_mut() {
            Some(last) if run.end - run.start < MIN_PIECE_SECS || last.voice == run.voice => {
                last.end = run.end.max(last.end);
            }
            _ => merged.push(run),
        }
    }
    if merged.len() > 1 && merged[0].end - merged[0].start < MIN_PIECE_SECS {
        let first = merged.remove(0);
        merged[0].start = first.start;
    }
    // The pieces cover the whole segment, edge to edge.
    if let Some(first) = merged.first_mut() {
        first.start = start;
    }
    if let Some(last) = merged.last_mut() {
        last.end = end;
    }
    for at in 1..merged.len() {
        merged[at].start = merged[at - 1].end;
    }
    merged
}

// ---- Labelling a finished meeting ----

/// The call channel (right) of a finished recording.
fn call_channel(recording: &Path) -> Result<Vec<f32>> {
    let mut reader = hound::WavReader::open(recording)
        .with_context(|| format!("Failed to open {}", recording.display()))?;
    reader
        .samples::<i16>()
        .skip(1)
        .step_by(2)
        .map(|sample| Ok(f32::from(sample?) / 32768.0))
        .collect()
}

/// Gives each `Them` segment its speaker, splitting and re-transcribing the
/// ones two people share. If the model isn't downloaded yet or anything
/// fails, the transcript comes back as it was.
pub fn label(
    app: &AppHandle,
    recording: &Path,
    transcript: Vec<MeetingSegment>,
) -> Vec<MeetingSegment> {
    let Some(model) = installed_model(app) else {
        info!("The speaker model isn't downloaded yet; speakers stay unseparated");
        prepare(app);
        return transcript;
    };
    match label_with(app, &model, recording, &transcript) {
        Ok(labelled) => labelled,
        Err(error) => {
            warn!("Couldn't separate speakers: {error:#}");
            transcript
        }
    }
}

fn label_with(
    app: &AppHandle,
    model: &Path,
    recording: &Path,
    transcript: &[MeetingSegment],
) -> Result<Vec<MeetingSegment>> {
    let started = std::time::Instant::now();
    let call = call_channel(recording)?;
    let turns = fold(diarize(model, &call)?);
    let voices = turns.iter().map(|turn| turn.voice).max().unwrap_or(0);
    if voices == 0 {
        return Ok(transcript.to_vec());
    }

    let manager = Arc::clone(app.state::<Arc<TranscriptionManager>>().inner());
    let control = YieldToDictation { app };
    let mut labelled: Vec<MeetingSegment> = Vec::new();
    let mut split = 0;
    for segment in transcript {
        if segment.speaker != Speaker::Them {
            labelled.push(segment.clone());
            continue;
        }
        let parts = pieces(segment.start, segment.end, &turns);
        match parts.as_slice() {
            [] => labelled.push(segment.clone()),
            [only] => labelled.push(MeetingSegment {
                name: voice_name(only.voice),
                ..segment.clone()
            }),
            several => {
                split += 1;
                for part in several {
                    let from = ((part.start * RATE) as usize).min(call.len());
                    let to = ((part.end * RATE) as usize).min(call.len());
                    let text = manager
                        .transcribe_in_chunks(call[from..to].to_vec(), &control)?
                        .map(|transcription| transcription.text)
                        .unwrap_or_default();
                    let text = text.trim();
                    if text.is_empty() {
                        continue;
                    }
                    labelled.push(MeetingSegment {
                        speaker: Speaker::Them,
                        name: voice_name(part.voice),
                        start: part.start,
                        end: part.end,
                        text: text.to_string(),
                    });
                }
            }
        }
    }
    labelled.sort_by(|a, b| a.start.total_cmp(&b.start));
    info!(
        "Separated {voices} speakers in {:.1}s ({split} shared segments split)",
        started.elapsed().as_secs_f64()
    );
    Ok(labelled)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn turn(start: f64, end: f64, voice: u32) -> Turn {
        Turn { start, end, voice }
    }

    #[test]
    fn the_catalog_lists_a_speaker_model() {
        let file = catalog_model().unwrap();
        assert!(file
            .url
            .starts_with("https://huggingface.co/handy-computer/"));
        assert!(file.filename.ends_with(".gguf"));
        assert_eq!(file.sha256.len(), 64);
        assert!(file.size > 0);
    }

    #[test]
    fn small_voices_fold_into_the_main_ones_numbered_by_talk_time() {
        let folded = fold(vec![
            turn(0.0, 10.0, 3),
            turn(10.0, 40.0, 1),
            // A scrap right after voice 1 was speaking.
            turn(40.5, 41.5, 4),
            turn(50.0, 60.0, 3),
        ]);
        // Voice 1 talked most (30 s), then voice 3 (20 s).
        assert_eq!(
            folded.iter().map(|t| t.voice).collect::<Vec<_>>(),
            vec![2, 1, 1, 2]
        );
    }

    #[test]
    fn one_voice_keeps_the_whole_segment() {
        let turns = [turn(0.0, 4.0, 1), turn(4.5, 9.0, 1), turn(20.0, 30.0, 2)];
        assert_eq!(pieces(1.0, 8.0, &turns), vec![turn(1.0, 8.0, 1)]);
    }

    #[test]
    fn a_shared_segment_is_cut_where_the_voice_changes() {
        let turns = [turn(0.0, 5.0, 1), turn(5.2, 9.0, 2)];
        assert_eq!(
            pieces(0.0, 9.0, &turns),
            vec![turn(0.0, 5.2, 1), turn(5.2, 9.0, 2)]
        );
    }

    #[test]
    fn a_blip_of_another_voice_does_not_cut_the_segment() {
        let turns = [turn(0.0, 5.0, 1), turn(5.0, 5.4, 2), turn(5.4, 9.0, 1)];
        assert_eq!(pieces(0.0, 9.0, &turns), vec![turn(0.0, 9.0, 1)]);
    }

    #[test]
    fn speech_the_model_missed_takes_the_nearest_voice() {
        let turns = [turn(0.0, 5.0, 1), turn(30.0, 40.0, 2)];
        assert_eq!(pieces(26.0, 28.0, &turns), vec![turn(26.0, 28.0, 2)]);
        assert!(pieces(1.0, 2.0, &[]).is_empty());
    }
}
