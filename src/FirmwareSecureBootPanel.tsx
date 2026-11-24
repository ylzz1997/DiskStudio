import { useState } from "react";
import { FormError } from "./components";
import { useLocale } from "./i18n/LanguageProvider";

type FirmwareStatus = "ready" | "missing" | "invalid" | "unavailable";
type DatabaseName = "PK" | "KEK" | "db" | "dbx" | "dbt";
type Observation = {
  name: string;
  status: FirmwareStatus;
  errorCode: number | null;
};
type Signature = {
  signatureType: string;
  owner: string;
  kind: string;
  bytes: number;
  sha256: string | null;
  digest: string | null;
  revocation: { kind: "always" } | { kind: "at"; time: string } | null;
  certificate: {
    subject: string | null;
    issuer: string | null;
    notBefore: string | null;
    notAfter: string | null;
    serial: string | null;
  } | null;
  invalid: boolean;
};

export type SecureBootReport = {
  flags: (Observation & { enabled: boolean | null })[];
  databases: (Observation & {
    name: DatabaseName;
    sha256: string | null;
    entries: Signature[];
  })[];
};

const PAGE_SIZE = 25;
const statusLabels = {
  missing: "缺失",
  invalid: "无效",
  unavailable: "读取失败",
} as const;
const databaseLabels = {
  PK: "平台密钥",
  KEK: "密钥交换密钥",
  db: "允许签名数据库",
  dbx: "禁止签名数据库",
  dbt: "时间戳签名数据库",
} as const;

