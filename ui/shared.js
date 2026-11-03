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
// Color theme ("palette"): pink, lavender, mint, sunset, or cosmos and grove (always dark).
const darkQuery = matchMedia("(prefers-color-scheme: dark)");
const currentPalette = () => store.get("pc-palette", "pink");
function currentTheme() {
  if (["cosmos", "grove"].includes(currentPalette())) return "dark";
  const pick = store.get("pc-theme", "auto");
  return pick === "auto" ? (darkQuery.matches ? "dark" : "light") : pick;
}
// theme colors for things drawn in code (canvases), refreshed when the theme changes
const themeColors = {};
function readThemeColors() {
  const cs = getComputedStyle(document.documentElement);
  for (const k of ["g1", "g2", "g3", "text", "ice-strong"]) themeColors[k] = cs.getPropertyValue("--" + k).trim();
}
// ---------- themes made from three colors: your own ("custom") or your wallpaper's ----------
const hexRgb = h => { const n = parseInt(h.replace("#", ""), 16); return [n >> 16 & 255, n >> 8 & 255, n & 255]; };
const rgbHex = c => "#" + c.map(v => Math.round(Math.max(0, Math.min(255, v))).toString(16).padStart(2, "0")).join("");
// a share of color a mixed into b (0 = all b, 1 = all a)
const mixRgb = (a, b, t) => a.map((v, i) => v * t + b[i] * (1 - t));
const WHITE = [255, 255, 255], BLACK = [0, 0, 0], NIGHT = [14, 9, 20];
const MADE_VARS = ["--panel", "--panel-hover", "--pink", "--pink-deep", "--ice", "--ice-strong", "--ice-hover", "--on-ice", "--text", "--text-2", "--border",
  "--g1", "--g2", "--g3", "--g1-soft", "--g2-soft", "--shade", "--lyric-sung", "--lyric-now", "--sky", "--tint"];
function madePalette(colors, dark) {
  const [a, b, c] = [colors.g1, colors.g2, colors.g3].map(hexRgb);
  const rgba = (x, al) => `rgba(${x.map(Math.round).join(", ")}, ${al})`;
  const hex = rgbHex;
  if (!dark) return {
    "--panel": rgba(mixRgb(a, WHITE, .12), .55), "--panel-hover": rgba(mixRgb(a, WHITE, .4), .5),
    "--pink": hex(mixRgb(a, WHITE, .45)), "--pink-deep": hex(mixRgb(a, WHITE, .65)),
    "--ice": hex(mixRgb(b, WHITE, .3)), "--ice-strong": hex(mixRgb(b, BLACK, .62)), "--ice-hover": hex(mixRgb(b, BLACK, .52)), "--on-ice": "#fff",
    "--text": hex(mixRgb(c, BLACK, .3)), "--text-2": hex(mixRgb(c, BLACK, .55)), "--border": rgba(mixRgb(a, WHITE, .6), .55),
    "--g1": hex(a), "--g2": hex(b), "--g3": hex(c), "--g1-soft": hex(mixRgb(a, WHITE, .45)), "--g2-soft": hex(mixRgb(b, WHITE, .4)),
    "--shade": hex(mixRgb(c, BLACK, .7)), "--lyric-sung": hex(mixRgb(a, WHITE, .1)), "--lyric-now": hex(mixRgb(b, WHITE, .4)),
    "--sky": `radial-gradient(120% 90% at 20% 10%, ${hex(mixRgb(a, WHITE, .35))} 0%, ${hex(mixRgb(a, WHITE, .7))} 30%, ${hex(mixRgb(c, WHITE, .6))} 62%, ${hex(mixRgb(b, WHITE, .7))} 100%)`,
    "--tint": mixRgb(a, WHITE, .12).map(Math.round).join(", ") + ", 110",
  };
  return {
    "--panel": rgba(mixRgb(a, NIGHT, .14), .45), "--panel-hover": rgba(a, .16),
    "--pink": hex(mixRgb(a, NIGHT, .35)), "--pink-deep": hex(mixRgb(a, NIGHT, .5)),
    "--ice": rgba(b, .2), "--ice-strong": hex(mixRgb(b, WHITE, .85)), "--ice-hover": hex(mixRgb(b, WHITE, .65)), "--on-ice": hex(mixRgb(b, BLACK, .12)),
    "--text": hex(mixRgb(a, WHITE, .1)), "--text-2": hex(mixRgb(a, WHITE, .4)), "--border": rgba(a, .22),
    "--g1": hex(a), "--g2": hex(b), "--g3": hex(c), "--g1-soft": hex(mixRgb(a, NIGHT, .45)), "--g2-soft": hex(mixRgb(b, NIGHT, .35)),
    "--shade": "#000", "--lyric-sung": hex(mixRgb(a, WHITE, .1)), "--lyric-now": hex(mixRgb(b, WHITE, .4)),
    "--sky": `radial-gradient(120% 90% at 20% 10%, ${hex(mixRgb(a, NIGHT, .4))} 0%, ${hex(mixRgb(c, NIGHT, .28))} 40%, ${hex(mixRgb(b, NIGHT, .2))} 75%, ${hex(mixRgb(b, NIGHT, .1))} 100%)`,
    "--tint": mixRgb(a, NIGHT, .14).map(Math.round).join(", ") + ", 170",
  };
}
const DEFAULT_CUSTOM = { g1: "#f2a6c0", g2: "#8cc6e6", g3: "#e7799c" };
function madeColors() {
  const p = currentPalette();
  if (p === "custom") return store.get("pc-custom-colors", DEFAULT_CUSTOM);
  if (p === "wallpaper") return store.get("pc-wall-colors", DEFAULT_CUSTOM);
  return null;
}

