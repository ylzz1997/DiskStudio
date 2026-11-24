import { useLocale } from "./i18n/LanguageProvider";

type Match = {
  status: "ready" | "missing" | "invalid" | "unavailable";
  matches: number;
  invalidEntries: number;
};

type CertificateTrust = {
  chainStatus: "matched" | "noMatch" | "incomplete" | "unavailable";
  anchorSha256: string | null;
  checkedCertificates: string[];
  revocationStatus:
    | "revoked"
    | "timestampRequired"
    | "timestampBeforeRevocation"
    | "noMatch"
    | "incomplete"
    | "unavailable";
  revocationTimes: string[];
};

type TimestampEvidence = {
  status: "verified" | "invalid" | "untrusted" | "unavailable" | "unsupported";
  signedAt: string | null;
  subject: string | null;
  anchorSha256: string | null;
};

export type LoaderSecurityReport = {
  partition: string;
  fileSha256: string;
  digests: {
    algorithm: string;
    digest: string;
    db: Match | null;
    dbx: Match | null;
  }[];
  signatures: {
    status: "verified" | "digestMismatch" | "invalid" | "unsupported";
    algorithm: string | null;
    signedDigest: string | null;
    subject: string | null;
    certificateSha256: string | null;
    certificateTrust: CertificateTrust | null;
    timestamp: TimestampEvidence | null;
  }[];
  secureBoot: boolean | null;
  firmwareError: string | null;
};

const signatureStatus = {
  verified: "签名与映像摘要均匹配",
  digestMismatch: "映像摘要与签名不符",
  invalid: "无法验证签名",
  unsupported: "不支持",
} as const;
const databaseStatus = {
  missing: "缺失",
  invalid: "无效",
  unavailable: "读取失败",
} as const;
const chainStatus = {
  matched: "找到 db 信任锚",
  noMatch: "未找到通往 db 的证书链",
  incomplete: "证书链核对不完整",
  unavailable: "未知",
} as const;
const revocationStatus = {
  revoked: "命中吊销记录",
  timestampRequired: "需要核对可信签名时间戳",
  timestampBeforeRevocation: "可信时间戳早于吊销生效时间",
  noMatch: "已核对证书未命中吊销记录",
  incomplete: "吊销记录核对不完整",
  unavailable: "未知",
} as const;
const timestampStatus = {
  verified: "时间戳通过 dbt 核对",
  invalid: "时间戳验证失败",
  untrusted: "时间戳签名者未获固件信任",
  unavailable: "时间戳信任状态未知",
  unsupported: "不支持",
} as const;

function TimestampDetails({ timestamp }: { timestamp: TimestampEvidence }) {
  const { t } = useLocale();
  return (
    <>
      <dt>{t("签名时间戳")}</dt>
      <dd>{t(timestampStatus[timestamp.status])}</dd>
      {timestamp.signedAt && (
        <>
          <dt>{t("时间戳记录时间")}</dt>
          <dd>
            <bdi>{timestamp.signedAt}</bdi>
          </dd>
        </>
      )}
      {timestamp.subject && (
        <>
          <dt>{t("时间戳签名者")}</dt>
          <dd>
            <bdi>{timestamp.subject}</bdi>
          </dd>
        </>
      )}
      {timestamp.anchorSha256 && (
        <>
          <dt>{t("dbt 信任锚 SHA-256")}</dt>
          <dd>
            <code>{timestamp.anchorSha256}</code>
          </dd>
        </>
      )}
    </>
  );
}

