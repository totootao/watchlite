"use strict";

/* watchlite mobile UI — polls /api/stats like the desktop app, touch-first layout */

const $ = (id) => document.getElementById(id);
const SPARK_LEN = 180;

let intervalMs = 2000;
let timer = null;
let lastData = null;
let procSortKey = "cpu";
let procQuery = "";

const hist = { cpu: [], mem: [], rx: [], tx: [] };
let histSeeded = false;

// 告警指标名 / 进程状态 中文化映射（仅用于展示，不影响后端字段）
const METRIC_CN = { cpu: "CPU", mem: "内存", swap: "交换区", disk: "磁盘", temp: "温度" };
const STATE_CN = { running: "运行中", sleeping: "休眠", idle: "空闲", zombie: "僵尸", stopped: "已停止", paused: "已暂停", "disk sleep": "磁盘休眠" };

/* ---------- formatting ---------- */

function fmtBytes(b, perSec) {
  if (b == null) return "-";
  const units = ["B", "KiB", "MiB", "GiB", "TiB"];
  let i = 0;
  let v = b;
  while (v >= 1024 && i < units.length - 1) { v /= 1024; i++; }
  return (v >= 100 ? v.toFixed(0) : v >= 10 ? v.toFixed(1) : v.toFixed(v < 1 && v > 0 ? 2 : 1))
    + " " + units[i] + (perSec ? "/s" : "");
}

function fmtUptime(s) {
  const d = Math.floor(s / 86400), h = Math.floor(s % 86400 / 3600), m = Math.floor(s % 3600 / 60);
  return (d ? d + "天 " : "") + h + "时 " + m + "分";
}

function pctClass(p) { return p >= 90 ? "crit" : p >= 70 ? "warn" : ""; }

function esc(s) {
  return String(s).replace(/[&<>"]/g, (c) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;" }[c]));
}

/* ---------- charts (same style as desktop: gridlines + gradient area) ---------- */

function drawChart(canvas, series, max) {
  const ctx = canvas.getContext("2d");
  const dpr = window.devicePixelRatio || 1;
  const w = canvas.clientWidth, h = canvas.clientHeight;
  if (w === 0 || h === 0) return;
  if (canvas.width !== Math.round(w * dpr)) { canvas.width = Math.round(w * dpr); canvas.height = Math.round(h * dpr); }
  ctx.setTransform(dpr, 0, 0, dpr, 0, 0);
  ctx.clearRect(0, 0, w, h);

  ctx.strokeStyle = getComputedStyle(document.documentElement).getPropertyValue("--grid").trim() || "#181e27";
  ctx.lineWidth = 1;
  for (const f of [0.25, 0.5, 0.75]) {
    const y = Math.round(h * f) + 0.5;
    ctx.beginPath(); ctx.moveTo(0, y); ctx.lineTo(w, y); ctx.stroke();
  }

  const peak = max || Math.max(1, ...series.map((s) => Math.max(...s.data)));
  const P = 3;
  const xy = (buf, i) => [
    (i / (Math.max(buf.length, 2) - 1)) * w,
    h - P - (Math.min(buf[i], peak) / peak) * (h - 2 * P)
  ];

  for (const s of series) {
    if (s.data.length < 2) continue;
    if (s.fill) {
      const grad = ctx.createLinearGradient(0, 0, 0, h);
      grad.addColorStop(0, s.color + "4d");
      grad.addColorStop(1, s.color + "05");
      ctx.beginPath();
      ctx.moveTo(0, h);
      for (let i = 0; i < s.data.length; i++) { const [x, y] = xy(s.data, i); ctx.lineTo(x, y); }
      ctx.lineTo(xy(s.data, s.data.length - 1)[0], h);
      ctx.closePath();
      ctx.fillStyle = grad;
      ctx.fill();
    }
    ctx.beginPath();
    for (let i = 0; i < s.data.length; i++) {
      const [x, y] = xy(s.data, i);
      i === 0 ? ctx.moveTo(x, y) : ctx.lineTo(x, y);
    }
    ctx.strokeStyle = s.color;
    ctx.lineWidth = s.width || 1.6;
    ctx.stroke();
  }
}

/* ---------- rendering ---------- */