// Picks three nice theme colors from a picture (a wallpaper or album art): the most common
// colorful hue, a second one that's clearly different, and a stronger accent.
function colorsFromPicture(src) {
  return new Promise(resolve => {
    const img = new Image();
    img.onload = () => {
      const N = 48, cv = Object.assign(document.createElement("canvas"), { width: N, height: N });
      const cx = cv.getContext("2d", { willReadFrequently: true });
      cx.drawImage(img, 0, 0, N, N);
      const px = cx.getImageData(0, 0, N, N).data;
      const bins = Array.from({ length: 24 }, () => ({ w: 0, s: 0, l: 0 }));
      for (let i = 0; i < px.length; i += 4) {
        const [h, s, l] = rgbHsl(px[i], px[i + 1], px[i + 2]);
        if (l < .08 || l > .96) continue;
        const w = s * s + .02;
        const bin = bins[Math.floor(h * 24) % 24];
        bin.w += w; bin.s += s * w; bin.l += l * w;
      }
      const ranked = bins.map((b, i) => ({ h: (i + .5) / 24, w: b.w, s: b.w ? b.s / b.w : 0, l: b.w ? b.l / b.w : 0 })).sort((x, y) => y.w - x.w);
      const first = ranked[0];
      const far = (x, y) => Math.min(Math.abs(x - y), 1 - Math.abs(x - y));
      const second = ranked.find(r => r.w > first.w * .08 && far(r.h, first.h) > .1) || { h: (first.h + .14) % 1, s: first.s, l: first.l };
      // keep them soft and bright enough to look good as a theme
      const soft = (r, light) => hslHex(r.h, Math.min(.85, Math.max(.45, r.s)), light);
      resolve({ g1: soft(first, .72), g2: soft(second, .74), g3: hslHex(first.h, Math.min(.9, Math.max(.55, first.s + .15)), .6) });
    };
    img.onerror = () => resolve(null);
    img.src = src;
  });
}
function rgbHsl(r, g, b) {
  r /= 255; g /= 255; b /= 255;
  const mx = Math.max(r, g, b), mn = Math.min(r, g, b), l = (mx + mn) / 2, d = mx - mn;
  if (!d) return [0, 0, l];
  const s = d / (1 - Math.abs(2 * l - 1));
  const h = mx === r ? ((g - b) / d + 6) % 6 : mx === g ? (b - r) / d + 2 : (r - g) / d + 4;
  return [h / 6, s, l];
}
function hslHex(h, s, l) {
  const f = n => { const k = (n + h * 12) % 12, a = s * Math.min(l, 1 - l); return 255 * (l - a * Math.max(-1, Math.min(k - 3, 9 - k, 1))); };
  return rgbHex([f(0), f(8), f(4)]);
}