function CertificateTrustDetails({ trust }: { trust: CertificateTrust }) {
  const { t } = useLocale();
  return (
    <>
      <dt>{t("db 证书链")}</dt>
      <dd>{t(chainStatus[trust.chainStatus])}</dd>
      {trust.anchorSha256 && (
        <>
          <dt>{t("db 信任锚 SHA-256")}</dt>
          <dd>
            <code>{trust.anchorSha256}</code>
          </dd>
        </>
      )}
      <dt>{t("dbx 证书吊销")}</dt>
      <dd
        className={
          trust.revocationStatus === "revoked" ? "loader-dbx-match" : undefined
        }
      >
        {t(revocationStatus[trust.revocationStatus])}
      </dd>
      {trust.revocationTimes.length > 0 && (
        <>
          <dt>{t("吊销生效时间")}</dt>
          <dd>
            {trust.revocationTimes.map((time) => (
              <div key={time}>
                <bdi>{time}</bdi>
              </div>
            ))}
          </dd>
        </>
      )}
      {trust.checkedCertificates.length > 1 && (
        <>
          <dt>{t("已核对的证书")}</dt>
          <dd>
            <details>
              <summary>SHA-256 · {trust.checkedCertificates.length}</summary>
              {trust.checkedCertificates.map((sha256) => (
                <div key={sha256}>
                  <code>{sha256}</code>
                </div>
              ))}
            </details>
          </dd>
        </>
      )}
    </>
  );
}

export function FirmwareLoaderSecurity({
  report,
}: {
  report: LoaderSecurityReport;
}) {
  const { t, catalogText } = useLocale();
  function matchStatus(match: Match | null) {
    if (match === null) return t("未知");
    if (match.status !== "ready") return t(databaseStatus[match.status]);
    if (match.matches > 0) {
      return t("匹配 {count} 条摘要", { count: match.matches });
    }
    return t(match.invalidEntries > 0 ? "摘要记录不完整" : "无匹配摘要");
  }

  return (
    <section className="loader-security-result">
      <p role="status">
        SecureBoot ·{" "}
        {t(
          report.secureBoot === null
            ? "未知"
            : report.secureBoot
              ? "已启用"
              : "已禁用",
        )}
      </p>
      {report.firmwareError && <p>{catalogText(report.firmwareError)}</p>}
      <dl className="secure-boot-entry">
        <dt>GPT</dt>
        <dd>
          <code>{report.partition}</code>
        </dd>
        <dt>{t("文件 SHA-256")}</dt>
        <dd>
          <code>{report.fileSha256}</code>
        </dd>
      </dl>
      <details open>
        <summary>{t("PE 映像摘要")}</summary>
        {report.digests.map((digest) => (
          <dl className="secure-boot-entry" key={digest.algorithm}>
            <dt>{digest.algorithm}</dt>
            <dd>
              <code>{digest.digest}</code>
            </dd>
            <dt>db</dt>
            <dd>{matchStatus(digest.db)}</dd>
            <dt>dbx</dt>
            <dd
              className={digest.dbx?.matches ? "loader-dbx-match" : undefined}
            >
              {matchStatus(digest.dbx)}
            </dd>
          </dl>
        ))}
      </details>
      <details open>
        <summary>{t("嵌入式签名")}</summary>
        {report.signatures.length === 0 && <p>{t("未找到嵌入式签名。")}</p>}
        {report.signatures.map((signature, index) => (
          <dl className="secure-boot-entry" key={index}>
            <dt>{t("状态")}</dt>
            <dd>{t(signatureStatus[signature.status])}</dd>
            {signature.subject && (
              <>
                <dt>{t("证书使用者")}</dt>
                <dd>
                  <bdi>{signature.subject}</bdi>
                </dd>
              </>
            )}
            {signature.certificateSha256 && (
              <>
                <dt>{t("证书 SHA-256")}</dt>
                <dd>
                  <code>{signature.certificateSha256}</code>
                </dd>
              </>
            )}
            {signature.signedDigest && (
              <>
                <dt>{signature.algorithm}</dt>
                <dd>
                  <code>{signature.signedDigest}</code>
                </dd>
              </>
            )}
            {signature.certificateTrust && (
              <CertificateTrustDetails trust={signature.certificateTrust} />
            )}
            {signature.timestamp && (
              <TimestampDetails timestamp={signature.timestamp} />
            )}
          </dl>
        ))}
      </details>
    </section>
  );
}
