// The offline report uses the same literal query and date semantics as Rust.
// All input is retained display metadata. No filesystem, IPC or network access.
const directoryQuery = (() => {
  const compare = (a, b) => a < b ? -1 : a > b ? 1 : 0;
  function textOrder(a, b) {
    const aa = Array.from(a), bb = Array.from(b);
    for (let i = 0; i < Math.min(aa.length, bb.length); i++) {
      const order = compare(aa[i].codePointAt(0), bb[i].codePointAt(0));
      if (order) return order;
    }
    return compare(aa.length, bb.length);
  }
  function date(value) {
    if (typeof value !== "string" || value.length !== 10 || !/^\d{4}-\d{2}-\d{2}$/.test(value)) return null;
    const [y, m, d] = value.split("-").map(Number);
    const days = [31, y % 4 === 0 && (y % 100 !== 0 || y % 400 === 0) ? 29 : 28, 31,30,31,30,31,31,30,31,30,31];
    return y > 0 && m >= 1 && m <= 12 && d >= 1 && d <= days[m-1] ? value : null;
  }
  function recordedDay(stamp) {
    const s = stamp?.text;
    return typeof s === "string" && (s.length === 10 || s[10] === " ") ? date(s.slice(0,10)) : null;
  }
  function instant(stamp) {
    const day = recordedDay(stamp);
    const clock = stamp?.text?.slice(11).match(/^(\d{2}):(\d{2}):(\d{2})(?:\.(\d{1,9}))?$/);
    const zone = stamp?.zone?.match(/^UTC(?:([+-])(\d{2}):(\d{2}))?$/);
    if (!day || !clock || !zone || clock[0] !== stamp.text.slice(11) || zone[0] !== stamp.zone) return null;
    const [h,m,s] = clock.slice(1,4).map(Number);
    const zh = Number(zone[2] || 0), zm = Number(zone[3] || 0);
    if (h > 23 || m > 59 || s > 59 || zh > 23 || zm > 59) return null;
    const [y,month,d] = day.split("-").map(Number);
    const utc = new Date(0);
    utc.setUTCFullYear(y,month-1,d);
    utc.setUTCHours(h,m,s,0);
    const offset = (zh * 3600 + zm * 60) * (zone[1] === "-" ? -1 : 1);
    return BigInt(utc.getTime()/1000 - offset) * 1000000000n + BigInt((clock[4] || "").padEnd(9,"0"));
  }
  function extension(name) {
    const at = name.lastIndexOf(".");
    return at > 0 ? name.slice(at+1) : "";
  }
  function prepare(query, filter = {}) {
    const options = { nameMatch:"contains", caseSensitive:false, sortBy:"scan", sortDirection:"asc", dateField:"modified", dateFrom:null, dateTo:null, ...filter };
    if (new TextEncoder().encode(query).length > 200 || /[\u0000-\u001f\u007f-\u009f]/.test(query)) throw Error("搜索内容不得超过 200 字节或包含控制字符。");
    for (const [key, allowed] of Object.entries({nameMatch:["contains","exact","prefix","extension"], sortBy:["scan","name","size","extension","modified"], sortDirection:["asc","desc"], dateField:["created","modified","accessed","changed"]}))
      if (!allowed.includes(options[key])) throw Error("未知的筛选选项。");
    for (const key of ["dateFrom","dateTo"]) if (options[key] !== null && !date(options[key])) throw Error("日期必须是有效的 YYYY-MM-DD。");
    if (options.dateFrom && options.dateTo && options.dateFrom > options.dateTo) throw Error("开始日期不能晚于结束日期。");
    const needle = options.caseSensitive ? query : query.toLowerCase();
    function matches(f) {
      const name = options.caseSensitive ? f.name : f.name.toLowerCase();
      if (needle) {
        const ext = needle.startsWith(".") ? needle.slice(1) : needle;
        const ok = options.nameMatch === "exact" ? name === needle : options.nameMatch === "prefix" ? name.startsWith(needle) : options.nameMatch === "extension" ? !!ext && extension(name) === ext : name.includes(needle);
        if (!ok) return false;
      }
      if (options.dateFrom || options.dateTo) {
        const day = recordedDay(f.metadata?.[options.dateField]);
        if (!day || options.dateFrom && day < options.dateFrom || options.dateTo && day > options.dateTo) return false;
      }
      return true;
    }
    function ordered(files) {
      const rows = files.map(f => {
        const key = options.sortBy === "name" ? f.name.toLowerCase() : options.sortBy === "extension" ? extension(f.name).toLowerCase() : options.sortBy === "size" ? BigInt(f.bytes ?? f.size) : options.sortBy === "modified" ? instant(f.metadata?.modified) : f.id;
        return {f,key};
      });
      rows.sort((a,b) => {
        if (options.sortBy === "modified") {
          if (a.key === null && b.key !== null) return 1;
          if (a.key !== null && b.key === null) return -1;
        }
        const order = typeof a.key === "string" ? textOrder(a.key,b.key) : compare(a.key,b.key);
        return (options.sortDirection === "desc" ? -order : order) || compare(a.f.id,b.f.id);
      });
      return rows.map(row => row.f);
    }
    return {matches, ordered};
  }
  return {prepare, date};
})();
