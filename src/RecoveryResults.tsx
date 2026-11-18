import { useEffect, useRef, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import {
  ChevronLeft,
  ChevronRight,
  FileSearch,
  FileText,
  Folder,
  FolderOpen,
  List,
  Search,
  SlidersHorizontal,
} from "lucide-react";
import { Dialog, Empty, FormError } from "./components";
import { useLocale } from "./i18n/LanguageProvider";
import { errorText } from "./partitionOutcome";
import { formatSize } from "./types";
import { FileProperties } from "./FileProperties";
import { defaultExportOptions, type ExportOptions } from "./recoveryModel";
import {
  fileState,
  dateFilter,
  initialRecoveryFilter,
  selectedFiles,
  sizeFilter,
} from "./recoveryModel";
import type {
  Candidate,
  RecoveryFilter,
  RecoveryFolders,
  RecoveryResultsPage,
  RecoveryLayout,
  RecoveryDirectory,
} from "./recoveryModel";

export interface RecoveryResultsService {
  exportReport(scanId: string): Promise<string | null>;
  results(
    scanId: string,
    offset: number,
    query: string,
    filter: RecoveryFilter,
  ): Promise<RecoveryResultsPage>;
  folders(
    scanId: string,
    folder: string | null,
    offset: number,
  ): Promise<RecoveryFolders>;
  select(
    scanId: string,
    query: string,
    filter: RecoveryFilter,
  ): Promise<number[]>;
}
const nativeService: RecoveryResultsService = {
  exportReport: (scanId) =>
    invoke("data_export_directory_report", { request: { scanId } }),
  results: (scanId, offset, query, filter) =>
    invoke("data_results", { scanId, offset, query, filter }),
  folders: (scanId, folder, offset) =>
    invoke("data_folders", { scanId, folder, offset }),
  select: (scanId, query, filter) =>
    invoke("data_select_results", { scanId, query, filter }),
};
export function RecoveryResults({
  scanId,
  revision,
  busy,
  running,
  onRecover,
  onPreview,
  service = nativeService,
  browsing = false,
}: {
  scanId: string;
  revision: string;
  busy: boolean;
  running: boolean;
  onRecover: (
    ids: number[],
    layout: RecoveryLayout,
    folder: string | undefined,
    options: ExportOptions,
  ) => void;
  onPreview: (candidate: Candidate) => void;
  service?: RecoveryResultsService;
  browsing?: boolean;
}) {
  const { t, language, catalogText } = useLocale();
  const count = (value = 0) => value.toLocaleString(language);
  const [filter, setFilter] = useState<RecoveryFilter>(() =>
    browsing
      ? {
          ...initialRecoveryFilter,
          folder: "root",
          descendants: false,
          state: "existing",
        }
      : initialRecoveryFilter,
  );
  const [properties, setProperties] = useState<Candidate | null>(null);
  const [directoryProperties, setDirectoryProperties] =
    useState<RecoveryDirectory | null>(null);
  const [exportOptions, setExportOptions] = useState<ExportOptions>(() => ({
    ...defaultExportOptions,
  }));
  const [query, setQuery] = useState("");
  const [offset, setOffset] = useState(0);
  const [folderOffset, setFolderOffset] = useState(0);
  const [results, setResults] = useState<RecoveryResultsPage | null>(null);
  const [folders, setFolders] = useState<RecoveryFolders | null>(null);
  const [checked, setChecked] = useState<Set<number>>(new Set());
  const [error, setError] = useState("");
  const [loading, setLoading] = useState(true);
  const [choosing, setChoosing] = useState(false);
  const [advanced, setAdvanced] = useState(false);
  const [reportOpen, setReportOpen] = useState(false);
  const [recoverOpen, setRecoverOpen] = useState(false);
  const [recoverFolder, setRecoverFolder] = useState<RecoveryDirectory | null>(
    null,
  );
  const [layout, setLayout] = useState<RecoveryLayout>("tree");
  const [exporting, setExporting] = useState(false);
  const [reportError, setReportError] = useState("");
  const [reportSaved, setReportSaved] = useState("");
  const [min, setMin] = useState("");
  const [max, setMax] = useState("");
  const [fromDate, setFromDate] = useState("");
  const [toDate, setToDate] = useState("");
  const checkAll = useRef<HTMLInputElement>(null);
  const selectionRequest = useRef(0);
  const disabled = busy || choosing || loading || exporting;
  const currentFolder = folders?.path.find((d) => d.id === filter.folder);
  const pageIds =
    results?.items.filter((c) => c.supported).map((c) => c.id) ?? [];
  const pageChecked = pageIds.filter((id) => checked.has(id)).length;
  const directoryName = (directory: RecoveryDirectory) =>
    directory.id === "root" ? t("根目录") : directory.name;
  function directoryPath(directory: string | null) {
    const path = directory ? results?.paths[directory] : undefined;
    if (!path?.length) return t("目录未知");
    return path[0] === "根目录"
      ? [t("根目录"), ...path.slice(1)].join(" / ")
      : path.join(" / ");
  }
  function candidateNote(note: string) {
    const history = note.lastIndexOf("；保存时的历史观察");
    if (history === -1 || language === "zh-CN") return catalogText(note);
    return `${catalogText(note.slice(0, history))} · ${catalogText(note.slice(history + 1))}`;
  }
  function directoryEvidence(note: string) {
    if (language === "zh-CN") return note;
    const aliases = "同一文件有多个名称；选中后分别保存副本";
    return note
      .replace(aliases, catalogText(aliases))
      .split("；")
      .map(catalogText)
      .join(" · ");
  }
  useEffect(() => {
    if (checkAll.current)
      checkAll.current.indeterminate =
        pageChecked > 0 && pageChecked < pageIds.length;
  }, [pageChecked, pageIds.length]);
  useEffect(() => {
    selectionRequest.current++;
    setChoosing(false);
    setLoading(true);
    let alive = true;
    const timer = setTimeout(() => {
      void Promise.all([
        service.results(scanId, offset, query, filter),
        service.folders(scanId, filter.folder, folderOffset),
      ])
        .then(([files, tree]) => {
          if (!alive) return;
          if (files.scanId !== scanId)
            throw Error("扫描结果已更新，请重新打开结果页。");
          setResults(files);
          setFolders(tree);
          setError("");
        })
        .catch((e) => {
          if (alive) {
            setError(errorText(e));
            setResults(null);
            setFolders(null);
          }
        })
        .finally(() => {
          if (alive) setLoading(false);
        });
    }, 150);
    return () => {
      alive = false;
      clearTimeout(timer);
      selectionRequest.current++;
    };
  }, [scanId, revision, offset, query, filter, folderOffset, service]);
  function update(patch: Partial<RecoveryFilter>) {
    setFilter((old) => ({ ...old, ...patch }));
    setOffset(0);
  }
  function navigate(folder: string | null) {
    update({ folder });
    setFolderOffset(0);
  }
  function toggle(ids: number[], select: boolean) {
    try {
      setChecked(selectedFiles(checked, ids, select));
      setError("");
    } catch (e) {
      setError(errorText(e));
    }
  }
  async function chooseMatching() {
    const token = ++selectionRequest.current;
    setChoosing(true);
    setError("");
    try {
      const ids = await service.select(scanId, query, filter);
      if (token === selectionRequest.current)
        setChecked(selectedFiles(checked, ids, true));
    } catch (e) {
      if (token === selectionRequest.current) setError(errorText(e));
    } finally {
      if (token === selectionRequest.current) setChoosing(false);
    }
  }
  function applySize() {
    try {
      update(sizeFilter(min, max));
      setError("");
    } catch (e) {
      setError(errorText(e));
    }
  }
  function applyDate() {
    try {
      update(dateFilter(fromDate, toDate));
      setError("");
    } catch (e) {
      setError(errorText(e));
    }
  }
  async function exportReport() {
    setExporting(true);
    setReportError("");
    try {
      const saved = await service.exportReport(scanId);
      if (saved !== null) {
        setReportSaved(saved);
        setReportOpen(false);
      }
    } catch (e) {
      setReportError(errorText(e));
    } finally {
      setExporting(false);
    }
  }
  return (
    <section
      className="data-results"
      aria-label={browsing ? t("分区文件列表") : t("恢复扫描结果")}
    >
      <div className="results-header">
        <div>
          <h2>
            {browsing ? t("文件与目录") : t("扫描结果")}{" "}
            <span>{count(results?.scannedTotal)}</span>
          </h2>
          <p>{results ? catalogText(results.source) : t("正在读取扫描结果")}</p>
        </div>
        <div className="results-actions">
          <button
            className="button"
            disabled={disabled || running || !results}
            onClick={() => {
              setReportError("");
              setReportOpen(true);
            }}
          >
            <FileText size={16} />
            {t("导出目录清单")}
          </button>
          <button
            className="button primary"
            disabled={disabled || running || checked.size === 0}
            onClick={() => {
              setRecoverFolder(null);
              setRecoverOpen(true);
            }}
          >
            {browsing ? t("复制所选") : t("恢复所选")}
            {checked.size ? ` (${count(checked.size)})` : ""}
          </button>
        </div>
      </div>
      {recoverOpen && (
        <Dialog
          title={
            recoverFolder
              ? browsing
                ? t("复制目录")
                : t("恢复目录")
              : browsing
                ? t("复制所选文件")
                : t("恢复所选文件")
          }
          subtitle={
            recoverFolder
              ? directoryName(recoverFolder)
              : t("已选择 {count} 个文件", { count: count(checked.size) })
          }
          onClose={() => setRecoverOpen(false)}
        >
          {!recoverFolder && (
            <fieldset className="recovery-layout">
              <legend>{t("保存方式")}</legend>
              <label>
                <input
                  type="radio"
                  name="recovery-layout"
                  checked={layout === "tree"}
                  onChange={() => setLayout("tree")}
                />
                <span>
                  {t("保留原目录结构")}
                  <small>
                    {t("按已确认的目录层级保存，目录未知的文件单独归组")}
                  </small>
                </span>
              </label>
              <label>
                <input
                  type="radio"
                  name="recovery-layout"
                  checked={layout === "flat"}
                  onChange={() => setLayout("flat")}
                />
                <span>
                  {t("放在同一目录")}
                  <small>{t("所有所选文件放在一起，添加编号避免重名")}</small>
                </span>
              </label>
            </fieldset>
          )}
          <details className="recovery-copy-details">
            <summary>{t("复制范围与限制")}</summary>
            <p className="report-dialog-description">
              {recoverFolder
                ? t(
                    "保留原目录结构，包含此目录下已读取的文件、子目录和空目录，不受搜索、筛选或文件勾选影响。尚未读取到的内容不会导出；不支持的文件将跳过并列入结果清单。",
                  )
                : t(
                    "仅包含所选文件及其父目录，不含未选文件或独立空目录。",
                  )}{" "}
              {t(
                "保存到另一块磁盘的新文件夹，重名和不兼容名称会调整并记录。每个成功文件都需通过完整 SHA-256 回读校验。",
              )}
            </p>
            <p className="report-dialog-description">
              {t(
                "同一文件的多个名称按选择分别保存为副本，不重建硬链接、权限或扩展属性。",
              )}{" "}
              {browsing
                ? t("在线文件可能在复制期间变化，这不是时间点快照。")
                : t("已删除文件的内容可能不完整。")}
            </p>
          </details>
          <fieldset className="recovery-copy-options">
            <legend>{t("复制选项")}</legend>
            <label>
              <input
                type="checkbox"
                checked={exportOptions.timestamps}
                onChange={(e) =>
                  setExportOptions({
                    ...exportOptions,
                    timestamps: e.target.checked,
                  })
                }
              />
              {t("保留文件与目录时间")}
            </label>
            <label>
              <input
                type="checkbox"
                checked={exportOptions.attributes}
                onChange={(e) =>
                  setExportOptions({
                    ...exportOptions,
                    attributes: e.target.checked,
                  })
                }
              />
              {t("保留受支持的属性")}
            </label>
            <label>
              {t("并发任务")}
              <select
                value={exportOptions.workers}
                onChange={(e) =>
                  setExportOptions({
                    ...exportOptions,
                    workers: Number(e.target.value) as 1 | 2 | 4,
                  })
                }
              >
                <option value={1}>1</option>
                <option value={2}>2</option>
                <option value={4}>4</option>
              </select>
            </label>
            <small>
              {t(
                "时区未知的时间不作推算；无法保留的字段和输出精度变化会记录在清单中。",
              )}
            </small>
          </fieldset>
          <div className="dialog-actions">
            <button className="button" onClick={() => setRecoverOpen(false)}>
              {t("取消")}
            </button>
            <button
              className="button primary"
              disabled={
                disabled || running || (!recoverFolder && checked.size === 0)
              }
              onClick={() => {
                setRecoverOpen(false);
                if (recoverFolder)
                  onRecover([], "tree", recoverFolder.id, exportOptions);
                else onRecover([...checked], layout, undefined, exportOptions);
              }}
            >
              {t("选择保存位置")}
            </button>
          </div>
        </Dialog>
      )}
      {reportSaved && (
        <p role="status" className="report-saved">
          {t("目录清单已保存：{path}", { path: reportSaved })}
        </p>
      )}
      {reportOpen && (
        <Dialog
          title={t("导出目录清单")}
          subtitle={t("保存为可离线浏览和搜索的 HTML 文件")}
          onClose={() => {
            if (!exporting) setReportOpen(false);
          }}
        >
          <p className="report-dialog-description">
            {t(
              "清单包含本次已读取的全部 {count} 个文件及已知目录，不受当前筛选或勾选影响。清单不包含文件内容，也不会重新读取源盘。",
              {
                count: count(results?.scannedTotal),
              },
            )}
          </p>
          <p className="report-dialog-description">
            {t(
              "目录和删除状态可能不完整。请保存到另一块磁盘，避免覆盖源盘上仍可能恢复的数据；已有文件不会被覆盖。",
            )}
          </p>
          <FormError message={catalogText(reportError)} />
          <div className="dialog-actions">
            <button
              className="button"
              disabled={exporting}
              onClick={() => setReportOpen(false)}
            >
              {t("取消")}
            </button>
            <button
              className="button primary"
              disabled={exporting || busy || running}
              onClick={() => void exportReport()}
            >
              {exporting ? t("正在保存…") : t("保存 HTML")}
            </button>
          </div>
        </Dialog>
      )}
      <div className="recovery-filters">
        <label className="recovery-search">
          <Search size={16} />
          <input
            aria-label={browsing ? t("筛选分区文件") : t("筛选恢复文件")}
            maxLength={200}
            placeholder={
              filter.nameMatch === "extension"
                ? t("扩展名，例如 txt 或 .jpg")
                : t("搜索已读取的文件名")
            }
            value={query}
            onChange={(e) => {
              setQuery(e.target.value);
              setOffset(0);
            }}
          />
        </label>
        {!browsing && (
          <select
            aria-label={t("删除状态筛选")}
            value={filter.state}
            onChange={(e) =>
              update({ state: e.target.value as RecoveryFilter["state"] })
            }
          >
            <option value="all">{t("全部状态")}</option>
            <option value="deleted">{t("已删除")}</option>
            <option value="existing">{t("现存文件")}</option>
            <option value="unknown">{t("状态未知")}</option>
          </select>
        )}
        <select
          aria-label={t("文件类型筛选")}
          title={t("按扩展名分类")}
          value={filter.kind}
          onChange={(e) =>
            update({ kind: e.target.value as RecoveryFilter["kind"] })
          }
        >
          <option value="all">{t("全部类型")}</option>
          <option value="documents">{t("文档")}</option>
          <option value="images">{t("图片")}</option>
          <option value="videos">{t("视频")}</option>
          <option value="audio">{t("音频")}</option>
          <option value="archives">{t("压缩包")}</option>
          <option value="other">{t("其他")}</option>
        </select>
        <select
          aria-label={t("文件排序")}
          value={filter.sortBy}
          onChange={(e) =>
            update({ sortBy: e.target.value as RecoveryFilter["sortBy"] })
          }
        >
          <option value="scan">{t("扫描顺序")}</option>
          <option value="name">{t("文件名")}</option>
          <option value="size">{t("文件大小")}</option>
          <option value="extension">{t("扩展名")}</option>
          <option value="modified">{t("修改时间")}</option>
        </select>
        <button
          className="button"
          aria-label={
            filter.sortDirection === "asc"
              ? t("当前升序，切换为降序")
              : t("当前降序，切换为升序")
          }
          onClick={() =>
            update({
              sortDirection: filter.sortDirection === "asc" ? "desc" : "asc",
            })
          }
        >
          {filter.sortDirection === "asc" ? t("↑ 升序") : t("↓ 降序")}
        </button>
        <button
          className="button"
          aria-expanded={advanced}
          onClick={() => setAdvanced(!advanced)}
        >
          <SlidersHorizontal size={15} />
          {t("更多筛选")}
        </button>
      </div>
      {advanced && (
        <div className="recovery-advanced">
          <label>
            {t("名称匹配")}
            <select
              aria-label={t("名称匹配方式")}
              value={filter.nameMatch}
              onChange={(e) =>
                update({
                  nameMatch: e.target.value as RecoveryFilter["nameMatch"],
                })
              }
            >
              <option value="contains">{t("包含文字")}</option>
              <option value="exact">{t("完整名称")}</option>
              <option value="prefix">{t("名称开头")}</option>
              <option value="extension">{t("完整扩展名")}</option>
            </select>
          </label>
          <label>
            <input
              type="checkbox"
              checked={filter.caseSensitive}
              onChange={(e) => update({ caseSensitive: e.target.checked })}
            />
            {t("区分大小写")}
          </label>
          <form
            onSubmit={(e) => {
              e.preventDefault();
              applySize();
            }}
          >
            <span>{t("容量 (MiB)")}</span>
            <input
              aria-label={t("最小文件容量 MiB")}
              inputMode="decimal"
              placeholder={t("最小")}
              value={min}
              onChange={(e) => setMin(e.target.value)}
            />
            <span>—</span>
            <input
              aria-label={t("最大文件容量 MiB")}
              inputMode="decimal"
              placeholder={t("最大")}
              value={max}
              onChange={(e) => setMax(e.target.value)}
            />
            <button className="button" type="submit">
              {t("应用")}
            </button>
          </form>
          <form
            className="recovery-date-filter"
            onSubmit={(e) => {
              e.preventDefault();
              applyDate();
            }}
          >
            <select
              aria-label={t("日期筛选字段")}
              value={filter.dateField}
              onChange={(e) =>
                update({
                  dateField: e.target.value as RecoveryFilter["dateField"],
                })
              }
            >
              <option value="modified">{t("修改日期")}</option>
              <option value="created">{t("创建日期")}</option>
              <option value="accessed">{t("访问日期")}</option>
              <option value="changed">{t("元数据变更日期")}</option>
            </select>
            <input
              type="date"
              aria-label={t("开始日期")}
              min="0001-01-01"
              max="9999-12-31"
              value={fromDate}
              onChange={(e) => setFromDate(e.target.value)}
            />
            <span>—</span>
            <input
              type="date"
              aria-label={t("结束日期")}
              min="0001-01-01"
              max="9999-12-31"
              value={toDate}
              onChange={(e) => setToDate(e.target.value)}
            />
            <button className="button" type="submit">
              {t("应用日期")}
            </button>
          </form>
          <label>
            <input
              type="checkbox"
              checked={filter.supportedOnly}
              onChange={(e) => update({ supportedOnly: e.target.checked })}
            />
            {t("仅可导出")}
          </label>
          <button
            className="button text-button"
            onClick={() => {
              setMin("");
              setMax("");
              setFromDate("");
              setToDate("");
              setQuery("");
              setFilter({
                ...initialRecoveryFilter,
                folder: filter.folder,
                ...(browsing
                  ? { state: "existing" as const, descendants: false }
                  : {}),
              });
              setOffset(0);
              setError("");
            }}
          >
            {t("重置筛选")}
          </button>
          <p className="recovery-query-note">
            {t(
              "日期范围包含首尾两天，按记录显示的日期筛选，不换算时区；未记录日期的文件不匹配。修改时间排序仅比较带明确时区的完整时间，其他值始终排在末尾。名称按文字匹配，* 和 ? 不作通配符。",
            )}
          </p>
        </div>
      )}
      {error && <FormError message={catalogText(error)} />}
      <div className="recovery-browser">
        <nav
          className="recovery-directories"
          aria-label={browsing ? t("分区目录") : t("扫描目录")}
        >
          <button
            className={filter.folder === null ? "active" : ""}
            onClick={() => navigate(null)}
          >
            <List size={16} />
            <span>{browsing ? t("全部文件") : t("全部结果")}</span>
            <small>{count(results?.scannedTotal)}</small>
          </button>
          <button
            className={filter.folder === "unknown" ? "active" : ""}
            onClick={() => navigate("unknown")}
          >
            <FileSearch size={16} />
            <span>{t("目录未知")}</span>
            <small>{count(folders?.unknownFiles)}</small>
          </button>
          <div className="directory-divider">{t("原目录")}</div>
          <button
            className={filter.folder === "root" ? "active" : ""}
            onClick={() => navigate("root")}
          >
            <FolderOpen size={16} />
            <span>{t("根目录")}</span>
          </button>
          {folders?.path
            .filter((d) => d.id !== "root")
            .map((d) => (
              <button
                key={d.id}
                title={d.name}
                className={`directory-ancestor ${filter.folder === d.id ? "active" : ""}`}
                onClick={() => navigate(d.id)}
              >
                <ChevronRight size={13} />
                <span>{d.name}</span>
                {d.deleted && <small>{t("已删除")}</small>}
              </button>
            ))}
          <div className="directory-children" aria-busy={loading}>
            {folders?.items.map((d) => (
              <button
                key={d.id}
                title={`${d.name} · ${d.id}`}
                onClick={() => navigate(d.id)}
              >
                <Folder size={16} />
                <span>{d.name}</span>
                <small>{count(d.files)}</small>
                <ChevronRight size={12} />
              </button>
            ))}
            {folders?.items.length === 0 && (
              <p>
                {filter.folder === "unknown"
                  ? t("无法确认原目录的文件")
                  : t("没有下级目录")}
              </p>
            )}
          </div>
          {folders && folders.total > 100 && (
            <div className="directory-pages">
              <button
                className="icon-button"
                aria-label={t("上一页目录")}
                disabled={loading || folders.offset === 0}
                onClick={() => setFolderOffset(folders.offset - 100)}
              >
                <ChevronLeft size={16} />
              </button>
              <span>
                {count(folders.offset + 1)}–
                {count(Math.min(folders.offset + 100, folders.total))} /{" "}
                {count(folders.total)}
              </span>
              <button
                className="icon-button"
                aria-label={t("下一页目录")}
                disabled={loading || folders.offset + 100 >= folders.total}
                onClick={() => setFolderOffset(folders.offset + 100)}
              >
                <ChevronRight size={16} />
              </button>
            </div>
          )}
        </nav>
        <div className="recovery-file-pane" aria-busy={loading}>
          {currentFolder && (
            <div className="recovery-directory-actions">
              <span title={directoryName(currentFolder)}>
                <FolderOpen size={16} />
                <strong>{directoryName(currentFolder)}</strong>
              </span>
              <button
                className="button text-button"
                disabled={disabled}
                onClick={() => setDirectoryProperties(currentFolder)}
              >
                {t("目录属性")}
              </button>
              <button
                className="button"
                disabled={
                  disabled ||
                  running ||
                  !results ||
                  !folders?.canExportDirectory
                }
                onClick={() => {
                  setRecoverFolder(currentFolder);
                  setRecoverOpen(true);
                }}
              >
                {browsing ? t("复制此目录…") : t("恢复此目录…")}
              </button>
            </div>
          )}
          <div className="recovery-selection">
            <span>
              {loading
                ? t("正在筛选…")
                : t("匹配数 {count}", { count: count(results?.total) })}
              {checked.size > 0 && (
                <>
                  {" · "}
                  {t("已选 {count}", { count: count(checked.size) })}
                </>
              )}
            </span>
            <button
              className="button text-button"
              disabled={disabled || !results?.total}
              onClick={() => void chooseMatching()}
            >
              {t("选择全部匹配")}
            </button>
            {checked.size > 0 && (
              <button
                className="button text-button"
                disabled={busy || choosing}
                onClick={() => setChecked(new Set())}
              >
                {t("清空选择")}
              </button>
            )}
          </div>
          {filter.folder && filter.folder !== "unknown" && (
            <label className="recovery-descendants">
              <input
                type="checkbox"
                checked={filter.descendants}
                onChange={(e) => update({ descendants: e.target.checked })}
              />
              {t("包含子目录")}
            </label>
          )}
          {results?.items.length ? (
            <>
              <div className="table-wrap">
                <table className="recovery-table">
                  <thead>
                    <tr>
                      <th>
                        <input
                          ref={checkAll}
                          type="checkbox"
                          aria-label={
                            browsing
                              ? t("选择当前页可复制文件")
                              : t("选择当前页可恢复文件")
                          }
                          disabled={disabled || !pageIds.length}
                          checked={
                            pageIds.length > 0 && pageChecked === pageIds.length
                          }
                          onChange={(e) => toggle(pageIds, e.target.checked)}
                        />
                      </th>
                      <th>{t("文件名 / 目录")}</th>
                      <th>{t("大小")}</th>
                      <th>{t("修改时间")}</th>
                      <th>{t("来源 / 状态")}</th>
                      <th />
                    </tr>
                  </thead>
                  <tbody>
                    {results.items.map((c) => {
                      const filesystem =
                        /^(FAT(?:12|16|32)?|exFAT|Ext) 目录$/.exec(
                          c.method,
                        )?.[1];
                      const directoryNote = directoryEvidence(c.directoryNote);
                      return (
                        <tr
                          key={c.id}
                          className={checked.has(c.id) ? "selected" : ""}
                        >
                          <td>
                            <input
                              type="checkbox"
                              aria-label={t("选择 {name}", { name: c.name })}
                              disabled={disabled || !c.supported}
                              checked={checked.has(c.id)}
                              onChange={(e) => toggle([c.id], e.target.checked)}
                            />
                          </td>
                          <td>
                            <span className="recovery-file">
                              <FileText size={18} />
                              <strong title={c.name}>{c.name}</strong>
                            </span>
                            <button
                              className="recovery-path"
                              disabled={loading}
                              title={
                                directoryNote || directoryPath(c.directory)
                              }
                              onClick={() => navigate(c.directory ?? "unknown")}
                            >
                              {directoryPath(c.directory)}
                            </button>
                            <small className="recovery-note">
                              {candidateNote(c.note)}
                              {directoryNote ? ` · ${directoryNote}` : ""}
                            </small>
                          </td>
                          <td>{formatSize(c.size)}</td>
                          <td
                            className="file-time"
                            title={
                              c.metadata?.modified
                                ? t("{zone} · 精度 {precision}", {
                                    zone: catalogText(c.metadata.modified.zone),
                                    precision: catalogText(
                                      c.metadata.modified.precision,
                                    ),
                                  })
                                : t("未记录或无法解析")
                            }
                          >
                            {c.metadata?.modified?.text.split(".")[0] ?? "—"}
                            <small>
                              {catalogText(c.metadata?.modified?.zone ?? "")}
                            </small>
                          </td>
                          <td>
                            {filesystem
                              ? t("{filesystem} 目录", { filesystem })
                              : catalogText(c.method)}
                            <small>{catalogText(fileState(c))}</small>
                            {!!c.metadata?.attributes.length && (
                              <small className="file-attributes">
                                {c.metadata.attributes
                                  .map(catalogText)
                                  .join(" · ")}
                              </small>
                            )}
                          </td>
                          <td>
                            <button
                              className="button text-button"
                              onClick={() => setProperties(c)}
                            >
                              {t("属性")}
                            </button>
                            <button
                              className="button text-button"
                              disabled={disabled || !c.supported}
                              onClick={() => onPreview(c)}
                            >
                              {t("预览")}
                            </button>
                          </td>
                        </tr>
                      );
                    })}
                  </tbody>
                </table>
              </div>
              <div className="results-pagination">
                <span>
                  {count(results.offset + 1)}–
                  {count(
                    Math.min(
                      results.offset + results.items.length,
                      results.total,
                    ),
                  )}{" "}
                  / {count(results.total)}
                </span>
                <button
                  className="icon-button"
                  aria-label={t("上一页文件")}
                  disabled={loading || results.offset === 0}
                  onClick={() => setOffset(results.offset - 100)}
                >
                  <ChevronLeft size={17} />
                </button>
                <button
                  className="icon-button"
                  aria-label={t("下一页文件")}
                  disabled={loading || results.offset + 100 >= results.total}
                  onClick={() => setOffset(results.offset + 100)}
                >
                  <ChevronRight size={17} />
                </button>
              </div>
            </>
          ) : (
            <Empty
              icon={<FileSearch size={30} />}
              title={
                loading
                  ? t("正在读取结果")
                  : running
                    ? t("正在寻找文件")
                    : t("没有匹配的文件")
              }
              text={t(
                "可更改目录或筛选条件。目录未知的文件不会被猜测归入其他目录。",
              )}
            />
          )}
        </div>
      </div>
      <p className="recovery-export-note">
        {t(
          "可按已确认的原目录结构保存，或集中保存。目录关系来自本次读取，可能不完整。名称调整和导出结果会记录在清单中。",
        )}
      </p>
      {properties && (
        <FileProperties
          file={properties}
          directory={directoryPath(properties.directory)}
          onClose={() => setProperties(null)}
        />
      )}
      {directoryProperties && (
        <FileProperties
          file={directoryProperties}
          kind="directory"
          directory={
            folders?.path
              .filter((d) => d.id !== directoryProperties.id)
              .map(directoryName)
              .join(" / ") || t("根目录")
          }
          onClose={() => setDirectoryProperties(null)}
        />
      )}
    </section>
  );
}
