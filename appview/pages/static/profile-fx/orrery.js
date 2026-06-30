const P = [
  { r: 0.22, hue: 350, phase: 0.0 },
  { r: 0.37, hue: 25, phase: 1.4 },
  { r: 0.53, hue: 140, phase: 2.8 },
  { r: 0.7, hue: 205, phase: 4.1 },
  { r: 0.88, hue: 280, phase: 5.4 },
];

const H = [320, 50, 170, 230, 95, 285];

const G = 0.15;
const m = 0.0016;
const mn = 0.002;
const mx = 0.08;
const M = 0.03;
const e = 0.006;
const S = 0.92;
const s = 1.1;
const h = 1 / 120;
const X = 0.1;
const b = 0.08;
const k = 0.6;
const z = 0.25;
const d = 0.8;
const fd = 0.92;
const l = 0.12;
const F = 0.05;
const E = 0.12;
const Q = 1.8;
const R = 1.7;
const T = 1.4;
const K = 2.0;
const L = 0.16;
const V = 1.5;
const B = 12;
const W = [-2, -1, 0, 1, 2].flatMap((ox) => [-2, -1, 0, 1, 2].map((oy) => [ox, oy]));

const cn = 28;
const cw = 14;
const wq = "(min-width: 768px)";
const dq = "(prefers-color-scheme: dark)";
const rq = "(prefers-reduced-motion: reduce)";
const q = 64;
const hz = 60;

const c = (f) => Math.max(0, Math.min(1, f));
const r = (a, b) => Array.from({ length: b - a + 1 }, (f, i) => a + i);
const C = (x, y, f) => {
  const m = Math.hypot(x, y);
  return m > f ? { x: (x / m) * f, y: (y / m) * f } : { x, y };
};
const v = (r) => Math.sqrt((G * r * r) / Math.pow(r * r + e, 1.5));

const p = (b, tx, ty, gm) => {
  const dx = tx - b.x;
  const dy = ty - b.y;
  const r2 = dx * dx + dy * dy + e;
  const inv = gm / (r2 * Math.sqrt(r2));
  return [dx * inv, dy * inv];
};

const U = (b, all, k) =>
  all.reduce(
    (acc, o, j) => {
      if (j === k) return acc;
      const f = p(b, o.x, o.y, o.mass);
      return [acc[0] + f[0], acc[1] + f[1]];
    },
    [0, 0],
  );

const a = (b, i, all, n, u) => {
  const s = p(b, n.x, n.y, G);
  const m = u.mass > 0 ? p(b, u.x, u.y, u.mass) : [0, 0];
  const f = U(b, all, i);
  return [s[0] + m[0] + f[0], s[1] + m[1] + f[1]];
};

const A = (n, all) => {
  const g = U(n, all, -1);
  return [b * g[0] - k * n.x - z * n.vx, b * g[1] - k * n.y - z * n.vy];
};

const I = (sim, u, dt) => {
  const n = sim.sun;
  const o = sim.bodies.map((b, i) => {
    const f = a(b, i, sim.bodies, n, u);
    const vx = b.vx + f[0] * dt;
    const vy = b.vy + f[1] * dt;
    return { ...b, vx, vy, x: b.x + vx * dt, y: b.y + vy * dt };
  });
  const j = A(n, sim.bodies);
  const vx = n.vx + j[0] * dt;
  const vy = n.vy + j[1] * dt;
  return { bodies: o, sun: { x: n.x + vx * dt, y: n.y + vy * dt, vx, vy } };
};

const O = (sim, u, t) => (t >= h ? O(I(sim, u, h), u, t - h) : { sim, rem: t });

const y = (b, n) => {
  const f = Math.hypot(b.x - n.x, b.y - n.y);
  if (f >= E && Math.hypot(b.x, b.y) <= Q) return { keep: b, ate: 0 };
  return { keep: null, ate: f < E ? 1 : 0 };
};

const D = () =>
  P.map((p) => {
    const f = v(p.r);
    return {
      x: p.r * Math.cos(p.phase),
      y: p.r * Math.sin(p.phase),
      vx: -Math.sin(p.phase) * f,
      vy: Math.cos(p.phase) * f,
      hue: p.hue,
      mass: m,
      size: 1,
    };
  });

const w = (arr) => (arr.length <= B ? arr : arr.slice(arr.length - B));

const o = (N, j) => {
  const f = Array.from(N.children, (c) => c.firstElementChild).filter(Boolean);
  f.forEach((el) => {
    el.style.transition = "none";
    el.style.transformOrigin = "center";
    el.style.willChange = "background-color, transform";
  });
  return { cells: f, cols: j, rows: Math.max(1, Math.ceil(f.length / j)) };
};

