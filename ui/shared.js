// Helpers shared by the taskbar, Start menu and quick settings.
const { invoke } = window.__TAURI__.core;
const { listen } = window.__TAURI__.event;

addEventListener("error", e => invoke("js_log", { msg: `${e.message} (${e.filename}:${e.lineno})` }));
addEventListener("unhandledrejection", e => invoke("js_log", { msg: "Unhandled: " + (e.reason?.message || e.reason) }));

const store = {
  get(key, fallback) {
    try { const v = localStorage.getItem(key); return v === null ? fallback : JSON.parse(v); } catch { return fallback; }
  },
  set(key, value) {
    try { localStorage.setItem(key, JSON.stringify(value)); } catch {}
  },
};

// Light/dark: "auto" follows Windows; every screen shares the choice.
// Color theme ("palette"): pink, lavender, mint, sunset or cosmos (which is always dark).
const darkQuery = matchMedia("(prefers-color-scheme: dark)");
const currentPalette = () => store.get("pc-palette", "pink");
function currentTheme() {
  if (currentPalette() === "cosmos") return "dark";
  const pick = store.get("pc-theme", "auto");
  return pick === "auto" ? (darkQuery.matches ? "dark" : "light") : pick;
}
// theme colors for things drawn in code (canvases), refreshed when the theme changes
const themeColors = {};
function readThemeColors() {
  const cs = getComputedStyle(document.documentElement);
  for (const k of ["g1", "g2", "g3", "text", "ice-strong"]) themeColors[k] = cs.getPropertyValue("--" + k).trim();
}
function applyTheme() {
  const t = currentTheme();
  document.documentElement.dataset.theme = t;
  document.documentElement.dataset.palette = currentPalette();
  readThemeColors();
  // the frosted glass behind popups takes the theme's tint
  const tint = getComputedStyle(document.documentElement).getPropertyValue("--tint").split(",").map(v => parseInt(v));
  if (tint.length === 4 && tint.every(n => !isNaN(n))) invoke("set_tint", { rgba: tint }).catch(() => {});
  dispatchEvent(new Event("pc-theme-change"));
}
applyTheme();
darkQuery.addEventListener("change", applyTheme);
addEventListener("storage", e => { if (e.key === "pc-theme" || e.key === "pc-palette") applyTheme(); });

// Icons are fetched once per path and shared.
const iconCache = new Map();
function iconFor(path) {
  const key = path.toLowerCase();
  if (!iconCache.has(key)) iconCache.set(key, invoke("get_icon", { path }).catch(() => null));
  return iconCache.get(key);
}

// 3D tilt: anything with class "tilt" leans toward the mouse.
let tilted = null;
const untilt = el => { if (el) el.style.transform = ""; };
addEventListener("pointermove", e => {
  const el = e.target.closest ? e.target.closest(".tilt") : null;
  if (tilted && tilted !== el) untilt(tilted);
  tilted = el;
  if (!el || matchMedia("(prefers-reduced-motion: reduce)").matches) return;
  const r = el.getBoundingClientRect();
  const x = (e.clientX - r.left) / r.width - .5, y = (e.clientY - r.top) / r.height - .5;
  el.style.transform = `perspective(520px) rotateX(${(-y * 16).toFixed(1)}deg) rotateY(${(x * 16).toFixed(1)}deg) translateY(-3px) scale(1.05)`;
});
document.addEventListener("pointerleave", () => { untilt(tilted); tilted = null; });

// Files and Settings windows: animate in when shown, animate out and then hide when closed.
function openAppWindow(el) {
  el.classList.remove("closing", "opening"); void el.offsetWidth; el.classList.add("opening");
  setTimeout(() => el.classList.remove("opening"), 1400);
}
function closeAppWindow(el, tauriWindow) {
  el.classList.remove("opening"); el.classList.add("closing");
  setTimeout(() => { tauriWindow.hide(); el.classList.remove("closing"); }, 320);
}

// Stagger index for cascading entrances.
function stagger(root) {
  root.querySelectorAll(".stagger").forEach(box => [...box.children].forEach((c, i) => c.style.setProperty("--i", Math.min(i, 24))));
}

// Popups: replay the opening animation each time, close when focus moves elsewhere.
function setupPopup(name, onOpen) {
  const panel = document.querySelector(".popup");
  listen("opened", () => {
    panel.classList.remove("open"); void panel.offsetWidth; panel.classList.add("open");
    stagger(document);
    onOpen && onOpen();
  });
  window.__TAURI__.window.getCurrentWindow().onFocusChanged(({ payload: focused }) => {
    if (!focused) invoke("popup_blurred", { name });
  });
  addEventListener("keydown", e => { if (e.key === "Escape") invoke("popup_blurred", { name }); });
}

// Pink right-click menu at the mouse, from [{ text, icon, danger, action }] items (null = separator).
const menuActions = new Map();
let menuSeq = 0;
listen("menu-pick", ({ payload: id }) => {
  const action = id && menuActions.get(id);
  menuActions.clear();
  if (action) action();
});
function popupMenu(items) {
  menuActions.clear();
  const spec = items.map(it => {
    if (!it) return { sep: true };
    const id = String(++menuSeq);
    menuActions.set(id, it.action);
    return { id, text: it.text, icon: it.icon || null, danger: !!it.danger };
  });
  return invoke("show_menu", { items: spec });
}
