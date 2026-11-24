import { useState } from "react";
import { Dialog, FormError } from "./components";
import { useLocale } from "./i18n/LanguageProvider";
import type { VirtualMachineOptions } from "./types";

export function SystemVirtualizationDialog({
  source, sector, busy, error, onClose, onPrepare,
}: {
  source: string;
  sector: number;
  busy: boolean;
  error: string;
  onClose: () => void;
  onPrepare: (options: VirtualMachineOptions) => void;
}) {
  const { t, catalogText } = useLocale();
  const [platform, setPlatform] = useState<VirtualMachineOptions["platform"]>("vmware");
  const [name, setName] = useState("DiskStudio Windows");
  const [cpus, setCpus] = useState("2");
  const [memory, setMemory] = useState("4096");
  const settings = { platform, name, cpus: Number(cpus), memoryMib: Number(memory) };
  const validName = /^[a-zA-Z0-9 _-]{1,48}$/.test(name) && name.trim() === name;
  const validCpus = /^\d+$/.test(cpus) && settings.cpus >= 1 && settings.cpus <= 32;
  const validMemory = /^\d+$/.test(memory) && settings.memoryMib >= 1024 &&
    settings.memoryMib <= 65536 && settings.memoryMib % 4 === 0;
  const issue = sector !== 512
    ? t("VMware 和 VirtualBox 系统导出需要 512 B 逻辑扇区。")
    : !validName
      ? t("虚拟机名称须为 1–48 个英文字母、数字、空格、连字符或下划线，首尾不可有空格。")
      : !validCpus || !validMemory
        ? t("虚拟机需要 1–32 个处理器及 1024–65536 MiB 内存，内存须按 4 MiB 对齐。")
        : "";
  return (
    <Dialog
      title={t("导出 Windows 虚拟机")}
      subtitle={source}
      onClose={() => { if (!busy) onClose(); }}
      footer={
        <>
          <button className="button" disabled={busy} onClick={onClose}>{t("取消")}</button>
          <button className="button primary" disabled={busy || !!issue} onClick={() => onPrepare(settings)}>
            {t("选择保存位置…")}
          </button>
        </>
      }
    >
      <div className="backup-settings backup-restore-settings">
        <label>
          <strong>{t("虚拟化软件")}</strong>
          <select value={platform} disabled={busy}
            onChange={(event) => setPlatform(event.target.value as VirtualMachineOptions["platform"])}>
            <option value="vmware">VMware</option>
            <option value="virtualBox">VirtualBox</option>
          </select>
        </label>
        <label>
          <strong>{t("虚拟机名称")}</strong>
          <input value={name} disabled={busy} maxLength={48} onChange={(event) => setName(event.target.value)} />
        </label>
        <label>
          <strong>{t("处理器数量")}</strong>
          <input type="number" min={1} max={32} step={1} disabled={busy} value={cpus} onChange={(event) => setCpus(event.target.value)} />
        </label>
        <label>
          <strong>{t("内存（MiB）")}</strong>
          <input type="number" min={1024} max={65536} step={4} disabled={busy} value={memory} onChange={(event) => setMemory(event.target.value)} />
        </label>
      </div>
      <p className="data-footnote">
        {t("先检查 Windows 启动项和驱动，再将虚拟磁盘及配置保存到另一块物理磁盘。")}
      </p>
      {(issue || error) && <FormError message={issue || catalogText(error)} />}
    </Dialog>
  );
}
