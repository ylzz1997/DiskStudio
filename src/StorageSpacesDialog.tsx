import { useEffect, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { Dialog, FormError } from "./components";
import { useLocale } from "./i18n/LanguageProvider";
import { formatSize, type Disk } from "./types";

type ProviderStatus = { health: number | null; operational: number[] };

type Member = ProviderStatus & {
  id: string;
  name: string | null;
  serial: string | null;
  size: number | null;
  usage: number | null;
};

type Space = ProviderStatus & {
  id: string;
  name: string | null;
  uniqueId: string | null;
  size: number | null;
  allocated: number | null;
  footprint: number | null;
  resiliency: string | null;
  columns: number | null;
  copies: number | null;
  availableCopies: number | null;
  redundancy: number | null;
  provisioning: number | null;
  interleave: number | null;
  logicalSector: number | null;
  physicalSector: number | null;
  detachedReason: number | null;
  complete: boolean;
  memberIds: string[];
  disk: {
    number: number;
    target: string | null;
    offline: boolean | null;
    readOnly: boolean | null;
  } | null;
};

type Pool = ProviderStatus & {
  id: string;
  name: string | null;
  uniqueId: string | null;
  size: number | null;
  allocated: number | null;
  readOnly: boolean | null;
  clustered: boolean | null;
  complete: boolean;
  members: Member[];
  spaces: Space[];
};

export function StorageSpacesDialog({
  disks,
  onClose,
  onOpenDisk,
}: {
  disks: Disk[];
  onClose: () => void;
  onOpenDisk: (disk: Disk) => void;
}) {
  const { t, catalogText } = useLocale();
  const [pools, setPools] = useState<Pool[] | null>(null);
  const [choice, setChoice] = useState("");
  const [spaceChoice, setSpaceChoice] = useState("");
  const [error, setError] = useState("");
  const [request, setRequest] = useState(0);
  useEffect(() => {
    let active = true;
    invoke<Pool[]>("read_storage_spaces")
      .then((result) => {
        if (active) setPools(result);
      })
      .catch((reason) => {
        if (active) setError(reason instanceof Error ? reason.message : String(reason));
      });
    return () => { active = false; };
  }, [request]);

  const pool = pools?.find((item) => item.id === choice) ?? pools?.[0];
  const space = pool?.spaces.find((item) => item.id === spaceChoice) ?? pool?.spaces[0];
  const mapped = space?.complete ? space.disk : null;
  const selected = mapped?.target
    ? disks.find((disk) => disk.hardware?.identity === mapped.target && disk.hardware.number === mapped.number)
    : undefined;
  const number = (value: number | null) => value?.toString() ?? t("未知");
  const condition = (status: ProviderStatus) => {
    const health = status.health === 0 ? t("正常")
      : status.health === 1 ? t("警告")
      : status.health === 2 ? t("异常") : t("状态未知");
    const operational = status.operational.filter((code) => code !== 0 && code !== 2).map((code) => {
      if (code === 3) return t("降级");
      if (code === 12 || code === 13) return t("通信中断");
      if (code === 0xd002) return t("未挂接");
      if (code === 0xd003) return t("存储空间不完整");
      return t("操作状态码 {code}", { code: `0x${code.toString(16).toUpperCase()}` });
    });
    return [health, ...operational].join(" · ");
  };

  return (
    <Dialog title={t("存储池与存储空间")} wide onClose={onClose}>
      {!pools && !error && <p role="status">{t("正在读取")}</p>}
      <FormError message={catalogText(error)} />
      {pools?.length === 0 && <p>{t("Windows 未报告已创建的存储池。")}</p>}
      {pool && (
        <>
          <label className="backup-browser-partition">
            {t("存储池")}
            <select value={pool.id} onChange={(event) => {
              setChoice(event.currentTarget.value);
              setSpaceChoice("");
            }}>
              {pools?.map((item) => (
                <option key={item.id} value={item.id}>{item.name ?? item.uniqueId ?? t("未知")}</option>
              ))}
            </select>
          </label>
          <section className="table-backup-result">
            <h3>{pool.name ?? t("存储池")}</h3>
            {pool.uniqueId && <p>{t("磁盘标识")} · {pool.uniqueId}</p>}
            <dl>
              <div><dt>{t("容量")}</dt><dd>{formatSize(pool.size)}</dd></div>
              <div><dt>{t("已分配容量")}</dt><dd>{formatSize(pool.allocated)}</dd></div>
              <div><dt>{t("状态")}</dt><dd>{condition(pool)}</dd></div>
            </dl>
            {(pool.readOnly || pool.clustered) && (
              <p>{[pool.readOnly && t("只读"), pool.clustered && t("群集存储池")].filter(Boolean).join(" · ")}</p>
            )}
            {!pool.complete && <p role="status">{t("Windows 存储池关联不完整或冲突，请重新读取。")}</p>}
          </section>
          {space && (
            <>
              <label className="backup-browser-partition">
                {t("存储空间")}
                <select value={space.id} onChange={(event) => setSpaceChoice(event.currentTarget.value)}>
                  {pool.spaces.map((item) => (
                    <option key={item.id} value={item.id}>{item.name ?? item.uniqueId ?? t("未知")}</option>
                  ))}
                </select>
              </label>
              <section className="table-backup-result">
                <h3>{space.name ?? t("存储空间")}</h3>
                {space.uniqueId && <p>{t("磁盘标识")} · {space.uniqueId}</p>}
                <dl>
                  <div><dt>{t("容量")}</dt><dd>{formatSize(space.size)}</dd></div>
                  <div><dt>{t("已分配容量")}</dt><dd>{formatSize(space.allocated)}</dd></div>
                  <div><dt>{t("池内占用")}</dt><dd>{formatSize(space.footprint)}</dd></div>
                  <div><dt>{t("RAID 布局")}</dt><dd>{space.resiliency ?? t("未知")} · {space.provisioning === 1 ? t("精简置备") : space.provisioning === 2 ? t("固定置备") : t("未知")}</dd></div>
                  <div><dt>{t("列数与条带大小")}</dt><dd>{number(space.columns)} · {formatSize(space.interleave)}</dd></div>
                  <div><dt>{t("数据副本与容错盘数")}</dt><dd>{number(space.copies)} · {number(space.redundancy)}</dd></div>
                  <div><dt>{t("可用数据副本")}</dt><dd>{number(space.availableCopies)}</dd></div>
                  <div><dt>{t("逻辑扇区 / 物理扇区")}</dt><dd>{formatSize(space.logicalSector)} / {formatSize(space.physicalSector)}</dd></div>
                  <div><dt>{t("状态")}</dt><dd>{condition(space)}</dd></div>
                </dl>
                <div className="data-action-row">
                  <span>
                    {mapped ? [
                      `${t("磁盘")} ${mapped.number}`,
                      mapped.offline === true ? t("脱机") : mapped.offline === false ? t("联机") : t("状态未知"),
                      mapped.readOnly ? t("只读") : "",
                    ].filter(Boolean).join(" · ") : t("Windows 未确认关联磁盘。")}
                    {space.detachedReason !== null && space.detachedReason > 1 && (
                      <> · {t("未挂接原因码 {code}", { code: space.detachedReason })}</>
                    )}
                  </span>
                  {selected && (
                    <button className="button" onClick={() => onOpenDisk(selected)}>{t("在磁盘管理中查看")}</button>
                  )}
                </div>
                {!space.complete && <p role="status">{t("Windows 存储池关联不完整或冲突，请重新读取。")}</p>}
              </section>
            </>
          )}
          <section className="table-backup-result">
            <h3>{t("存储池成员")}</h3>
            {pool.members.map((member) => (
              <div className="data-action-row" key={member.id}>
                <span>{member.name ?? t("未知")} · {formatSize(member.size)}{member.serial && <> · {member.serial}</>}</span>
                <span>
                  {condition(member)}
                  {member.usage === 3 && <> · {t("热备")}</>}
                  {member.usage === 4 && <> · {t("已停用")}</>}
                  {space?.memberIds.includes(member.id) && <> · {t("存有此空间的数据")}</>}
                </span>
              </div>
            ))}
          </section>
        </>
      )}
      <div className="dialog-actions">
        <button className="button" onClick={onClose}>{t("关闭对话框")}</button>
        <button className="button" disabled={!pools && !error} onClick={() => {
          setPools(null);
          setError("");
          setRequest((value) => value + 1);
        }}>{t("刷新信息")}</button>
      </div>
    </Dialog>
  );
}
