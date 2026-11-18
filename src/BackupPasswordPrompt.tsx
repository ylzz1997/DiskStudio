import { useEffect, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { LoaderCircle, LockKeyhole } from "lucide-react";
import { Dialog, FormError } from "./components";
import { native } from "./backend";
import { useLocale } from "./i18n/LanguageProvider";

interface Prompt {
  id: string;
  purpose: "create" | "unlock";
  filename: string;
}

// Memory-only status. No source-file or device access is triggered by polling.
export function BackupPasswordPrompt() {
  const [prompt, setPrompt] = useState<Prompt | null>(null);
  useEffect(() => {
    if (!native) return;
    let stopped = false;
    let timer: ReturnType<typeof setTimeout>;
    const poll = async () => {
      try {
        const next = await invoke<Prompt | null>("backup_password_status");
        if (!stopped) setPrompt(next);
      } catch {
        /* A disconnected application must not create a prompt. */
      }
      if (!stopped) timer = setTimeout(poll, 500);
    };
    void poll();
    return () => {
      stopped = true;
      clearTimeout(timer);
    };
  }, []);
  return prompt ? (
    <PasswordForm
      key={prompt.id}
      prompt={prompt}
      onDone={() => setPrompt(null)}
    />
  ) : null;
}
function PasswordForm({
  prompt,
  onDone,
}: {
  prompt: Prompt;
  onDone: () => void;
}) {
  const { t } = useLocale();
  const [value, setValue] = useState("");
  const [confirmation, setConfirmation] = useState("");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState("");
  const creating = prompt.purpose === "create";
  const submit = async (cancel: boolean) => {
    if (busy) return;
    if (
      !cancel &&
      (!value ||
        (creating && (Array.from(value).length < 8 || value !== confirmation)))
    ) {
      setError(
        creating
          ? t("请输入至少 8 个字符，并确保两次密码相同。")
          : t("请输入备份密码。"),
      );
      return;
    }
    setBusy(true);
    setError("");
    try {
      await invoke("backup_password_submit", {
        reply: {
          id: prompt.id,
          answer: cancel
            ? { kind: "cancel" }
            : {
                kind: "password",
                value,
                confirmation: creating ? confirmation : null,
              },
        },
      });
      onDone();
    } catch {
      setError(t("密码请求已变化或输入不符合要求，请重新输入或取消。"));
    } finally {
      setValue("");
      setConfirmation("");
      setBusy(false);
    }
  };
  return (
    <Dialog
      title={creating ? t("设置备份密码") : t("打开加密备份")}
      subtitle={prompt.filename}
      onClose={() => void submit(true)}
      footer={
        <>
          <button
            className="button"
            disabled={busy}
            onClick={() => void submit(true)}
          >
            {t("取消")}
          </button>
          <button
            className="button primary"
            form={`backup-password-${prompt.id}`}
            type="submit"
            disabled={busy || !value}
          >
            {busy ? (
              <LoaderCircle className="spin" size={16} />
            ) : (
              <LockKeyhole size={16} />
            )}
            {creating ? t("使用此密码") : t("解锁备份")}
          </button>
        </>
      }
    >
      <form
        id={`backup-password-${prompt.id}`}
        className="backup-password-form"
        onSubmit={(e) => {
          e.preventDefault();
          void submit(false);
        }}
      >
        <label>
          {t("备份密码")}
          <input
            type="password"
            value={value}
            maxLength={1024}
            autoComplete={creating ? "new-password" : "off"}
            spellCheck={false}
            disabled={busy}
            onChange={(e) => setValue(e.target.value)}
          />
        </label>
        {creating && (
          <label>
            {t("再次输入密码")}
            <input
              type="password"
              value={confirmation}
              maxLength={1024}
              autoComplete="new-password"
              spellCheck={false}
              disabled={busy}
              onChange={(e) => setConfirmation(e.target.value)}
            />
          </label>
        )}
        <p className="muted">
          {creating
            ? t("请妥善保管密码，打开和还原这份备份时需要使用。")
            : t("密码仅用于当前备份；其他依赖文件可能需要各自的密码。")}
        </p>
        <FormError message={error} />
      </form>
    </Dialog>
  );
}