function applyTheme() {
  const t = currentTheme();
  document.documentElement.dataset.theme = t;
  document.documentElement.dataset.palette = currentPalette();
  const made = madeColors();
  const vars = made ? madePalette(made, t === "dark") : {};
  MADE_VARS.forEach(k => vars[k] ? document.documentElement.style.setProperty(k, vars[k]) : document.documentElement.style.removeProperty(k));
  readThemeColors();
  // the frosted glass behind popups takes the theme's tint
  const tint = getComputedStyle(document.documentElement).getPropertyValue("--tint").split(",").map(v => parseInt(v));
  if (tint.length === 4 && tint.every(n => !isNaN(n))) invoke("set_tint", { rgba: tint }).catch(() => {});
  dispatchEvent(new Event("pc-theme-change"));
}
applyTheme();
darkQuery.addEventListener("change", applyTheme);
addEventListener("storage", e => { if (["pc-theme", "pc-palette", "pc-custom-colors", "pc-wall-colors"].includes(e.key)) applyTheme(); });

// ---------- Grove glyphs ----------
// Each glyph is a few strokes picked on a small grid, so every one is different and none of
// them is real writing. Returns SVG path data centered on (cx, cy), s = grid step.
function glyphPath(cx, cy, s, rnd = Math.random) {
  const pts = [[0, 0], [1, 0], [2, 0], [0, 1], [1, 1], [2, 1], [0, 2], [1, 2], [2, 2], [0, 3], [1, 3], [2, 3]];
  const at = p => `${cx + (p[0] - 1) * s} ${cy + (p[1] - 1.5) * s}`;
  let d = "";
  const strokes = 2 + Math.floor(rnd() * 3);
  for (let k = 0; k < strokes; k++) {
    const a = pts[Math.floor(rnd() * pts.length)], b = pts[Math.floor(rnd() * pts.length)];
    d += a === b ? `M${at(a)}m-1.6 0a1.6 1.6 0 1 0 3.2 0a1.6 1.6 0 1 0 -3.2 0` : `M${at(a)}L${at(b)}`;
  }
  return d;
}
// In Grove, short lines of glowing glyphs now and then fade in somewhere on the panel and
// fade away again. They sit on top but never catch clicks, and stay faint so text reads.
(function groveGlyphs() {
  if (/taskbar|lyrics|toast|idle|overlay/.test(location.pathname)) return;
  let layer = null;
  const css = `#pc-glyphs { position: fixed; inset: 0; pointer-events: none; z-index: 2147483000; overflow: hidden; }
    #pc-glyphs svg { position: absolute; opacity: 0; filter: drop-shadow(0 0 4px #3ff0c0) drop-shadow(0 0 10px rgba(63,240,192,.45));
      animation: pc-glyph var(--life) ease-in-out forwards; }
    @keyframes pc-glyph { 0% { opacity: 0; transform: translateY(4px); filter: blur(2px) drop-shadow(0 0 4px #3ff0c0); }
      30%, 70% { opacity: var(--peak); transform: none; } 100% { opacity: 0; transform: translateY(-4px); filter: blur(2px) drop-shadow(0 0 4px #3ff0c0); } }`;
  function spawn() {
    if (currentPalette() !== "grove" || document.hidden || store.get("pc-grove-glyphs", true) === false) return;
    const panel = document.querySelector(".popup, .win, .w");
    if (!panel || !panel.offsetWidth) return;
    if (!layer) {
      document.head.append(Object.assign(document.createElement("style"), { textContent: css }));
      layer = Object.assign(document.createElement("div"), { id: "pc-glyphs" });
      document.body.append(layer);
    }
    if (layer.childElementCount >= 4) return;
    // only the part of the panel you can see
    const p = panel.getBoundingClientRect(), left = Math.max(0, p.left), top = Math.max(0, p.top);
    const r = { left, top, width: Math.min(innerWidth, p.right) - left, height: Math.min(innerHeight, p.bottom) - top };
    if (r.width < 120 || r.height < 60) return;
    const n = 2 + Math.floor(Math.random() * 5), s = 4 + Math.random() * 2.5, w = n * s * 3.4;
    const vertical = Math.random() < .35;
    let paths = "";
    for (let i = 0; i < n; i++) {
      const o = 8 + i * s * 3.4 + s * 1.7;
      paths += `<path d="${vertical ? glyphPath(s * 2 + 4, o, s) : glyphPath(o, s * 2.5 + 4, s)}"/>`;
    }
    const [W, H] = vertical ? [s * 4 + 8, w + 16] : [w + 16, s * 5 + 8];
    const svg = `<svg width="${W}" height="${H}" viewBox="0 0 ${W} ${H}" fill="none" stroke="#7df7d6" stroke-width="1.6" stroke-linecap="round" stroke-linejoin="round">${paths}</svg>`;
    layer.insertAdjacentHTML("beforeend", svg);
    const el = layer.lastElementChild, life = 5 + Math.random() * 4;
    Object.assign(el.style, {
      left: r.left + 10 + Math.random() * Math.max(0, r.width - W - 20) + "px",
      top: r.top + 10 + Math.random() * Math.max(0, r.height - H - 20) + "px",
    });
    el.style.setProperty("--life", life + "s");
    el.style.setProperty("--peak", (.3 + Math.random() * .2).toFixed(2));
    setTimeout(() => el.remove(), life * 1000 + 100);
  }
  setInterval(() => { if (Math.random() < .5) spawn(); }, 1800);
})();

