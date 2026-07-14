// Draggable 3D punchcard, rendered on a canvas. We measure the real commit
// dots once (position, size, colour, and an activity-based depth), hide the
// original grid, and redraw the dots ourselves as a tilted 3D plane you can
// grab and spin. Doing the projection in JS + canvas keeps 365 dots cheap where
// per-dot CSS transforms did not. Self-contained, vanilla ES module.

const REDUCED = matchMedia("(prefers-reduced-motion: reduce)");
const DEG = Math.PI / 180;

const PERSPECTIVE = 1e6; // effectively infinite: near-orthographic, so the grid
                         // looks identical to the flat default until you rotate
const DEPTH = 46; // px — pillar height for the most active day
const SHAFT_SHADE = 0.72; // shaft/base sit a touch darker than the lit top cap
const HUE_SHIFT = 5; // max degrees the hue drifts (toward yellow/blue) at full yaw
const GROUND_RING = 0.18; // outline thickness of ground dots, as a fraction of radius
const GHOST_COLOR = "rgba(128,128,128,0.5)"; // fallback outline for empty days
const DRAG_SENS = 0.45; // degrees of rotation per pixel dragged
const REST_TILT_X = 0; // resting pitch the grid springs back to
const REST_TILT_Y = 0; // resting yaw the grid springs back to
const STIFFNESS = 0.08; // spring pull back toward the resting tilt
const DAMPING = 0.82; // spring velocity decay per frame
const MAX_ANGLE = 88; // clamp every rotation axis to just under 90°

const clampAngle = (v) => Math.max(-MAX_ANGLE, Math.min(MAX_ANGLE, v));

let grid = null;
let canvas = null;
let ctx = null;
let dpr = 1;
let cx = 0; // grid centre, in CSS px
let cy = 0;
let dots = []; // { x, y, h, r, rgb } — x/y centred on the grid centre
let ghosts = []; // { x, y, r, color } — empty days, drawn as flat ground outlines
let rotX = REST_TILT_X;
let rotY = REST_TILT_Y;
let velX = 0;
let velY = 0;
let dragging = false;
let lastX = 0;
let lastY = 0;
let raf = 0;

// Parse a dot's fill to [r,g,b]; null if fully transparent (empty day).
function parseColor(str) {
  const m = str.match(/[\d.]+/g);
  if (!m) return null;
  const [r, g, b, a = 1] = m.map(Number);
  return a === 0 ? null : [r, g, b];
}

// Chroma is a theme-agnostic proxy for activity: grey empty days sit flat,
// saturated active days pop toward the viewer.
function activity([r, g, b]) {
  return (Math.max(r, g, b) - Math.min(r, g, b)) / 255;
}

// Rotate an [r,g,b]'s hue by `deg` degrees, preserving saturation/lightness.
// deg 0 returns the colour untouched, and greys (no hue) are left as-is.
function shiftHue([r, g, b], deg) {
  if (!deg) return [r, g, b];
  const rn = r / 255, gn = g / 255, bn = b / 255;
  const max = Math.max(rn, gn, bn), min = Math.min(rn, gn, bn);
  const c = max - min;
  if (c === 0) return [r, g, b];
  const l = (max + min) / 2;
  const s = l > 0.5 ? c / (2 - max - min) : c / (max + min);
  let h;
  if (max === rn) h = (gn - bn) / c + (gn < bn ? 6 : 0);
  else if (max === gn) h = (bn - rn) / c + 2;
  else h = (rn - gn) / c + 4;
  h = (((h * 60 + deg) % 360) + 360) % 360 / 360;
  const q = l < 0.5 ? l * (1 + s) : l + s - l * s;
  const p = 2 * l - q;
  const chan = (t) => {
    if (t < 0) t += 1;
    if (t > 1) t -= 1;
    if (t < 1 / 6) return p + (q - p) * 6 * t;
    if (t < 1 / 2) return q;
    if (t < 2 / 3) return p + (q - p) * (2 / 3 - t) * 6;
    return p;
  };
  return [
    Math.round(chan(h + 1 / 3) * 255),
    Math.round(chan(h) * 255),
    Math.round(chan(h - 1 / 3) * 255),
  ];
}

