import { useRef, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import {
  AlertTriangle,
  CheckCircle2,
  FileCheck2,
  LoaderCircle,
  Save,
} from "lucide-react";
import { Dialog, FormError } from "./components";
import type { Disk, TableReport } from "./types";
import { TableRestoreDialog } from "./TableRestoreDialog";

interface BackupReport {
  name: string;
  fileSha256: string;
  info: {
    scheme: string;
    size: number;
    sector: number;
    partitions: number;
    tableBytes: number;
    bootBytes: number;
    sha256: string;
    report: TableReport;
    layout: { name: string; offset: number; size: number }[];
  };
}
export function TableBackupDialog({
  disk,
  onClose,
  onBusy,
  onRefresh,
}: {
  disk: Disk;
  onClose: () => void;
  onBusy: (busy: boolean) => void;
  onRefresh: () => Promise<void>;
}) {
  const [restore, setRestore] = useState(false);
  const [boot, setBoot] = useState(true);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState("");
  const [result, setResult] = useState<BackupReport | null>(null);
  const [saved, setSaved] = useState(false);
  const inFlight = useRef(false);
  async function run(mode: "backup" | "verify") {
    if (inFlight.current) return;
    inFlight.current = true;
    setBusy(true);
    onBusy(true);
    setError("");
    setResult(null);
    try {
      const report =
        mode === "backup"
          ? await invoke<BackupReport | null>("table_backup", {
              request: {
                source: {
                  target: disk.hardware?.identity,
                  layoutKey: disk.hardware?.layoutKey,
                  partition: null,
                },
                includesBoot: boot,
              },
            })
          : await invoke<BackupReport | null>("table_verify");
      setResult(report);
      setSaved(mode === "backup");
    } catch (e) {
      setError(String(e));
    } finally {
      inFlight.current = false;
      setBusy(false);
      onBusy(false);
    }
  }
  if (restore)
    return (
      <TableRestoreDialog
        disk={disk}
        onClose={onClose}
        onBack={() => setRestore(false)}
        onBusy={onBusy}
        onRefresh={onRefresh}
      />
    );
  return (
    <Dialog
      title="分区表备份与恢复"
      subtitle={`${disk.name} · ${disk.hardware?.model ?? ""}`}
      wide
      onClose={() => {
        if (!inFlight.current) onClose();
      }}
    >
      <p className="table-backup-intro">
        保存 MBR / EBR 或 GPT
        主备分区表，用于保留当前分区布局。备份文件需要存到另一块磁盘。
      </p>
      <label className="table-backup-option">
        <input
          type="checkbox"
          checked={boot}
          disabled={busy}
          onChange={(e) => setBoot(e.target.checked)}
        />
        <span>
          <strong>同时备份各分区的首扇区</strong>
          <small>保存原始字节，不代表完整的文件系统或文件备份。</small>
        </span>
      </label>
      <div className="table-backup-actions">
        <button
          className="button primary"
          disabled={busy}
          onClick={() => void run("backup")}
        >
          <Save size={16} />
          保存备份…
        </button>
        <button
          className="button"
          disabled={busy}
          onClick={() => void run("verify")}
        >
          <FileCheck2 size={16} />
          验证备份文件…
        </button>
        <button
          className="button"
          disabled={busy}
          onClick={() => setRestore(true)}
        >
          从备份恢复…
        </button>
      </div>
      {busy && (
        <p className="table-backup-progress" role="status">
          <LoaderCircle className="spin" size={16} />
          正在处理，请等待文件选择或校验完成…
        </p>
      )}
      <FormError message={error} />
      {result && (
        <section className="table-backup-result" aria-label="备份校验结果">
          <h3>
            <CheckCircle2 size={19} />
            {saved ? "备份已保存并通过回读校验" : "备份文件校验通过"}
          </h3>
          <p>{result.name}</p>
          <dl>
            <div>
              <dt>分区表</dt>
              <dd>{result.info.scheme}</dd>
            </div>
            <div>
              <dt>磁盘容量</dt>
              <dd>
                {(result.info.size / 2 ** 30).toLocaleString(undefined, {
                  maximumFractionDigits: 2,
                })}{" "}
                GiB
              </dd>
            </div>
            <div>
              <dt>逻辑扇区</dt>
              <dd>{result.info.sector} B</dd>
            </div>
            <div>
              <dt>分区数量</dt>
              <dd>{result.info.partitions}</dd>
            </div>
            <div>
              <dt>分区表字节</dt>
              <dd>{result.info.tableBytes.toLocaleString()} B</dd>
            </div>
            <div>
              <dt>分区首扇区</dt>
              <dd>{result.info.bootBytes.toLocaleString()} B</dd>
            </div>
          </dl>
          {result.info.report.items
            .filter((item) => item.state === "warning")
            .map((item) => (
              <p className="table-backup-warning" key={item.code}>
                <AlertTriangle size={17} />
                <span>
                  {item.title}：{item.detail}
                </span>
              </p>
            ))}
          <details>
            <summary>完整性与分区表检查</summary>
            <p className="table-backup-digest">
              文件 SHA-256
              <br />
              <code>{result.fileSha256}</code>
            </p>
            <ul>
              {result.info.report.items.map((item) => (
                <li key={item.code}>
                  <strong>{item.title}</strong>：{item.detail}
                </li>
              ))}
            </ul>
          </details>
        </section>
      )}
      <p className="table-backup-note">
        备份前会核对磁盘身份并锁定同盘卷。分区表损坏、状态不完整或卷被占用时会停止；验证文件只检查备份本身，不会读取或修改当前磁盘。
      </p>
    </Dialog>
  );
}
