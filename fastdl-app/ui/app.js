// FastDL 前端 — 通过 window.__TAURI__ 与 Rust 后端交互（无打包器）
window.__FASTDL_BOOTED = true; // index.html 诊断脚本用
if (!window.__TAURI__ || !window.__TAURI__.core) {
  (window.showFastdlBanner || function () {})(
    "Tauri 运行时未注入（window.__TAURI__ 缺失），无法调用后端命令", true);
  throw new Error("window.__TAURI__ missing");
}
const { invoke } = window.__TAURI__.core;
// Tauri 2：事件 API 在顶层 .event（listen/emit/once）；.core 只有 invoke
const { listen } = window.__TAURI__.event;

function banner(msg, sticky) {
  if (window.showFastdlBanner) window.showFastdlBanner(msg, sticky);
}

let dir = null; // null = 默认 ~/Downloads
const els = {
  url: document.getElementById("url"),
  segs: document.getElementById("segs"),
  threads: document.getElementById("threads"),
  pickDir: document.getElementById("pickDir"),
  dirLabel: document.getElementById("dirLabel"),
  start: document.getElementById("start"),
  tasks: document.getElementById("tasks"),
};

// ---------- 工具 ----------
function fmtBytes(n) {
  if (!n) return "0 B";
  const u = ["B", "KB", "MB", "GB", "TB"];
  let i = 0;
  let v = n;
  while (v >= 1024 && i < u.length - 1) { v /= 1024; i++; }
  return `${v >= 100 ? v.toFixed(0) : v >= 10 ? v.toFixed(1) : v.toFixed(2)} ${u[i]}`;
}

// ---------- 任务卡片 ----------
function ensureTask(id) {
  let el = document.getElementById("task-" + id);
  if (el) return el;
  const empty = document.querySelector(".empty");
  if (empty) empty.remove();
  el = document.createElement("div");
  el.className = "task";
  el.id = "task-" + id;
  el.innerHTML = `
    <div class="top">
      <span class="name"></span>
      <span class="pill queued">排队</span>
      <div class="actions"></div>
    </div>
    <div class="bar"><i></i></div>
    <div class="meta">
      <span class="stat">进度 <b class="m-prog">0%</b></span>
      <span class="stat">大小 <b class="m-size">—</b></span>
      <span class="stat">速度 <b class="m-speed">—</b></span>
      <span class="stat">段 <b class="m-seg">—</b></span>
    </div>
    <div class="err-line" hidden></div>`;
  els.tasks.appendChild(el);
  return el;
}

function setState(el, view) {
  el.classList.remove("done", "failed");
  const pill = el.querySelector(".pill");
  const label = { running: "下载中", paused: "已暂停", done: "完成", failed: "失败", queued: "排队" }[view.state];
  pill.textContent = label;
  pill.className = "pill " + view.state;
  if (view.state === "done") el.classList.add("done");
  if (view.state === "failed") el.classList.add("failed");

  const actions = el.querySelector(".actions");
  actions.innerHTML = "";
  const mk = (txt, cls, fn) => {
    const b = document.createElement("button");
    b.textContent = txt;
    if (cls) b.className = cls;
    b.onclick = fn;
    actions.appendChild(b);
  };
  if (view.state === "running" || view.state === "queued") mk("暂停", null, () => invoke("pause_task", { id: view.id }));
  if (view.state === "paused") mk("继续", null, () => invoke("resume_task", { id: view.id }));
  if (view.state === "failed") mk("重试", null, () => invoke("resume_task", { id: view.id }));
  if (view.state === "done") mk("定位", null, () => invoke("reveal", { out: view.out }));
  mk("移除", "ghost", () => {
    invoke("remove_task", { id: view.id }).then(() => el.remove());
  });

  const err = el.querySelector(".err-line");
  if (view.error) { err.hidden = false; err.textContent = view.error; } else err.hidden = true;

  applyProgress(el, view);
}

function applyProgress(el, p) {
  const pct = p.total ? Math.min(100, (p.done / p.total) * 100) : 0;
  el.querySelector(".bar > i").style.width = pct + "%";
  el.querySelector(".m-prog").textContent = p.total ? pct.toFixed(1) + "%" : "…";
  el.querySelector(".m-size").textContent = p.total ? fmtBytes(p.done) + " / " + fmtBytes(p.total) : fmtBytes(p.done);
  el.querySelector(".m-speed").textContent = p.state === "running" || p.state === "queued" ? fmtBytes(p.bps || 0) + "/s" : "—";
  el.querySelector(".m-seg").textContent = p.total ? (p.segments + " 段 × " + p.threads + " 线程") : "探测中…";
}

function viewFromProgress(evt, fallback) {
  return {
    id: evt.taskId,
    state: fallback?.state || "running",
    done: evt.done,
    total: evt.total,
    bps: evt.bps,
    segments: fallback?.segments || 0,
    threads: fallback?.threads || 0,
    out: fallback?.out || "",
    url: fallback?.url || "",
    error: "",
  };
}

const known = {}; // id -> TaskView（缓存元数据）

// ---------- 事件 ----------
listen("task-progress", (evt) => {
  const p = evt.payload;
  const prev = known[p.taskId];
  const v = prev
    ? { ...prev, done: p.done, total: p.total, bps: p.bps }
    : viewFromProgress(p, null);
  known[p.taskId] = v;
  const el = ensureTask(v.id);
  el.querySelector(".name").textContent = v.url || v.out;
  setState(el, v);
});

listen("task-state", async (evt) => {
  const p = evt.payload;
  // 拉一次权威视图
  const list = await invoke("list_tasks");
  const view = list.find((t) => t.id === p.taskId);
  if (!view) return;
  known[p.taskId] = view;
  const el = ensureTask(p.taskId);
  el.querySelector(".name").textContent = view.url;
  setState(el, view);
});

// ---------- 交互 ----------
els.pickDir.onclick = async () => {
  const d = await invoke("pick_dir");
  if (d) { dir = d; els.dirLabel.textContent = d; }
};

els.start.onclick = async () => {
  const url = els.url.value.trim();
  if (!url) {
    els.url.focus();
    banner("请先在上方输入下载链接", false);
    return;
  }
  try {
    const id = await invoke("start_download", {
      args: {
        url,
        dir,
        filename: null,
        segments: parseInt(els.segs.value) || 32,
        threads: parseInt(els.threads.value) || 8,
      },
    });
    els.url.value = "";
    // 立即渲染新任务卡片（不必等第一个进度事件）
    const list = await invoke("list_tasks");
    const view = list.find((t) => t.id === id);
    if (view) {
      known[id] = view;
      const el = ensureTask(id);
      el.querySelector(".name").textContent = view.url;
      setState(el, view);
    }
  } catch (e) {
    banner("启动失败: " + e, true);
  }
};

els.url.addEventListener("keydown", (e) => { if (e.key === "Enter") els.start.click(); });

// 启动时恢复已有任务列表
(async () => {
  const d = await invoke("default_dir_cmd");
  els.dirLabel.textContent = d;
  const list = await invoke("list_tasks");
  for (const t of list) {
    known[t.id] = t;
    const el = ensureTask(t.id);
    el.querySelector(".name").textContent = t.url;
    setState(el, t);
  }
  if (!list.length) {
    const e = document.createElement("div");
    e.className = "empty";
    e.textContent = "还没有任务 — 在上方粘贴一个直链开始下载";
    els.tasks.appendChild(e);
  }
})();
