import { Dialog, FormError, Hint } from "./components";
import type { PartitionOutcome } from "./partitionOutcome";
import { useLocale } from "./i18n/LanguageProvider";

export function PartitionOutcomeDialog({
  outcome,
  busy,
  onClose,
}: {
  outcome: PartitionOutcome;
  busy: boolean;
  onClose: () => void;
}) {
  const { t, catalogText } = useLocale();
  const { plan } = outcome;
  const close = () => {
    if (!busy) onClose();
  };
  return (
    <Dialog
      title={
        outcome.status === "verified"
          ? t("操作已完成并核对")
          : outcome.status === "unverified"
            ? t("操作结果尚未核对完整")
            : t("操作未能完成")
      }
      subtitle={
        plan.partition == null
          ? plan.targetName
          : `${plan.targetName} · ${t("分区 {partition}", { partition: plan.partition })}`
      }
      onClose={close}
      footer={
        <button className="button primary" disabled={busy} onClick={close}>
          {busy ? t("正在刷新列表…") : t("关闭结果")}
        </button>
      }
    >
      <dl className="detail-list">
        <dt>{t("操作")}</dt>
        <dd>{catalogText(plan.title)}</dd>
        <dt>{t("目标序列号")}</dt>
        <dd>{outcome.plan.serial}</dd>
      </dl>
      <p
        className="partition-outcome-message"
        role={outcome.status === "verified" ? "status" : "alert"}
      >
        {catalogText(outcome.message)}
      </p>
      {outcome.status !== "verified" && (
        <Hint>
          {t("请先检查实际状态。操作可能已部分执行；不会自动重试或回滚。")}
        </Hint>
      )}
      <FormError
        message={
          outcome.refreshError &&
          t("列表刷新失败：{error}", {
            error: catalogText(outcome.refreshError),
          })
        }
      />
    </Dialog>
  );
}
