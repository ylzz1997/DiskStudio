import { signatureTypes, zipPackageKinds } from "./signatureModel";
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
  const hasZipType = value.kinds.some((kind) => zipPackageKinds.includes(kind));
  const hasOgg = value.kinds.includes("ogg");
  const hasFlac = value.kinds.includes("flac");
  const hasPng = value.kinds.includes("png");
  const fragmentScan = hasZipType && value.zipFragments || hasOgg && value.oggFragments
    || hasFlac && value.flacFragments || hasPng && value.pngFragments;
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
        {hasZipType && (
          <label>
            <input
              type="checkbox"
              checked={value.zipFragments}
              onChange={(e) => onChange({ ...value, zipFragments: e.target.checked })}
            />
            {t("重组 ZIP 碎片")}
          </label>
        )}
        {hasOgg && (
          <label>
            <input
              type="checkbox"
              checked={value.oggFragments}
              onChange={(e) => onChange({ ...value, oggFragments: e.target.checked })}
            />
            {t("重组 Ogg 碎片")}
          </label>
        )}
        {hasFlac && (
          <label>
            <input
              type="checkbox"
              checked={value.flacFragments}
              onChange={(e) => onChange({ ...value, flacFragments: e.target.checked })}
            />
            {t("重组 FLAC 碎片")}
          </label>
        )}
        {hasPng && (
          <label>
            <input
              type="checkbox"
              checked={value.pngFragments}
              onChange={(e) => onChange({ ...value, pngFragments: e.target.checked })}
            />
            {t("重组 PNG 碎片")}
          </label>
        )}
      </div>
      {!canScanFree && <p>{catalogText("空闲簇扫描需要未加密且分配信息可核对的分区。")}</p>}
      {canScanFree && value.freeOnly && (
        <p>{fragmentScan
          ? t("每段碎片都须完整位于可核对的空闲范围。")
          : t("核对分区分配信息，只列出完整位于空闲范围内的连续候选。")}</p>
      )}
      {value.customRange && (
        <p>{t("位置相对于上方所选磁盘或分区，结束位置不包含在扫描范围内。")}</p>
      )}
      {hasZipType && (
        <p>
          {t(
            "ZIP、Office Open XML 和 OpenDocument 的单卷扫描支持未加密 Stored/Deflate 条目，逐项验证 CRC。扫描限每包 16,384 个条目、8 MiB 目录和 512 MiB 解压内容，每次累计归档检查最多 64 GiB。识别到的文档包只在勾选对应类型时列出。",
          )}
        </p>
      )}
      {hasZipType && value.zipFragments && (
        <p>{t("重组需要完整的 ZIP 目录和各自连续的成员记录，每包最多 256 个成员。扫描会再读取一次所选范围。暂停后从碎片阶段起点继续。")}</p>
      )}
      {hasOgg && value.oggFragments && (
        <p>{t("额外扫描一次，按序列号重组完整的 Ogg 逻辑流。每页须连续并通过 CRC 校验，缺页或重复页会跳过该流。暂停后重新检查碎片。")}</p>
      )}
      {hasFlac && value.flacFragments && (
        <p>{t("额外扫描一次，重组完整的 FLAC 元数据和各个音频帧。解码样本须匹配 STREAMINFO 的 MD5。有缺帧或多个组合通过校验时跳过。")}</p>
      )}
      {hasPng && value.pngFragments && (
        <p>{t("额外扫描一次，按完整 IDAT 块重组静态 PNG。首段图像数据须与文件头相邻，末段须保留结束块。仅列出唯一通过校验的组合。")}</p>
      )}
    </fieldset>
  );
}
