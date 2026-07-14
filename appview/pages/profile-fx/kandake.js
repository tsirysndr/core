// neon-terrain.js — profile fx: renders the punchcard as a polycss-style 3D
// terrain (perspective + rotateX/rotateZ + translateZ elevation, as in the
// terrain demo) lit with twinkl "y2kringe" neon — cyan/magenta pulses sweeping
// across the grid. One self-contained ES module, vanilla JS, no imports.
//
// CSS is loaded by injecting a <style> element via the DOM at runtime, so the
// module stays a single file with no build step.

const STYLE_ID = "npx-neon-terrain-style";
const GRID_CLASS = "npx-grid";

// twinkl y2kringe palette
const VOID_BG = "#0a001a";
const HUE_CYAN = 185;
const HUE_MAGENTA = 300;

const WIDE_Q = "(min-width: 768px)";
const REDUCED_Q = "(prefers-reduced-motion: reduce)";
const COLS_NARROW = 28;
const COLS_WIDE = 14;

const MAX_LIFT = 22; // px of translateZ at full energy
const WAVE_SPEED = 1.7; // rad/s of the diagonal pulse
const SHIMMER_SPEED = 0.6;
const ROT_SPEED = 9; // deg/s turntable rotation of the whole terrain
const TILT = 55; // deg rotateX of the board
const BOARD_SCALE = 0.72;
const QUANT = 48; // energy quantization levels — skip redundant style writes

const clamp01 = (n) => Math.max(0, Math.min(1, n));

const css = `
[data-punchcard].${GRID_CLASS} {
  transform-style: preserve-3d;
  background-color: ${VOID_BG};
  background-image:
    radial-gradient(1px 1px at 10% 20%, rgb(255 255 255 / 0.5) 0%, transparent 100%),
    radial-gradient(1px 1px at 30% 80%, rgb(255 255 255 / 0.4) 0%, transparent 100%),
    radial-gradient(1.5px 1.5px at 65% 85%, rgb(255 255 255 / 0.45) 0%, transparent 100%),
    radial-gradient(1px 1px at 85% 15%, rgb(255 255 255 / 0.4) 0%, transparent 100%),
    radial-gradient(1.5px 1.5px at 15% 55%, rgb(255 255 255 / 0.5) 0%, transparent 100%);
  background-size: 200px 200px;
  border-radius: 12px;
  box-shadow: 0 0 24px rgb(255 0 255 / 0.25), inset 0 0 40px rgb(0 255 255 / 0.06);
}
[data-punchcard].${GRID_CLASS} > * {
  transform-style: preserve-3d;
  position: relative;
}
/* ground shadow under each lifted dot — cheap depth cue at floor level */
[data-punchcard].${GRID_CLASS} > *::after {
  content: "";
  position: absolute;
  inset: 20%;
  border-radius: 50%;
  background: rgb(255 0 255 / 0.14);
  filter: blur(1.5px);
  transform: translateZ(0.5px);
  pointer-events: none;
}
[data-punchcard].${GRID_CLASS} > * > :first-child {
  transform-style: preserve-3d;
  transition: none;
  will-change: transform, background-color, box-shadow;
}
`;

const injectStyle = () => {
  let el = document.getElementById(STYLE_ID);
  if (!el) {
    el = document.createElement("style");
    el.id = STYLE_ID;
    el.textContent = css;
    document.head.appendChild(el);
  }
  return el;
};

// Luminance of the dot's current color -> base terrain height, so activity
// level (darker/lighter contribution dots) becomes elevation.
const luminance = (el) => {
  const m = getComputedStyle(el).backgroundColor.match(/[\d.]+/g);
  if (!m || m.length < 3) return 0;
  const [r, g, b] = m.map(Number);
  return clamp01((0.2126 * r + 0.7152 * g + 0.0722 * b) / 255);
};

const collectDots = (grid) =>
  Array.from(grid.children, (c) => c.firstElementChild).filter(Boolean);

const clearDot = (el) => {
  el.style.backgroundColor = "";
  el.style.transform = "";
  el.style.boxShadow = "";
  el.style.transition = "";
  el.style.transformOrigin = "";
  el.style.willChange = "";
};

const paintDot = (el, energy, hueMix) => {
  const hue = HUE_CYAN + (HUE_MAGENTA - HUE_CYAN) * hueMix;
  const light = 55 + energy * 20;
  const color = `hsl(${hue.toFixed(1)} 100% ${light.toFixed(1)}%)`;
  const z = (energy * MAX_LIFT).toFixed(2);
  const s = (1 + energy * 1.2).toFixed(3);
  el.style.backgroundColor = color;
  el.style.transform = `translateZ(${z}px) scale(${s})`;
  el.style.boxShadow =
    `0 0 ${(2 + energy * 8).toFixed(1)}px ${color}, ` +
    `0 0 ${(6 + energy * 16).toFixed(1)}px hsl(${hue.toFixed(1)} 100% 50% / 0.55)`;
};

