import { CheckCircle2, CircleHelp, TriangleAlert, XCircle } from "lucide-react";
import type { TableCheckState, TableReport } from "./types";
import { Hint } from "./components";
import { useLocale } from "./i18n/LanguageProvider";
import type { Message } from "./i18n/translate";

const states: Record<
  TableCheckState,
  { label: Message; Icon: typeof CheckCircle2 }
> = {
  passed: { label: "检查通过", Icon: CheckCircle2 },
  warning: { label: "有提示", Icon: TriangleAlert },
  failed: { label: "发现问题", Icon: XCircle },
  unchecked: { label: "未完整检查", Icon: CircleHelp },
};
export function TableCheckReport({
  report,
  physical = false,
}: {
  report: TableReport;
  physical?: boolean;
}) {
  const { t, catalogText } = useLocale();
  const summary = states[report.state];
  return (
    <div className="table-check-report">
      <div className={`table-check-summary ${report.state}`}>
        <summary.Icon size={25} />
        <div>
          <strong>{t(summary.label)}</strong>
          <span>
            {report.scheme}
            {report.sectorSize
              ? ` · ${t("{size} B / 扇区", { size: report.sectorSize })}`
              : ""}
          </span>
        </div>
      </div>
      <ul className="table-check-items">
        {report.items.map((item) => {
          const state = states[item.state];
          return (
            <li key={item.code}>
              <state.Icon
                className={`check-icon ${item.state}`}
                size={19}
                aria-hidden="true"
              />
              <div>
                <div className="check-item-heading">
                  <strong>{catalogText(item.title)}</strong>
                  <span>{t(state.label)}</span>
                </div>
                <p>{catalogText(item.detail)}</p>
              </div>
            </li>
          );
        })}
      </ul>
      <Hint>
        {physical
          ? t(
              "仅检查显式读取的磁盘首尾各 1 MiB，以及 512 字节 MBR 链路指向的最多 64 个 EBR；不检查文件系统、文件内容或介质健康。检查本身不锁卷、不写盘；磁盘状态改变后需重新检查。",
            )
          : t(
              "检查读取逻辑磁盘首尾各 1 MiB，以及 MBR 链路指向的最多 64 个 EBR 扇区。虚拟磁盘还核对容器元数据与块映射；不检查文件系统、文件内容或介质健康，也不会修复或写回镜像。源文件改变后需重新打开。",
            )}
      </Hint>
    </div>
  );
}
