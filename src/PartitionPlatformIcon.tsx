import type { CSSProperties } from "react";
import type { Partition } from "./types";
import linuxIcon from "./assets/platforms/linux.svg";
import windowsIcon from "./assets/platforms/windows.svg";

export function PartitionPlatformIcon({
  platform,
}: Pick<Partition, "platform">) {
  if (!platform) return null;
  const label = platform === "linux" ? "Linux 系统分区" : "Windows 系统分区";
  return (
    <span
      className={`partition-platform-icon ${platform}`}
      role="img"
      aria-label={label}
      title={label}
      style={
        {
          "--platform-icon": `url("${platform === "linux" ? linuxIcon : windowsIcon}")`,
        } as CSSProperties
      }
    />
  );
}