// Snapshot dot geometry/colour off the live DOM and size the canvas to match.
function measure() {
  const gr = grid.getBoundingClientRect();
  cx = gr.width / 2;
  cy = gr.height / 2;

  ghosts = [];
  dots = Array.from(grid.children)
    .map((cell) => {
      const el = cell.firstElementChild;
      if (!el || el === canvas) return null;
      const cs = getComputedStyle(el);
      const rect = el.getBoundingClientRect();
      const x = rect.left - gr.left + rect.width / 2 - cx;
      const y = rect.top - gr.top + rect.height / 2 - cy;
      const rad = Math.min(rect.width, rect.height) / 2;
      const rgb = parseColor(cs.backgroundColor);
      if (!rgb) {
        // Empty day: keep it as a flat outline on the ground plane, matching
        // the real dot's border colour and thickness.
        const bc = parseColor(cs.borderTopColor);
        const bw = parseFloat(cs.borderTopWidth) || 0;
        ghosts.push({
          x, y, r: rad,
          color: bc ? `rgb(${bc[0]},${bc[1]},${bc[2]})` : GHOST_COLOR,
          ring: bw > 0 ? Math.min(bw / rad, 0.9) : GROUND_RING,
        });
        return null;
      }
      return { x, y, h: activity(rgb) * DEPTH, r: rad, rgb };
    })
    .filter(Boolean);

  dpr = window.devicePixelRatio || 1;
  canvas.width = Math.round(gr.width * dpr);
  canvas.height = Math.round(gr.height * dpr);
  canvas.style.width = gr.width + "px";
  canvas.style.height = gr.height + "px";
  ctx.setTransform(dpr, 0, 0, dpr, 0, 0);
}

// Rotate a plane point (x, y, z) into camera space, then perspective-project
// it to screen. Returns null if it lands behind the camera.
function project(x, y, z, sinX, cosX, sinY, cosY) {
  // rotateY then rotateX, matching CSS `rotateX(..) rotateY(..)`.
  const x1 = x * cosY + z * sinY;
  const z1 = -x * sinY + z * cosY;
  const y2 = y * cosX - z1 * sinX;
  const z2 = y * sinX + z1 * cosX;
  const denom = PERSPECTIVE - z2;
  if (denom <= 1) return null; // at or behind the camera
  const s = PERSPECTIVE / denom;
  return { sx: cx + x1 * s, sy: cy + y2 * s, s, z: z2 };
}

// Draw a dot's disc at height z as a filled ellipse — the true perspective
// projection of a circle lying flat in the grid plane, so it foreshortens with
// tilt instead of always facing the camera. `c` is the already-projected centre.
function drawCap(c, d, z, color, trig) {
  const u = project(d.x + d.r, d.y, z, trig[0], trig[1], trig[2], trig[3]);
  const v = project(d.x, d.y + d.r, z, trig[0], trig[1], trig[2], trig[3]);
  if (!u || !v) return;
  ctx.fillStyle = color;
  ctx.save();
  // Map the unit circle through the two projected radius vectors.
  ctx.transform(u.sx - c.sx, u.sy - c.sy, v.sx - c.sx, v.sy - c.sy, c.sx, c.sy);
  ctx.beginPath();
  ctx.arc(0, 0, 1, 0, Math.PI * 2);
  ctx.fill();
  ctx.restore();
}

// Draw a height-0 dot as a foreshortened ring lying flat on the ground plane.
// `c` is the already-projected centre.
function drawRing(c, x, y, r, color, ring, trig) {
  const u = project(x + r, y, 0, trig[0], trig[1], trig[2], trig[3]);
  const v = project(x, y + r, 0, trig[0], trig[1], trig[2], trig[3]);
  if (!u || !v) return;
  ctx.fillStyle = color;
  ctx.save();
  ctx.transform(u.sx - c.sx, u.sy - c.sy, v.sx - c.sx, v.sy - c.sy, c.sx, c.sy);
  ctx.beginPath();
  ctx.arc(0, 0, 1, 0, Math.PI * 2); // outer edge
  ctx.arc(0, 0, 1 - ring, 0, Math.PI * 2, true); // inner edge (hole)
  ctx.fill();
  ctx.restore();
}

// Project each pillar (base at z=0, top at z=h) and draw back-to-front.
function render() {
  if (!ctx) return;
  const rx = rotX * DEG;
  const ry = rotY * DEG;
  const trig = [Math.sin(rx), Math.cos(rx), Math.sin(ry), Math.cos(ry)];

  const drawn = [];
  for (let i = 0; i < dots.length; i++) {
    const d = dots[i];
    const b = project(d.x, d.y, 0, trig[0], trig[1], trig[2], trig[3]);
    const t = project(d.x, d.y, d.h, trig[0], trig[1], trig[2], trig[3]);
    if (!b || !t) continue;
    drawn.push({ d, b, t, z: b.z }); // sort by base depth: far rows drawn first
  }
  for (let i = 0; i < ghosts.length; i++) {
    const g = ghosts[i];
    const c = project(g.x, g.y, 0, trig[0], trig[1], trig[2], trig[3]);
    if (!c) continue;
    drawn.push({ g, c, z: c.z });
  }
  drawn.sort((a, b) => a.z - b.z);

  // Hue drifts toward yellow/blue with yaw; exactly zero (green) when centred.
  const hueDelta = (rotY / MAX_ANGLE) * HUE_SHIFT;

  ctx.clearRect(0, 0, cx * 2, cy * 2);
  for (let i = 0; i < drawn.length; i++) {
    if (drawn[i].g) {
      const { g, c } = drawn[i];
      drawRing(c, g.x, g.y, g.r, g.color, g.ring, trig);
      continue;
    }
    const { d, b, t } = drawn[i];
    const [cr, cg, cb] = shiftHue(d.rgb, hueDelta);
    const topColor = `rgb(${cr},${cg},${cb})`;
    const shaftColor = `rgb(${Math.round(cr * SHAFT_SHADE)},${Math.round(cg * SHAFT_SHADE)},${Math.round(cb * SHAFT_SHADE)})`;
    // Base cap first (farthest), in the pillar-body colour.
    drawCap(b, d, 0, shaftColor, trig);
    // Shaft: a tapered quad from the base circle to the top circle.
    const ax = t.sx - b.sx, ay = t.sy - b.sy;
    const len = Math.hypot(ax, ay) || 1;
    const nx = -ay / len, ny = ax / len; // screen-space perpendicular
    const rB = d.r * b.s, rT = d.r * t.s;
    ctx.fillStyle = shaftColor;
    ctx.beginPath();
    ctx.moveTo(b.sx + nx * rB, b.sy + ny * rB);
    ctx.lineTo(t.sx + nx * rT, t.sy + ny * rT);
    ctx.lineTo(t.sx - nx * rT, t.sy - ny * rT);
    ctx.lineTo(b.sx - nx * rB, b.sy - ny * rB);
    ctx.closePath();
    ctx.fill();
    // Top cap last (nearest), as the lit colour.
    drawCap(t, d, d.h, topColor, trig);
  }
}

