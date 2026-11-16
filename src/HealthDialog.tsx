import { useRef, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { Activity, LoaderCircle, RefreshCcw } from "lucide-react";
import { Dialog, FormError } from "./components";
import type { Disk } from "./types";

interface HealthReport {
  observedAt: number;
  health: {
    protocol: "nvme" | "ata";
    summary: string;
    warnings: string[];
    metrics: {
      label: string;
      value: string;
      unit: string;
      detail: string | null;
    }[];
    attributes: {
      id: number;
      current: number;
      worst: number;
      threshold: number | null;
      flags: number;
      raw: string;
      thresholdReached: boolean;
    }[];
    notes: string[];
  };
}

export function HealthDialog({
  disk,
  onClose,
  onBusy,
}: {
  disk: Disk;
  onClose: () => void;
  onBusy: (busy: boolean) => void;
}) {
  const [report, setReport] = useState<HealthReport | null>(null);
  const [busy, setBusy] = useState(false),
    [error, setError] = useState("");
  const inFlight = useRef(false);
  async function read() {
    if (inFlight.current) return;
    const target = disk.hardware?.identity,
      layoutKey = disk.hardware?.layoutKey;
    if (!target || !layoutKey) {
      setError("请刷新磁盘信息后重试。");
      return;
    }
    inFlight.current = true;
    setBusy(true);
    onBusy(true);
    setError("");
    setReport(null);
    try {
      setReport(
        await invoke<HealthReport>("health_read", {
          request: { target, layoutKey },
        }),
      );
    } catch (e) {
      setError(String(e));
    } finally {
      inFlight.current = false;
      setBusy(false);
      onBusy(false);
    }
  }
  const health = report?.health;
  return (
    <Dialog
      title="健康信息 · SMART"
      subtitle={`${disk.name} · ${disk.hardware?.model ?? ""}`}
      wide
      onClose={() => {
        if (!inFlight.current) onClose();
      }}
    >
      <div className="health-toolbar">
        <span>Windows 状态：{disk.hardware?.health ?? "未知"}</span>
        <button
          className="button primary"
          disabled={busy}
          onClick={() => void read()}
        >
          {busy ? (
            <LoaderCircle size={16} className="spin" />
          ) : (
            <RefreshCcw size={16} />
          )}
          {busy ? "正在读取…" : report ? "重新读取" : "读取健康信息"}
        </button>
      </div>
      <FormError message={error} />
      {!report && !busy && !error && (
        <div className="health-empty">
          <Activity size={36} strokeWidth={1.5} />
          <h3>查看设备报告的健康数据</h3>
          <p>
            读取后显示温度、累计计数或 SMART 属性。Windows
            状态与设备健康信息分别显示。
          </p>
        </div>
      )}
      {busy && (
        <p className="health-note" role="status">
          正在核对设备身份并读取健康日志，请等待请求完成。
        </p>
      )}
      {report && health && (
        <>
          <div
            className={`health-summary ${health.warnings.length ? "has-warning" : ""}`}
          >
            <div>
              <strong>{health.summary}</strong>
              <p>
                {health.protocol === "nvme"
                  ? "NVMe 健康日志"
                  : "ATA SMART 属性"}
              </p>
            </div>
            <time dateTime={new Date(report.observedAt * 1000).toISOString()}>
              {new Date(report.observedAt * 1000).toLocaleString()}
            </time>
          </div>
          {!!health.warnings.length && (
            <ul className="health-warnings">
              {health.warnings.map((w) => (
                <li key={w}>{w}</li>
              ))}
            </ul>
          )}
          <div className="health-metrics">
            {health.metrics.map((m) => (
              <div className="health-metric" key={m.label}>
                <span>{m.label}</span>
                <div>
                  <strong>{m.value}</strong> <small>{m.unit}</small>
                </div>
                {m.detail && (
                  <details>
                    <summary>计数说明</summary>
                    <p>{m.detail}</p>
                  </details>
                )}
              </div>
            ))}
          </div>
          {!!health.attributes.length && (
            <div className="health-attributes">
              <table>
                <thead>
                  <tr>
                    <th>ID</th>
                    <th>当前值</th>
                    <th>最差值</th>
                    <th>阈值</th>
                    <th>原始值（字节）</th>
                    <th>标志</th>
                    <th>阈值比较</th>
                  </tr>
                </thead>
                <tbody>
                  {health.attributes.map((a) => (
                    <tr
                      key={a.id}
                      className={a.thresholdReached ? "has-warning" : ""}
                    >
                      <td>
                        {a.id.toString(16).toUpperCase().padStart(2, "0")}
                      </td>
                      <td>{a.current}</td>
                      <td>{a.worst}</td>
                      <td>{a.threshold ?? "—"}</td>
                      <td className="health-raw">{a.raw}</td>
                      <td>
                        {a.flags.toString(16).toUpperCase().padStart(4, "0")}
                      </td>
                      <td>
                        {a.thresholdReached
                          ? "达到阈值"
                          : a.threshold && a.current
                            ? "未达阈值"
                            : "未知"}
                      </td>
                    </tr>
                  ))}
                </tbody>
              </table>
            </div>
          )}
          <div className="health-notes">
            {health.notes.map((note) => (
              <p key={note}>{note}</p>
            ))}
          </div>
        </>
      )}
    </Dialog>
  );
}
