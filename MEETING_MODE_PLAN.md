# Handy Fork: Meeting Mode

Build plan for Claude Code. Drop this file in the repo root of the Handy fork and start with: "Read AGENTS.md and MEETING_MODE_PLAN.md, then do Phase 1."

## Goal

Add a meeting mode to Handy that records a call with no bot, transcribes it locally, labels who spoke (me vs them), and saves the transcript as a markdown file in my Obsidian vault. Push-to-talk dictation must keep working exactly as it does now.

## Ground rules

- Read `AGENTS.md` first and follow its conventions (manager pattern, commands and events, notices, tauri-specta bindings, lint and format before committing).
- Put all new backend code in a new module, `src-tauri/src/meeting/`, plus new files under `commands/` and `src/components/meeting/`. Touch existing files only to register things (lib.rs, settings.rs, bindings). This keeps upstream merges easy.
- Do not change the push-to-talk pipeline in `transcription_coordinator.rs` or `managers/audio.rs`. Reuse their pieces; don't edit their behavior.
- macOS only for this fork. Gate meeting mode at runtime so the rest of the app still runs on older macOS.
- Plan each phase before writing code, show me the plan, and wait for my OK.
- After each phase, run the app and give me a short test checklist to run by hand.

## Phase 1: Capture call audio

- Our calls are Microsoft Teams. Capture call audio with a Core Audio process tap (macOS 14.4+) scoped to the Teams process only, so notifications, music and other apps stay out of the recording. Fall back to a whole-system tap if Teams isn't running, and make the target app a setting so Zoom or Meet can be picked later.
- The tap needs only the "System Audio Recording" permission, not Screen Recording. Evaluate `cidre` or direct `objc2` bindings and tell me which and why before building.
- Capture the mic with the existing `handy-recorder` path, on the same device Handy already uses.
- Resample both streams to 16 kHz mono, keep them as separate channels, and write them to a temp stereo WAV (left = me, right = them) as they record, so a crash doesn't lose the call.
- Add the permission to `Info.plist` / entitlements and a notice if it's denied.
- Done when: I can record a 5-minute Teams call and play back a WAV with my voice on the left and the call on the right.

## Phase 2: Transcribe long recordings

- Run VAD on each channel separately and transcribe speech segments as they finish, during the call, using the model Handy already has loaded. Look at `audio_toolkit/chunking.rs` and `managers/transcription_jobs.rs` before writing anything new.
- Each segment keeps its start time and speaker (`Me` or `Them`). Merge both channels into one timeline sorted by start time.
- Echo handling: on speakers, my mic also hears the call, which duplicates text. When the `Them` channel has speech in the same window, drop or down-weight overlapping `Me` segments whose text closely matches. Add a setting to turn this off when wearing headphones.
- No length cap. A 2-hour call must not run out of memory or block the push-to-talk shortcut.
- Done when: a 30-minute call gives a correct, speaker-labeled transcript within a minute of hitting stop.

## Phase 3: Save to the vault

- Settings: output folder (folder picker, defaults to empty and meeting mode refuses to start until set), filename pattern, keep audio yes/no.
- On stop, write one markdown file:

```markdown
---
date: 2026-11-10
start: 14:02
duration: 34m
type: meeting
audio: "[[2026-11-10 1402 Meeting.wav]]"
tags: [meeting]
---

# Meeting 2026-11-10 14:02

**Me** 00:00:04
Text...

**Them** 00:00:11
Text...
```

- Default filename: `YYYY-MM-DD HHmm Meeting.md`. Let me rename the title from the UI before or after saving.
- If keep audio is on, move the WAV next to the note. Otherwise delete it after the note saves successfully.
- Done when: stopping a call drops a clean note into my vault and Obsidian shows it immediately.

## Phase 4: Clean API for my own UI

I'm designing my own interface, so the backend must be fully drivable without Handy's existing settings screens.

Commands:
- `meeting_start(title?)`, `meeting_stop()`, `meeting_pause()`, `meeting_resume()`, `meeting_status()`, `meeting_rename(path, title)`

Events:
- `meeting-state` (idle, recording, paused, finishing, saved, error)
- `meeting-segment` (speaker, start, end, text) as each segment is transcribed
- `meeting-levels` (me, them) roughly 10 times a second, for meters
- `meeting-saved` (note path, audio path)

Also add a tray menu item and a global shortcut to start and stop a meeting.

Build only a bare functional screen in `src/components/meeting/` to test with: start/stop, two level meters, live transcript list. No styling work; I'll design the real one.

Done when: every action in the UI goes through these commands and events, documented in `src/components/meeting/README.md`.

## Phase 5 (later, don't build yet)

- Prompt to start meeting mode when Teams (or another chosen meeting app) starts using the mic.
- Recording consent reminder at start (Pennsylvania is two-party consent).
- Optional summary and action items appended to the note via `llm_client.rs`.

## Out of scope

Real speaker diarization beyond me vs them, cloud anything, Windows and Linux support, iPad.
