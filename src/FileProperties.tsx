import { Dialog } from "./components";
import { useLocale } from "./i18n/LanguageProvider";
import type { Candidate, FileStamp } from "./recoveryModel";
import { formatSize } from "./types";
function Timestamp({ value }: { value: FileStamp | null | undefined }) {
  const { t, catalogText } = useLocale();
  return value ? (
    <>
      <span>{value.text}</span>
      <small>
        {t("{zone} · 精度 {precision}", {
          zone: catalogText(value.zone),
          precision: catalogText(value.precision),
        })}
      </small>
    </>
  ) : (
    <>{t("未记录或无法解析")}</>
  );
}
export function FileProperties({
  file,
  directory,
  onClose,
  kind = "file",
}: {
  file: Pick<Candidate, "name" | "metadata"> & { size?: number };
  directory: string;
  onClose: () => void;
  kind?: "file" | "directory";
}) {
  const { t, language, catalogText } = useLocale();
  const m = file.metadata;
  return (
    <Dialog
      title={kind === "directory" ? t("目录属性") : t("文件属性")}
      subtitle={file.name}
      onClose={onClose}
      wide
    >
      <dl className="file-properties">
        <dt>{t("原目录")}</dt>
        <dd>{directory}</dd>
        {file.size != null && (
          <>
            <dt>{t("大小")}</dt>
            <dd>{formatSize(file.size)}</dd>
          </>
        )}
        <dt>{t("创建时间")}</dt>
        <dd>
          <Timestamp value={m?.created} />
        </dd>
        <dt>{t("修改时间")}</dt>
        <dd>
          <Timestamp value={m?.modified} />
        </dd>
        <dt>{t("访问时间")}</dt>
        <dd>
          <Timestamp value={m?.accessed} />
        </dd>
        <dt>{t("元数据变更")}</dt>
        <dd>
          <Timestamp value={m?.changed} />
        </dd>
        <dt>{t("属性标志")}</dt>
        <dd>
          {m?.source
            ? new Intl.ListFormat(language).format(
                m.attributes.map(catalogText),
              ) || t("无已识别标志")
            : t("未知")}
          {m?.rawFlags && (
            <small>{t("原始标志 {flags}", { flags: m.rawFlags })}</small>
          )}
        </dd>
        {m?.permissions != null && (
          <>
            <dt>{t("POSIX 权限")}</dt>
            <dd>{t("{value}（八进制）", { value: m.permissions })}</dd>
          </>
        )}
        {m?.uid != null && (
          <>
            <dt>UID</dt>
            <dd>{m.uid}</dd>
          </>
        )}
        {m?.gid != null && (
          <>
            <dt>GID</dt>
            <dd>{m.gid}</dd>
          </>
        )}
        {m?.links != null && (
          <>
            <dt>{t("链接计数")}</dt>
            <dd>{m.links}</dd>
          </>
        )}
        <dt>{t("信息来源")}</dt>
        <dd>
          {m?.source ? catalogText(m.source) : t("本次读取未提供属性记录")}
        </dd>
      </dl>
      <p className="report-dialog-description">
        {t(
          "显示本次读取时的记录；时间和属性可能滞后、被修改或已经失效。未知时区不按当前电脑时区推算。元数据变更时间不是创建时间。",
        )}
      </p>
      <p className="report-dialog-description">
        {t(
          "这里只查看属性。复制时的保留选项和逐项结果以复制窗口及输出清单为准。",
        )}
      </p>
      <div className="dialog-actions">
        <button className="button" onClick={onClose}>
          {t("关闭对话框")}
        </button>
      </div>
    </Dialog>
  );
}