const x = (el) => {
  el.style.backgroundColor = "";
  el.style.transform = "";
  el.style.border = "";
};

const g = (N) => {
  const ac = new AbortController();
  const op = { signal: ac.signal };
  const mq = matchMedia(wq);
  const dm = matchMedia(dq);
  const rd = matchMedia(rq).matches;
  const cl = () => (mq.matches ? cw : cn);

  N.style.userSelect = "none";
  N.style.WebkitUserSelect = "none";
  N.style.cursor = "crosshair";

  let v = o(N, cl());
  let dk = dm.matches;
  let bs = dk ? "rgb(55 65 81)" : "rgb(229 231 235)";
  let dl = dk ? 60 : 45;

  const al = (i) => ({ energy: new Float32Array(i), hue: new Float32Array(i) });
  let f = al(v.cells.length);
  let G = al(v.cells.length);
  let dr = new Float32Array(v.cells.length).fill(-1);
  let dh = new Float32Array(v.cells.length);

  let sim = { bodies: D(), sun: { x: 0, y: 0, vx: 0, vy: 0 } };
  let u = { x: 0, y: 0, over: false, mass: 0 };
  let pd = null;
  let vel = { x: 0, y: 0 };
  let lm = 0;
  let cu = 0;
  let hp = 0;
  let fl = 0;
  let acc = 0;
  let lt = 0;
  let raf = 0;
  let vs = true;

  const rl = () => {
    v = o(N, cl());
    v.cells.forEach(x);
    f = al(v.cells.length);
    G = al(v.cells.length);
    dr = new Float32Array(v.cells.length).fill(-1);
    dh = new Float32Array(v.cells.length);
  };

  const os = () => {
    dk = dm.matches;
    bs = dk ? "rgb(55 65 81)" : "rgb(229 231 235)";
    dl = dk ? 60 : 45;
    dr.fill(-1);
  };

  mq.addEventListener("change", rl, op);
  dm.addEventListener("change", os, op);

  const ts = (a, e) => {
    const cx = (v.cols - 1) / 2;
    const cy = (v.rows - 1) / 2;
    if (cx === 0 || cy === 0) return { x: 0, y: 0 };
    const rc = N.getBoundingClientRect();
    const p = ((a - rc.left) / rc.width) * v.cols - 0.5;
    const q = ((e - rc.top) / rc.height) * v.rows - 0.5;
    return { x: (p - cx) / (S * cx), y: (q - cy) / (S * cy) };
  };

  const mv = (E) => {
    const tt = E.timeStamp / 1000;
    const s = ts(E.clientX, E.clientY);
    const dtm = tt - lm;
    if (dtm > 0 && dtm < 0.1) {
      vel = {
        x: vel.x * 0.5 + ((s.x - u.x) / dtm) * 0.5,
        y: vel.y * 0.5 + ((s.y - u.y) / dtm) * 0.5,
      };
    }
    lm = tt;
    u = { ...u, x: s.x, y: s.y, over: true };
  };

  const dn = (E) => {
    if (E.pointerType === "touch") return;
    E.preventDefault();
    N.setPointerCapture(E.pointerId);
    mv(E);
    vel = { x: 0, y: 0 };
    pd = { start: E.timeStamp / 1000, hue: H[hp % H.length] };
    hp += 1;
  };

  const up = (E) => {
    if (!pd) return;
    if (N.hasPointerCapture(E.pointerId)) N.releasePointerCapture(E.pointerId);
    const tt = E.timeStamp / 1000;
    const c2 = c((tt - pd.start) / T);
    const fs = tt - lm < 0.09;
    const lc = C(fs ? vel.x * L : 0, fs ? vel.y * L : 0, V);
    sim = {
      ...sim,
      bodies: w([
        ...sim.bodies,
        {
          x: u.x,
          y: u.y,
          vx: lc.x,
          vy: lc.y,
          hue: pd.hue,
          mass: mn + c2 * (mx - mn),
          size: 0.8 + c2 * 2.2,
        },
      ]),
    };
    cu = tt + K;
    pd = null;
  };

  N.addEventListener("pointermove", mv, { passive: true, signal: ac.signal });
  N.addEventListener("pointerdown", dn, op);
  N.addEventListener("pointerup", up, op);
  N.addEventListener(
    "pointerleave",
    () => {
      if (!pd) u = { ...u, over: false };
    },
    op,
  );

  const add = (buf, x, y, hue, e) => {
    if (x < 0 || x >= v.cols || y < 0 || y >= v.rows) return;
    const i = y * v.cols + x;
    if (i >= 0 && i < buf.energy.length && e > buf.energy[i]) {
      buf.energy[i] = e;
      buf.hue[i] = hue;
    }
  };

  const bl = (buf, fx, fy, hue, e, rad) => {
    const bx = Math.round(fx);
    const by = Math.round(fy);
    const sp = r(-Math.ceil(rad), Math.ceil(rad));
    sp.forEach((ox) =>
      sp.forEach((oy) => {
        const px = bx + ox;
        const py = by + oy;
        const dt = Math.hypot(px - fx, py - fy);
        if (dt > rad) return;
        add(buf, px, py, hue, e * (1 - dt / rad));
      }),
    );
  };

  const fm = (ms) => {
    const t = ms / 1000;
    const re = lt ? Math.min(X, (ms - lt) / 1000) : 0;
    lt = ms;
    const k = re * hz;
    const dK = Math.pow(d, k);
    const fK = Math.pow(fd, k);
    const uK = 1 - Math.pow(1 - l, k);

    const tg = u.over && t >= cu ? M : 0;
    u = { ...u, mass: u.mass + (tg - u.mass) * uK };

    acc += re * s;
    const ot = O(sim, u, acc);
    acc = ot.rem;
    const rs = ot.sim.bodies.map((b) => y(b, ot.sim.sun));
    sim = { bodies: rs.map((j) => j.keep).filter(Boolean), sun: ot.sim.sun };
    const ea = rs.reduce((a, j) => a + j.ate, 0);
    if (ea > 0) fl = Math.min(1.6, fl + ea * 0.9);
    fl *= fK;

    const cx = (v.cols - 1) / 2;
    const cy = (v.rows - 1) / 2;
    const n = sim.sun;

    f.energy.forEach((j, i) => {
      G.energy[i] = j * dK;
      G.hue[i] = f.hue[i];
    });

    const br = 0.88 + 0.12 * Math.sin(t) + fl;
    const Fx = cx + n.x * S * cx;
    const Fy = cy + n.y * S * cy;
    const bX = Math.round(Fx);
    const bY = Math.round(Fy);
    W.forEach(([ox, oy]) => {
      const px = bX + ox;
      const py = bY + oy;
      const dt = Math.hypot(px - Fx, py - Fy);
      if (dt > R) return;
      add(G, px, py, 45, br * (0.45 + 0.55 * (1 - dt / R)));
    });

    sim.bodies.forEach((b) => {
      const dt = Math.hypot(b.x - n.x, b.y - n.y);
      const ht = c((0.45 - dt) / 0.45);
      bl(G, cx + b.x * S * cx, cy + b.y * S * cy, b.hue, 0.85 + 0.15 * ht, 0.4 + b.size * 0.5);
    });

    if (pd) {
      const c2 = c((t - pd.start) / T);
      const sz = 0.8 + c2 * 2.2;
      bl(G, cx + u.x * S * cx, cy + u.y * S * cy, pd.hue, 0.55 + 0.45 * c2, 0.4 + sz * 0.5);
    } else if (u.over) {
      add(G, Math.round(cx + u.x * S * cx), Math.round(cy + u.y * S * cy), 200, 0.5);
    }

    v.cells.forEach((el, i) => {
      const e = G.energy[i];
      if (e >= F) {
        const Q = Math.round(Math.min(1, e) * q);
        const hue = G.hue[i];
        if (Q !== dr[i] || hue !== dh[i]) {
          const qe = Q / q;
          const mi = Math.min(100, qe * 170).toFixed(1);
          el.style.backgroundColor = `color-mix(in srgb, hsl(${hue} 95% ${dl}%) ${mi}%, ${bs})`;
          el.style.transform = `scale(${(1 + qe * 0.8).toFixed(3)})`;
          el.style.border = "0";
          dr[i] = Q;
          dh[i] = hue;
        }
      } else if (dr[i] !== -1) {
        x(el);
        dr[i] = -1;
      }
    });

    const tp = f;
    f = G;
    G = tp;

    if (!rd && vs) raf = requestAnimationFrame(fm);
  };

  const io = new IntersectionObserver((en) => {
    const vi = en.some((E) => E.isIntersecting);
    if (vi === vs) return;
    vs = vi;
    if (vs) {
      lt = 0;
      raf = requestAnimationFrame(fm);
    } else {
      cancelAnimationFrame(raf);
    }
  });
  io.observe(N);

  raf = requestAnimationFrame(fm);

  return () => {
    cancelAnimationFrame(raf);
    io.disconnect();
    ac.abort();
  };
};

let Y = null;
let Z = null;
const J = () => {
  const t = document.querySelector("[data-punchcard]");
  if (t === Z) return;
  if (Y) Y();
  Z = t;
  Y = t ? g(t) : null;
};
J();
document.addEventListener("htmx:load", J);