function render(d) {
  $("host-name").textContent = d.host.hostname;

  // alert bar
  $("alert-bar").hidden = !d.alerts.length;
  if (d.alerts.length) {
    $("alert-bar").textContent =
      "▲ " + d.alerts.map((a) => { const u = a.unit || "%"; return `${METRIC_CN[a.metric] || a.metric} ${a.value}${u} > ${a.threshold}${u}`; }).join(" · ");
  }

  /* ----- overview ----- */

  $("host-meta").innerHTML =
    `<span>系统</span><b>${esc(d.host.os)} · ${esc(d.host.kernel)}</b>` +
    `<span>CPU</span><b>${esc(d.host.cpu_model)}</b>` +
    `<span>核心</span><b>${d.host.cpu_count}${d.host.cpu_freq_mhz ? " · " + (d.host.cpu_freq_mhz / 1000).toFixed(1) + " GHz" : ""} · ${esc(d.host.arch)}</b>` +
    `<span>运行时长</span><b>${fmtUptime(d.host.uptime_secs)}</b>`;

  const cpu = d.cpu.total_pct;
  setVal($("cpu-val"), cpu.toFixed(1) + "%", cpu);
  setBar($("cpu-bar"), cpu);
  $("cpu-cores").innerHTML = d.cpu.per_core_pct.map((p, i) =>
    `<div class="core"><i style="width:${Math.min(100, p)}%;background:${p >= 90 ? "color-mix(in srgb,var(--red) 45%,transparent)" : p >= 70 ? "color-mix(in srgb,var(--yellow) 45%,transparent)" : ""}"></i><b>${p.toFixed(0)}</b></div>`
  ).join("");
  $("cpu-sub").textContent = "负载 " + d.cpu.load_avg.map((l) => l.toFixed(2)).join(" ");

  const memPct = d.memory.total ? (d.memory.used / d.memory.total) * 100 : 0;
  setVal($("mem-val"), memPct.toFixed(0) + "%", memPct);
  setBar($("mem-bar"), memPct);
  $("mem-used").textContent = fmtBytes(d.memory.used);
  $("mem-free").textContent = fmtBytes(d.memory.total - d.memory.used);
  $("swap-row").hidden = !d.memory.swap_total;
  if (d.memory.swap_total) {
    const sw = (d.memory.swap_used / d.memory.swap_total) * 100;
    setBar($("swap-bar"), sw);
    $("swap-bar").classList.add("dim");
    $("swap-val").textContent = `${fmtBytes(d.memory.swap_used)} / ${fmtBytes(d.memory.swap_total)} · ${sw.toFixed(0)}%`;
  }

  const rx = d.net.reduce((a, n) => a + n.rx_bps, 0);
  const tx = d.net.reduce((a, n) => a + n.tx_bps, 0);
  $("net-rx").textContent = fmtBytes(rx, 1);
  $("net-tx").textContent = fmtBytes(tx, 1);
  const ifaces = d.net.filter((n) => n.rx_total + n.tx_total > 0)
    .sort((a, b) => (b.rx_bps + b.tx_bps) - (a.rx_bps + a.tx_bps)).slice(0, 3);
  $("net-ifaces").innerHTML = ifaces.map((n) =>
    `<div class="iface-row"><b>${esc(n.iface)}</b><span class="spacer"></span>` +
    `<span>↓ ${fmtBytes(n.rx_bps, 1)}</span><span>↑ ${fmtBytes(n.tx_bps, 1)}</span>` +
    `<span class="iface-total">${fmtBytes(n.rx_total + n.tx_total)}</span></div>`
  ).join("");

  $("disks").innerHTML = d.disks.map((dk) => {
    const pct = dk.total ? (dk.used / dk.total) * 100 : 0;
    return `<div class="stack-row"><div class="label">` +
      `<b>${esc(dk.mount)}</b><span class="fs">${esc(dk.fs)}</span><span class="spacer"></span>` +
      `<span class="val">${fmtBytes(dk.used)} / ${fmtBytes(dk.total)} · ${pct.toFixed(0)}%</span></div>` +
      `<div class="bar"><div class="bar-fill ${pctClass(pct)}" style="width:${Math.min(100, pct)}%"></div></div></div>`;
  }).join("");

  // disk i/o
  $("card-io").hidden = !d.disk_io;
  if (d.disk_io) {
    const r = d.disk_io.reduce((a, io) => a + io.read_bps, 0);
    const w = d.disk_io.reduce((a, io) => a + io.write_bps, 0);
    $("io-read").textContent = fmtBytes(r, 1);
    $("io-write").textContent = fmtBytes(w, 1);
  }

  // sensors
  $("card-sensors").hidden = !d.sensors;
  if (d.sensors) {
    $("temps").innerHTML = d.sensors.temps.map((t) => {
      const crit = t.critical_c || 100;
      const pct = Math.min(100, (t.temp_c / crit) * 100);
      return `<div class="sensor-row"><span class="s-label">${esc(t.label)}</span>` +
        `<div class="bar"><div class="bar-fill ${pctClass(pct)}" style="width:${pct}%"></div></div>` +
        `<span class="s-val">${t.temp_c.toFixed(1)}°${t.critical_c ? " / " + t.critical_c.toFixed(0) + "°C" : ""}</span></div>`;
    }).join("");
    $("fans").textContent = d.sensors.fans.length
      ? d.sensors.fans.map((f) => `${f.label} ${f.rpm} rpm`).join(" · ")
      : "";
  }

  // connections
  $("card-conns").hidden = !d.connections;
  if (d.connections) {
    $("conn-est").textContent = d.connections.established;
    $("conn-tw").textContent = d.connections.time_wait;
    $("listen-ports").innerHTML = d.connections.listening.length
      ? d.connections.listening.map((p) =>
          `<a class="port port-link" href="http://${location.hostname}:${esc(p)}" target="_blank" rel="noopener">${esc(p)}</a>`
        ).join("")
      : "";
  }

  /* ----- charts ----- */

  push(hist.cpu, cpu);
  push(hist.mem, memPct);
  push(hist.rx, rx);
  push(hist.tx, tx);

  setVal($("chart-cpu-val"), cpu.toFixed(1) + "%", cpu);
  setVal($("chart-mem-val"), memPct.toFixed(0) + "%", memPct);
  const netMax = Math.max(2e5, ...hist.rx, ...hist.tx) * 1.08;
  $("chart-net-val").textContent = `↓ ${fmtBytes(rx, 1)} ↑ ${fmtBytes(tx, 1)}`;
  drawChart($("cpu-chart"), [{ data: hist.cpu, color: cssVar("--accent"), fill: true }], 100);
  drawChart($("mem-chart"), [{ data: hist.mem, color: cssVar("--green"), fill: true }], 100);
  drawChart($("net-chart"), [
    { data: hist.rx, color: cssVar("--accent"), fill: true, alpha: "38" },
    { data: hist.tx, color: cssVar("--yellow"), fill: false, width: 1.2 }
  ], netMax);

  /* ----- processes ----- */

  const q = procQuery.trim().toLowerCase();
  const procs = d.processes.list
    .filter((p) => !q || p.name.toLowerCase().includes(q) || String(p.pid).includes(q))
    .sort((a, b) => {
      const va = procSortKey === "cpu" ? a.cpu_pct : procSortKey === "mem" ? a.mem_bytes : a.pid;
      const vb = procSortKey === "cpu" ? b.cpu_pct : procSortKey === "mem" ? b.mem_bytes : b.pid;
      return procSortKey === "pid" ? va - vb : vb - va;
    })
    .slice(0, 100);
  $("proc-count").textContent = `共 ${d.processes.total} 个 · 显示 ${procs.length} 个`;
  $("proc-list").innerHTML = procs.length ? procs.map((p) =>
    `<div class="proc-row">` +
    `<span class="proc-name">${esc(p.name)}</span>` +
    `<span class="proc-val ${pctClass(p.cpu_pct)}">${p.cpu_pct.toFixed(1)}<small>%</small></span>` +
    `<span class="proc-sub"><span class="${p.state === "running" ? "st-run" : p.state === "zombie" ? "st-zombie" : ""}">${STATE_CN[p.state] || esc(p.state)}</span><span>PID ${p.pid}</span></span>` +
    `<span class="proc-val">${fmtBytes(p.mem_bytes)}</span></div>`
  ).join("") : `<div class="empty">无匹配进程</div>`;

  /* ----- docker ----- */

  const hasDocker = !!d.docker;
  $("tab-docker").hidden = !hasDocker && !d.docker_hint;
  if (hasDocker) {
    const running = d.docker.containers.filter((c) => c.state === "running").length;
    $("docker-count").textContent = `共 ${d.docker.containers.length} 个 · ${running} 运行中`;
    $("docker-na").hidden = true;
    $("docker-list").innerHTML = d.docker.containers.length ? d.docker.containers.map((c) =>
      `<div class="proc-row">` +
      `<span class="proc-name">${esc(c.name)}</span>` +
      `<span class="proc-val ${c.state === "running" ? "" : "st-sleep"}">${STATE_CN[c.state] || esc(c.state)}</span>` +
      `<span class="proc-sub">${esc(c.image)}</span>` +
      `<span class="proc-val">${c.state === "running" ? c.cpu_pct.toFixed(1) + "<small>%</small>" : "-"}<br><small>${c.state === "running" ? fmtBytes(c.mem_bytes) : ""}</small></span></div>`
    ).join("") : `<div class="empty">无容器</div>`;
  } else {
    $("docker-count").textContent = "";
    $("docker-na").hidden = !d.docker_hint;
    $("docker-na").textContent = d.docker_hint || "";
    $("docker-list").innerHTML = "";
  }

  flashUpdated();
}

