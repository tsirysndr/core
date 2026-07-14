// Super Mario Bros. World 1-1, rendered onto the punchcard.
//
// The punchcard is a grid of small dots (one per day of the year). We treat it
// as a low-res dot-matrix display and side-scroll World 1-1 across it: ground,
// bricks, ? blocks, pipes, Goombas, hills, bushes, clouds, a flagpole and a
// castle, with Mario auto-running and hopping over the obstacles. It loops.
//
// Textures are base64: every sprite is an array of strings, and each character
// is one pixel whose value is its index in the base64 alphabet (A=0, B=1, ...).
// That index selects an entry from PAL below. Index 0 (`A`) is transparent.

const B64 = "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

// Palette. Index lines up with the base64 alphabet so a texture char maps
// straight to a colour: A->sky, D->ground, M->mario-red, and so on.
const PAL = [
  null,       //  0 A  transparent
  "#5c94fc",  //  1 B  sky
  "#ffffff",  //  2 C  white (clouds, flag)
  "#c84c0c",  //  3 D  ground
  "#8a3b08",  //  4 E  ground / seam dark
  "#e39d5b",  //  5 F  ground top light / block edge
  "#b8560f",  //  6 G  brick
  "#fac000",  //  7 H  ? block yellow
  "#7a3b08",  //  8 I  shadow / mortar
  "#00a800",  //  9 J  pipe green
  "#5fdd5f",  // 10 K  pipe light
  "#006000",  // 11 L  pipe dark
  "#e21b0c",  // 12 M  mario red
  "#ffa060",  // 13 N  mario skin
  "#2038ec",  // 14 O  mario overalls blue
  "#6a2a00",  // 15 P  brown (mario hair, goomba feet)
  "#b06a3c",  // 16 Q  goomba tan
  "#000000",  // 17 R  black
  "#3ca03c",  // 18 S  hill / bush green
  "#78d060",  // 19 T  hill light
  "#b0b0b0",  // 20 U  castle grey
  "#707070",  // 21 V  castle dark
];
const SKY = PAL[1];

// Decode a base64 texture (array of rows) into a {w,h,d} sprite of palette ids.
const spr = (rows) => ({
  w: rows[0].length,
  h: rows.length,
  d: rows.map((r) => Array.prototype.map.call(r, (c) => B64.indexOf(c))),
});

const HILL = spr(["AASAA", "ASTSA", "SSSSS"]);
const BUSH = spr(["ASSSA", "SSSSS"]);
const CLOUD = spr(["ACCCA", "CCCCC"]);

// Mario's three fixed top rows (hat, face, body); the fourth row is his legs,
// swapped per animation frame below.
const MTOP = ["AMM", "PNN", "MMM"];
const LEGS = ["OAO", "AOO", "OAO", "OOA"]; // running cycle
const LEGS_AIR = "OOA"; // tucked while jumping

// --- Level layout (one looping period, measured in dot-columns) --------------
const P = 72; // period width
const SCROLL = 5.5; // dot-columns per second

const CLOUDS = [
  { x: 8, f: 0.18 },
  { x: 26, f: 0.1 },
  { x: 42, f: 0.24 },
  { x: 62, f: 0.14 },
];
const HILLS = [3, 46];
const BUSHES = [14, 58];
const QSINGLE = [{ x: 10, u: 4 }]; // lone ? block
const HIGHQ = { x: 21, u: 8 }; // high ? block
const RUN = [
  // the classic brick / ? / brick / ? / brick row, 4 tiles up
  { x: 18, t: "b" },
  { x: 19, t: "q" },
  { x: 20, t: "b" },
  { x: 21, t: "q" },
  { x: 22, t: "b" },
];
const PIPES = [
  { x: 28, h: 2 },
  { x: 34, h: 3 },
  { x: 48, h: 4 },
  { x: 56, h: 2 },
];
const PITS = [[44, 45]]; // inclusive column ranges with no ground
const GOOMBAS = [24, 40, 52]; // spawn columns; they walk left
const STAIRS = [
  { x: 61, h: 1 },
  { x: 62, h: 2 },
  { x: 63, h: 3 },
  { x: 64, h: 4 },
];
const FLAGX = 68;
const CASTLEX = 70;