export function FirmwareSecureBootPanel({
  report,
  busy,
  error,
  onRead,
  onBack,
}: {
  report: SecureBootReport | null;
  busy: boolean;
  error: string;
  onRead: () => void;
  onBack: () => void;
}) {
  const { t, catalogText, language } = useLocale();
  const formatNumber = new Intl.NumberFormat(language).format;
  const [name, setName] = useState<DatabaseName>("db");
  const [query, setQuery] = useState("");
  const [page, setPage] = useState(0);
  const database = report?.databases.find((item) => item.name === name);
  const search = query.trim().toLowerCase();
  const entries = (database?.entries ?? []).filter((entry) =>
    [
      entry.kind,
      entry.signatureType,
      entry.owner,
      entry.digest,
      entry.sha256,
      entry.certificate?.subject,
      entry.certificate?.issuer,
      entry.certificate?.serial,
    ].some((value) => value?.toLowerCase().includes(search)),
  );
  const pages = Math.max(1, Math.ceil(entries.length / PAGE_SIZE));
  const currentPage = Math.min(page, pages - 1);
  const start = currentPage * PAGE_SIZE;

  function status(item: Observation) {
    if (item.status === "ready") return null;
    const label = t(statusLabels[item.status]);
    return item.errorCode === null
      ? label
      : `${label} · Win32 ${item.errorCode}`;
  }

  return (
    <div className="secure-boot-panel" aria-busy={busy}>
      <div className="partition-action-buttons">
        <button className="button" onClick={onBack} disabled={busy}>
          {t("返回")}
        </button>
        <button
          className="button"
          disabled={busy}
          onClick={() => {
            setPage(0);
            onRead();
          }}
        >
          {t("重新读取")}
        </button>
      </div>
      <FormError message={catalogText(error)} />
      {busy && <p role="status">{t("正在读取安全启动证书…")}</p>}
      {report && (
        <>
          <dl className="secure-boot-flags">
            {report.flags.map((flag) => (
              <div key={flag.name}>
                <dt>{flag.name}</dt>
                <dd>{status(flag) ?? t(flag.enabled ? "已启用" : "已禁用")}</dd>
              </div>
            ))}
          </dl>
          <div className="secure-boot-fields">
            <label>
              {t("安全启动数据库")}
              <select
                value={name}
                onChange={(event) => {
                  setName(event.target.value as DatabaseName);
                  setPage(0);
                }}
              >
                {report.databases.map((item) => (
                  <option key={item.name} value={item.name}>
                    {item.name} · {t(databaseLabels[item.name])}
                  </option>
                ))}
              </select>
            </label>
            {database?.status === "ready" && database.entries.length > 0 && (
              <label>
                {t("搜索证书或摘要")}
                <input
                  type="search"
                  maxLength={256}
                  value={query}
                  onChange={(event) => {
                    setQuery(event.target.value);
                    setPage(0);
                  }}
                />
              </label>
            )}
          </div>
          {database && (
            <>
              <p role="status">
                {status(database) ??
                  t("{shown} / {total} 条记录", {
                    shown: formatNumber(entries.length),
                    total: formatNumber(database.entries.length),
                  })}
              </p>
              {database.sha256 && (
                <p>
                  {t("数据库 SHA-256")} · <code>{database.sha256}</code>
                </p>
              )}
              {entries.slice(start, start + PAGE_SIZE).map((entry, index) => (
                <details key={`${name}/${start + index}`}>
                  <summary>
                    {entry.kind || t("未知")}
                    {entry.invalid && ` · ${t("无效")}`}
                    {" · "}
                    <bdi>
                      {entry.certificate?.subject ??
                        entry.digest ??
                        entry.sha256 ??
                        entry.owner}
                    </bdi>
                  </summary>
                  <dl className="secure-boot-entry">
                    {entry.certificate && (
                      <>
                        <dt>{t("证书使用者")}</dt>
                        <dd>
                          <bdi>{entry.certificate.subject ?? t("未知")}</bdi>
                        </dd>
                        <dt>{t("证书颁发者")}</dt>
                        <dd>
                          <bdi>{entry.certificate.issuer ?? t("未知")}</bdi>
                        </dd>
                        <dt>{t("证书有效期")}</dt>
                        <dd>
                          <bdi>{entry.certificate.notBefore ?? t("未知")}</bdi>
                          {" → "}
                          <bdi>{entry.certificate.notAfter ?? t("未知")}</bdi>
                        </dd>
                        <dt>{t("证书序列号")}</dt>
                        <dd>
                          <code>{entry.certificate.serial ?? t("未知")}</code>
                        </dd>
                      </>
                    )}
                    <dt>{t("签名类型 GUID")}</dt>
                    <dd>
                      <code>{entry.signatureType}</code>
                    </dd>
                    <dt>{t("签名所有者 GUID")}</dt>
                    <dd>
                      <code>{entry.owner}</code>
                    </dd>
                    {entry.digest && (
                      <>
                        <dt>{entry.kind}</dt>
                        <dd>
                          <code>{entry.digest}</code>
                        </dd>
                      </>
                    )}
                    {entry.sha256 && (
                      <>
                        <dt>{t("条目 SHA-256")}</dt>
                        <dd>
                          <code>{entry.sha256}</code>
                        </dd>
                      </>
                    )}
                    {entry.revocation && (
                      <>
                        <dt>{t("吊销生效时间")}</dt>
                        <dd>
                          {entry.revocation.kind === "always" ? (
                            t("始终吊销")
                          ) : (
                            <bdi>{entry.revocation.time}</bdi>
                          )}
                        </dd>
                      </>
                    )}
                    <dt>{t("大小")}</dt>
                    <dd>{formatNumber(entry.bytes)} B</dd>
                  </dl>
                </details>
              ))}
              {pages > 1 && (
                <div className="partition-action-buttons">
                  <button
                    className="button"
                    disabled={currentPage === 0}
                    onClick={() => setPage(currentPage - 1)}
                  >
                    {t("上一页")}
                  </button>
                  <span>
                    {formatNumber(currentPage + 1)} / {formatNumber(pages)}
                  </span>
                  <button
                    className="button"
                    disabled={currentPage >= pages - 1}
                    onClick={() => setPage(currentPage + 1)}
                  >
                    {t("下一页")}
                  </button>
                </div>
              )}
            </>
          )}
        </>
      )}
    </div>
  );
}
