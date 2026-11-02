if (!window.__TAURI_INTERNALS__) {
  throw new Error("Camera view has to run inside Camera Live.");
}

const invoke = window.__TAURI_INTERNALS__.invoke;

class Channel {
  constructor() {
    const self = this;
    const queue = new Map();
    let next = 0;
    self.onmessage = () => {};
    self.id = window.__TAURI_INTERNALS__.transformCallback((response) => {
      if (!response || response.end) return;
      queue.set(response.index, response.message);
      while (queue.has(next)) {
        const message = queue.get(next);
        queue.delete(next);
        next += 1;
        self.onmessage(message);
      }
    });
    self.__TAURI_TO_IPC_KEY__ = () => `__CHANNEL__:${self.id}`;
  }
}

const view = document.querySelector("#view");
const ctx = view.getContext("2d");
const pill = document.querySelector("#live-pill");
const motionPill = document.querySelector("#motion-pill");
const pin = document.querySelector("#pin");
const borders = document.querySelector("#borders");
let lastBitmap = null;
let drawing = false;
let pending = null;
let pinned = false;
let bordersOn = localStorage.getItem("camera-live-view-borders") === "1";
let bordersTouch = 0;
let regions = [];
let regionsHold = 0;

function syncBordersButton() {
  borders.classList.toggle("text-btn-strong", bordersOn);
  borders.textContent = bordersOn ? "Borders on" : "Borders";
  borders.setAttribute("aria-pressed", bordersOn ? "true" : "false");
}

function paint() {
  if (!lastBitmap) return;
  const rect = view.getBoundingClientRect();
  const dpr = window.devicePixelRatio || 1;
  const width = Math.max(1, Math.round(rect.width * dpr));
  const height = Math.max(1, Math.round(rect.height * dpr));
  if (view.width !== width || view.height !== height) {
    view.width = width;
    view.height = height;
  }
  const scale = Math.min(width / lastBitmap.width, height / lastBitmap.height);
  const dw = lastBitmap.width * scale;
  const dh = lastBitmap.height * scale;
  ctx.fillStyle = "#070807";
  ctx.fillRect(0, 0, width, height);
  const ox = (width - dw) / 2;
  const oy = (height - dh) / 2;
  ctx.drawImage(lastBitmap, ox, oy, dw, dh);
  if (!bordersOn || regions.length === 0) return;
  ctx.save();
  ctx.strokeStyle = "#ff2a2a";
  ctx.lineWidth = Math.max(2, 2.5 * dpr);
  ctx.lineJoin = "round";
  for (const box of regions) {
    const x = Number(box.x);
    const y = Number(box.y);
    const bw = Number(box.w);
    const bh = Number(box.h);
    if (![x, y, bw, bh].every(Number.isFinite) || bw <= 0 || bh <= 0) continue;
    ctx.strokeRect(ox + x * dw, oy + y * dh, bw * dw, bh * dh);
  }
  ctx.restore();
}

async function drawFrame(bytes) {
  pending = bytes;
  if (drawing) return;
  drawing = true;
  while (pending) {
    const current = pending;
    pending = null;
    try {
      const bitmap = await createImageBitmap(new Blob([current], { type: "image/jpeg" }));
      if (lastBitmap) lastBitmap.close();
      lastBitmap = bitmap;
      paint();
    } catch (error) {
      console.error(error);
    }
  }
  drawing = false;
}

borders.addEventListener("click", async () => {
  bordersOn = !bordersOn;
  bordersTouch = performance.now();
  localStorage.setItem("camera-live-view-borders", bordersOn ? "1" : "0");
  syncBordersButton();
  paint();
  try {
    await invoke("set_borders", { enabled: bordersOn });
  } catch (error) {
    console.error(error);
  }
});

pin.addEventListener("click", async () => {
  pinned = !pinned;
  pin.classList.toggle("text-btn-strong", pinned);
  pin.textContent = pinned ? "On top" : "Keep on top";
  try {
    await invoke("set_view_on_top", { on: pinned });
  } catch (error) {
    console.error(error);
  }
});

document.querySelector("#fs").addEventListener("click", () => {
  invoke("toggle_fullscreen").catch((error) => console.error(error));
});
view.addEventListener("dblclick", () => {
  invoke("toggle_fullscreen").catch((error) => console.error(error));
});
window.addEventListener("resize", paint);

async function boot() {
  syncBordersButton();
  try {
    const cfg = await invoke("load_config");
    const saved = localStorage.getItem("camera-live-view-borders") === "1";
    const migrated = localStorage.getItem("camera-live-borders-migrated") === "1";
    if (cfg && !migrated && saved && cfg.motionBorders !== true) {
      bordersOn = true;
      bordersTouch = performance.now();
      await invoke("set_borders", { enabled: true });
    } else if (cfg) {
      bordersOn = Boolean(cfg.motionBorders);
    }
    localStorage.setItem("camera-live-borders-migrated", "1");
    localStorage.setItem("camera-live-view-borders", bordersOn ? "1" : "0");
    syncBordersButton();
  } catch (error) {
    console.error(error);
  }
  const frames = new Channel();
  frames.onmessage = (message) => {
    const bytes = message instanceof ArrayBuffer
      ? new Uint8Array(message)
      : message instanceof Uint8Array
        ? message
        : null;
    if (bytes) drawFrame(bytes);
  };
  await invoke("watch_frames", { frames });
  setInterval(async () => {
    try {
      const stats = await invoke("stream_stats");
      const receiving = Boolean(stats?.receiving);
      pill.textContent = receiving ? "Live" : "Connecting";
      pill.classList.toggle("live", receiving);
      motionPill.hidden = !stats?.motionActive;
      if (
        performance.now() - bordersTouch > 800 &&
        typeof stats?.motionBorders === "boolean" &&
        stats.motionBorders !== bordersOn
      ) {
        bordersOn = stats.motionBorders;
        localStorage.setItem("camera-live-view-borders", bordersOn ? "1" : "0");
        syncBordersButton();
      }
      const next = bordersOn && Array.isArray(stats?.motionRegions) ? stats.motionRegions : [];
      if (next.length) {
        regions = next;
        regionsHold = performance.now() + 700;
      } else if (performance.now() > regionsHold) {
        regions = [];
      }
      paint();
    } catch {}
  }, 150);
}

boot().catch((error) => console.error(error));
