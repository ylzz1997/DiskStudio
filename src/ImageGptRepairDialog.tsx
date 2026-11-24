import { Dialog } from "./components";
import { useLocale } from "./i18n/LanguageProvider";
import { formatSize, type GptRepairChanges, type GptSide } from "./types";
export interface ImageGptReport {
  digest: string;
  sector: number;
  conflict: boolean;
  protectiveMbr: "valid" | "repairable" | "blocked";
  copies: {
    side: GptSide;
    error: string | null;
    repairError: string | null;
    changes: GptRepairChanges | null;
    layout: {
      diskId: string;
      firstLba: number;
      lastLba: number;
      partitions: {
        slot: number;
        guid: string;
        kind: string;
        name: string;
        offset: number;
        size: number;
        attributes: string;
      }[];
    } | null;
  }[];
}

export function ImageGptRepairDialog({
  name, report, onClose, onChoose,
}: {
  name: string;
  report: ImageGptReport;
  onClose: () => void;
  onChoose: (side: GptSide) => void;
}) {
  const { t, language, catalogText } = useLocale();
  return (
    <Dialog title={t("检查并修复镜像 GPT")} subtitle={`${name} · ${report.sector} B`} onClose={onClose} wide>
      {report.conflict && (
        <p className="table-backup-warning">
          {t("主 GPT 与备份 GPT 均有效，但布局不同。比较两份布局后，选择副本使用的依据。")}
        </p>
      )}
      {report.protectiveMbr !== "valid" && (
        <p className="table-backup-note">
          {t(report.protectiveMbr === "repairable"
            ? "保护性 MBR 损坏，可用有效 GPT 重建。"
            : "检测到混合或传统 MBR 条目，无法安全修复此镜像。")}
        </p>
      )}
      <div className="image-gpt-copies">
        {report.copies.map((copy) => (
          <section className="table-backup-result" key={copy.side}>
            <h3>{t(copy.side === "primary" ? "主 GPT" : "备份 GPT")}</h3>
            {copy.error && <p>{catalogText(copy.error)}</p>}
            {copy.layout && (
              <>
                <p>{t("磁盘标识")} <code>{copy.layout.diskId}</code></p>
                <p>{t("可用 LBA")} {copy.layout.firstLba.toLocaleString(language)}–{copy.layout.lastLba.toLocaleString(language)}</p>
                {copy.layout.partitions.length > 0 ? (
                  <div className="table-wrap image-gpt-entries">
                    <table className="partition-table">
                      <thead><tr>
                        <th>{t("分区名称")}</th>
                        <th>{t("起点（字节）")}</th>
                        <th>{t("容量")}</th>
                      </tr></thead>
                      <tbody>
                        {copy.layout.partitions.map((partition) => (
                          <tr key={partition.slot}>
                            <td>
                              <details>
                                <summary>{partition.slot} · {partition.name || "—"}</summary>
                                <div>{t("分区 GUID")} <code>{partition.guid}</code></div>
                                <div>{t("类型 GUID")} <code>{partition.kind}</code></div>
                                <div>{t("属性")} <code>{partition.attributes}</code></div>
                              </details>
                            </td>
                            <td>{partition.offset.toLocaleString(language)}</td>
                            <td>{formatSize(partition.size)}</td>
                          </tr>
                        ))}
                      </tbody>
                    </table>
                  </div>
                ) : <p>{t("没有已登记的分区。")}</p>}
              </>
            )}
            {copy.repairError && <p className="table-backup-note">{catalogText(copy.repairError)}</p>}
            {copy.changes && (
              <button className="button" onClick={() => onChoose(copy.side)}>
                {t(copy.side === "primary" ? "以主 GPT 为准" : "以备份 GPT 为准")}
              </button>
            )}
          </section>
        ))}
      </div>
    </Dialog>
  );
}
