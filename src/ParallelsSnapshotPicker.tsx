import { useEffect, useState } from "react";
import { useLocale } from "./i18n/LanguageProvider";
import { listParallelsSnapshots } from "./parallelsBundle";
import type { ImageFile } from "./imageSource";

export type ParallelsSnapshots = Awaited<
  ReturnType<typeof listParallelsSnapshots>
>;

export function useParallelsSnapshots(file?: ImageFile) {
  const [result, setResult] = useState<{
    file: ImageFile;
    list?: ParallelsSnapshots;
    error?: string;
  }>();
  useEffect(() => {
    if (!file) return;
    let alive = true;
    void listParallelsSnapshots(file).then(
      (list) => {
        if (alive) setResult({ file, list });
      },
      (error) => {
        if (alive)
          setResult({
            file,
            error: error instanceof Error ? error.message : String(error),
          });
      },
    );
    return () => {
      alive = false;
    };
  }, [file]);
  return result?.file === file ? result : undefined;
}

export function ParallelsSnapshotPicker({
  list,
  id,
  disabled,
  onChange,
}: {
  list: ParallelsSnapshots;
  id: string | undefined;
  disabled?: boolean;
  onChange: (id: string) => void;
}) {
  const { t } = useLocale();
  return (
    <label className="parallels-snapshot-picker">
      {t("Parallels 快照")}
      <select
        value={id ?? list.top}
        disabled={disabled}
        onChange={(e) => onChange(e.target.value)}
      >
        {list.snapshots.map((point) => (
          <option key={point.id} value={point.id} disabled={!point.supported}>
            {point.id === list.top ? `${t("当前快照")} · ` : ""}
            {point.name}
          </option>
        ))}
      </select>
    </label>
  );
}