function setBar(el, pct) {
  el.style.width = Math.min(100, pct) + "%";
  el.className = "bar-fill " + pctClass(pct);
}

function setVal(el, text, pct) {
  el.textContent = text;
  el.className = "card-val " + pctClass(pct);
}

function cssVar(name) {
  return getComputedStyle(document.documentElement).getPropertyValue(name).trim() || "#58a6ff";
}

function push(buf, v) {
  buf.push(v);
  if (buf.length > SPARK_LEN) buf.shift();
}

/* ---------- refresh loop ---------- */

let updatedTimer = null;
function flashUpdated() {
  const el = $("updated");
  const now = new Date();
  el.textContent = "已更新 " + now.toLocaleTimeString();
  el.classList.add("show");
  clearTimeout(updatedTimer);
  updatedTimer = setTimeout(() => el.classList.remove("show"), 1200);
}

async function refresh() {
  try {
    const res = await fetch("../api/stats");
    if (!res.ok) throw new Error(res.status);
    const d = await res.json();
    $("conn").className = "conn-dot ok";
    if (d.warming_up) return;
    const ms = Math.max(500, d.interval_secs * 1000);
    if (ms !== intervalMs) {
      intervalMs = ms;
      clearInterval(timer);
      timer = setInterval(refresh, intervalMs);
    }
    lastData = d;
    render(d);
  } catch {
    $("conn").className = "conn-dot lost";
  }
}

