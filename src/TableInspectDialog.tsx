import { useEffect, useRef, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import {
  AlertTriangle,
  CheckCircle2,
  LoaderCircle,
  ScanLine,
  Wrench,
} from "lucide-react";
import { Dialog, FormError } from "./components";
import { TableCheckReport } from "./TableCheckReport";
import type { Disk, TableReport } from "./types";
interface Report {
  id: string;
  target: string;
  report: TableReport;
  retainedBytes: number;
  sha256: string;
  repairAvailable: boolean;
  repairNote: string;
}
interface Plan {
  repair: boolean;
  token: string | null;
  target: string;
  name: string;
  fileSha256: string;
  changedBytes: number;
  writeBytes: number;
  ranges: number;
}
interface Outcome {
  message: string;
  verification: { writtenBytes: number; ranges: number; sha256: string };
}
export function TableInspectDialog({
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
  const [report, setReport] = useState<Report | null>(null);
  const [plan, setPlan] = useState<Plan | null>(null);
  const [outcome, setOutcome] = useState<Outcome | null>(null);
  const [error, setError] = useState("");
  const [phase, setPhase] = useState("");
  const inFlight = useRef(false);
  const checkId = useRef<string | null>(null);
  const token = useRef<string | null>(null);
  function clearPlan() {
    if (token.current)
      void invoke("table_restore_discard", { token: token.current }).catch(
        () => {},
      );
    token.current = null;
  }
  function clearCheck() {
    if (checkId.current)
      void invoke("table_inspect_discard", { id: checkId.current }).catch(
        () => {},
      );
    checkId.current = null;
  }
  useEffect(
    () => () => {
      clearPlan();
      clearCheck();
    },
    [],
  );
  async function run(mode: "inspect" | "preview" | "execute") {
    if (
      inFlight.current ||
      (mode === "preview" && !checkId.current) ||
      (mode === "execute" && !token.current)
    )
      return;
    inFlight.current = true;
    onBusy(true);
    setError("");
    setOutcome(null);
    setPhase(
      mode === "inspect"
        ? "正在读取并检查分区表…"
        : mode === "preview"
          ? "正在复核修复来源和保护状态…"
          : "正在锁定、修复并回读校验…",
    );
    try {
      if (mode === "inspect") {
        clearPlan();
        clearCheck();
        setPlan(null);
        setReport(null);
        const next = await invoke<Report>("table_inspect", {
          request: {
            source: {
              target: disk.hardware?.identity,
              layoutKey: disk.hardware?.layoutKey,
              partition: null,
            },
          },
        });
        checkId.current = next.id;
        setReport(next);
      } else if (mode === "preview") {
        clearPlan();
        setPlan(null);
        const next = await invoke<Plan>("table_repair_prepare", {
          request: { checkId: checkId.current },
        });
        if (!next.repair) throw new Error("修复预览类型不匹配，请重新检查。");
        token.current = next.token;
        setPlan(next);
      } else {
        const once = token.current;
        token.current = null;
        setPlan(null);
        clearCheck();
        setReport(null);
        setOutcome(
          await invoke<Outcome>("table_restore_execute", { token: once }),
        );
      }
    } catch (e) {
      setError(String(e));
    } finally {
      if (mode === "execute") await onRefresh().catch(() => {});
      inFlight.current = false;
      setPhase("");
      onBusy(false);
    }
  }
  return (
    <Dialog
      title="分区表检查与修复"
      subtitle={`${disk.name} · ${disk.hardware?.model ?? ""}`}
      wide
      onClose={() => {
        if (!inFlight.current) onClose();
      }}
    >
      <p className="table-backup-intro">
        检查 GPT 主备表的完整性与分区范围，或 512 字节 MBR
        的主分区和扩展分区链。打开此面板不会读取设备。
      </p>
      <div className="table-backup-actions">
        <button
          className="button primary"
          disabled={!!phase}
          onClick={() => void run("inspect")}
        >
          <ScanLine size={16} />
          {report || outcome ? "重新检查" : "开始检查"}
        </button>
        {report?.repairAvailable && (
          <button
            className="button"
            disabled={!!phase}
            onClick={() => void run("preview")}
          >
            <Wrench size={16} />
            预览 GPT 修复
          </button>
        )}
      </div>
      {phase && (
        <p className="table-backup-progress" role="status">
          <LoaderCircle className="spin" size={16} />
          {phase}
        </p>
      )}
      <FormError message={error} />
      {plan && (
        <section className="table-backup-result" aria-label="GPT 修复预览">
          <h3>{plan.name}</h3>
          <p>
            <strong>{plan.target}</strong>
          </p>
          <dl>
            <div>
              <dt>改变的字节</dt>
              <dd>{plan.changedBytes.toLocaleString()} B</dd>
            </div>
            <div>
              <dt>写入范围</dt>
              <dd>{plan.ranges} 处</dd>
            </div>
            <div>
              <dt>写入字节</dt>
              <dd>{plan.writeBytes.toLocaleString()} B</dd>
            </div>
          </dl>
          <p>
            保留完整 GPT 副本、保护性 MBR
            和全部分区条目，不改变分区起点、容量、GUID、名称或属性。
          </p>
          <details>
            <summary>修复内容 SHA-256</summary>
            <p className="table-backup-digest">
              <code>{plan.fileSha256}</code>
            </p>
          </details>
          {plan.token && (
            <>
              <p className="table-backup-warning">
                <AlertTriangle size={17} />
                <span>
                  确认后将锁定全部同盘卷并使整盘脱机。逐段写入并回读校验，成功或失败均保持脱机；不会自动重试或回滚。确认有效期
                  5 分钟。
                </span>
              </p>
              <button
                className="button danger"
                disabled={!!phase}
                onClick={() => void run("execute")}
              >
                确认修复 GPT
              </button>
            </>
          )}
        </section>
      )}
      {outcome && (
        <section className="table-backup-result" role="status">
          <h3>
            <CheckCircle2 size={19} />
            GPT 修复完成
          </h3>
          <p>{outcome.message}</p>
          <p>
            已回读校验 {outcome.verification.ranges} 个范围、
            {outcome.verification.writtenBytes.toLocaleString()} B。
          </p>
        </section>
      )}
      {report && (
        <section className="physical-table-report">
          <p className="table-backup-note">{report.target}</p>
          <TableCheckReport report={report.report} physical />
          <p className="table-backup-note">{report.repairNote}</p>
          <details className="table-diagnostic-digest">
            <summary>本次检查范围摘要</summary>
            <p>
              保留 {report.retainedBytes.toLocaleString()} B
              的检查快照，重复读取确认内容未变。
            </p>
            <p className="table-backup-digest">
              <code>{report.sha256}</code>
            </p>
          </details>
        </section>
      )}
      <p className="table-backup-note">
        修复限非系统 GPT 磁盘，必须有一份完整有效且与 Windows
        布局一致的表。两份有效表冲突、双表损坏、混合 MBR
        和未知存储结构不自动修复。
      </p>
    </Dialog>
  );
}