// Damped spring back to the resting tilt, seeded with the drag's leftover
// velocity so it eases home with a little overshoot.
function recenter() {
  raf = 0;
  if (dragging) return;
  velX = (velX + (REST_TILT_X - rotX) * STIFFNESS) * DAMPING;
  velY = (velY + (REST_TILT_Y - rotY) * STIFFNESS) * DAMPING;
  rotX += velX;
  rotY += velY;
  render();
  const settled =
    Math.abs(velX) < 0.02 && Math.abs(velY) < 0.02 &&
    Math.abs(rotX - REST_TILT_X) < 0.05 && Math.abs(rotY - REST_TILT_Y) < 0.05;
  if (settled) {
    rotX = REST_TILT_X;
    rotY = REST_TILT_Y;
    render();
  } else {
    raf = requestAnimationFrame(recenter);
  }
}

function onDown(e) {
  dragging = true;
  velX = velY = 0;
  if (raf) cancelAnimationFrame(raf), (raf = 0);
  lastX = e.clientX;
  lastY = e.clientY;
  canvas.style.cursor = "grabbing";
  if (e.pointerId != null) canvas.setPointerCapture(e.pointerId);
  e.preventDefault();
}

function onMove(e) {
  if (!dragging) return;
  const dx = e.clientX - lastX;
  const dy = e.clientY - lastY;
  lastX = e.clientX;
  lastY = e.clientY;
  rotY = clampAngle(rotY + dx * DRAG_SENS);
  rotX = clampAngle(rotX - dy * DRAG_SENS);
  velX = -dy * DRAG_SENS; // remember last motion to fling on release
  velY = dx * DRAG_SENS;
  render();
}

function onUp(e) {
  if (!dragging) return;
  dragging = false;
  canvas.style.cursor = "grab";
  if (e && e.pointerId != null && canvas.hasPointerCapture(e.pointerId)) {
    canvas.releasePointerCapture(e.pointerId);
  }
  // Spring back to the resting tilt, carrying the leftover drag velocity.
  raf = requestAnimationFrame(recenter);
}

function mount() {
  const next = document.querySelector("[data-punchcard]");
  if (next === grid && grid) return; // already wired to this element
  if (raf) cancelAnimationFrame(raf), (raf = 0);
  grid = next;
  if (!grid) return;

  if (getComputedStyle(grid).position === "static") {
    grid.style.position = "relative";
  }

  if (!canvas || canvas.parentElement !== grid) {
    canvas = document.createElement("canvas");
    canvas.dataset.punchcardFx = "";
    Object.assign(canvas.style, {
      position: "absolute",
      left: "0",
      top: "0",
      touchAction: "none",
    });
    ctx = canvas.getContext("2d");
    grid.appendChild(canvas);
  }

  measure();

  // Hide the originals but keep them occupying space, so the grid keeps its
  // size and our canvas has the same footprint.
  for (const cell of grid.children) {
    if (cell !== canvas) cell.style.visibility = "hidden";
  }

  rotX = REST_TILT_X;
  rotY = REST_TILT_Y;
  velX = velY = 0;
  render();

  // Reduced motion: render the static tilted relief but wire no dragging.
  if (REDUCED.matches) {
    canvas.style.cursor = "";
    return;
  }
  canvas.style.cursor = "grab";
  canvas.addEventListener("pointerdown", onDown);
  canvas.addEventListener("pointermove", onMove);
  canvas.addEventListener("pointerup", onUp);
  canvas.addEventListener("pointercancel", onUp);
  canvas.addEventListener("lostpointercapture", onUp);
}

function remeasure() {
  if (!grid || !canvas) return;
  measure();
  render();
}

window.addEventListener("resize", remeasure);
document.addEventListener("htmx:load", mount);
mount();
