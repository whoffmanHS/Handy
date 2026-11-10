# Handy Fork: Meeting Mode

Build plan for Claude Code. Drop this file in the repo root of the Handy fork and start with: "Read AGENTS.md and MEETING_MODE_PLAN.md, then do Phase 1."

## Goal

Add a meeting mode to Handy that records a call with no bot, transcribes it locally, labels who spoke (me vs them), and saves the transcript as a markdown file in my Obsidian vault. Push-to-talk dictation must keep working exactly as it does now.

## Ground rules

- Read `AGENTS.md` first and follow its conventions (manager pattern, commands and events, notices, tauri-specta bindings, lint and format before committing).
- Put all new backend code in a new module, `src-tauri/src/meeting/`, plus new files under `commands/` and `src/components/meeting/`. Touch existing files only to register things (lib.rs, settings.rs, bindings). This keeps upstream merges easy.
- Do not change the push-to-talk pipeline in `transcription_coordinator.rs` or `managers/audio.rs`. Reuse their pieces; don't edit their behavior.
- macOS only for this fork. Gate meeting mode at runtime so the rest of the app still runs on older macOS.
- Build it so coworkers can use it later: no hardcoded paths, names, keys or vault locations. Everything personal lives in settings.
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
- Custom vocabulary: a word list in settings (brand, dealer and people names like Sub-Zero, Thermador, Fisher & Paykel) passed to the model as a prompt or bias, used by meeting mode AND push-to-talk dictation. Check whether Handy already has a custom words feature and extend it rather than building a second one.
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
audio: "[[2026-11-10 1402 Meeting.m4a]]"
tags: [meeting]
---

# Meeting 2026-11-10 14:02

**Me** 00:00:04
Text...

