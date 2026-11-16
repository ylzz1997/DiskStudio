import type { PartitionPlan } from "./types";

export interface PartitionOutcome {
  plan: Omit<PartitionPlan, "token" | "expiresAt">;
  status: "verified" | "unverified" | "failed";
  message: string;
  refreshError: string;
}

export const errorText = (error: unknown) =>
  String(error instanceof Error ? error.message : error);

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
    outcome = { ...outcome, refreshError: `列表刷新失败：${errorText(error)}` };
    publish(outcome);
  }
  return outcome;
}
