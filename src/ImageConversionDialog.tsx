import { useEffect, useRef, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import {
  ArrowRightLeft,
  FolderOpen,
  LoaderCircle,
  StopCircle,
} from "lucide-react";
import { Dialog, FormError } from "./components";
import { useLocale } from "./i18n/LanguageProvider";
import { conversionReader, type ImageFiles } from "./imageReader";
import { isParallelsDescriptor } from "./parallelsBundle";
import { ParallelsSnapshotPicker, useParallelsSnapshots } from "./ParallelsSnapshotPicker";
import { MissingImageFilesError } from "./imageDependencies";
import {
  conversionFormats,
  conversionFrame,
  convertImage,
  type ImageConversionReport,
  type ImageOutputFormat,
} from "./imageConversion";
import { formatSize } from "./types";

export function ImageConversionDialog({
  initial,
  onClose,
  onBusy,
}: {
  initial?: ImageFiles;
  onClose: () => void;
  onBusy: (busy: boolean) => void;
}) {
  const { t, language, catalogText } = useLocale();
  const [files, setFiles] = useState<ImageFiles | undefined>(initial),
    [format, setFormat] = useState<ImageOutputFormat>("vhdDynamic");
  const [report, setReport] = useState<ImageConversionReport | null>(null),
    [error, setError] = useState(""),
    [standaloneConfirmed, setStandaloneConfirmed] = useState(false),
    [busy, setBusy] = useState(false),
    [stopping, setStopping] = useState(false);
  const primary = useRef<HTMLInputElement>(null),
    companions = useRef<HTMLInputElement>(null),
    controller = useRef<AbortController | null>(null),
    jobId = useRef<string | null>(null),
    progress = useRef<HTMLElement>(null),
    alive = useRef(true);
  const phaseText =
    report?.state === "checkingSource"
      ? t("复核源镜像")
      : report?.state === "verifying"
        ? t("回读目标镜像")
        : report?.state === "completed"
          ? t("转换完成")
          : report?.state === "receiving"
            ? t("转换写入")
            : report?.state === "canceled"
              ? t("转换已停止")
              : report?.state === "failed"
                ? t("转换未完成")
                : t("准备转换");
  const needsStandalone = /\.(hdd|hds)$/i.test(files?.file.name ?? "");
  const bundle = !!files && isParallelsDescriptor(files.file.name);
  const snapshotList = useParallelsSnapshots(bundle ? files?.file : undefined);
  const snapshot = bundle ? files?.parallelsSnapshot ?? snapshotList?.list?.top : undefined;
  const snapshotReady = !bundle || !!snapshotList?.list?.snapshots.some((point) => point.id === snapshot && point.supported);
  useEffect(() => {
    alive.current = true;
    return () => {
      alive.current = false;
      controller.current?.abort();
      const id = jobId.current;
      if (id) void invoke("image_conversion_cancel", { id }).catch(() => {});
    };
  }, []);
  useEffect(() => {
    if (!busy) return;
    let canceled = false,
      timer: ReturnType<typeof setTimeout>;
    async function poll() {
      try {
        const r = await invoke<ImageConversionReport | null>(
          "image_conversion_status",
        );
        if (!canceled && r?.id === jobId.current) setReport(r);
      } catch {
        /* The explicit job command reports failures. */
      }
      if (!canceled) timer = setTimeout(poll, 350);
    }
    void poll();
    return () => {
      canceled = true;
      clearTimeout(timer);
    };
  }, [busy]);
  // Once per terminal state; do not continually steal scroll during progress.
  useEffect(() => {
    if (["completed", "failed", "canceled"].includes(report?.state ?? ""))
      progress.current?.scrollIntoView({ block: "start", behavior: "smooth" });
  }, [report?.state]);
  async function start() {
    if (
      !files ||
      !snapshotReady ||
      controller.current ||
      (needsStandalone && !standaloneConfirmed)
    )
      return;
    const c = new AbortController();
    controller.current = c;
    setBusy(true);
    onBusy(true);
    setStopping(false);
    setError("");
    setReport(null);
    jobId.current = null;
    try {
      const r = await convertImage(
        () => conversionReader({ ...files, parallelsSnapshot: snapshot }),
        format,
        {
          begin: (options) => invoke("image_conversion_begin", { options }),
          push: (id, offset, bytes, check) =>
            invoke(
              "image_conversion_chunk",
              conversionFrame(id, offset, bytes, check),
            ),
          finish: (id) => invoke("image_conversion_finish", { id }),
          cancel: (id) => invoke("image_conversion_cancel", { id }),
        },
        c.signal,
        (r) => {
          jobId.current = r.id;
          if (alive.current) setReport(r);
        },
      );
      if (alive.current && r) setReport(r);
    } catch (e) {
      const id = jobId.current;
      if (id) {
        const current = await invoke<ImageConversionReport | null>(
          "image_conversion_status",
        ).catch(() => null);
        if (alive.current && current?.id === id) setReport(current);
      }
      if (alive.current) {
        setError(e instanceof MissingImageFilesError
          ? t("{error} 还缺少：{files}", {
            error: catalogText(e.message),
            files: new Intl.ListFormat(language).format(e.missing),
          })
          : catalogText(e instanceof Error ? e.message : String(e)));
      }
    } finally {
      controller.current = null;
      jobId.current = null;
      if (alive.current) {
        setBusy(false);
        setStopping(false);
      }
      onBusy(false);
    }
  }
  function stop() {
    controller.current?.abort();
    setStopping(true);
    const id = jobId.current;
    if (id) void invoke("image_conversion_cancel", { id }).catch(() => {});
  }
  const total = report?.options.size ?? 0,
    done =
      (report?.received ?? 0) +
      (report?.sourceVerified ?? 0) +
      (report?.verified ?? 0);
  return (
    <Dialog
      title={t("镜像格式转换")}
      subtitle={t("将镜像保存为另一种格式的独立新文件")}
      onClose={() => {
        if (!busy) onClose();
      }}
      footer={
        <>
          {busy ? (
            <button className="button" disabled={stopping} onClick={stop}>
              <StopCircle size={16} />
              {stopping ? t("正在停止…") : t("停止转换")}
            </button>
          ) : (
            <button
              className="button primary"
              disabled={!files || !snapshotReady || (needsStandalone && !standaloneConfirmed)}
              onClick={() => void start()}
            >
              <ArrowRightLeft size={16} />
              {t("选择保存位置并转换")}
            </button>
          )}
          <button className="button" disabled={busy} onClick={onClose}>
            {t("关闭对话框")}
          </button>
        </>
      }
    >
      <fieldset className="maintenance-fields" disabled={busy}>
        <div className="image-conversion-source">
          <strong>{files?.file.name ?? t("选择源镜像")}</strong>
          <span>
            {files
              ? formatSize(files.file.size)
              : "RAW / IMG, VHD / AVHD, VHDX / AVHDX, VDI, VMDK, HDD / HDS, DiskDescriptor.xml"}
          </span>
          <button className="button" onClick={() => primary.current?.click()}>
            <FolderOpen size={16} />
            {files ? t("更换文件") : t("选择文件")}
          </button>
        </div>
        <input
          hidden
          ref={primary}
          type="file"
          accept=".raw,.img,.vhd,.avhd,.vhdx,.avhdx,.vdi,.vmdk,.hdd,.hds,.xml"
          onChange={(e) => {
            const file = e.target.files?.[0];
            if (file) {
              setFiles({ file, companions: [], sector: 512 });
              setStandaloneConfirmed(false);
              setError("");
              setReport(null);
            }
            e.target.value = "";
          }}
        />
        {needsStandalone && (
          <label className="image-conversion-standalone">
            <input
              type="checkbox"
              checked={standaloneConfirmed}
              onChange={(e) => setStandaloneConfirmed(e.target.checked)}
            />
            <span>
              {t(
                "确认这是独立 HDD/HDS 数据文件，不依赖其他快照。单个文件无法判断目录包中的快照关系。",
              )}
            </span>
          </label>
        )}
        {files && snapshotList?.list && (
          <ParallelsSnapshotPicker list={snapshotList.list} id={snapshot} onChange={(id) => {
            setFiles({ ...files, parallelsSnapshot: id });
            setReport(null);
            setError("");
          }} />
        )}
        {snapshotList?.error && <FormError message={catalogText(snapshotList.error)} />}
        {bundle && snapshotList?.list && !snapshotReady && <FormError message={catalogText("所选 Parallels 快照需要超过 16 层或 64 个文件。")} />}
        {files && (
          <>
            <div className="image-conversion-companions">
              <button
                className="button text-button"
                onClick={() => companions.current?.click()}
              >
                {t("选择分卷或父盘文件…")}
              </button>
              {files.companions.length ? (
                <small>{t("已选择 {count} 个文件", {
                  count: files.companions.length.toLocaleString(language),
                })}</small>
              ) : !isParallelsDescriptor(files.file.name) ? (
                <small>{t("用于差分 VHD、VHDX、VDI、VMDK 或分卷 VMDK")}</small>
              ) : null}
            </div>
            {!!files.companions.length && (
              <p className="image-conversion-names">
                {files.companions.map((f) => f.name).join(" · ")}
              </p>
            )}
            <input
              hidden
              multiple
              ref={companions}
              type="file"
              onChange={(e) => {
                const selected = Array.from(e.target.files ?? []);
                if (selected.length) {
                  setFiles({ ...files, companions: selected });
                  setReport(null);
                }
                e.target.value = "";
                setError("");
              }}
            />
            {/\.(raw|img)$/i.test(files.file.name) && (
              <label>
                {t("源逻辑扇区")}
                <select
                  value={files.sector}
                  onChange={(e) => {
                    setFiles({
                      ...files,
                      sector: Number(e.target.value) as 512 | 4096,
                    });
                    setReport(null);
                    setError("");
                  }}
                >
                  <option value={512}>512 B</option>
                  <option value={4096}>4 KiB</option>
                </select>
              </label>
            )}
          </>
        )}
        <label>
          {t("目标格式")}
          <select
            value={format}
            onChange={(e) => {
              setFormat(e.target.value as ImageOutputFormat);
              setReport(null);
              setError("");
            }}
          >
            {conversionFormats.map((f) => (
              <option key={f.value} value={f.value}>
                {catalogText(f.label)}
              </option>
            ))}
          </select>
        </label>
        <p className="image-conversion-note">
          {catalogText(
            conversionFormats.find((f) => f.value === format)?.detail ?? "",
          )}
        </p>
      </fieldset>
      <p className="image-conversion-note">
        {t(
          "请使用已关闭虚拟机的完整镜像副本。转换保留逻辑内容和容量，不调整文件系统，不覆盖源文件；差分链输出为独立镜像。需读取源两遍并完整回读目标。",
        )}
      </p>
      <FormError message={catalogText(error)} />
      {(busy || report) && (
        <section
          ref={progress}
          className="maintenance-progress"
          aria-live="polite"
        >
          <h3>
            {busy && <LoaderCircle size={16} className="spin" />}
            {phaseText}
          </h3>
          {report && total > 0 && (
            <>
              <progress value={Math.min(done, total * 3)} max={total * 3} />
              <p>
                {t("写入 {written} · 源复核 {checked} · 目标回读 {readBack}", {
                  written: formatSize(report.received),
                  checked: formatSize(report.sourceVerified),
                  readBack: formatSize(report.verified),
                })}
              </p>
            </>
          )}
          <p>{report ? catalogText(report.message) : t("正在检查源镜像…")}</p>
          {report?.result && (
            <dl className="detail-list">
              <dt>{t("输出文件")}</dt>
              <dd>{report.outputName}</dd>
              <dt>{t("逻辑容量")}</dt>
              <dd>{formatSize(report.result.logicalBytes)}</dd>
              <dt>{t("文件长度")}</dt>
              <dd>{formatSize(report.result.fileBytes)}</dd>
              <dt>{t("逻辑扇区")}</dt>
              <dd>{report.options.sector} B</dd>
              <dt>{t("逻辑 SHA-256")}</dt>
              <dd className="image-conversion-hash">{report.result.sha256}</dd>
            </dl>
          )}
          {report && !report.result && (
            <small>
              {t(
                "未完成文件 {file}。停止或失败时保留该文件，不会发布为成功镜像。",
                {
                  file: report.partialName,
                },
              )}
            </small>
          )}
        </section>
      )}
    </Dialog>
  );
}
