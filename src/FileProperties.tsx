import { Dialog } from "./components";
import type { Candidate, FileStamp } from "./recoveryModel";
import { formatSize } from "./types";
function Timestamp({ value }: { value: FileStamp | null | undefined }) {
  return value ? (
    <>
      <span>{value.text}</span>
      <small>
        {value.zone} · 精度 {value.precision}
      </small>
    </>
  ) : (
    <>未记录或无法解析</>
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
  const m = file.metadata;
  return (
    <Dialog
      title={kind === "directory" ? "目录属性" : "文件属性"}
      subtitle={file.name}
      onClose={onClose}
      wide
    >
      <dl className="file-properties">
        <dt>原目录</dt>
        <dd>{directory}</dd>
        {file.size != null && (
          <>
            <dt>大小</dt>
            <dd>{formatSize(file.size)}</dd>
          </>
        )}
        <dt>创建时间</dt>
        <dd>
          <Timestamp value={m?.created} />
        </dd>
        <dt>修改时间</dt>
        <dd>
          <Timestamp value={m?.modified} />
        </dd>
        <dt>访问时间</dt>
        <dd>
          <Timestamp value={m?.accessed} />
        </dd>
        <dt>元数据变更</dt>
        <dd>
          <Timestamp value={m?.changed} />
        </dd>
        <dt>属性标志</dt>
        <dd>
          {m?.source ? m.attributes.join("、") || "无已识别标志" : "未知"}
          {m?.rawFlags && <small>原始标志 {m.rawFlags}</small>}
        </dd>
        {m?.permissions != null && (
          <>
            <dt>POSIX 权限</dt>
            <dd>{m.permissions}（八进制）</dd>
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
            <dt>链接计数</dt>
            <dd>{m.links}</dd>
          </>
        )}
        <dt>信息来源</dt>
        <dd>{m?.source ?? "本次读取未提供属性记录"}</dd>
      </dl>
      <p className="report-dialog-description">
        显示本次读取时的记录；时间和属性可能滞后、被修改或已经失效。未知时区不按当前电脑时区推算。元数据变更时间不是创建时间。
      </p>
      <p className="report-dialog-description">
        这里只查看属性。复制时的保留选项和逐项结果以复制窗口及输出清单为准。
      </p>
      <div className="dialog-actions">
        <button className="button" onClick={onClose}>
          关闭
        </button>
      </div>
    </Dialog>
  );
}
