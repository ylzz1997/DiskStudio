import { signatureTypes } from "./signatureModel";
import type { SignatureSettings } from "./signatureModel";
import { useLocale } from "./i18n/LanguageProvider";
export function SignatureOptions({
  value,
  onChange,
  disabled,
  canScanFree,
}: {
  value: SignatureSettings;
  onChange: (value: SignatureSettings) => void;
  disabled: boolean;
  canScanFree: boolean;
}) {
  const { t, catalogText } = useLocale();
  return (
    <fieldset className="signature-options" disabled={disabled}>
      <legend>{t("文件类型与扫描范围")}</legend>
      <div className="signature-types">
        {signatureTypes.map((type) => (
          <label
            key={type.id}
            title={"detail" in type ? catalogText(type.detail) : type.label}
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
          {t("全选类型")}
        </button>
        <button
          className="button text-button"
          type="button"
          onClick={() => onChange({ ...value, kinds: [] })}
        >
          {t("清空类型")}
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
          {t("指定扫描范围")}
        </label>
        {value.customRange ? (
          <>
            <label>
              {t("开始位置 (MiB)")}
              <input
                aria-label={t("签名扫描开始位置 MiB")}
                inputMode="decimal"
                value={value.start}
                onChange={(e) => onChange({ ...value, start: e.target.value })}
              />
            </label>
            <label>
              {t("结束位置 (MiB)")}
              <input
                aria-label={t("签名扫描结束位置 MiB")}
                inputMode="decimal"
                placeholder={t("到末尾")}
                value={value.end}
                onChange={(e) => onChange({ ...value, end: e.target.value })}
              />
            </label>
          </>
        ) : (
          <span>{t("扫描所选磁盘或分区的全部范围")}</span>
        )}
      </div>
      <div className="signature-range">
        <label>
          <input
            type="checkbox"
            disabled={!canScanFree}
            checked={canScanFree && value.freeOnly}
            onChange={(e) => onChange({ ...value, freeOnly: e.target.checked })}
          />
          {t("仅扫描可核对的空闲簇")}
        </label>
        <label>
          <input
            type="checkbox"
            checked={value.sectorAligned}
            onChange={(e) => onChange({ ...value, sectorAligned: e.target.checked })}
          />
          {t("文件起点按逻辑扇区对齐")}
        </label>
      </div>
      {!canScanFree && <p>{t("空闲簇扫描需要选择一个未加密的实体分区。")}</p>}
      {canScanFree && value.freeOnly && <p>{t("核对分区分配信息，只列出完整位于空闲范围内的连续候选。")}</p>}
      {value.customRange && (
        <p>{t("位置相对于上方所选磁盘或分区，结束位置不包含在扫描范围内。")}</p>
      )}
      {value.kinds.some((k) => ["zip", "docx", "xlsx", "pptx"].includes(k)) && (
        <p>
          {t(
            "ZIP / Office：最多 16384 个条目、8 MiB 目录、512 MiB 解压内容；每次扫描的归档检查工作量上限 64 GiB。 识别为 DOCX、XLSX、PPTX 的包只在勾选对应类型时列出；其他受支持的 ZIP 容器归入 ZIP。 不解包到磁盘、不运行文档内容，CRC 通过不代表 Office 文档语义完整。",
          )}
        </p>
      )}
    </fieldset>
  );
}
