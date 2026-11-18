import type { CSSProperties } from "react";
import { useLocale } from "./i18n/LanguageProvider";
import type { Partition } from "./types";
import linuxIcon from "./assets/platforms/linux.svg";
import windowsIcon from "./assets/platforms/windows.svg";

export function PartitionPlatformIcon({
  platform,
}: Pick<Partition, "platform">) {
  const { t } = useLocale();
  if (!platform) return null;
  const label = t(platform === "linux" ? "Linux 系统分区" : "Windows 系统分区");
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
