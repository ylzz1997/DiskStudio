import { useEffect, useRef, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import {
  CheckCircle2,
  LoaderCircle,
  LockKeyhole,
  Plug,
  Unplug,
  UnlockKeyhole,
} from "lucide-react";
import { Dialog, FormError } from "./components";
import { formatSize, type Disk } from "./types";
type Action = "online" | "offline" | "readOnly" | "readWrite";
interface State {
  offline: boolean;
  readOnly: boolean;
}
interface Preview {
  token: string;
  title: string;
  target: string;
  size: number;
  partitions: number;
  before: State;
  after: State;
  warning: string;
}
const choices = [
  {
    value: "online",
    title: "联机",
    detail: "允许 Windows 访问磁盘",
    icon: Plug,
  },
  {
    value: "offline",
    title: "脱机",
    detail: "停止 Windows 对各分区的访问",
    icon: Unplug,
  },
  {
    value: "readOnly",
    title: "设为只读",
    detail: "保留读取，禁止写入",
    icon: LockKeyhole,
  },
  {
    value: "readWrite",
    title: "解除只读",
    detail: "恢复磁盘写入权限",
    icon: UnlockKeyhole,
  },
] as const;
const label = (s: State) =>
  `${s.offline ? "脱机" : "联机"} · ${s.readOnly ? "只读" : "可读写"}`;
export function DeviceStateDialog({
  disk,
  onClose,
  onBusy,
  onRefresh,
}: {
  disk: Disk;
  onClose: () => void;
  onBusy: (b: boolean) => void;
  onRefresh: () => Promise<void>;
}) {
  const [action, setAction] = useState<Action | null>(null);
  const [preview, setPreview] = useState<Preview | null>(null);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState("");
  const [result, setResult] = useState("");
  const token = useRef<string | null>(null),
    inFlight = useRef(false);
  function discard() {
    if (token.current)
      void invoke("device_state_discard", { token: token.current }).catch(
        () => {},
      );
    token.current = null;
  }
  useEffect(() => () => discard(), []);
  const offline = disk.hardware?.offline,
    readOnly = disk.hardware?.deviceReadOnly;
  const known = typeof offline === "boolean" && typeof readOnly === "boolean";
  const protectedDisk =
    !known ||
    disk.hardware?.isSystem ||
    !disk.hardware?.identity ||
    !disk.hardware.layoutKey;
  function unavailable(value: Action) {
    return (
      !!protectedDisk ||
      (value === "online"
        ? !offline
        : value === "offline"
          ? offline
          : offline ||
            disk.scheme === "RAW" ||
            (value === "readOnly" ? readOnly : !readOnly))
    );
  }
  async function run(execute: boolean) {
    if (inFlight.current || !action || (execute && !token.current)) return;
    inFlight.current = true;
    setBusy(true);
    onBusy(true);
    setError("");
    setResult("");
    try {
      if (execute) {
        const once = token.current;
        token.current = null;
        setPreview(null);
        const outcome = await invoke<{ message: string }>(
          "device_state_execute",
          { token: once },
        );
        setResult(outcome.message);
        setAction(null);
      } else {
        discard();
        setPreview(null);
        const next = await invoke<Preview>("device_state_prepare", {
          request: {
            source: {
              target: disk.hardware?.identity,
              layoutKey: disk.hardware?.layoutKey,
              partition: null,
            },
            action,
          },
        });
        token.current = next.token;
        setPreview(next);
      }
    } catch (e) {
      setError(String(e));
    } finally {
      if (execute) await onRefresh().catch(() => {});
      inFlight.current = false;
      setBusy(false);
      onBusy(false);
    }
  }
  return (
    <Dialog
      title="更改设备状态"
      subtitle={`${disk.name} · ${formatSize(disk.size)}`}
      onClose={() => {
        if (!inFlight.current) onClose();
      }}
    >
      <p className="device-state-current">
        当前状态{" "}
        <strong>{known ? label({ offline, readOnly }) : "尚未确认"}</strong>
      </p>
      <div
        className="device-state-options"
        role="group"
        aria-label="目标设备状态"
      >
        {choices.map(({ value, title, detail, icon: Icon }) => (
          <button
            key={value}
            type="button"
            aria-pressed={action === value}
            disabled={busy || unavailable(value)}
            className={`device-state-option ${action === value ? "selected" : ""}`}
            onClick={() => {
              discard();
              setAction(value);
              setPreview(null);
              setError("");
              setResult("");
            }}
          >
            <Icon size={19} />
            <span>
              <strong>{title}</strong>
              <small>{detail}</small>
            </span>
          </button>
        ))}
      </div>
      {protectedDisk ? (
        <p className="table-backup-intro">
          系统磁盘或设备信息不完整，不能更改状态。
        </p>
      ) : (
        offline && (
          <p className="table-backup-intro">
            更改只读属性需要先单独联机；每次只执行所确认的一项操作。
          </p>
        )
      )}
      {busy && (
        <p className="table-backup-progress" role="status">
          <LoaderCircle className="spin" size={17} />
          正在核对设备并处理…
        </p>
      )}
      <FormError message={error} />
      {result && (
        <div className="table-backup-result" role="status">
          <CheckCircle2 size={19} />
          <p>{result}</p>
        </div>
      )}
      {preview && (
        <div className="table-backup-result" aria-label="设备状态预览">
          <strong>{preview.target}</strong>
          <p>
            {formatSize(preview.size)} · {preview.partitions} 个分区
          </p>
          <p className="device-state-transition">
            {label(preview.before)} <span aria-hidden="true">→</span>{" "}
            <strong>{label(preview.after)}</strong>
          </p>
          <p className="table-backup-intro">{preview.warning}</p>
        </div>
      )}
      <div className="dialog-actions">
        <button className="button" disabled={busy} onClick={onClose}>
          关闭
        </button>
        {preview ? (
          <button
            className="button primary"
            disabled={busy}
            onClick={() => void run(true)}
          >
            确认{preview.title}
          </button>
        ) : (
          <button
            className="button primary"
            disabled={busy || !action || unavailable(action)}
            onClick={() => void run(false)}
          >
            预览变更
          </button>
        )}
      </div>
    </Dialog>
  );
}
