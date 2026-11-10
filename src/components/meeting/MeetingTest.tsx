import React, { useState } from "react";
import { useTranslation } from "react-i18next";
import { commands, type CallAudioSource } from "@/bindings";
import { Button } from "../ui/Button";
import { SettingsGroup } from "../ui/SettingsGroup";

type Status =
  | { state: "idle" }
  | { state: "recording"; callAudio: CallAudioSource; path: string }
  | { state: "saved"; path: string; seconds: number }
  | { state: "error"; message: string };

// Bare screen for testing meeting mode. Not the real interface.
export const MeetingTest: React.FC = () => {
  const { t } = useTranslation();
  const [status, setStatus] = useState<Status>({ state: "idle" });
  const [busy, setBusy] = useState(false);
  const recording = status.state === "recording";

  const toggle = async () => {
    setBusy(true);
    try {
      if (recording) {
        const result = await commands.meetingStop();
        setStatus(
          result.status === "ok"
            ? {
                state: "saved",
                path: result.data.path,
                seconds: Math.round(result.data.duration_secs),
              }
            : { state: "error", message: result.error },
        );
      } else {
        const result = await commands.meetingStart();
        setStatus(
          result.status === "ok"
            ? {
                state: "recording",
                callAudio: result.data.call_audio,
                path: result.data.path,
              }
            : { state: "error", message: result.error },
        );
      }
    } finally {
      setBusy(false);
    }
  };

  return (
    <div className="max-w-3xl w-full mx-auto space-y-6">
      <SettingsGroup title={t("meeting.title")}>
        <div className="p-4 space-y-3">
          <Button onClick={toggle} disabled={busy}>
            {recording ? t("meeting.stop") : t("meeting.start")}
          </Button>
          {status.state === "recording" && (
            <p className="text-sm">
              {status.callAudio === "app"
                ? t("meeting.recordingApp")
                : t("meeting.recordingSystem")}
            </p>
          )}
          {status.state === "saved" && (
            <p className="text-sm">
              {t("meeting.saved", { seconds: status.seconds })}
            </p>
          )}
          {status.state === "error" && (
            <p className="text-sm text-red-500">{status.message}</p>
          )}
          {(status.state === "recording" || status.state === "saved") && (
            <p className="text-xs opacity-70 break-all select-text">
              {status.path}
            </p>
          )}
        </div>
      </SettingsGroup>
    </div>
  );
};
