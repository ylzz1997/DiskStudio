import type { PartitionPlan } from "./types";
import type { Translate } from "./i18n/translate";

export interface PartitionOutcome {
  plan: Omit<PartitionPlan, "token" | "expiresAt">;
  status: "verified" | "unverified" | "failed";
  message: string;
  refreshError: string;
}

export const errorText = (error: unknown) =>
  String(error instanceof Error ? error.message : error);

export interface BatchExecution {
  message: string;
  verified: boolean;
  completed: number;
  failure: string | null;
}

export function batchResultLines(
  result: BatchExecution,
  labels: readonly string[],
  t: Translate,
  catalogText: (text: string) => string,
): string[] {
  return labels.map((label, index) => {
    const status =
      index < result.completed
        ? t("已完成并核对")
        : index === result.completed
          ? t("已停止，结果待核对")
          : t("未执行");
    const line = `${index + 1}. ${status} · ${label}`;
    return index === result.completed && result.failure !== null
      ? `${line}\n${catalogText(result.failure)}`
      : line;
  });
}

// The caller consumes the confirmation before calling this function. Publish to
// the application owner before refreshing: the selected disk may disappear.
export async function runPartitionOperation(
  submitted: PartitionPlan,
  execute: (token: string) => Promise<{ verified: boolean; message: string }>,
  refresh: () => Promise<void>,
  publish: (outcome: PartitionOutcome) => void,
): Promise<PartitionOutcome> {
  const { token, expiresAt: _expiresAt, ...plan } = submitted;
  let outcome: PartitionOutcome;
  try {
    const result = await execute(token);
    outcome = {
      plan,
      status: result.verified ? "verified" : "unverified",
      message: result.message,
      refreshError: "",
    };
  } catch (error) {
    outcome = {
      plan,
      status: "failed",
      message: errorText(error),
      refreshError: "",
    };
  }
  publish(outcome);
  try {
    await refresh();
  } catch (error) {
    outcome = { ...outcome, refreshError: errorText(error) };
    publish(outcome);
  }
  return outcome;
}