const start = (grid) => {
  const ac = new AbortController();
  const opts = { signal: ac.signal };
  const wideMq = matchMedia(WIDE_Q);
  const reduced = matchMedia(REDUCED_Q).matches;

  const styleEl = injectStyle();
  grid.classList.add(GRID_CLASS);

  const boardTransform = (deg) =>
    `perspective(1200px) rotateX(${TILT}deg) rotateZ(${deg.toFixed(2)}deg) scale(${BOARD_SCALE})`;
  grid.style.transform = boardTransform(45);

  let dots = collectDots(grid);
  // Inline overrides beat any Tailwind transition/duration classes on the dots.
  dots.forEach((el) => {
    el.style.transition = "none";
    el.style.transformOrigin = "center";
    el.style.willChange = "transform, background-color, box-shadow";
  });
  // Base heights are read from the pre-fx dot colors, before we repaint them.
  // In dark mode empty dots are darker than active ones; in light mode it is
  // inverted, so normalize against the dominant (most common) luminance.
  let base = dots.map(luminance);
  {
    const counts = new Map();
    base.forEach((l) => {
      const k = Math.round(l * 20);
      counts.set(k, (counts.get(k) || 0) + 1);
    });
    const idle = [...counts.entries()].sort((a, b) => b[1] - a[1])[0]?.[0] / 20 ?? 0;
    base = base.map((l) => clamp01(Math.abs(l - idle) * 1.6));
  }

  let cols = wideMq.matches ? COLS_WIDE : COLS_NARROW;
  let lastQ = new Int16Array(dots.length).fill(-1);
  let lastHue = new Float32Array(dots.length).fill(-1);

  const relayout = () => {
    cols = wideMq.matches ? COLS_WIDE : COLS_NARROW;
    lastQ.fill(-1);
  };
  wideMq.addEventListener("change", relayout, opts);

  const frame = (t) => {
    // Turntable: rotate the entire terrain around its Z axis.
    grid.style.transform = boardTransform(45 + t * ROT_SPEED);
    for (let i = 0; i < dots.length; i++) {
      const col = i % cols;
      const row = (i / cols) | 0;
      const wave = 0.5 + 0.5 * Math.sin(t * WAVE_SPEED - (col + row) * 0.35);
      const shimmer = 0.5 + 0.5 * Math.sin(t * SHIMMER_SPEED + col * 0.5 - row * 0.3);
      const energy = clamp01(0.12 + base[i] * 0.5 + wave * 0.45);
      const q = Math.round(energy * QUANT);
      const hueQ = Math.round(shimmer * 24) / 24;
      if (q === lastQ[i] && hueQ === lastHue[i]) continue;
      lastQ[i] = q;
      lastHue[i] = hueQ;
      paintDot(dots[i], q / QUANT, hueQ);
    }
  };

  let raf = 0;
  let visible = true;

  const loop = (ms) => {
    frame(ms / 1000);
    if (visible) raf = requestAnimationFrame(loop);
  };

  if (reduced) {
    // Static 3D render: elevation from activity, fixed neon tint, no motion.
    dots.forEach((el, i) => {
      const col = i % cols;
      const row = (i / cols) | 0;
      paintDot(el, clamp01(0.15 + base[i] * 0.85), ((col + row) % 8) / 8);
    });
    wideMq.addEventListener(
      "change",
      () => dots.forEach((el, i) => paintDot(el, clamp01(0.15 + base[i] * 0.85), (((i % cols) + ((i / cols) | 0)) % 8) / 8)),
      opts,
    );
  } else {
    const io = new IntersectionObserver((entries) => {
      const vis = entries.some((e) => e.isIntersecting);
      if (vis === visible) return;
      visible = vis;
      if (visible) raf = requestAnimationFrame(loop);
      else cancelAnimationFrame(raf);
    });
    io.observe(grid);
    raf = requestAnimationFrame(loop);
    ac.signal.addEventListener("abort", () => {
      cancelAnimationFrame(raf);
      io.disconnect();
    });
  }

  return () => {
    ac.abort();
    grid.classList.remove(GRID_CLASS);
    grid.style.transform = "";
    dots.forEach(clearDot);
    styleEl.remove();
  };
};

let cleanup = null;
let current = null;
const init = () => {
  const grid = document.querySelector("[data-punchcard]");
  if (grid === current) return;
  if (cleanup) cleanup();
  current = grid;
  cleanup = grid ? start(grid) : null;
};
// Guard against import before the DOM is parsed — without this, a script tag
// missing `defer` finds no punchcard and plain page loads never fire htmx:load.
if (document.readyState === "loading") {
  document.addEventListener("DOMContentLoaded", init, { once: true });
} else {
  init();
}
document.addEventListener("htmx:load", init);
