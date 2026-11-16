export interface QuickProgress {
  phase: "checking" | "running" | "verified" | "stopped";
  completed: number;
  total: number;
  current: number | null;
}

// One ordered channel per invocation. Ignore malformed, duplicate or late
// events; progress never authorizes another operation or a successful result.
export function advanceQuickProgress(
  previous: QuickProgress,
  event: QuickProgress,
): QuickProgress {
  if (
    previous.phase === "stopped" ||
    event.total !== previous.total ||
    !Number.isInteger(event.completed) ||
    event.completed < 0 ||
    event.completed > event.total
  )
    return previous;
  if (
    event.phase === "running" &&
    (previous.phase === "checking" || previous.phase === "verified") &&
    event.completed === previous.completed &&
    event.completed < event.total &&
    event.current === event.completed
  )
    return event;
  if (
    previous.phase === "running" &&
    event.phase === "verified" &&
    event.completed === previous.completed + 1 &&
    event.current === null
  )
    return event;
  if (
    previous.phase === "running" &&
    event.phase === "stopped" &&
    event.completed === previous.completed &&
    event.current === previous.current
  )
    return event;
  return previous;
}
