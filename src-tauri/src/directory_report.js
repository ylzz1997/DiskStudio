"use strict";
(() => {
  const data = JSON.parse(document.getElementById("report-data").textContent);
  const el = (id) => document.getElementById(id);
  const dirs = new Map(data.directories.map((d) => [d.id, d]));
  const children = new Map();
  for (const d of data.directories) {
    if (d.parent !== null) {
      if (!children.has(d.parent)) children.set(d.parent, []);
      children.get(d.parent).push(d);
    }
  }
  for (const list of children.values()) list.sort((a,b) => a.name.localeCompare(b.name, "zh-CN") || a.id.localeCompare(b.id));
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
    el("breadcrumbs").replaceChildren(...chain.map(d => button(d.name + (d.deleted ? " · 已删除" : ""), d.id)));
    const list = children.get(folder || "root") || [];
    directoryOffset = Math.min(directoryOffset, Math.floor(Math.max(0, list.length - 1) / pageSize) * pageSize);
    el("directories").replaceChildren(...list.slice(directoryOffset, directoryOffset + pageSize).map(d => {
      const b = button(d.name + (d.deleted ? " · 已删除" : ""), d.id);
      b.append(text("small", `${(counts.get(d.id) || 0).toLocaleString()} 个文件（含子目录）`));
      return b;
    }));
    el("dir-page").textContent = `${list.length ? directoryOffset + 1 : 0}–${Math.min(directoryOffset + pageSize, list.length)} / ${list.length}`;
    el("dir-prev").disabled = directoryOffset === 0;
    el("dir-next").disabled = directoryOffset + pageSize >= list.length;
    el("location").textContent = folder === null ? "所有已扫描文件" : folder === "unknown" ? "目录未知" : chain.map(d => d.name).join(" / ");
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
      el("query-error").textContent = error.message;
      el("query-error").hidden = false;
      el("files").replaceChildren(); el("count").textContent = "筛选无效";
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
      name.append(text("small", `编号 ${f.id} · ${f.method}`));
      const directory = document.createElement("td");
      directory.append(button(f.directory === "unknown" ? "目录未知" : path(f.directory).map(d => d.name).join(" / "), f.directory));
      const status = text("td", f.state);
      if (!f.supported) status.append(text("small", "当前引擎不支持导出内容"));
      if (f.note) status.append(text("small", f.note));
      if (f.directoryNote) status.append(text("small", f.directoryNote));
      const metadata = document.createElement("td");
      const stamp = value => value ? `${value.text} · ${value.zone} · 精度 ${value.precision}` : "未记录或无法解析";
      metadata.append(text("span", `修改：${stamp(f.metadata?.modified)}`));
      const detail = document.createElement("details");
      detail.append(text("summary", "全部属性"));
      for (const [key, label] of [["created", "创建"], ["accessed", "访问"], ["changed", "元数据变更"]])
        detail.append(text("small", `${label}：${stamp(f.metadata?.[key])}`));
      detail.append(text("small", `属性：${f.metadata?.source ? f.metadata.attributes.join("、") || "无已识别标志" : "未知"}`));
      if (f.metadata?.rawFlags) detail.append(text("small", `原始标志：${f.metadata.rawFlags}`));
      if (f.metadata?.permissions) detail.append(text("small", `POSIX 权限：${f.metadata.permissions}`));
      for (const [key, label] of [["uid", "UID"], ["gid", "GID"], ["links", "链接数"]])
        if (f.metadata?.[key] != null) detail.append(text("small", `${label}：${f.metadata[key]}`));
      detail.append(text("small", `信息来源：${f.metadata?.source || "未提供"}`));
      metadata.append(detail);
      row.append(name, directory, text("td", BigInt(f.bytes).toLocaleString()), metadata, status);
      return row;
    }));
    el("empty").hidden = matched.length > 0;
    el("count").textContent = `${matched.length ? offset + 1 : 0}–${Math.min(offset + pageSize, matched.length)} / ${matched.length.toLocaleString()} 个文件`;
    el("prev").disabled = offset === 0;
    el("next").disabled = offset + pageSize >= matched.length;
  }
  function render() { renderFolders(); renderFiles(); }
  el("summary").textContent = `${data.mode} · ${data.files.length.toLocaleString()} 个文件 · ${Math.max(0, data.directories.length - 1).toLocaleString()} 个已知目录`;
  el("completion").textContent = data.completion;
  el("all").textContent = `所有文件 (${data.files.length.toLocaleString()})`;
  el("unknown").textContent = `目录未知 (${(counts.get("unknown") || 0).toLocaleString()})`;
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
