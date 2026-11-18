import { Hint } from "./components";
import { useLocale } from "./i18n/LanguageProvider";
import type { QuickProgress } from "./quickProgress";

export function PartitionBatchProgress({
  labels,
  progress,
  note,
}: {
  labels: string[];
  progress: QuickProgress;
  note: string;
}) {
  const { t } = useLocale();
  const { phase, completed, total, current } = progress;
  const status =
    phase === "checking"
      ? t("正在重新核对磁盘、完整布局和卷关联…")
      : phase === "stopped"
        ? t("当前步骤未完整确认，后续步骤已停止。正在接收最终结果…")
        : completed === total
          ? t("全部步骤已核对，正在接收最终结果…")
          : current !== null
            ? t("正在执行并核对第 {current} 步，共 {total} 步", {
                current: current + 1,
                total,
              })
            : t("正在准备下一步…");
  return (
    <section className="quick-progress" aria-label={t("批量操作进度")}>
      <p role="status" aria-live="polite">
        {status}
      </p>
      <progress
        value={completed}
        max={total}
        aria-label={t("已完成并核对的步骤")}
      />
      <p className="muted">
        {t(
          "已完成并核对 {completed} / {total} 步。步骤耗时不同，此处不代表已处理的数据比例。",
          { completed, total },
        )}
      </p>
      <Hint>{note}</Hint>
      <ol className="quick-steps quick-progress-steps">
        {labels.map((title, index) => {
          const state =
            index < completed
              ? "verified"
              : index === current
                ? phase
                : phase === "stopped"
                  ? "skipped"
                  : "waiting";
          const label =
            state === "verified"
              ? t("已完成并核对")
              : state === "running"
                ? t("正在执行与核对")
                : state === "stopped"
                  ? t("已停止，结果待核对")
                  : state === "skipped"
                    ? t("未执行")
                    : t("待执行");
          return (
            <li
              key={index}
              data-state={state}
              aria-current={state === "running" ? "step" : undefined}
            >
              <span className="quick-step-status">{label}</span>
              <span>{title}</span>
            </li>
          );
        })}
      </ol>
    </section>
  );
}
