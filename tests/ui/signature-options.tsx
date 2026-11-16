// Shipped component with memory-only state. No Tauri IPC or device/file access.
import { useState } from "react";
import { createRoot } from "react-dom/client";
import { SignatureOptions } from "../../src/SignatureOptions";
import {
  defaultSignatureSettings,
  signatureRequest,
} from "../../src/signatureModel";
import "../../src/styles.css";
function Harness() {
  const [value, setValue] = useState(defaultSignatureSettings);
  const [busy, setBusy] = useState(false);
  const [result, setResult] = useState("尚未验证");
  return (
    <main
      className="data-page"
      style={{ padding: 32, maxWidth: 1100, margin: "auto" }}
    >
      <h1>按文件签名扫描</h1>
      <p>内存 UI 验证 · 未连接任何磁盘</p>
      <SignatureOptions value={value} onChange={setValue} disabled={busy} />
      <div className="dialog-actions">
        <button className="button" onClick={() => setBusy(!busy)}>
          切换忙碌状态
        </button>
        <button
          className="button"
          onClick={() => {
            document.documentElement.dataset.theme =
              document.documentElement.dataset.theme === "dark"
                ? "light"
                : "dark";
          }}
        >
          切换主题
        </button>
        <button
          className="button primary"
          disabled={busy}
          onClick={() => {
            try {
              setResult(
                JSON.stringify(signatureRequest(value, 16 * 1024 ** 3)),
              );
            } catch (e) {
              setResult(String(e));
            }
          }}
        >
          验证请求
        </button>
      </div>
      <p role="status" style={{ overflowWrap: "anywhere" }}>
        {result}
      </p>
    </main>
  );
}
createRoot(document.getElementById("root")!).render(<Harness />);
