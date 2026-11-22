import { useState } from "react";
import { useLocale } from "./i18n/LanguageProvider";

type Row = { number: number; cells: (string | null)[] };
type Sheet = { name: string; rows: Row[] };
type Slide = { number: number; paragraphs: string[] };
export type OfficePreview = { limited: boolean } & (
  | { kind: "document"; paragraphs: string[] }
  | { kind: "workbook"; sheets: Sheet[] }
  | { kind: "presentation"; slides: Slide[] }
);

function Paragraphs({ paragraphs }: { paragraphs: string[] }) {
  const { catalogText } = useLocale();
  if (!paragraphs.some((paragraph) => paragraph.trim())) {
    return (
      <p className="data-footnote">{catalogText("此处没有可预览的文本。")}</p>
    );
  }
  return (
    <div className="office-preview-paragraphs">
      {paragraphs.map((paragraph, i) => (
        <p key={i}>{paragraph || "\u00a0"}</p>
      ))}
    </div>
  );
}

function Worksheet({ sheet }: { sheet: Sheet }) {
  const { catalogText } = useLocale();
  if (!sheet.rows.length) {
    return (
      <p className="data-footnote">{catalogText("此处没有可预览的单元格。")}</p>
    );
  }
  const columns = sheet.rows[0].cells.map((_, i) =>
    String.fromCharCode(65 + i),
  );
  return (
    <div className="office-preview-sheet" tabIndex={0}>
      <table aria-label={sheet.name}>
        <thead>
          <tr>
            <th scope="col">{catalogText("行号")}</th>
            {columns.map((column) => (
              <th scope="col" key={column}>
                {column}
              </th>
            ))}
          </tr>
        </thead>
        <tbody>
          {sheet.rows.map((row) => (
            <tr key={row.number}>
              <th scope="row">{row.number}</th>
              {row.cells.map((value, i) => (
                <td key={i} title={value ?? undefined}>
                  {value}
                </td>
              ))}
            </tr>
          ))}
        </tbody>
      </table>
    </div>
  );
}

export function OfficeFilePreview({ preview }: { preview: OfficePreview }) {
  const { catalogText } = useLocale();
  const [selected, setSelected] = useState(0);
  const sheet =
    preview.kind === "workbook" ? preview.sheets[selected] : undefined;
  const slide =
    preview.kind === "presentation" ? preview.slides[selected] : undefined;
  return (
    <section className="office-preview">
      {preview.kind === "document" && (
        <Paragraphs paragraphs={preview.paragraphs} />
      )}
      {preview.kind === "workbook" &&
        (sheet ? (
          <>
            <label className="office-preview-picker">
              {catalogText("工作表")}
              <select
                value={selected}
                onChange={(event) => setSelected(Number(event.target.value))}
              >
                {preview.sheets.map(({ name }, i) => (
                  <option key={i} value={i}>
                    {name}
                  </option>
                ))}
              </select>
            </label>
            <Worksheet sheet={sheet} />
          </>
        ) : (
          <p className="data-footnote">
            {catalogText("此处没有可预览的单元格。")}
          </p>
        ))}
      {preview.kind === "presentation" &&
        (slide ? (
          <>
            <label className="office-preview-picker">
              {catalogText("幻灯片")}
              <select
                value={selected}
                onChange={(event) => setSelected(Number(event.target.value))}
              >
                {preview.slides.map(({ number }, i) => (
                  <option key={number} value={i}>
                    {number}
                  </option>
                ))}
              </select>
            </label>
            <Paragraphs paragraphs={slide.paragraphs} />
          </>
        ) : (
          <p className="data-footnote">
            {catalogText("此处没有可预览的文本。")}
          </p>
        ))}
      <p className="data-footnote">
        {preview.kind === "workbook"
          ? catalogText("单元格显示保存的值，公式仅显示已缓存的结果。")
          : catalogText("文字预览省略排版和嵌入内容。")}
      </p>
      {preview.limited && (
        <p className="data-footnote">{catalogText("预览只显示部分内容。")}</p>
      )}
    </section>
  );
}