**Them** 00:00:11
Text...
```

- Calendar: read my calendar through macOS EventKit (my work Outlook account is added to Apple Calendar). At start, match the event happening now, preferring one with a Teams link. Use its title for the note and filename, and put its attendees in the frontmatter (`attendees: [...]`). No match: fall back to the default title. Ask for calendar permission the first time, and never block recording if it's denied.
- Default filename: `YYYY-MM-DD HHmm <Meeting title>.md`, falling back to `YYYY-MM-DD HHmm Meeting.md`. Let me rename the title from the UI before or after saving.
- If keep audio is on, save it next to the note as compressed audio (AAC .m4a or Opus), not WAV. Otherwise delete it after the note saves successfully.
- Audio retention setting: auto-delete meeting audio older than N days (default 60, or never), always keeping the note. Notes whose audio is gone still open fine; replay just says the audio was removed.
- Auto-stop: if the call app closes, or both channels stay silent for a set time (default 3 minutes), stop and save automatically and trim the trailing silence.
- Crash recovery: if Handy quits or the Mac shuts down mid-call, detect the unfinished recording on next launch and offer to transcribe and save it.
- Tagging: tag each note with any dealer names from the custom vocabulary list and any Jira keys (pattern like `PMNAMC-72`) found in the calendar title or transcript. Add them to frontmatter as `dealers: [...]` and `tickets: [...]` and as Obsidian tags, so a dealer's calls are findable together. Let me remove a wrong tag from the UI.
- Done when: stopping a call drops a clean note into my vault and Obsidian shows it immediately, a call I forget to stop saves itself, and a force-quit mid-call is recoverable.

## Phase 4: Clean API for my own UI

I'm designing my own interface, so the backend must be fully drivable without Handy's existing settings screens.

Commands:
- `meeting_start(title?)`, `meeting_stop()`, `meeting_pause()`, `meeting_resume()`, `meeting_status()`, `meeting_rename(path, title)`, `meeting_bookmark(label?)`

Events:
- `meeting-state` (idle, recording, paused, finishing, saved, error)
- `meeting-segment` (speaker, start, end, text) as each segment is transcribed
- `meeting-levels` (me, them) roughly 10 times a second, for meters
- `meeting-bookmark` (time, label)
- `meeting-saved` (note path, audio path)

Also add a tray menu item and a global shortcut to start and stop a meeting, and a second shortcut that drops a bookmark. Bookmarks show in the note as a marked line at that timestamp (`> ★ 00:14:21 label`).

Build only a bare functional screen in `src/components/meeting/` to test with: start/stop, two level meters, live transcript list. No styling work; I'll design the real one.

Done when: every action in the UI goes through these commands and events, documented in `src/components/meeting/README.md`.

## Phase 5: iPad meeting canvas

A Pencil note page served by Handy itself, opened in Safari on my iPad and added to the home screen. No App Store app, no hosting, nothing leaves my network.

- Local server: Handy serves the canvas page and a WebSocket API on the home network, pinned to my devices with a pairing code shown on the Mac. It reuses the Phase 4 commands and events, so the iPad can start, stop and watch a meeting.
- Canvas: fixed width, grows downward only as I write (no sideways panning). Pen, highlighter, eraser, lasso, three ink colors. Pencil writes, finger scrolls.
- Every stroke stores its points plus the meeting clock time it was drawn. Mac and iPad agree on one clock at meeting start.
- Bookmark button on the iPad page that calls `meeting_bookmark`.
- Tap to replay: tapping a stroke plays the call audio from a few seconds before that moment and highlights the matching transcript segment.
- Auto-link: if the page is open when a meeting starts on the Mac, it switches to a fresh page tied to that meeting. If it's closed, send a web push notification that opens it.
- On stop, save the page as one tall PDF next to the meeting note in the vault, embed it in the note, and keep stroke data in a sidecar JSON so replay still works later.
- Design: build to my Handy Meeting Canvas mockups (writing, replay, idle states). HomeSource Gotham SSm, greys, red only for recording, stop and replay.
- Done when: I write on the iPad during a Teams call, tap a note afterward, and hear what was said when I wrote it.

## Phase 6: Replace Otter and Wispr Flow

- Speaker separation: run a local speaker-diarization model on the `Them` channel after the call so group calls become Speaker 1, Speaker 2, and so on. In the note and my UI, let me rename each speaker once, with the calendar attendees offered as choices, and update every line. `Me` stays from the mic channel. Tell me which model and why before building.
- Summary and action items: after saving, generate a short summary, decisions, and action items with owners, and add them to the top of the note under `## Summary` and `## Action items` (checkbox list). Use Apple Intelligence's on-device model through Handy's existing `apple_intelligence.rs`, so nothing leaves the Mac and there's no key or extra install. Its context window is small, so summarize the transcript in chunks and then combine the chunk summaries. If Apple Intelligence isn't available on the Mac, skip the summary quietly and still save the note. Keep the provider swappable so an API can be added later, but don't build one now.
- Import: drag in an audio or video file (m4a, mp3, wav, mp4, mov) and get the same note, run through the same pipeline. Speakers come from diarization since there's only one channel.
- Dictation cleanup for push-to-talk: remove filler words and fix punctuation and capitalization before pasting, with a raw mode toggle. Check how far Handy's existing post-processing already goes and extend it.
- Done when: a 4-person Teams call produces a note with named speakers, a summary and action items, and push-to-talk output reads clean without ums.

## Phase 7 (later, don't build yet)

- Prompt to start meeting mode when Teams (or another chosen meeting app) starts using the mic.
- Recording consent reminder at start (Pennsylvania is two-party consent).

## Phase 8: After it's real (don't build yet)

Only after I've used Phases 1 to 6 on real calls for a while.

- Last-time recap: when a meeting starts and its dealer tag or attendees match an earlier note, show that note's unchecked action items on the Mac and iPad screens.
- Action items to Reminders: action items assigned to me go into Apple Reminders through EventKit, each linking back to its meeting note. Never post to Jira or anywhere else automatically.
- Tempo suggestions: from each note's duration and ticket tags, draft a time entry for me to review and confirm. Nothing is logged without my OK.

## Out of scope

Slide or screen capture, sharing links, cloud storage or sync, Windows and Linux support, a native iPad app.
