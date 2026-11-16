import { Dialog, FormError, Hint } from "./components";
import type { PartitionOutcome } from "./partitionOutcome";

export function PartitionOutcomeDialog({
  outcome,
  busy,
  onClose,
}: {
  outcome: PartitionOutcome;
  busy: boolean;
  onClose: () => void;
}) {
  const close = () => {
    if (!busy) onClose();
  };
  return (
    <Dialog
      title={
        outcome.status === "verified"
          ? "操作已完成并核对"
          : outcome.status === "unverified"
            ? "操作结果尚未核对完整"
            : "操作未能完成"
      }
      subtitle={`${outcome.plan.targetName}${outcome.plan.partition == null ? "" : ` · 分区 ${outcome.plan.partition}`}`}
      onClose={close}
      footer={
        <button className="button primary" disabled={busy} onClick={close}>
          {busy ? "正在刷新列表…" : "关闭结果"}
        </button>
      }
    >
      <dl className="detail-list">
        <dt>操作</dt>
        <dd>{outcome.plan.title}</dd>
        <dt>目标序列号</dt>
        <dd>{outcome.plan.serial}</dd>
      </dl>
      <p
        className="partition-outcome-message"
        role={outcome.status === "verified" ? "status" : "alert"}
      >
        {outcome.message}
      </p>
      {outcome.status !== "verified" && (
        <Hint>请先检查实际状态。操作可能已部分执行；不会自动重试或回滚。</Hint>
      )}
      <FormError message={outcome.refreshError} />
    </Dialog>
  );
}