// Scheduled jumps (column ranges + peak height) that carry Mario over each
// obstacle. Between them he runs along the ground.
const JUMPS = [
  { a: 25, b: 31, p: 4 }, // pipe @28
  { a: 31.5, b: 38, p: 5 }, // pipe @34
  { a: 42.5, b: 52, p: 7 }, // pit @44-45 + pipe @48
  { a: 53.5, b: 59, p: 4 }, // pipe @56
  { a: 60, b: 65, p: 5 }, // staircase
];

const isPit = (local) => PITS.some(([a, b]) => local >= a && local <= b);
const jumpY = (local) => {
  let y = 0;
  for (const j of JUMPS) {
    if (local > j.a && local < j.b) {
      const u = (local - j.a) / (j.b - j.a);
      const h = 4 * j.p * u * (1 - u); // parabola, peak at the middle
      if (h > y) y = h;
    }
  }
  return y;
};

// --- Renderer ----------------------------------------------------------------
const run = (card) => {
  const dots = Array.from(card.children, (c) => c.firstElementChild).filter(Boolean);
  const count = dots.length;
  if (!count) return () => {};

  const cols = matchMedia("(min-width: 768px)").matches ? 14 : 28;
  const rows = Math.max(1, Math.ceil(count / cols));
  const W = cols;
  const G = Math.max(2, Math.round(rows * 0.16)); // ground thickness
  const horizon = rows - G; // first ground row; sky is above
  const MSC = Math.max(2, Math.round(W * 0.32)); // Mario's fixed screen column
  const reduce = matchMedia("(prefers-reduced-motion: reduce)").matches;

  // Fill the cells so the picture reads as a solid dot-matrix screen.
  for (const d of dots) {
    d.style.transition = "none";
    d.style.transform = "scale(2)";
    d.style.border = "none";
    d.style.willChange = "background-color";
  }

  const buf = new Uint8Array(W * rows);
  const last = new Array(count).fill(null);

  const setPx = (x, y, idx) => {
    if (idx > 0 && x >= 0 && x < W && y >= 0 && y < rows) buf[y * W + x] = idx;
  };
  const blit = (s, x, y) => {
    for (let ry = 0; ry < s.h; ry++) {
      const row = s.d[ry];
      for (let cx = 0; cx < s.w; cx++) setPx(x + cx, y + ry, row[cx]);
    }
  };
  const blitStr = (str, x, y) => {
    for (let k = 0; k < str.length; k++) setPx(x + k, y, B64.indexOf(str[k]));
  };

  // Draw one instance of a level feature per visible period.
  let camCol = 0;
  const eachBase = (cb) => {
    const start = Math.floor((camCol - 8) / P) * P;
    for (let b = start; b <= camCol + W + 8; b += P) cb(b);
  };
  const at = (lx, draw) =>
    eachBase((b) => {
      const sx = b + lx - camCol;
      if (sx > -8 && sx < W + 2) draw(Math.round(sx));
    });

  const drawPipe = (sx, h) => {
    const top = horizon - h;
    for (let y = top; y <= horizon - 1; y++) {
      setPx(sx, y, 10);
      setPx(sx + 1, y, 9);
      setPx(sx + 2, y, 11);
    }
    setPx(sx + 1, top, 10); // brighten the cap lip
  };
  const drawStep = (sx, h) => {
    for (let y = horizon - h; y <= horizon - 1; y++) setPx(sx, y, y === horizon - h ? 5 : 3);
  };
  const drawFlag = (sx) => {
    const top = horizon - 9;
    for (let y = top; y <= horizon - 1; y++) setPx(sx, y, 20);
    setPx(sx, top - 1, 2); // ball
    setPx(sx - 1, top, 18);
    setPx(sx - 2, top + 1, 18);
    setPx(sx - 1, top + 1, 18);
    setPx(sx - 1, top + 2, 18);
  };
  const drawCastle = (sx) => {
    const top = horizon - 4;
    for (let ry = 0; ry < 4; ry++) {
      for (let cx = 0; cx < 5; cx++) {
        if (ry === 0 && cx % 2 === 1) continue; // crenellations
        setPx(sx + cx, top + ry, 20);
      }
    }
    setPx(sx + 2, horizon - 1, 17); // door
    setPx(sx + 2, horizon - 2, 17);
    setPx(sx + 1, top + 1, 17); // windows
    setPx(sx + 3, top + 1, 17);
  };

  let marioY = 0; // set each frame, read by the Goomba stomp check
  const drawGoomba = (sx, t) => {
    if ((sx === MSC || sx === MSC + 1) && marioY <= 1) {
      blitStr("PPP", sx, horizon - 1); // squished flat
      return;
    }
    blitStr("QQQ", sx, horizon - 2);
    blitStr(Math.floor(t / 200) % 2 ? "APA" : "PAP", sx, horizon - 1); // waddle
  };

  const render = (t) => {
    const camX = (t / 1000) * SCROLL;
    camCol = Math.floor(camX);
    buf.fill(1); // sky

    for (const c of CLOUDS) at(c.x, (sx) => blit(CLOUD, sx, Math.round(horizon * c.f)));
    for (const hx of HILLS) at(hx, (sx) => blit(HILL, sx, horizon - HILL.h));
    for (const bx of BUSHES) at(bx, (sx) => blit(BUSH, sx, horizon - BUSH.h));

    // Ground, per screen column, with a pit here and there.
    for (let x = 0; x < W; x++) {
      const local = ((camCol + x) % P + P) % P;
      if (isPit(local)) continue;
      const seam = local % 4 === 0;
      setPx(x, horizon, seam ? 4 : 5);
      for (let y = horizon + 1; y < rows; y++) setPx(x, y, (local + y) % 4 === 0 ? 4 : 3);
    }

    const blink = Math.floor(t / 350) % 4 === 0;
    for (const p of PIPES) at(p.x, (sx) => drawPipe(sx, p.h));
    for (const q of QSINGLE) at(q.x, (sx) => setPx(sx, horizon - q.u, blink ? 5 : 7));
    at(HIGHQ.x, (sx) => setPx(sx, horizon - HIGHQ.u, blink ? 5 : 7));
    for (const r of RUN) at(r.x, (sx) => setPx(sx, horizon - 4, r.t === "q" ? (blink ? 5 : 7) : 6));
    for (const s of STAIRS) at(s.x, (sx) => drawStep(sx, s.h));
    at(FLAGX, drawFlag);
    at(CASTLEX, drawCastle);

    // Goombas walk left; a full period of travel loops seamlessly.
    const phase = ((t / 1000) * 3) % P;
    eachBase((b) => {
      for (const s of GOOMBAS) {
        const sx = Math.round(b + s - phase - camCol);
        if (sx > -3 && sx < W + 1) drawGoomba(sx, t);
      }
    });

    // Mario, pinned to screen column MSC, hopping the level as it scrolls by.
    const local = ((camCol + MSC) % P + P) % P;
    marioY = Math.min(horizon - 4, Math.round(jumpY(local)));
    const legs = marioY > 0 ? LEGS_AIR : LEGS[Math.floor(t / 90) % LEGS.length];
    const topY = horizon - 1 - marioY - 3;
    const body = [MTOP[0], MTOP[1], MTOP[2], legs];
    for (let r = 0; r < 4; r++) blitStr(body[r], MSC, topY + r);

    // Commit only the dots that changed.
    for (let i = 0; i < count; i++) {
      const hex = PAL[buf[(i / W | 0) * W + (i % W)]] || SKY;
      if (hex !== last[i]) {
        dots[i].style.backgroundColor = hex;
        last[i] = hex;
      }
    }
  };

  if (reduce) {
    render(2600); // one static frame, no animation
    return () => {};
  }

  let raf = 0;
  let running = false;
  const loop = (t) => {
    render(t);
    raf = requestAnimationFrame(loop);
  };
  const start = () => {
    if (running) return;
    running = true;
    raf = requestAnimationFrame(loop);
  };
  const stop = () => {
    running = false;
    cancelAnimationFrame(raf);
  };

  // Only animate while the punchcard is on screen.
  const io = new IntersectionObserver((es) => (es.some((e) => e.isIntersecting) ? start() : stop()));
  io.observe(card);
  start();

  return () => {
    stop();
    io.disconnect();
  };
};

// Boot, and re-bind across htmx navigations (matching the reference effect).
let target = null;
let teardown = null;
const boot = () => {
  const el = document.querySelector("[data-punchcard]");
  if (el === target) return;
  if (teardown) teardown();
  target = el;
  teardown = el ? run(el) : null;
};
boot();
document.addEventListener("htmx:load", boot);