// ---------- sound effects: tiny synthesized pops and chimes (no sound files) ----------
let audioCtx = null;
const SOUNDS = {
  open: [[520, 880, .09, "sine", .07]],
  close: [[700, 420, .08, "sine", .05]],
  tap: [[1400, 1100, .025, "triangle", .035]],
  tick: [[1800, 1800, .015, "square", .012]],
  toggle: [[660, 990, .05, "sine", .05]],
  note: [[880, 880, .12, "sine", .06], [1320, 1320, .18, "sine", .05, .09]],
  game: [[440, 660, .08, "triangle", .05], [660, 990, .12, "triangle", .05, .08]],
};
function sfx(name) {
  if (!store.get("pc-sfx", true) || !SOUNDS[name]) return;
  const vol = store.get("pc-sfx-vol", .6);
  try {
    audioCtx = audioCtx || new AudioContext();
    if (audioCtx.state === "suspended") audioCtx.resume();
    const now = audioCtx.currentTime;
    for (const [f0, f1, len, type, gain, delay = 0] of SOUNDS[name]) {
      const o = audioCtx.createOscillator(), g = audioCtx.createGain();
      o.type = type;
      o.frequency.setValueAtTime(f0, now + delay);
      o.frequency.exponentialRampToValueAtTime(f1, now + delay + len);
      g.gain.setValueAtTime(0, now + delay);
      g.gain.linearRampToValueAtTime(gain * vol, now + delay + .008);
      g.gain.exponentialRampToValueAtTime(.0001, now + delay + len + .08);
      o.connect(g).connect(audioCtx.destination);
      o.start(now + delay); o.stop(now + delay + len + .1);
    }
  } catch {}
}

// The normal Windows File Explorer (its shortcut, its app ID or explorer.exe itself): opened
// through Pink Cloud so it stays Explorer instead of turning into Pink Cloud Files.
const isExplorer = (path, exe) => /(^|\\)explorer\.exe$/i.test(exe || "") || /(^|\\)explorer\.exe$/i.test(path || "") ||
  /Microsoft\.Windows\.Explorer/i.test(path || "") || /\\File Explorer\.lnk$/i.test(path || "");

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
  sfx("open");
  el.classList.remove("closing", "opening"); void el.offsetWidth; el.classList.add("opening");
  setTimeout(() => el.classList.remove("opening"), 1400);
}
function closeAppWindow(el, tauriWindow) {
  sfx("close");
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
    sfx("open");
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
