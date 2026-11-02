if (!window.__TAURI_INTERNALS__) {
  throw new Error("Camera Live has to run inside its own window.");
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
const overlay = document.querySelector("#overlay");
const overlayTitle = document.querySelector("#overlay-title");
const overlayBody = document.querySelector("#overlay-body");
const overlayAction = document.querySelector("#overlay-action");
const pill = document.querySelector("#live-pill");
const camName = document.querySelector("#cam-name");
const kicker = document.querySelector("#kicker");
const settings = document.querySelector("#settings");
const form = document.querySelector("#settings-form");
const testResult = document.querySelector("#test-result");
const presetsEl = document.querySelector("#presets");
const ptzNote = document.querySelector("#ptz-note");
const toastEl = document.querySelector("#toast");
const speedInput = document.querySelector("#speed");
const speedRead = document.querySelector("#speed-read");

const fields = {
  name: document.querySelector("#f-name"),
  host: document.querySelector("#f-host"),
  rtsp: document.querySelector("#f-rtsp"),
  onvif: document.querySelector("#f-onvif"),
  user: document.querySelector("#f-user"),
  pass: document.querySelector("#f-pass"),
};

let config = null;
let lastBitmap = null;
let drawing = false;
let pending = null;
let toastTimer = 0;
const keys = new Set();
let pointer = null;
let pumping = false;
let moved = false;

function messageOf(error) {
  if (typeof error === "string") return error;
  return error?.message || "Something went wrong.";
}

function toast(text) {
  toastEl.textContent = text;
  toastEl.hidden = false;
  clearTimeout(toastTimer);
  toastTimer = setTimeout(() => {
    toastEl.hidden = true;
  }, 4200);
}

function sleep(ms) {
  return new Promise((resolve) => setTimeout(resolve, ms));
}

function formConfig() {
  const stream = form.querySelector("input[name=stream]:checked")?.value || "sub";
  return {
    name: fields.name.value,
    host: fields.host.value,
    rtspPort: Number(fields.rtsp.value) || 554,
    onvifPort: Number(fields.onvif.value) || 2020,
    username: fields.user.value,
    password: fields.pass.value,
    stream,
    motion: document.querySelector("#motion").checked,
    motionBorders: document.querySelector("#borders").checked,
    motionSensitivity: Number(document.querySelector("#motion-sens").value) || 5,
  };
}

function fillForm(cfg) {
  fields.name.value = cfg?.name || "Tapo C200";
  fields.host.value = cfg?.host || "192.168.0.101";
  fields.rtsp.value = cfg?.rtspPort || 554;
  fields.onvif.value = cfg?.onvifPort || 2020;
  fields.user.value = cfg?.username || "";
  fields.pass.value = cfg?.password || "";
  const stream = cfg?.stream === "main" ? "main" : "sub";
  form.querySelectorAll("input[name=stream]").forEach((input) => {
    input.checked = input.value === stream;
  });
  const motion = document.querySelector("#motion");
  const borders = document.querySelector("#borders");
  const sens = document.querySelector("#motion-sens");
  motion.checked = cfg ? cfg.motion !== false : true;
  borders.checked = Boolean(cfg?.motionBorders);
  sens.value = cfg?.motionSensitivity || 5;
  document.querySelector("#motion-read").textContent = sens.value;
}

function syncQuality() {
  const stream = config?.stream === "main" ? "main" : "sub";
  document.querySelector("#q-sub").classList.toggle("is-on", stream === "sub");
  document.querySelector("#q-main").classList.toggle("is-on", stream === "main");
  camName.textContent = config?.name || "Camera Live";
}

function openSettings() {
  fillForm(config);
  testResult.textContent = "";
  settings.hidden = false;
  fields.host.focus();
}

function closeSettings() {
  settings.hidden = true;
}

function showOverlay(title, body, actionLabel) {
  overlay.hidden = false;
  overlay.classList.toggle("dim", Boolean(config));
  overlayTitle.textContent = title;
  overlayBody.textContent = body;
  if (actionLabel) {
    overlayAction.hidden = false;
    overlayAction.textContent = actionLabel;
  } else {
    overlayAction.hidden = true;
  }
}

function renderStats(stats) {
  const receiving = Boolean(stats?.receiving);
  const running = Boolean(stats?.running);
  pill.textContent = receiving ? "Live" : running ? "Connecting" : "Idle";
  pill.classList.toggle("live", receiving);
  const motionPill = document.querySelector("#motion-pill");
  motionPill.hidden = !stats?.motionActive;
  const bordersBox = document.querySelector("#borders");
  if (
    performance.now() - bordersTouch > 800 &&
    typeof stats?.motionBorders === "boolean" &&
    bordersBox.checked !== stats.motionBorders
  ) {
    bordersBox.checked = stats.motionBorders;
  }
  document.querySelector("#motion-state").textContent = stats?.motionActive
    ? "Movement in the picture just now."
    : "";
  if (receiving && stats.width) {
    document.querySelector("#stat-res").textContent = `${stats.width}×${stats.height}`;
    document.querySelector("#stat-fps").textContent = `${stats.fps.toFixed(1)} fps`;
    document.querySelector("#stat-rate").textContent = `${Math.round(stats.kbps)} kbps`;
  }
  if (!config) {
    showOverlay(
      "Connect your Tapo C200",
      "In the Tapo app, open the camera, then Settings, Advanced Settings, Camera Account. Create a username and password there. It is separate from your TP-Link login, and this computer has to be on the same network as the camera.",
      "Add camera",
    );
    return;
  }
  if (receiving) {
    overlay.hidden = true;
    return;
  }
  if (stats?.error) {
    showOverlay("Reconnecting", stats.error, "Try again");
  } else if (running) {
    showOverlay("Opening the stream", "Asking the camera for its picture.", "");
  }
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
  ctx.drawImage(lastBitmap, (width - dw) / 2, (height - dh) / 2, dw, dh);
}

function speed() {
  return Math.min(1, Math.max(0.2, Number(speedInput.value) / 10));
}

function desired() {
  let pan = 0;
  let tilt = 0;
  if (pointer) {
    pan += pointer.pan;
    tilt += pointer.tilt;
  }
  if (keys.has("ArrowLeft")) pan -= 1;
  if (keys.has("ArrowRight")) pan += 1;
  if (keys.has("ArrowUp")) tilt += 1;
  if (keys.has("ArrowDown")) tilt -= 1;
  pan = Math.max(-1, Math.min(1, pan));
  tilt = Math.max(-1, Math.min(1, tilt));
  if (!pan && !tilt) return null;
  const factor = speed();
  return { pan: pan * factor, tilt: tilt * factor };
}

async function pump() {
  if (pumping) return;
  pumping = true;
  try {
    while (desired()) {
      moved = true;
      await invoke("ptz_move", desired());
      await sleep(260);
    }
  } catch (error) {
    keys.clear();
    pointer = null;
    ptzNote.textContent = messageOf(error);
  } finally {
    if (moved) {
      moved = false;
      try {
        await invoke("ptz_stop");
      } catch {}
    }
    pumping = false;
  }
  if (desired()) pump();
}

function typing(target) {
  const tag = target?.tagName;
  return tag === "INPUT" || tag === "TEXTAREA" || tag === "SELECT";
}

async function loadPresets() {
  try {
    const presets = await invoke("ptz_presets");
    presetsEl.replaceChildren();
    if (!presets.length) {
      ptzNote.textContent = "No presets yet. Aim the camera and save this view.";
      return;
    }
    ptzNote.textContent = "Hold a direction to turn. A short press nudges.";
    for (const preset of presets) {
      const button = document.createElement("button");
      button.type = "button";
      button.className = "preset-chip";
      button.textContent = preset.name || preset.token;
      button.addEventListener("click", async () => {
        try {
          await invoke("ptz_goto", { token: preset.token });
        } catch (error) {
          toast(messageOf(error));
        }
      });
      presetsEl.appendChild(button);
    }
  } catch (error) {
    ptzNote.textContent = messageOf(error);
  }
}

async function loadInfo() {
  try {
    const info = await invoke("camera_info");
    if (info.model) kicker.textContent = info.model;
  } catch {
    kicker.textContent = "Tapo C200";
  }
}

document.querySelector("#open-settings").addEventListener("click", openSettings);
overlayAction.addEventListener("click", async () => {
  if (overlayAction.textContent === "Add camera") {
    openSettings();
    return;
  }
  try {
    await invoke("start_stream");
  } catch (error) {
    toast(messageOf(error));
  }
});
document.querySelector("#close-settings").addEventListener("click", closeSettings);
document.querySelector("#f-show").addEventListener("change", (event) => {
  fields.pass.type = event.target.checked ? "text" : "password";
});
speedInput.addEventListener("input", () => {
  speedRead.textContent = speedInput.value;
});

document.querySelector("#pop").addEventListener("click", async () => {
  try {
    await invoke("pop_out_view");
  } catch (error) {
    toast(messageOf(error));
  }
});

const motionBox = document.querySelector("#motion");
const bordersBox = document.querySelector("#borders");
const motionSens = document.querySelector("#motion-sens");
let bordersTouch = 0;
async function pushMotion() {
  document.querySelector("#motion-read").textContent = motionSens.value;
  try {
    await invoke("set_motion", {
      enabled: motionBox.checked,
      sensitivity: Number(motionSens.value),
    });
  } catch (error) {
    toast(messageOf(error));
  }
}
motionBox.addEventListener("change", pushMotion);
bordersBox.addEventListener("change", async () => {
  bordersTouch = performance.now();
  try {
    await invoke("set_borders", { enabled: bordersBox.checked });
  } catch (error) {
    toast(messageOf(error));
  }
});
motionSens.addEventListener("change", pushMotion);
motionSens.addEventListener("input", () => {
  document.querySelector("#motion-read").textContent = motionSens.value;
});

document.querySelector("#q-sub").addEventListener("click", () => chooseStream("sub"));
document.querySelector("#q-main").addEventListener("click", () => chooseStream("main"));

async function chooseStream(stream) {
  if (!config) {
    openSettings();
    return;
  }
  if (config.stream === stream) return;
  config = { ...config, stream };
  syncQuality();
  try {
    config = await invoke("save_config", { config });
    syncQuality();
    loadInfo();
    loadPresets();
  } catch (error) {
    toast(messageOf(error));
  }
}

form.addEventListener("submit", async (event) => {
  event.preventDefault();
  try {
    config = await invoke("save_config", { config: formConfig() });
    syncQuality();
    closeSettings();
    loadInfo();
    loadPresets();
  } catch (error) {
    testResult.textContent = messageOf(error);
  }
});

document.querySelector("#test").addEventListener("click", async () => {
  testResult.textContent = "Checking the camera…";
  try {
    const report = await invoke("test_connection", { config: formConfig() });
    testResult.textContent = report.detail;
  } catch (error) {
    testResult.textContent = messageOf(error);
  }
});

document.querySelectorAll(".pad-btn[data-pan]").forEach((button) => {
  button.addEventListener("pointerdown", (event) => {
    if (!config) return;
    event.preventDefault();
    button.setPointerCapture(event.pointerId);
    pointer = { pan: Number(button.dataset.pan), tilt: Number(button.dataset.tilt) };
    pump();
  });
});
window.addEventListener("pointerup", () => {
  pointer = null;
});
window.addEventListener("pointercancel", () => {
  pointer = null;
});

document.querySelector("#home").addEventListener("click", async () => {
  try {
    await invoke("ptz_home");
  } catch (error) {
    toast(messageOf(error));
  }
});

document.querySelector("#preset-form").addEventListener("submit", async (event) => {
  event.preventDefault();
  const name = document.querySelector("#preset-name").value.trim();
  if (!name) return;
  try {
    await invoke("ptz_save", { name });
    document.querySelector("#preset-name").value = "";
    toast(`Saved “${name}”`);
    loadPresets();
  } catch (error) {
    toast(messageOf(error));
  }
});

document.querySelector("#snap").addEventListener("click", saveSnapshot);
document.querySelector("#fs").addEventListener("click", async () => {
  try {
    await invoke("toggle_fullscreen");
  } catch (error) {
    toast(messageOf(error));
  }
});
view.parentElement.addEventListener("dblclick", async () => {
  try {
    await invoke("toggle_fullscreen");
  } catch {}
});

window.addEventListener("keydown", (event) => {
  if (!settings.hidden || typing(event.target)) return;
  if (event.key === "f" || event.key === "F") {
    event.preventDefault();
    invoke("toggle_fullscreen").catch((error) => toast(messageOf(error)));
    return;
  }
  if (event.key === "s" || event.key === "S") {
    event.preventDefault();
    saveSnapshot();
    return;
  }
  if (event.key === "h" || event.key === "H") {
    event.preventDefault();
    invoke("ptz_home").catch((error) => toast(messageOf(error)));
    return;
  }
  if (event.key.startsWith("Arrow") && !event.repeat) {
    event.preventDefault();
    keys.add(event.key);
    pump();
  }
});
window.addEventListener("keyup", (event) => {
  keys.delete(event.key);
});
window.addEventListener("blur", () => {
  keys.clear();
  pointer = null;
});
window.addEventListener("resize", paint);
settings.addEventListener("click", (event) => {
  if (event.target === settings) closeSettings();
});
window.addEventListener("keydown", (event) => {
  if (event.key === "Escape" && !settings.hidden) closeSettings();
});

async function saveSnapshot() {
  try {
    const path = await invoke("save_snapshot");
    toast(`Saved ${path}`);
  } catch (error) {
    toast(messageOf(error));
  }
}

async function boot() {
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

  config = await invoke("load_config");
  syncQuality();
  fillForm(config);
  if (config) {
    renderStats({ running: true, receiving: false });
    loadInfo();
    loadPresets();
  } else {
    renderStats(null);
  }

  setInterval(async () => {
    try {
      renderStats(await invoke("stream_stats"));
    } catch {}
  }, 1000);
}

boot().catch((error) => {
  showOverlay("Camera Live could not start", messageOf(error), "");
});
