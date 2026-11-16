import { signatureTypes } from "./signatureModel";
import type { SignatureSettings } from "./signatureModel";
export function SignatureOptions({
  value,
  onChange,
  disabled,
}: {
  value: SignatureSettings;
  onChange: (value: SignatureSettings) => void;
  disabled: boolean;
}) {
  return (
    <fieldset className="signature-options" disabled={disabled}>
      <legend>文件类型与扫描范围</legend>
      <div className="signature-types">
        {signatureTypes.map((type) => (
          <label
            key={type.id}
            title={"detail" in type ? type.detail : type.label}
          >
            <input
              type="checkbox"
              checked={value.kinds.includes(type.id)}
              onChange={(e) =>
                onChange({
                  ...value,
                  kinds: e.target.checked
                    ? [...value.kinds, type.id]
                    : value.kinds.filter((k) => k !== type.id),
                })
              }
            />
            {type.label}
          </label>
        ))}
        <button
          className="button text-button"
          type="button"
          onClick={() =>
            onChange({ ...value, kinds: signatureTypes.map((t) => t.id) })
          }
        >
          全选类型
        </button>
        <button
          className="button text-button"
          type="button"
          onClick={() => onChange({ ...value, kinds: [] })}
        >
          清空类型
        </button>
      </div>
      <div className="signature-range">
        <label>
          <input
            type="checkbox"
            checked={value.customRange}
            onChange={(e) =>
              onChange({ ...value, customRange: e.target.checked })
            }
          />
          指定扫描范围
        </label>
        {value.customRange ? (
          <>
            <label>
              开始位置 (MiB)
              <input
                aria-label="签名扫描开始位置 MiB"
                inputMode="decimal"
                value={value.start}
                onChange={(e) => onChange({ ...value, start: e.target.value })}
              />
            </label>
            <label>
              结束位置 (MiB)
              <input
                aria-label="签名扫描结束位置 MiB"
                inputMode="decimal"
                placeholder="到末尾"
                value={value.end}
                onChange={(e) => onChange({ ...value, end: e.target.value })}
              />
            </label>
          </>
        ) : (
          <span>扫描所选磁盘或分区的全部范围</span>
        )}
      </div>
      {value.customRange && (
        <p>位置相对于上方所选磁盘或分区，结束位置不包含在扫描范围内。</p>
      )}
      {value.kinds.some((k) => ["zip", "docx", "xlsx", "pptx"].includes(k)) && (
        <p>
          ZIP / Office：最多 16384 个条目、8 MiB 目录、512 MiB
          解压内容；每次扫描的归档检查工作量上限 64 GiB。 识别为
          DOCX、XLSX、PPTX 的包只在勾选对应类型时列出；其他受支持的 ZIP 容器归入
          ZIP。 不解包到磁盘、不运行文档内容，CRC 通过不代表 Office
          文档语义完整。
        </p>
      )}
    </fieldset>
  );
}
