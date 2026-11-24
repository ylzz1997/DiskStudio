import { useEffect, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { errorText } from "./partitionOutcome";

type Preview<T> = { token: string; summary: T };
type Report<T> = { backupPath: string; summary: T };

export function useBootRepair<T>(
  onBusy: (busy: boolean) => boolean,
  canceledMessage: string,
  onChanged: () => void,
) {
  const [preview, setPreview] = useState<Preview<T> | null>(null);
  const [result, setResult] = useState<Report<T> | null>(null);
  const [phase, setPhase] = useState<"preview" | "apply" | null>(null);
  const [message, setMessage] = useState("");
  const [error, setError] = useState("");
  const token = preview?.token;

  useEffect(() => {
    if (!token) return;
    return () => {
      void invoke("data_discard_boot_repair", { token }).catch(() => undefined);
    };
  }, [token]);

  async function run(kind: "preview" | "apply", action: () => Promise<void>) {
    if (!onBusy(true)) return;
    setPhase(kind);
    setMessage("");
    setError("");
    try {
      await action();
    } catch (error) {
      setError(errorText(error));
    } finally {
      setPhase(null);
      onBusy(false);
    }
  }

  function prepare(
    command: "data_preview_boot_repair" | "data_preview_boot_files",
    request: object,
  ) {
    return run("preview", async () => {
      setPreview(await invoke<Preview<T>>(command, { request }));
    });
  }

  function apply() {
    if (!token) return;
    return run("apply", async () => {
      try {
        const report = await invoke<Report<T> | null>(
          "data_apply_boot_repair",
          {
            token,
          },
        );
        if (report) {
          setResult(report);
          onChanged();
        } else {
          setMessage(canceledMessage);
        }
      } catch (error) {
        onChanged();
        throw error;
      } finally {
        setPreview(null);
      }
    });
  }

  return {
    preview,
    result,
    phase,
    error,
    message,
    prepare,
    apply,
    discard: () => setPreview(null),
  };
}
