import React, { useEffect, useState } from "react";
import { useTranslation } from "react-i18next";
import { open } from "@tauri-apps/plugin-dialog";
import {
  commands,
  events,
  type CallAudioSource,
  type MeetingSegment,
  type TagKind,
} from "@/bindings";
import { useSettings } from "../../hooks/useSettings";
import { Button } from "../ui/Button";
import { Input } from "../ui/Input";
import { SettingsGroup } from "../ui/SettingsGroup";

interface SavedMeeting {
  notePath: string;
  audioPath: string | null;
  seconds: number;
  dealers: string[];
  tickets: string[];
}

type Status =
  | { state: "idle" }
  | { state: "recording"; callAudio: CallAudioSource }
  | { state: "saved"; meeting: SavedMeeting }
  | { state: "error"; message: string };

const clock = (seconds: number) => {
  const whole = Math.floor(seconds);
  const part = (value: number) => String(value).padStart(2, "0");
  return `${part(Math.floor(whole / 3600))}:${part(Math.floor(whole / 60) % 60)}:${part(whole % 60)}`;
};

// Bare screen for testing meeting mode. Not the real interface.
export const MeetingTest: React.FC = () => {
  const { t } = useTranslation();
  const { settings, refreshSettings } = useSettings();
  const [status, setStatus] = useState<Status>({ state: "idle" });
  const [busy, setBusy] = useState(false);
  const [segments, setSegments] = useState<MeetingSegment[]>([]);
  const [title, setTitle] = useState("");
  const [newTitle, setNewTitle] = useState("");
  const [speakerName, setSpeakerName] = useState("");
  const [dealers, setDealers] = useState<string | null>(null);
  const recording = status.state === "recording";
  const folder = settings?.meeting_output_folder ?? null;
  const dealersText = dealers ?? (settings?.meeting_dealers ?? []).join(", ");

  useEffect(() => {
    const unlisten = events.meetingSegment.listen((event) => {
      setSegments((current) =>
        [...current, event.payload].sort((a, b) => a.start - b.start),
      );
    });
    return () => {
      unlisten.then((stop) => stop());
    };
  }, []);

  const chooseFolder = async () => {
    const picked = await open({ directory: true, multiple: false });
    if (typeof picked !== "string") return;
    await commands.setMeetingOutputFolder(picked);
    await refreshSettings();
  };

  const saveDealers = async () => {
    await commands.setMeetingDealers(dealersText.split(","));
    await refreshSettings();
    setDealers(null);
  };

  const toggle = async () => {
    setBusy(true);
    try {
      if (recording) {
        const result = await commands.meetingStop();
        if (result.status === "ok") {
          setSegments(result.data.transcript);
          setStatus({
            state: "saved",
            meeting: {
              notePath: result.data.note_path,
              audioPath: result.data.audio_path,
              seconds: Math.round(result.data.duration_secs),
              dealers: result.data.dealers,
              tickets: result.data.tickets,
            },
          });
        } else {
          setStatus({ state: "error", message: result.error });
        }
      } else {
        setSegments([]);
        const result = await commands.meetingStart(title.trim() || null);
        setStatus(
          result.status === "ok"
            ? { state: "recording", callAudio: result.data.call_audio }
            : { state: "error", message: result.error },
        );
      }
    } finally {
      setBusy(false);
    }
  };

  const rename = async (meeting: SavedMeeting) => {
    const result = await commands.meetingRename(meeting.notePath, newTitle);
    if (result.status === "ok") {
      // The audio file is renamed along with the note.
      const audioPath =
        meeting.audioPath &&
        result.data.replace(/\.md$/, meeting.audioPath.slice(-4));
      setStatus({
        state: "saved",
        meeting: { ...meeting, notePath: result.data, audioPath },
      });
      setNewTitle("");
    } else {
      setStatus({ state: "error", message: result.error });
    }
  };

  const renameSpeaker = async (meeting: SavedMeeting, from: string) => {
    const to = speakerName.trim();
    const result = await commands.meetingRenameSpeaker(
      meeting.notePath,
      from,
      to,
    );
    if (result.status !== "ok") {
      setStatus({ state: "error", message: result.error });
      return;
    }
    setSegments((current) =>
      current.map((segment) =>
        segment.name === from ? { ...segment, name: to } : segment,
      ),
    );
    setSpeakerName("");
  };

  const removeTag = async (
    meeting: SavedMeeting,
    kind: TagKind,
    tag: string,
  ) => {
    const result = await commands.meetingRemoveTag(meeting.notePath, kind, tag);
    if (result.status !== "ok") {
      setStatus({ state: "error", message: result.error });
      return;
    }
    const without = (tags: string[]) => tags.filter((other) => other !== tag);
    setStatus({
      state: "saved",
      meeting:
        kind === "dealer"
          ? { ...meeting, dealers: without(meeting.dealers) }
          : { ...meeting, tickets: without(meeting.tickets) },
    });
  };

  const saved = status.state === "saved" ? status.meeting : null;
  const others = [
    ...new Set(
      segments
        .filter((segment) => segment.speaker === "Them")
        .map((segment) => segment.name),
    ),
  ].sort();
  const tags: [TagKind, string][] = saved
    ? [
        ...saved.dealers.map((tag): [TagKind, string] => ["dealer", tag]),
        ...saved.tickets.map((tag): [TagKind, string] => ["ticket", tag]),
      ]
    : [];

  return (
    <div className="max-w-3xl w-full mx-auto space-y-6">
      <SettingsGroup title={t("meeting.notes")}>
        <div className="p-4 space-y-3">
          <div className="flex items-center gap-3">
            <Button onClick={chooseFolder} disabled={recording}>
              {t("meeting.chooseFolder")}
            </Button>
            <p className="text-xs opacity-70 break-all select-text">
              {folder ?? t("meeting.noFolder")}
            </p>
          </div>
          <label className="flex items-center gap-2 text-sm">
            <input
              type="checkbox"
              checked={settings?.meeting_keep_audio ?? true}
              onChange={async (event) => {
                await commands.setMeetingKeepAudio(event.target.checked);
                await refreshSettings();
              }}
            />
            {t("meeting.keepAudio")}
          </label>
          <div className="flex items-center gap-3">
            <Input
              className="flex-1"
              value={dealersText}
              placeholder={t("meeting.dealers")}
              onChange={(event) => setDealers(event.target.value)}
            />
            <Button onClick={saveDealers} disabled={dealers === null}>
              {t("meeting.saveDealers")}
            </Button>
          </div>
        </div>
      </SettingsGroup>
      <SettingsGroup title={t("meeting.title")}>
        <div className="p-4 space-y-3">
          <div className="flex items-center gap-3">
            <Input
              className="flex-1"
              value={title}
              placeholder={t("meeting.titlePlaceholder")}
              disabled={recording}
              onChange={(event) => setTitle(event.target.value)}
            />
            <Button onClick={toggle} disabled={busy}>
              {recording ? t("meeting.stop") : t("meeting.start")}
            </Button>
          </div>
          {status.state === "recording" && (
            <p className="text-sm">
              {status.callAudio === "app"
                ? t("meeting.recordingApp")
                : t("meeting.recordingSystem")}
            </p>
          )}
          {status.state === "error" && (
            <p className="text-sm text-red-500">{status.message}</p>
          )}
          {saved && (
            <>
              <p className="text-sm">
                {t("meeting.saved", { seconds: saved.seconds })}
              </p>
              <p className="text-xs opacity-70 break-all select-text">
                {saved.notePath}
              </p>
              {saved.audioPath && (
                <p className="text-xs opacity-70 break-all select-text">
                  {saved.audioPath}
                </p>
              )}
              <div className="flex items-center gap-3">
                <Input
                  className="flex-1"
                  value={newTitle}
                  placeholder={t("meeting.newTitle")}
                  onChange={(event) => setNewTitle(event.target.value)}
                />
                <Button
                  onClick={() => rename(saved)}
                  disabled={!newTitle.trim()}
                >
                  {t("meeting.rename")}
                </Button>
              </div>
              {others.length > 0 && (
                <div className="flex flex-wrap items-center gap-2">
                  <Input
                    className="flex-1"
                    value={speakerName}
                    placeholder={t("meeting.speakerName")}
                    onChange={(event) => setSpeakerName(event.target.value)}
                  />
                  {others.map((name) => (
                    <Button
                      key={name}
                      onClick={() => renameSpeaker(saved, name)}
                      disabled={!speakerName.trim()}
                    >
                      {t("meeting.renameSpeaker", { name })}
                    </Button>
                  ))}
                </div>
              )}
              {tags.length > 0 && (
                <div className="flex flex-wrap gap-2">
                  {tags.map(([kind, tag]) => (
                    <Button
                      key={`${kind}-${tag}`}
                      onClick={() => removeTag(saved, kind, tag)}
                    >
                      {t("meeting.removeTag", { tag })}
                    </Button>
                  ))}
                </div>
              )}
            </>
          )}
        </div>
      </SettingsGroup>
      {segments.length > 0 && (
        <SettingsGroup title={t("meeting.transcript")}>
          <div className="p-4 space-y-3 select-text">
            {segments.map((segment) => (
              <div key={`${segment.name}-${segment.start}`}>
                <p className="text-xs opacity-70">
                  <span className="font-semibold">{segment.name}</span>{" "}
                  {clock(segment.start)}
                </p>
                <p className="text-sm">{segment.text}</p>
              </div>
            ))}
          </div>
        </SettingsGroup>
      )}
    </div>
  );
};
