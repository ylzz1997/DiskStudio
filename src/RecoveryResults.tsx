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
            setError(String(e));
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
      setError(String(e));
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
      if (token === selectionRequest.current) setError(String(e));
    } finally {
      if (token === selectionRequest.current) setChoosing(false);
    }
  }
  function applySize() {
    try {
      update(sizeFilter(min, max));
      setError("");
    } catch (e) {
      setError(String(e));
    }
  }
  function applyDate() {
    try {
      update(dateFilter(fromDate, toDate));
      setError("");
    } catch (e) {
      setError(String(e));
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
      setReportError(e instanceof Error ? e.message : String(e));
    } finally {
      setExporting(false);
    }
  }
  return (
    <section
      className="data-results"
      aria-label={browsing ? "分区文件列表" : "恢复扫描结果"}
    >
      <div className="results-header">
        <div>
          <h2>
            {browsing ? "文件与目录" : "扫描结果"}{" "}
            <span>{results?.scannedTotal.toLocaleString() ?? 0}</span>
          </h2>
          <p>{results?.source ?? "正在读取扫描结果"}</p>
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
            导出目录清单
          </button>
          <button
            className="button primary"
            disabled={disabled || running || checked.size === 0}
            onClick={() => {
              setRecoverFolder(null);
              setRecoverOpen(true);
            }}
          >
            {browsing ? "复制所选" : "恢复所选"}
            {checked.size ? ` (${checked.size})` : ""}
          </button>
        </div>
      </div>
      {recoverOpen && (
        <Dialog
          title={
            recoverFolder
              ? browsing
                ? "复制目录"
                : "恢复目录"
              : browsing
                ? "复制所选文件"
                : "恢复所选文件"
          }
          subtitle={
            recoverFolder
              ? recoverFolder.name
              : `已选择 ${checked.size.toLocaleString()} 个文件`
          }
          onClose={() => setRecoverOpen(false)}
        >
          {!recoverFolder && (
            <fieldset className="recovery-layout">
              <legend>保存方式</legend>
              <label>
                <input
                  type="radio"
                  name="recovery-layout"
                  checked={layout === "tree"}
                  onChange={() => setLayout("tree")}
                />
                <span>
                  保留原目录结构
                  <small>按已确认的目录层级保存，目录未知的文件单独归组</small>
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
                  放在同一目录
                  <small>所有所选文件放在一起，添加编号避免重名</small>
                </span>
              </label>
            </fieldset>
          )}
          <details className="recovery-copy-details">
            <summary>复制范围与限制</summary>
            <p className="report-dialog-description">
              {recoverFolder
                ? "保留原目录结构，包含此目录下已读取的文件、子目录和空目录，不受搜索、筛选或文件勾选影响。尚未读取到的内容不会导出；不支持的文件将跳过并列入结果清单。"
                : "仅包含所选文件及其父目录，不含未选文件或独立空目录。"}
              保存到另一块磁盘的新文件夹，重名和不兼容名称会调整并记录。每个成功文件都需通过完整
              SHA-256 回读校验。
            </p>
            <p className="report-dialog-description">
              同一文件的多个名称按选择分别保存为副本，不重建硬链接、权限或扩展属性；
              {browsing
                ? "在线文件可能在复制期间变化，这不是时间点快照。"
                : "已删除文件的内容可能不完整。"}
            </p>
          </details>
          <fieldset className="recovery-copy-options">
            <legend>复制选项</legend>
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
              保留文件与目录时间
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
              保留受支持的属性
            </label>
            <label>
              并发任务
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
              时区未知的时间不作推算；无法保留的字段和输出精度变化会记录在清单中。
            </small>
          </fieldset>
          <div className="dialog-actions">
            <button className="button" onClick={() => setRecoverOpen(false)}>
              取消
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
              选择保存位置
            </button>
          </div>
        </Dialog>
      )}
      {reportSaved && (
        <p role="status" className="report-saved">
          目录清单已保存：{reportSaved}
        </p>
      )}
      {reportOpen && (
        <Dialog
          title="导出目录清单"
          subtitle="保存为可离线浏览和搜索的 HTML 文件"
          onClose={() => {
            if (!exporting) setReportOpen(false);
          }}
        >
          <p className="report-dialog-description">
            包含本次{browsing ? "目录读取" : "扫描"}已读取的全部{" "}
            {results?.scannedTotal.toLocaleString() ?? 0}{" "}
            个文件及已知目录，不受当前筛选或勾选影响。清单不包含文件内容，也不会重新读取源盘。
          </p>
          <p className="report-dialog-description">
            目录和删除状态可能不完整。请保存到另一块磁盘，避免覆盖源盘上仍可能恢复的数据；已有文件不会被覆盖。
          </p>
          <FormError message={reportError} />
          <div className="dialog-actions">
            <button
              className="button"
              disabled={exporting}
              onClick={() => setReportOpen(false)}
            >
              取消
            </button>
            <button
              className="button primary"
              disabled={exporting || busy || running}
              onClick={() => void exportReport()}
            >
              {exporting ? "正在保存…" : "保存 HTML"}
            </button>
          </div>
        </Dialog>
      )}
      <div className="recovery-filters">
        <label className="recovery-search">
          <Search size={16} />
          <input
            aria-label={browsing ? "筛选分区文件" : "筛选恢复文件"}
            maxLength={200}
            placeholder={
              filter.nameMatch === "extension"
                ? "扩展名，例如 txt 或 .jpg"
                : "搜索已读取的文件名"
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
            aria-label="删除状态筛选"
            value={filter.state}
            onChange={(e) =>
              update({ state: e.target.value as RecoveryFilter["state"] })
            }
          >
            <option value="all">全部状态</option>
            <option value="deleted">已删除</option>
            <option value="existing">现存文件</option>
            <option value="unknown">状态未知</option>
          </select>
        )}
        <select
          aria-label="文件类型筛选"
          title="按扩展名分类"
          value={filter.kind}
          onChange={(e) =>
            update({ kind: e.target.value as RecoveryFilter["kind"] })
          }
        >
          <option value="all">全部类型</option>
          <option value="documents">文档</option>
          <option value="images">图片</option>
          <option value="videos">视频</option>
          <option value="audio">音频</option>
          <option value="archives">压缩包</option>
          <option value="other">其他</option>
        </select>
        <select
          aria-label="文件排序"
          value={filter.sortBy}
          onChange={(e) =>
            update({ sortBy: e.target.value as RecoveryFilter["sortBy"] })
          }
        >
          <option value="scan">扫描顺序</option>
          <option value="name">文件名</option>
          <option value="size">文件大小</option>
          <option value="extension">扩展名</option>
          <option value="modified">修改时间</option>
        </select>
        <button
          className="button"
          aria-label={
            filter.sortDirection === "asc"
              ? "当前升序，切换为降序"
              : "当前降序，切换为升序"
          }
          onClick={() =>
            update({
              sortDirection: filter.sortDirection === "asc" ? "desc" : "asc",
            })
          }
        >
          {filter.sortDirection === "asc" ? "↑ 升序" : "↓ 降序"}
        </button>
        <button
          className="button"
          aria-expanded={advanced}
          onClick={() => setAdvanced(!advanced)}
        >
          <SlidersHorizontal size={15} />
          更多筛选
        </button>
      </div>
      {advanced && (
        <div className="recovery-advanced">
          <label>
            名称匹配
            <select
              aria-label="名称匹配方式"
              value={filter.nameMatch}
              onChange={(e) =>
                update({
                  nameMatch: e.target.value as RecoveryFilter["nameMatch"],
                })
              }
            >
              <option value="contains">包含文字</option>
              <option value="exact">完整名称</option>
              <option value="prefix">名称开头</option>
              <option value="extension">完整扩展名</option>
            </select>
          </label>
          <label>
            <input
              type="checkbox"
              checked={filter.caseSensitive}
              onChange={(e) => update({ caseSensitive: e.target.checked })}
            />
            区分大小写
          </label>
          <form
            onSubmit={(e) => {
              e.preventDefault();
              applySize();
            }}
          >
            <span>容量 (MiB)</span>
            <input
              aria-label="最小文件容量 MiB"
              inputMode="decimal"
              placeholder="最小"
              value={min}
              onChange={(e) => setMin(e.target.value)}
            />
            <span>—</span>
            <input
              aria-label="最大文件容量 MiB"
              inputMode="decimal"
              placeholder="最大"
              value={max}
              onChange={(e) => setMax(e.target.value)}
            />
            <button className="button" type="submit">
              应用
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
              aria-label="日期筛选字段"
              value={filter.dateField}
              onChange={(e) =>
                update({
                  dateField: e.target.value as RecoveryFilter["dateField"],
                })
              }
            >
              <option value="modified">修改日期</option>
              <option value="created">创建日期</option>
              <option value="accessed">访问日期</option>
              <option value="changed">元数据变更日期</option>
            </select>
            <input
              type="date"
              aria-label="开始日期"
              min="0001-01-01"
              max="9999-12-31"
              value={fromDate}
              onChange={(e) => setFromDate(e.target.value)}
            />
            <span>—</span>
            <input
              type="date"
              aria-label="结束日期"
              min="0001-01-01"
              max="9999-12-31"
              value={toDate}
              onChange={(e) => setToDate(e.target.value)}
            />
            <button className="button" type="submit">
              应用日期
            </button>
          </form>
          <label>
            <input
              type="checkbox"
              checked={filter.supportedOnly}
              onChange={(e) => update({ supportedOnly: e.target.checked })}
            />
            仅可导出
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
            重置筛选
          </button>
          <p className="recovery-query-note">
            日期范围包含首尾两天，按记录显示的日期筛选，不换算时区；未记录日期的文件不匹配。
            修改时间排序仅比较带明确时区的完整时间，其他值始终排在末尾。名称按文字匹配，*
            和 ? 不作通配符。
          </p>
        </div>
      )}
      {error && <FormError message={error} />}
      <div className="recovery-browser">
        <nav
          className="recovery-directories"
          aria-label={browsing ? "分区目录" : "扫描目录"}
        >
          <button
            className={filter.folder === null ? "active" : ""}
            onClick={() => navigate(null)}
          >
            <List size={16} />
            <span>{browsing ? "全部文件" : "全部结果"}</span>
            <small>{results?.scannedTotal ?? 0}</small>
          </button>
          <button
            className={filter.folder === "unknown" ? "active" : ""}
            onClick={() => navigate("unknown")}
          >
            <FileSearch size={16} />
            <span>目录未知</span>
            <small>{folders?.unknownFiles ?? 0}</small>
          </button>
          <div className="directory-divider">原目录</div>
          <button
            className={filter.folder === "root" ? "active" : ""}
            onClick={() => navigate("root")}
          >
            <FolderOpen size={16} />
            <span>根目录</span>
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
                {d.deleted && <small>已删除</small>}
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
                <small>{d.files}</small>
                <ChevronRight size={12} />
              </button>
            ))}
            {folders?.items.length === 0 && (
              <p>
                {filter.folder === "unknown"
                  ? "无法确认原目录的文件"
                  : "没有下级目录"}
              </p>
            )}
          </div>
          {folders && folders.total > 100 && (
            <div className="directory-pages">
              <button
                className="icon-button"
                aria-label="上一页目录"
                disabled={loading || folders.offset === 0}
                onClick={() => setFolderOffset(folders.offset - 100)}
              >
                <ChevronLeft size={16} />
              </button>
              <span>
                {folders.offset + 1}–
                {Math.min(folders.offset + 100, folders.total)} /{" "}
                {folders.total}
              </span>
              <button
                className="icon-button"
                aria-label="下一页目录"
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
              <span title={currentFolder.name}>
                <FolderOpen size={16} />
                <strong>{currentFolder.name}</strong>
              </span>
              <button
                className="button text-button"
                disabled={disabled}
                onClick={() => setDirectoryProperties(currentFolder)}
              >
                目录属性
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
                {browsing ? "复制此目录…" : "恢复此目录…"}
              </button>
            </div>
          )}
          <div className="recovery-selection">
            <span>
              {loading
                ? "正在筛选…"
                : `${results?.total.toLocaleString() ?? 0} 个匹配`}
              {checked.size ? ` · 已选 ${checked.size}` : ""}
            </span>
            <button
              className="button text-button"
              disabled={disabled || !results?.total}
              onClick={() => void chooseMatching()}
            >
              选择全部匹配
            </button>
            {checked.size > 0 && (
              <button
                className="button text-button"
                disabled={busy || choosing}
                onClick={() => setChecked(new Set())}
              >
                清空选择
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
              包含子目录
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
                              ? "选择当前页可复制文件"
                              : "选择当前页可恢复文件"
                          }
                          disabled={disabled || !pageIds.length}
                          checked={
                            pageIds.length > 0 && pageChecked === pageIds.length
                          }
                          onChange={(e) => toggle(pageIds, e.target.checked)}
                        />
                      </th>
                      <th>文件名 / 目录</th>
                      <th>大小</th>
                      <th>修改时间</th>
                      <th>来源 / 状态</th>
                      <th />
                    </tr>
                  </thead>
                  <tbody>
                    {results.items.map((c) => (
                      <tr
                        key={c.id}
                        className={checked.has(c.id) ? "selected" : ""}
                      >
                        <td>
                          <input
                            type="checkbox"
                            aria-label={`选择 ${c.name}`}
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
                              c.directoryNote ||
                              (c.directory
                                ? results.paths[c.directory]?.join(" / ")
                                : "目录未知")
                            }
                            onClick={() => navigate(c.directory ?? "unknown")}
                          >
                            {c.directory
                              ? (results.paths[c.directory]?.join(" / ") ??
                                "目录未知")
                              : "目录未知"}
                          </button>
                          <small className="recovery-note">
                            {c.note}
                            {c.directoryNote ? ` · ${c.directoryNote}` : ""}
                          </small>
                        </td>
                        <td>{formatSize(c.size)}</td>
                        <td
                          className="file-time"
                          title={
                            c.metadata?.modified
                              ? `${c.metadata.modified.zone} · 精度 ${c.metadata.modified.precision}`
                              : "未记录或无法解析"
                          }
                        >
                          {c.metadata?.modified?.text.split(".")[0] ?? "—"}
                          <small>{c.metadata?.modified?.zone ?? ""}</small>
                        </td>
                        <td>
                          {c.method}
                          <small>{fileState(c)}</small>
                          {!!c.metadata?.attributes.length && (
                            <small className="file-attributes">
                              {c.metadata.attributes.join(" · ")}
                            </small>
                          )}
                        </td>
                        <td>
                          <button
                            className="button text-button"
                            onClick={() => setProperties(c)}
                          >
                            属性
                          </button>
                          <button
                            className="button text-button"
                            disabled={disabled || !c.supported}
                            onClick={() => onPreview(c)}
                          >
                            预览
                          </button>
                        </td>
                      </tr>
                    ))}
                  </tbody>
                </table>
              </div>
              <div className="results-pagination">
                <span>
                  {results.offset + 1}–
                  {Math.min(
                    results.offset + results.items.length,
                    results.total,
                  )}{" "}
                  / {results.total}
                </span>
                <button
                  className="icon-button"
                  aria-label="上一页文件"
                  disabled={loading || results.offset === 0}
                  onClick={() => setOffset(results.offset - 100)}
                >
                  <ChevronLeft size={17} />
                </button>
                <button
                  className="icon-button"
                  aria-label="下一页文件"
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
                  ? "正在读取结果"
                  : running
                    ? "正在寻找文件"
                    : "没有匹配的文件"
              }
              text="可更改目录或筛选条件。目录未知的文件不会被猜测归入其他目录。"
            />
          )}
        </div>
      </div>
      <p className="recovery-export-note">
        可按已确认的原目录结构{browsing ? "复制" : "恢复"}
        ，或集中保存。目录关系来自本次{browsing ? "读取" : "扫描"}
        ，可能不完整；名称调整和导出结果会记录在清单中。
      </p>
      {properties && (
        <FileProperties
          file={properties}
          directory={
            properties.directory
              ? (results?.paths[properties.directory]?.join(" / ") ??
                "目录未知")
              : "目录未知"
          }
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
              .map((d) => d.name)
              .join(" / ") || "根目录"
          }
          onClose={() => setDirectoryProperties(null)}
        />
      )}
    </section>
  );
}
