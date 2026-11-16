import { useEffect, useRef, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { CheckCircle2, LoaderCircle } from "lucide-react";
import { Dialog, FormError } from "./components";
import { formatSize, type Disk, type Partition } from "./types";

const choices = [
  ["hide", "隐藏分区"],
  ["show", "显示分区"],
  ["readOnly", "分区设为只读"],
  ["readWrite", "解除分区只读"],
  ["noDefaultLetter", "禁止自动分配盘符"],
  ["automaticLetter", "允许自动分配盘符"],
] as const;
type Action = (typeof choices)[number][0];
interface Attributes {
  hidden: boolean;
  readOnly: boolean;
  noDefaultLetter: boolean;
}
interface Preview {
  token: string;
  title: string;
  target: string;
  partition: number;
  offset: number;
  size: number;
  before: Attributes;
  after: Attributes;
  warning: string;
}
const rows = [
  ["hidden", "隐藏"],
  ["readOnly", "只读"],
  ["noDefaultLetter", "禁止自动盘符"],
] as const;
export function PartitionAttributesDialog({
  disk,
  partition,
  onClose,
  onBusy,
  onRefresh,
}: {
  disk: Disk;
  partition: Partition;
  onClose: () => void;
  onBusy: (busy: boolean) => void;
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
      void invoke("partition_attributes_discard", {
        token: token.current,
      }).catch(() => {});
    token.current = null;
  }
  useEffect(() => () => discard(), []);
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
        const outcome = await invoke<{ message: string; sha256: string }>(
          "partition_attributes_execute",
          { token: once },
        );
        setResult(outcome.message);
        setAction(null);
      } else {
        discard();
        setPreview(null);
        const next = await invoke<Preview>("partition_attributes_prepare", {
          request: {
            source: {
              target: disk.hardware?.identity,
              layoutKey: disk.hardware?.layoutKey,
              partition: partition.number,
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
      title="分区属性"
      subtitle={`${disk.name} · 分区 ${partition.number} · ${formatSize(partition.size)}`}
      onClose={() => {
        if (!inFlight.current) onClose();
      }}
    >
      <div
        className="partition-attribute-options"
        role="group"
        aria-label="选择分区属性操作"
      >
        {choices.map(([value, title]) => (
          <button
            key={value}
            type="button"
            className={`button ${action === value ? "attribute-selected" : ""}`}
            aria-pressed={action === value}
            disabled={busy}
            onClick={() => {
              discard();
              setAction(value);
              setPreview(null);
              setError("");
              setResult("");
            }}
          >
            {title}
          </button>
        ))}
      </div>
      {!preview && !result && (
        <p className="table-backup-intro">
          适用于普通 GPT Windows
          数据分区。选择一项操作，预览时读取并核对当前属性。
        </p>
      )}
      {preview && (
        <div className="table-backup-result" aria-label="分区属性预览">
          <strong>
            {preview.target} · 分区 {preview.partition}
          </strong>
          <p>
            {formatSize(preview.size)} · 起始位置 {formatSize(preview.offset)}
          </p>
          <table className="partition-attribute-table">
            <thead>
              <tr>
                <th>属性</th>
                <th>当前</th>
                <th>变更后</th>
              </tr>
            </thead>
            <tbody>
              {rows.map(([key, title]) => (
                <tr
                  key={key}
                  className={
                    preview.before[key] !== preview.after[key]
                      ? "attribute-changed"
                      : ""
                  }
                >
                  <th>{title}</th>
                  <td>{preview.before[key] ? "是" : "否"}</td>
                  <td>{preview.after[key] ? "是" : "否"}</td>
                </tr>
              ))}
            </tbody>
          </table>
          <p className="table-backup-intro">{preview.warning}</p>
        </div>
      )}
      {busy && (
        <p className="table-backup-progress" role="status">
          <LoaderCircle size={17} className="spin" />
          正在核对设备和分区属性…
        </p>
      )}
      <FormError message={error} />
      {result && (
        <div className="table-backup-result" role="status">
          <CheckCircle2 size={19} />
          <p>{result}</p>
        </div>
      )}
      <div className="dialog-actions">
        <button className="button" disabled={busy} onClick={onClose}>
          关闭
        </button>
        <button
          className="button primary"
          disabled={busy || !action}
          onClick={() => void run(!!preview)}
        >
          {preview ? `确认${preview.title}` : "预览变更"}
        </button>
      </div>
    </Dialog>
  );
}