/* seed charts from server-side history so they survive page reloads */
async function seedHistory() {
  if (histSeeded) return;
  histSeeded = true;
  try {
    const res = await fetch("../api/history");
    const h = await res.json();
    const pts = h.points || [];
    const stride = Math.max(1, Math.ceil(pts.length / SPARK_LEN));
    for (let i = 0; i < pts.length; i += stride) {
      push(hist.cpu, pts[i].cpu);
      push(hist.mem, pts[i].mem);
      push(hist.rx, pts[i].rx);
      push(hist.tx, pts[i].tx);
    }
  } catch { /* no history yet — charts fill in live */ }
}

/* ---------- interactions ---------- */

// tab bar
document.querySelectorAll(".tab").forEach((btn) => {
  btn.addEventListener("click", () => {
    document.querySelectorAll(".tab").forEach((b) => b.classList.toggle("on", b === btn));
    document.querySelectorAll(".view").forEach((v) => { v.hidden = v.id !== "view-" + btn.dataset.view; });
    window.scrollTo(0, 0);
    $("views").scrollTop = 0;
    if (lastData) render(lastData); // charts may have been 0-sized while hidden
  });
});

// process filter + sort
$("proc-search").addEventListener("input", (e) => {
  procQuery = e.target.value;
  if (lastData) render(lastData);
});
$("proc-sort").addEventListener("click", (e) => {
  const btn = e.target.closest("[data-sort]");
  if (!btn) return;
  procSortKey = btn.dataset.sort;
  document.querySelectorAll("#proc-sort button").forEach((b) => b.classList.toggle("on", b === btn));
  if (lastData) render(lastData);
});

// theme toggle
$("theme-toggle").addEventListener("click", () => {
  const light = document.documentElement.getAttribute("data-theme") === "light";
  if (light) document.documentElement.removeAttribute("data-theme");
  else document.documentElement.setAttribute("data-theme", "light");
  try { localStorage.setItem("wl-theme", light ? "dark" : "light"); } catch { /* private mode */ }
  if (lastData) render(lastData);
});

// redraw on orientation change / resize
let resizeT = null;
window.addEventListener("resize", () => {
  clearTimeout(resizeT);
  resizeT = setTimeout(() => { if (lastData) render(lastData); }, 150);
});

seedHistory().then(refresh);
timer = setInterval(refresh, intervalMs);
