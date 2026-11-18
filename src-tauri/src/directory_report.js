"use strict";
(() => {
  const data = JSON.parse(document.getElementById("report-data").textContent);
  const el = (id) => document.getElementById(id);
  const language = document.documentElement.lang;
  const number = new Intl.NumberFormat(language);
  function t(message, values) {
    const template = Object.hasOwn(data.messages, message) ? data.messages[message] : message;
    return values ? template.replace(/\{([a-zA-Z][\w]*)\}/g, (part, key) =>
      Object.hasOwn(values, key) ? String(values[key]) : part) : template;
  }
  const directoryName = (directory) => directory.id === "root" ? t("根目录") : directory.name;
  const directoryLabel = (directory) => directoryName(directory) + (directory.deleted ? " · " + t("已删除") : "");
  function candidateMethod(method) {
    const filesystem = /^(FAT(?:12|16|32)?|exFAT|Ext) 目录$/.exec(method)?.[1];
    return filesystem ? t("{filesystem} 目录", { filesystem }) : t(method);
  }
  function candidateNote(note) {
    const at = note.lastIndexOf("；保存时的历史观察");
    if (at === -1 || language === "zh-CN") return t(note);
    return `${t(note.slice(0, at))} · ${t(note.slice(at + 1))}`;
  }
  function directoryNote(note) {
    if (language === "zh-CN") return note;
    const aliases = "同一文件有多个名称；选中后分别保存副本";
    return note.replace(aliases, t(aliases)).split("；").map(part => t(part)).join(" · ");
  }
  function stamp(value) {
    return value
      ? `${value.text} · ${t("{zone} · 精度 {precision}", { zone: t(value.zone), precision: t(value.precision) })}`
      : t("未记录或无法解析");
  }
  const dirs = new Map(data.directories.map((d) => [d.id, d]));
  const children = new Map();
  for (const d of data.directories) {
    if (d.parent !== null) {
      if (!children.has(d.parent)) children.set(d.parent, []);
      children.get(d.parent).push(d);
    }
  }
  for (const list of children.values()) list.sort((a,b) => a.name.localeCompare(b.name, language) || a.id.localeCompare(b.id));
  const paths = new Map();
  function path(id) {
    if (paths.has(id)) return paths.get(id);
    const chain = [];
    let d = dirs.get(id);
    while (d && chain.length < 64) { chain.unshift(d); d = dirs.get(d.parent); }
    paths.set(id, chain);
    return chain;
  }
  const counts = new Map();
  for (const f of data.files) {
    if (f.directory === "unknown") counts.set("unknown", (counts.get("unknown") || 0) + 1);
    else for (const d of path(f.directory)) counts.set(d.id, (counts.get(d.id) || 0) + 1);
  }
  let folder = null, offset = 0, directoryOffset = 0;
  const pageSize = 100;
  function text(tag, value, className) {
    const node = document.createElement(tag);
    node.textContent = value;
    if (className) node.className = className;
    return node;
  }
  function button(label, id) {
    const b = text("button", label);
    b.type = "button";
    b.classList.toggle("active", folder === id);
    b.addEventListener("click", () => navigate(id));
    return b;
  }
  function navigate(id) { folder = id; offset = 0; directoryOffset = 0; render(); }
  function renderFolders() {
    el("all").classList.toggle("active", folder === null);
    el("unknown").classList.toggle("active", folder === "unknown");
    const chain = folder === "unknown" ? [] : path(folder || "root");
    el("breadcrumbs").replaceChildren(...chain.map(d => button(directoryLabel(d), d.id)));
    const list = children.get(folder || "root") || [];
    directoryOffset = Math.min(directoryOffset, Math.floor(Math.max(0, list.length - 1) / pageSize) * pageSize);
    el("directories").replaceChildren(...list.slice(directoryOffset, directoryOffset + pageSize).map(d => {
      const b = button(directoryLabel(d), d.id);
      b.append(text("small", t("文件数 {count}（含子目录）", { count: number.format(counts.get(d.id) || 0) })));
      return b;
    }));
    el("dir-page").textContent = `${number.format(list.length ? directoryOffset + 1 : 0)}–${number.format(Math.min(directoryOffset + pageSize, list.length))} / ${number.format(list.length)}`;
    el("dir-prev").disabled = directoryOffset === 0;
    el("dir-next").disabled = directoryOffset + pageSize >= list.length;
    el("location").textContent = folder === null ? t("全部文件") : folder === "unknown" ? t("目录未知") : chain.map(directoryName).join(" / ");
  }
  function renderFiles() {
    let query;
    try {
      query = directoryQuery.prepare(el("search").value, {
        nameMatch: el("name-match").value, caseSensitive: el("case-sensitive").checked,
        sortBy: el("sort-by").value, sortDirection: el("sort-direction").value,
        dateField: el("date-field").value, dateFrom: el("date-from").value || null, dateTo: el("date-to").value || null,
      });
      el("query-error").hidden = true;
    } catch (error) {
      el("query-error").textContent = t(error.message);
      el("query-error").hidden = false;
      el("files").replaceChildren(); el("count").textContent = t("筛选无效");
      el("prev").disabled = el("next").disabled = true;
      el("empty").hidden = true;
      return;
    }
    const state = el("state").value;
    const descendants = el("descendants").checked;
    const matched = query.ordered(data.files.filter(f =>
      query.matches(f) && (!state || f.state === state) &&
      (folder === null || f.directory === folder || (descendants && folder !== "unknown" && path(f.directory).some(d => d.id === folder)))));
    offset = Math.min(offset, Math.floor(Math.max(0, matched.length - 1) / pageSize) * pageSize);
    el("files").replaceChildren(...matched.slice(offset, offset + pageSize).map(f => {
      const row = document.createElement("tr");
      const name = text("td", f.name);
      name.append(text("small", `${t("编号 {number}", { number: f.id })} · ${candidateMethod(f.method)}`));
      const directory = document.createElement("td");
      directory.append(button(f.directory === "unknown" ? t("目录未知") : path(f.directory).map(directoryName).join(" / "), f.directory));
      const status = text("td", t(f.state));
      if (!f.supported) status.append(text("small", t("当前引擎不支持导出内容")));
      if (f.note) status.append(text("small", candidateNote(f.note)));
      if (f.directoryNote) status.append(text("small", directoryNote(f.directoryNote)));
      const metadata = document.createElement("td");
      metadata.append(text("span", `${t("修改时间")} · ${stamp(f.metadata?.modified)}`));
      const detail = document.createElement("details");
      detail.append(text("summary", t("全部属性")));
      for (const [key, label] of [["created", "创建时间"], ["accessed", "访问时间"], ["changed", "元数据变更"]])
        detail.append(text("small", `${t(label)} · ${stamp(f.metadata?.[key])}`));
      const attributes = f.metadata?.source
        ? f.metadata.attributes.map(attribute => t(attribute)).join(" · ") || t("无已识别标志")
        : t("未知");
      detail.append(text("small", `${t("属性")} · ${attributes}`));
      if (f.metadata?.rawFlags) detail.append(text("small", t("原始标志 {flags}", { flags: f.metadata.rawFlags })));
      if (f.metadata?.permissions) detail.append(text("small", `${t("POSIX 权限")} · ${f.metadata.permissions}`));
      for (const [key, label] of [["uid", "UID"], ["gid", "GID"], ["links", "链接计数"]])
        if (f.metadata?.[key] != null) detail.append(text("small", `${t(label)} · ${f.metadata[key]}`));
      detail.append(text("small", `${t("信息来源")} · ${t(f.metadata?.source || "未提供")}`));
      metadata.append(detail);
      row.append(name, directory, text("td", number.format(BigInt(f.bytes))), metadata, status);
      return row;
    }));
    el("empty").hidden = matched.length > 0;
    el("count").textContent = `${number.format(matched.length ? offset + 1 : 0)}–${number.format(Math.min(offset + pageSize, matched.length))} / ${t("文件数 {count}", { count: number.format(matched.length) })}`;
    el("prev").disabled = offset === 0;
    el("next").disabled = offset + pageSize >= matched.length;
  }
  function render() { renderFolders(); renderFiles(); }
  el("summary").textContent = [
    t(data.mode),
    t("文件数 {count}", { count: number.format(data.files.length) }),
    t("已知目录数 {count}", { count: number.format(Math.max(0, data.directories.length - 1)) }),
  ].join(" · ");
  el("completion").textContent = t(data.completion);
  el("all").textContent = `${t("全部文件")} (${number.format(data.files.length)})`;
  el("unknown").textContent = `${t("目录未知")} (${number.format(counts.get("unknown") || 0)})`;
  el("all").addEventListener("click", () => navigate(null));
  el("unknown").addEventListener("click", () => navigate("unknown"));
  let timer;
  el("search").addEventListener("input", () => { clearTimeout(timer); timer = setTimeout(() => { offset = 0; renderFiles(); }, 150); });
  el("state").addEventListener("change", () => { offset = 0; renderFiles(); });
  el("descendants").addEventListener("change", () => { offset = 0; renderFiles(); });
  for (const id of ["name-match", "case-sensitive", "sort-by", "sort-direction", "date-field", "date-from", "date-to"])
    el(id).addEventListener("change", () => { offset = 0; renderFiles(); });
  el("reset").addEventListener("click", () => {
    clearTimeout(timer);
    for (const [id, value] of Object.entries({search:"",state:"","name-match":"contains","sort-by":"scan","sort-direction":"asc","date-field":"modified","date-from":"","date-to":""})) el(id).value = value;
    el("case-sensitive").checked = false; el("descendants").checked = true;
    offset = 0; renderFiles();
  });
  el("prev").addEventListener("click", () => { offset -= pageSize; renderFiles(); });
  el("next").addEventListener("click", () => { offset += pageSize; renderFiles(); });
  el("dir-prev").addEventListener("click", () => { directoryOffset -= pageSize; renderFolders(); });
  el("dir-next").addEventListener("click", () => { directoryOffset += pageSize; renderFolders(); });
  render();
})();
