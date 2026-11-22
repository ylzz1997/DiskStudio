import { useRef, useState, type FormEvent } from "react";
import { ChevronLeft, ChevronRight, LoaderCircle } from "lucide-react";
import { useLocale } from "./i18n/LanguageProvider";
import { errorText } from "./partitionOutcome";

export type RenderedPage = { index: number; count: number; pngBase64: string };
export type RasterPreview = RenderedPage & { id: string };

export function PagedFilePreview({
  name,
  page,
  onPage,
}: {
  name: string;
  page: RenderedPage;
  onPage: (index: number) => Promise<void>;
}) {
  const { t, catalogText } = useLocale();
  const requested = useRef(false);
  const [loading, setLoading] = useState(false);
  const [fitWidth, setFitWidth] = useState(false);
  const [error, setError] = useState<string | null>(null);

  async function go(index: number) {
    if (
      requested.current ||
      !Number.isInteger(index) ||
      index < 0 ||
      index >= page.count ||
      index === page.index
    )
      return;
    requested.current = true;
    setLoading(true);
    setError(null);
    try {
      await onPage(index);
    } catch (error) {
      setError(errorText(error));
    } finally {
      requested.current = false;
      setLoading(false);
    }
  }

  function jump(event: FormEvent<HTMLFormElement>) {
    event.preventDefault();
    const value = new FormData(event.currentTarget).get("page");
    if (typeof value === "string" && value.trim()) void go(Number(value) - 1);
  }

  return (
    <section className="paged-file-preview" aria-busy={loading}>
      <div className="paged-preview-toolbar">
        {page.count > 1 && (
          <>
            <button
              type="button"
              className="icon-button"
              aria-label={t("上一页")}
              disabled={loading || page.index === 0}
              onClick={() => void go(page.index - 1)}
            >
              <ChevronLeft size={17} />
            </button>
            <output aria-live="polite">
              {page.index + 1} / {page.count}
            </output>
            <button
              type="button"
              className="icon-button"
              aria-label={t("下一页")}
              disabled={loading || page.index + 1 === page.count}
              onClick={() => void go(page.index + 1)}
            >
              <ChevronRight size={17} />
            </button>
            <form className="paged-preview-jump" onSubmit={jump}>
              <input
                type="number"
                name="page"
                min={1}
                max={page.count}
                key={page.index}
                defaultValue={page.index + 1}
                aria-label={catalogText("预览页码")}
                disabled={loading}
                required
              />
              <button type="submit" className="secondary" disabled={loading}>
                {catalogText("跳转")}
              </button>
            </form>
          </>
        )}
        <button
          type="button"
          className="secondary paged-preview-fit"
          onClick={() => setFitWidth(!fitWidth)}
        >
          {catalogText(fitWidth ? "适合页面" : "适合宽度")}
        </button>
        {loading && (
          <LoaderCircle
            className="spin"
            size={17}
            role="status"
            aria-label={catalogText("正在渲染页面")}
          />
        )}
      </div>
      {error && (
        <p className="data-footnote" role="alert">
          {catalogText(error)}
        </p>
      )}
      <div
        className={`paged-preview-viewport${fitWidth ? " fit-width" : ""}`}
        tabIndex={0}
      >
        <img
          src={`data:image/png;base64,${page.pngBase64}`}
          alt={name}
          onError={() => setError("无法显示页面图像。")}
        />
      </div>
    </section>
  );
}
