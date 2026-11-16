import { useEffect, useRef, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { Dialog, FormError } from "./components";
import { formatSize, type Disk } from "./types";
import {
  differingBytes,
  interpretBytes,
  sectorPosition,
  stageSectorBytes,
  buildSectorQuery,
  type SectorConditionInput,
  type SectorQuery,
  type SectorWritePlan,
  type SectorPage,
  type SectorSearch,
  type SectorSelection,
} from "./sectorModel";
import { surfaceRange } from "./surfaceModel";

export function SectorViewer({
  disk,
  initialPartition,
  onClose,
  onBusy,
  onRefresh,
}: {
  disk: Disk;
  initialPartition?: number;
  onClose: () => void;
  onBusy: (busy: boolean) => void;
  onRefresh: () => Promise<unknown>;
}) {
  const [partition, setPartition] = useState(
    initialPartition === undefined ? "" : String(initialPartition),
  );
  const [tab, setTab] = useState<"view" | "search">("view");
  const [position, setPosition] = useState("0"),
    [unit, setUnit] = useState<"sector" | "byte">("sector");
  const [page, setPage] = useState<SectorPage | null>(null),
    [anchor, setAnchor] = useState<SectorPage | null>(null);
  const [byte, setByte] = useState(0),
    [little, setLittle] = useState(true);
  const [busy, setBusyState] = useState(false),
    [error, setError] = useState(""),
    [message, setMessage] = useState("");
  const [encoding, setEncoding] =
      useState<SectorConditionInput["encoding"]>("hex"),
    [pattern, setPattern] = useState(""),
    [start, setStart] = useState("0");
  const [ignoreAsciiCase, setIgnoreAsciiCase] = useState(false);
  const [conditionOffset, setConditionOffset] = useState("0");
  const [extraConditions, setExtraConditions] = useState<
    SectorConditionInput[]
  >([]);
  const [matchMode, setMatchMode] = useState<SectorQuery["mode"]>("all");
  const [direction, setDirection] =
    useState<SectorSearch["direction"]>("forward");
  const [advanced, setAdvanced] = useState(false);
  const selected = disk.partitions.find((p) => p.number === Number(partition));
  const size = partition && selected ? selected.size : disk.size;
  const [count, setCount] = useState(String(size / disk.sectorSize));
  const [search, setSearch] = useState<SectorSearch | null>(null),
    [resultPage, setResultPage] = useState(0);
  const [draft, setDraft] = useState<number[] | null>(null),
    [editHex, setEditHex] = useState("");
  const [plan, setPlan] = useState<SectorWritePlan | null>(null);
  const editing = draft !== null;
  const displayed = draft ?? page?.bytes ?? [];
  const changes = page && draft ? differingBytes(page.bytes, draft) : [];
  const control = useRef({ busy: false, pending: false, id: "" });
  function setBusy(value: boolean) {
    control.current.busy = value;
    setBusyState(value);
    onBusy(value);
  }
  function selection(): SectorSelection {
    if (!disk.hardware?.identity || !disk.hardware.layoutKey)
      throw Error("请刷新磁盘信息后重试。");
    return {
      target: disk.hardware.identity,
      layoutKey: disk.hardware.layoutKey,
      partition: partition ? Number(partition) : null,
    };
  }
  useEffect(() => {
    let alive = true,
      timer: ReturnType<typeof setTimeout>;
    async function poll() {
      if (control.current.id && !control.current.pending) {
        try {
          const next = await invoke<SectorSearch | null>("sector_status");
          if (
            alive &&
            !control.current.pending &&
            next?.id === control.current.id
          ) {
            setSearch(next);
            setBusy(next.result.state === "running");
          }
        } catch (e) {
          if (alive) setError(String(e));
        }
      }
      if (alive) timer = setTimeout(() => void poll(), 400);
    }
    void poll();
    return () => {
      alive = false;
      clearTimeout(timer);
    };
  }, []);
  function choose(value: string) {
    setPartition(value);
    setPage(null);
    setSearch(null);
    control.current.id = "";
    setPosition("0");
    setStart("0");
    setMessage("");
    setError("");
    setCount(
      String(
        (value
          ? disk.partitions.find((p) => p.number === Number(value))!.size
          : disk.size) / disk.sectorSize,
      ),
    );
  }
  function invalidateSearch() {
    setSearch(null);
    setResultPage(0);
    control.current.id = "";
    setError("");
  }
  function updateCondition(
    index: number,
    patch: Partial<SectorConditionInput>,
  ) {
    setExtraConditions((conditions) =>
      conditions.map((c, i) => (i === index ? { ...c, ...patch } : c)),
    );
    invalidateSearch();
  }
  async function task<T>(work: () => Promise<T>) {
    if (control.current.busy) return;
    control.current.pending = true;
    setBusy(true);
    setError("");
    setMessage("");
    try {
      return await work();
    } catch (e) {
      setError(String(e));
    } finally {
      control.current.pending = false;
      setBusy(false);
    }
  }
  async function read(sector?: number, highlight = 0) {
    if (editing) return;
    await task(async () => {
      const index =
        sector ?? sectorPosition(position, unit, size, disk.sectorSize);
      const next = await invoke<SectorPage>("sector_read", {
        request: { source: selection(), sector: index },
      });
      setPage(next);
      setByte(highlight);
      setPosition(String(index));
      setUnit("sector");
      setTab("view");
    });
  }
  async function begin() {
    if (control.current.busy || editing) return;
    control.current.pending = true;
    setBusy(true);
    setError("");
    setMessage("");
    try {
      const options = surfaceRange(start, count, size, disk.sectorSize);
      const id = await invoke<string>("sector_search", {
        request: {
          source: selection(),
          options,
          direction,
          query: buildSectorQuery(
            [
              {
                encoding,
                value: pattern,
                offset: conditionOffset,
                ignoreAsciiCase,
              },
              ...extraConditions,
            ],
            matchMode,
          ),
        },
      });
      control.current.id = id;
      setResultPage(0);
      setSearch(await invoke<SectorSearch | null>("sector_status"));
    } catch (e) {
      setError(String(e));
      setBusy(false);
    } finally {
      control.current.pending = false;
    }
  }
  const differences = new Set(
    page && anchor ? differingBytes(page.bytes, anchor.bytes) : [],
  );
  const running = search?.result.state === "running";
  const hex = (n: number, width = 2) =>
    n.toString(16).toUpperCase().padStart(width, "0");
  function backToEdit() {
    if (plan) void invoke("sector_discard_write", { token: plan.token });
    setPlan(null);
    setError("");
  }
  async function previewWrite() {
    if (!page || !draft) return;
    await task(async () =>
      setPlan(
        await invoke<SectorWritePlan>("sector_prepare_write", {
          request: {
            pageId: page.id,
            edits: changes.map((offset) => ({ offset, value: draft[offset] })),
          },
        }),
      ),
    );
  }
  async function saveWrite() {
    if (!plan) return;
    await task(async () => {
      const token = plan.token;
      setPlan(null);
      try {
        const result = await invoke<{ message: string; sha256: string }>(
          "sector_execute_write",
          { token },
        );
        setMessage(result.message);
      } finally {
        setDraft(null);
        setPage(null);
        await onRefresh();
      }
    });
  }
  if (plan)
    return (
      <Dialog
        wide
        title="确认扇区修改"
        subtitle={plan.target}
        onClose={() => {
          if (!control.current.busy) backToEdit();
        }}
      >
        <p className="surface-message">{plan.warning}</p>
        <p className="surface-note">
          相对 LBA {plan.relativeSector} · 绝对 LBA {plan.absoluteSector} · 写入{" "}
          {plan.sectorSize} B · 改动 {plan.changes.length} 字节
        </p>
        <div className="sector-change-table">
          <table>
            <thead>
              <tr>
                <th>字节偏移</th>
                <th>原值</th>
                <th>新值</th>
              </tr>
            </thead>
            <tbody>
              {plan.changes.map((change) => (
                <tr key={change.offset}>
                  <td>
                    {change.offset} / 0x{hex(change.offset)}
                  </td>
                  <td>{hex(change.before)}</td>
                  <td>{hex(change.after)}</td>
                </tr>
              ))}
            </tbody>
          </table>
        </div>
        <details className="sector-hashes">
          <summary>整个扇区 SHA-256</summary>
          <p>原内容：{plan.beforeSha256}</p>
          <p>新内容：{plan.afterSha256}</p>
        </details>
        <FormError message={error} />
        <div className="sector-file-actions">
          <button className="button" disabled={busy} onClick={backToEdit}>
            返回编辑
          </button>
          <button
            className="button primary"
            disabled={busy}
            onClick={() => void saveWrite()}
          >
            确认写入
          </button>
        </div>
      </Dialog>
    );
  return (
    <Dialog
      wide
      title={editing ? "编辑扇区" : "扇区查看"}
      subtitle={`${disk.name} · ${formatSize(disk.size)}`}
      onClose={() => {
        if (control.current.busy) return;
        if (changes.length) {
          setError("还有未保存的改动，请先预览保存或丢弃改动。");
          return;
        }
        onClose();
      }}
    >
      <div className="sector-top">
        <label>
          查看范围
          <select
            value={partition}
            disabled={busy || editing}
            onChange={(e) => choose(e.target.value)}
          >
            <option value="">整个磁盘</option>
            {disk.partitions
              .filter((p) => p.number !== undefined)
              .map((p) => (
                <option key={p.id} value={p.number}>
                  分区 {p.number} · {p.driveLetter ? `${p.driveLetter}: ` : ""}
                  {p.name} · {formatSize(p.size)}
                </option>
              ))}
          </select>
        </label>
        <div className="sector-tabs">
          <button
            className={tab === "view" ? "active" : ""}
            disabled={editing}
            onClick={() => setTab("view")}
          >
            查看
          </button>
          <button
            className={tab === "search" ? "active" : ""}
            disabled={editing}
            onClick={() => setTab("search")}
          >
            搜索
          </button>
        </div>
      </div>
      {tab === "view" ? (
        <>
          <div className="sector-location">
            <label>
              位置
              <input
                value={position}
                disabled={busy}
                onChange={(e) => setPosition(e.target.value)}
                placeholder="0 或 0x0000"
              />
            </label>
            <label>
              单位
              <select
                value={unit}
                disabled={busy}
                onChange={(e) => {
                  setUnit(e.target.value as typeof unit);
                  setPosition(
                    String(
                      page
                        ? page.sector *
                            (e.target.value === "byte" ? page.sectorSize : 1)
                        : 0,
                    ),
                  );
                }}
              >
                <option value="sector">相对 LBA</option>
                <option value="byte">相对字节</option>
              </select>
            </label>
            <button
              className="button primary"
              disabled={busy || editing}
              onClick={() => void read()}
            >
              读取
            </button>
          </div>
          {page ? (
            <>
              {draft && (
                <div className="sector-edit-bar">
                  <label>
                    从字节 {byte} 开始（十六进制）
                    <input
                      value={editHex}
                      maxLength={12288}
                      disabled={busy}
                      onChange={(e) => setEditHex(e.target.value)}
                    />
                  </label>
                  <button
                    className="button"
                    disabled={busy}
                    onClick={() => {
                      try {
                        setDraft(stageSectorBytes(draft, byte, editHex));
                        setError("");
                      } catch (e) {
                        setError(String(e));
                      }
                    }}
                  >
                    暂存改动
                  </button>
                  <span className="surface-note">
                    {changes.length} 字节未保存
                  </span>
                </div>
              )}
              <div className="sector-page-meta">
                <span>
                  LBA {page.sector} · 绝对 LBA{" "}
                  {page.sourceOffset / page.sectorSize + page.sector}
                </span>
                <span>{page.sectorSize} B</span>
              </div>
              <div
                className="sector-hex"
                aria-label="十六进制与 ASCII 字节"
                role="region"
              >
                {Array.from({ length: page.bytes.length / 16 }, (_, row) => (
                  <div className="sector-row" key={row}>
                    <span className="sector-offset">
                      {hex(page.sector * page.sectorSize + row * 16, 8)}
                    </span>
                    <span className="sector-bytes">
                      {displayed
                        .slice(row * 16, row * 16 + 16)
                        .map((value, i) => {
                          const index = row * 16 + i;
                          return (
                            <button
                              key={i}
                              className={`${index === byte ? "selected" : ""} ${differences.has(index) ? "different" : ""} ${draft && draft[index] !== page.bytes[index] ? "edited" : ""}`}
                              aria-label={`字节 ${index}：${hex(value)}`}
                              title={`偏移 ${index} / 0x${hex(index)}`}
                              onClick={() => {
                                setByte(index);
                                if (draft) setEditHex(hex(draft[index]));
                              }}
                            >
                              {hex(value)}
                            </button>
                          );
                        })}
                    </span>
                    <span className="sector-ascii">
                      {displayed
                        .slice(row * 16, row * 16 + 16)
                        .map((n) =>
                          n >= 32 && n <= 126 ? String.fromCharCode(n) : "·",
                        )
                        .join("")}
                    </span>
                  </div>
                ))}
              </div>
              <div className="sector-navigation">
                <button
                  className="button"
                  disabled={busy || editing || page.sector === 0}
                  onClick={() => void read(0)}
                >
                  首扇区
                </button>
                <button
                  className="button"
                  disabled={busy || editing || page.sector === 0}
                  onClick={() => void read(page.sector - 1)}
                >
                  上一扇区
                </button>
                <button
                  className="button"
                  disabled={
                    busy || editing || page.sector + 1 >= size / disk.sectorSize
                  }
                  onClick={() => void read(page.sector + 1)}
                >
                  下一扇区
                </button>
                <button
                  className="button"
                  disabled={
                    busy || editing || page.sector + 1 >= size / disk.sectorSize
                  }
                  onClick={() => void read(size / disk.sectorSize - 1)}
                >
                  末扇区
                </button>
              </div>
              <details className="sector-interpreter">
                <summary>字节解释 · 偏移 {byte}</summary>
                <label>
                  字节序
                  <select
                    value={little ? "little" : "big"}
                    onChange={(e) => setLittle(e.target.value === "little")}
                  >
                    <option value="little">小端 LE</option>
                    <option value="big">大端 BE</option>
                  </select>
                </label>
                <table>
                  <thead>
                    <tr>
                      <th>位数</th>
                      <th>无符号整数</th>
                      <th>有符号整数</th>
                    </tr>
                  </thead>
                  <tbody>
                    {interpretBytes(displayed, byte, little).map((v) => (
                      <tr key={v.bits}>
                        <td>{v.bits}</td>
                        <td>{v.unsigned}</td>
                        <td>{v.signed}</td>
                      </tr>
                    ))}
                  </tbody>
                </table>
              </details>
              <div className="sector-file-actions">
                {draft ? (
                  <>
                    <button
                      className="button"
                      disabled={busy}
                      onClick={() => {
                        setDraft(null);
                        setError("");
                      }}
                    >
                      丢弃改动
                    </button>
                    <button
                      className="button primary"
                      disabled={busy || !changes.length}
                      onClick={() => void previewWrite()}
                    >
                      预览修改
                    </button>
                  </>
                ) : (
                  <button
                    className="button"
                    disabled={busy || page.source.partition === null}
                    title={
                      page.source.partition === null
                        ? "请先选择普通 GPT 数据分区"
                        : "先在内存编辑，预览后确认写入"
                    }
                    onClick={() => {
                      setDraft([...page.bytes]);
                      setEditHex(hex(page.bytes[byte]));
                      setMessage("");
                    }}
                  >
                    编辑字节
                  </button>
                )}
                <button
                  className="button"
                  disabled={busy || editing}
                  onClick={() => {
                    setAnchor(page);
                    setMessage("");
                  }}
                >
                  作为比较基准
                </button>
                <button
                  className="button"
                  disabled={busy || editing}
                  onClick={() =>
                    void task(async () => {
                      const path = await invoke<string | null>(
                        "sector_export",
                        { id: page.id },
                      );
                      if (path) setMessage(`已导出并校验 SHA-256：${path}`);
                    })
                  }
                >
                  导出当前扇区
                </button>
              </div>
              {anchor && (
                <p className="surface-note sector-comparison">
                  基准：{anchor.label} · LBA {anchor.sector} ·{" "}
                  {anchor.sectorSize} B<br />
                  差异 {differences.size} 字节
                  <button
                    className="link-button"
                    onClick={() => setAnchor(null)}
                  >
                    清除基准
                  </button>
                </p>
              )}
            </>
          ) : (
            <div className="sector-empty">
              输入位置后读取一个扇区。支持十六进制、ASCII 和字节解释。
            </div>
          )}
        </>
      ) : (
        <>
          <div className="sector-search-pattern">
            <label>
              搜索内容
              <input
                value={pattern}
                disabled={busy}
                placeholder={
                  encoding === "hex" ? "如 46 4? ?? 33 32" : "输入要查找的文本"
                }
                onChange={(e) => {
                  setPattern(e.target.value);
                  invalidateSearch();
                }}
              />
            </label>
            <label>
              编码
              <select
                value={encoding}
                disabled={busy}
                onChange={(e) => {
                  setEncoding(e.target.value as typeof encoding);
                  invalidateSearch();
                }}
              >
                <option value="hex">十六进制（支持 ?）</option>
                <option value="utf8">UTF-8 文本</option>
                <option value="utf16Le">UTF-16 LE</option>
              </select>
            </label>
          </div>
          {encoding !== "hex" && (
            <label className="sector-case-option">
              <input
                type="checkbox"
                checked={ignoreAsciiCase}
                disabled={busy}
                onChange={(e) => {
                  setIgnoreAsciiCase(e.target.checked);
                  invalidateSearch();
                }}
              />
              忽略 ASCII 字母大小写
            </label>
          )}
          <div className="sector-presets">
            常用标记：
            {Object.entries({
              GPT: "45 46 49 20 50 41 52 54",
              NTFS: "4E 54 46 53 20 20 20 20",
              FAT32: "46 41 54 33 32",
              "FAT 系列": "46 41 54 ?? ??",
              exFAT: "45 58 46 41 54 20 20 20",
            }).map(([name, value]) => (
              <button
                key={name}
                disabled={busy}
                onClick={() => {
                  setEncoding("hex");
                  setPattern(value);
                  setIgnoreAsciiCase(false);
                  setConditionOffset("0");
                  setExtraConditions([]);
                  setMatchMode("all");
                  setAdvanced(false);
                  invalidateSearch();
                }}
              >
                {name}
              </button>
            ))}
            <button
              disabled={busy}
              onClick={() => {
                setEncoding("hex");
                setPattern("46 41 54 33 32");
                setIgnoreAsciiCase(false);
                setConditionOffset("82");
                setExtraConditions([
                  {
                    encoding: "hex",
                    value: "55 AA",
                    offset: "510",
                    ignoreAsciiCase: false,
                  },
                ]);
                setMatchMode("all");
                setAdvanced(true);
                invalidateSearch();
              }}
            >
              FAT32 引导特征
            </button>
          </div>
          <details
            className="sector-conditions"
            open={advanced}
            onToggle={(e) => setAdvanced(e.currentTarget.open)}
          >
            <summary>
              多条件与相对偏移 · {extraConditions.length + 1} 项
            </summary>
            <div className="sector-condition-options">
              <label>
                组合方式
                <select
                  value={matchMode}
                  disabled={busy}
                  onChange={(e) => {
                    setMatchMode(e.target.value as typeof matchMode);
                    invalidateSearch();
                  }}
                >
                  <option value="all">同时满足全部</option>
                  <option value="any">满足任意一项</option>
                </select>
              </label>
              <label>
                首项相对偏移（字节）
                <input
                  value={conditionOffset}
                  disabled={busy}
                  onChange={(e) => {
                    setConditionOffset(e.target.value);
                    invalidateSearch();
                  }}
                />
              </label>
            </div>
            {extraConditions.map((condition, index) => (
              <div className="sector-condition" key={index}>
                <div className="sector-condition-fields">
                  <label>
                    条件 {index + 2}
                    <input
                      value={condition.value}
                      disabled={busy}
                      onChange={(e) =>
                        updateCondition(index, { value: e.target.value })
                      }
                    />
                  </label>
                  <label>
                    编码
                    <select
                      value={condition.encoding}
                      disabled={busy}
                      onChange={(e) =>
                        updateCondition(index, {
                          encoding: e.target.value as typeof encoding,
                        })
                      }
                    >
                      <option value="hex">Hex（支持 ?）</option>
                      <option value="utf8">UTF-8</option>
                      <option value="utf16Le">UTF-16 LE</option>
                    </select>
                  </label>
                  <label>
                    相对偏移（字节）
                    <input
                      value={condition.offset}
                      disabled={busy}
                      onChange={(e) =>
                        updateCondition(index, { offset: e.target.value })
                      }
                    />
                  </label>
                  <button
                    className="button"
                    disabled={busy}
                    aria-label={`移除条件 ${index + 2}`}
                    onClick={() => {
                      setExtraConditions((rows) =>
                        rows.filter((_, i) => i !== index),
                      );
                      invalidateSearch();
                    }}
                  >
                    移除
                  </button>
                </div>
                {condition.encoding !== "hex" && (
                  <label className="sector-case-option">
                    <input
                      type="checkbox"
                      disabled={busy}
                      checked={condition.ignoreAsciiCase}
                      onChange={(e) =>
                        updateCondition(index, {
                          ignoreAsciiCase: e.target.checked,
                        })
                      }
                    />
                    忽略 ASCII 字母大小写
                  </label>
                )}
              </div>
            ))}
            <button
              className="button"
              disabled={busy || extraConditions.length >= 15}
              onClick={() => {
                setExtraConditions((rows) => [
                  ...rows,
                  {
                    encoding: "hex",
                    value: "",
                    offset: "0",
                    ignoreAsciiCase: false,
                  },
                ]);
                invalidateSearch();
              }}
            >
              添加条件
            </button>
            <p className="surface-note">
              偏移相对于同一个匹配基准位置；结果定位到该基准。特征匹配不等于文件系统有效。
            </p>
          </details>
          <div className="sector-search-range">
            <label>
              搜索方向
              <select
                value={direction}
                disabled={busy}
                onChange={(e) => {
                  setDirection(e.target.value as SectorSearch["direction"]);
                  invalidateSearch();
                }}
              >
                <option value="forward">从头向后</option>
                <option value="backward">从尾向前</option>
              </select>
            </label>
            <label>
              起始相对 LBA
              <input
                value={start}
                disabled={busy}
                onChange={(e) => {
                  setStart(e.target.value);
                  invalidateSearch();
                }}
              />
            </label>
            <label>
              扇区数量
              <input
                value={count}
                disabled={busy}
                onChange={(e) => {
                  setCount(e.target.value);
                  invalidateSearch();
                }}
              />
            </label>
            {running ? (
              <button
                className="button"
                onClick={() =>
                  void invoke("data_cancel", { id: search.id }).catch((e) =>
                    setError(String(e)),
                  )
                }
              >
                停止搜索
              </button>
            ) : (
              <button
                className="button primary"
                disabled={
                  busy || !pattern || extraConditions.some((c) => !c.value)
                }
                onClick={() => void begin()}
              >
                开始搜索
              </button>
            )}
          </div>
          {search && (
            <>
              <progress
                className="surface-progress"
                max={search.result.totalBytes}
                value={search.result.checkedBytes}
                aria-label="搜索进度"
              />
              <p className="surface-note" role="status">
                {search.query.conditions.length} 项条件 ·{" "}
                {search.query.mode === "all" ? "全部" : "任意"} ·{" "}
                {search.direction === "backward" ? "从尾向前" : "从头向后"} ·{" "}
                {formatSize(search.result.checkedBytes)} /{" "}
                {formatSize(search.result.totalBytes)} ·{" "}
                {search.result.hits.length} 个匹配
                {running ? " · 正在搜索…" : ""}
              </p>
              {search.result.message && (
                <p className="surface-message">{search.result.message}</p>
              )}
              <div className="sector-results">
                {search.result.hits
                  .slice(resultPage * 50, resultPage * 50 + 50)
                  .map((hit) => {
                    const offset =
                      search.options.startSector * search.sectorSize + hit;
                    return (
                      <button
                        key={hit}
                        disabled={busy}
                        onClick={() =>
                          void read(
                            Math.floor(offset / search.sectorSize),
                            offset % search.sectorSize,
                          )
                        }
                      >
                        <span>0x{hex(offset, 8)}</span>
                        <span>
                          LBA {Math.floor(offset / search.sectorSize)} +{" "}
                          {offset % search.sectorSize}
                        </span>
                      </button>
                    );
                  })}
              </div>
              {search.result.hits.length > 50 && (
                <div className="sector-navigation">
                  <button
                    className="button"
                    disabled={resultPage === 0}
                    onClick={() => setResultPage((p) => p - 1)}
                  >
                    上一页
                  </button>
                  <span>
                    {resultPage + 1} /{" "}
                    {Math.ceil(search.result.hits.length / 50)}
                  </span>
                  <button
                    className="button"
                    disabled={
                      (resultPage + 1) * 50 >= search.result.hits.length
                    }
                    onClick={() => setResultPage((p) => p + 1)}
                  >
                    下一页
                  </button>
                </div>
              )}
            </>
          )}
        </>
      )}
      <p className="surface-note">
        {tab === "view"
          ? "位置相对于所选范围；在线卷的内容可能变化。"
          : encoding === "hex"
            ? "? 匹配一个十六进制位，?? 匹配一个字节；最多保留 1024 项。"
            : "文本按所选编码匹配，最多保留 1024 项；遇到读取错误会停止。"}
      </p>
      <FormError message={error} />
      {message && (
        <p className="surface-note" role="status">
          {message}
        </p>
      )}
    </Dialog>
  );
}
