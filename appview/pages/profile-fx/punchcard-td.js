// Punchcard Tower Defense: the punchcard itself becomes the battlefield.
// Click "play" to overlay the game on your commit grid; the dots march the
// track and their punchcard color is their strength. Build towers on the dark
// tiles to stop them. Hit "exit" to return to the normal punchcard.

// Tangled swaps profiles in via htmx, so the module only executes once. Re-bind
// to whichever punchcard is on the page after each swap, tearing down the old
// instance first so nothing (rAF loop, window listener, injected DOM) leaks.
let tdTeardown = null;
let tdGrid = null;
function tdBoot() {
  const grid = document.querySelector("[data-punchcard]");
  if (grid === tdGrid) return;
  if (tdTeardown) tdTeardown();
  tdGrid = grid;
  tdTeardown = grid ? run(grid) : null;
}
tdBoot();
document.addEventListener("htmx:load", tdBoot);

function run(grid) {
  grid.dataset.tdOn = "1";
  const parent = grid.parentElement || document.body;
  const ac = new AbortController();
  const sig = ac.signal;

  // ---- read commit data off the real punchcard dots ----
  const parseCount = (dot) => {
    const m = dot && dot.title && dot.title.match(/(\d+)\s*commits?/i);
    return m ? parseInt(m[1], 10) : 0;
  };
  const roster = [];
  Array.from(grid.children).forEach((cell) => {
    const dot = cell.firstElementChild;
    const count = parseCount(dot);
    if (count > 0) {
      const month = parseInt(dot.title.slice(5, 7), 10) - 1;
      roster.push({ dot, count, month, color: getComputedStyle(dot).backgroundColor });
    }
  });
  if (!roster.length) return;

  // ---- difficulty tunables (cranked up) ----
  const START_GOLD = 120, SPAWN_GAP = 0.42, WAVE_GAP = 1.2;
  const RAD = 6;                                             // every enemy is the same size; color shows strength
  const speedFor = (c) => Math.max(0.9, 1.9 - c * 0.05);     // base tiles / second (brighter = slower)
  const waveSpeed = (w) => 1 + 0.11 * w;                     // ...and everything speeds up each wave
  const rewardFor = (c) => 1 + c;
  const hpBase = (c) => 12 + c * 9;                          // health scales with commits (color)
  const hpFor = (c, w) => Math.round(hpBase(c) * (1 + 0.14 * w)); // ...and ramps up hard each wave
  // peon & rapid deal damage; frost deals none but chills (slows) so your killers get more shots in
  const TOWERS = {
    peon:  { cost: 50, range: 2.0, cd: 0.35, dmg: 9, body: "#38bdf8", proj: "#7dd3fc", slow: 0,   slowT: 0 },
    frost: { cost: 70, range: 2.4, cd: 0.50, dmg: 0, body: "#67e8f9", proj: "#a5f3fc", slow: 0.4, slowT: 1.4 },
    rapid: { cost: 90, range: 1.7, cd: 0.16, dmg: 4, body: "#a78bfa", proj: "#c4b5fd", slow: 0,   slowT: 0 },
  };
  const MONTHS = ["Jan","Feb","Mar","Apr","May","Jun","Jul","Aug","Sep","Oct","Nov","Dec"];

  // ---- waves (grouped by month) ----
  const greenFor = (c) => (c >= 8 ? "#22c55e" : c >= 4 ? "#4ade80" : c >= 2 ? "#86efac" : "#bbf7d0");
  const groupWaves = (list) => {
    const bm = Array.from({ length: 12 }, () => []);
    list.forEach((e) => bm[e.month].push(e));
    return bm.map((l, m) => ({ m, list: l })).filter((w) => w.list.length);
  };
  let waves = groupWaves(roster);
  let totalEnemies = roster.length;
  let boss = false;

  // The "final boss": a nod to Lewis (oyster.cafe) and his absurd commit count. We don't
  // fetch anything, just conjure a swarm sized off the number, ~1 dot per 100 commits.
  const BOSS_NAME = "Lewis";
  const BOSS_COMMITS = 25241;
  const bossRoster = () => {
    const n = Math.round(BOSS_COMMITS / 100); // ~252 dots, each worth ~100 commits
    const list = [];
    for (let i = 0; i < n; i++) {
      const count = 8 + ((i * 7) % 8); // all maxed-out (brightest); every day is a grind for Lewis
      list.push({ count, month: i % 12, color: greenFor(count) });
    }
    return list;
  };

  // ---- board geometry (recomputed on enter, sized to the punchcard) ----
  let COLS, ROWS, TILE, W, H, pts, segLen, cum, pathLen, blocked, buildable;
  const keyOf = (c, r) => c + "," + r;

  function serpentine(cols, rows) {
    const lanes = [];
    for (let r = 0; r < rows; r += 2) lanes.push(r);
    const wp = [[-1, lanes[0]]];
    for (let i = 0; i < lanes.length; i++) {
      const ltr = i % 2 === 0, a = ltr ? 0 : cols - 1, b = ltr ? cols - 1 : 0;
      wp.push([a, lanes[i]], [b, lanes[i]]);
      if (i < lanes.length - 1) wp.push([b, lanes[i + 1]]);
    }
    const li = lanes.length - 1;
    wp.push([li % 2 === 0 ? cols : -1, lanes[li]]);
    return wp;
  }

  function layout() {
    const cw = Math.max(160, grid.clientWidth);
    const ch = Math.max(160, grid.clientHeight);
    COLS = 8;
    TILE = cw / COLS;
    ROWS = Math.max(7, Math.min(20, Math.floor(ch / TILE)));
    W = cw; H = ch; // exactly cover the punchcard, never taller than it
    const wp = serpentine(COLS, ROWS);
    pts = wp.map(([c, r]) => ({ x: c * TILE + TILE / 2, y: r * TILE + TILE / 2 }));
    segLen = []; cum = [0];
    for (let i = 0; i < pts.length - 1; i++) {
      const L = Math.hypot(pts[i + 1].x - pts[i].x, pts[i + 1].y - pts[i].y);
      segLen.push(L); cum.push(cum[i] + L);
    }
    pathLen = cum[cum.length - 1];
    blocked = new Set();
    for (let i = 0; i < wp.length - 1; i++) {
      let [c, r] = wp[i]; const [c1, r1] = wp[i + 1];
      const dc = Math.sign(c1 - c), dr = Math.sign(r1 - r);
      blocked.add(keyOf(c, r));
      while (c !== c1 || r !== r1) { c += dc; r += dr; blocked.add(keyOf(c, r)); }
    }
    buildable = [];
    for (let r = 0; r < ROWS; r++)
      for (let c = 0; c < COLS; c++)
        if (!blocked.has(keyOf(c, r))) buildable.push([c, r]);
  }

  const pointAt = (d) => {
    if (d <= 0) return { x: pts[0].x, y: pts[0].y };
    if (d >= pathLen) return { x: pts[pts.length - 1].x, y: pts[pts.length - 1].y };
    let i = 0;
    while (i < segLen.length && cum[i + 1] < d) i++;
    const t = (d - cum[i]) / segLen[i];
    return { x: pts[i].x + (pts[i + 1].x - pts[i].x) * t, y: pts[i].y + (pts[i + 1].y - pts[i].y) * t };
  };

  // ---- DOM ----
  const el = (tag, style, text) => {
    const n = document.createElement(tag);
    if (style) n.style.cssText = style;
    if (text != null) n.textContent = text;
    return n;
  };
  const btn = (label) =>
    el("button", "font-family:ui-monospace,Menlo,monospace;font-size:11px;padding:5px 8px;border-radius:6px;border:1px solid rgba(120,120,135,0.4);background:rgba(2,6,23,0.6);color:#cbd5e1;cursor:pointer;", label);

  grid.style.position = "relative";

  const canvas = el("canvas", "position:absolute;left:0;top:0;display:none;border-radius:6px;z-index:5;touch-action:none;cursor:crosshair;");
  const hudBar = el("div", "position:absolute;left:0;right:0;top:0;display:none;padding:3px 54px 3px 5px;white-space:nowrap;overflow:hidden;font-family:ui-monospace,Menlo,monospace;font-size:10px;color:#e2e8f0;background:linear-gradient(#0b1220ee,#0b122000);border-radius:6px 6px 0 0;z-index:6;pointer-events:none;");
  const overlayMsg = el("div", "position:absolute;left:0;top:0;display:none;flex-direction:column;align-items:center;justify-content:center;gap:9px;text-align:center;padding:14px;border-radius:6px;background:rgba(2,6,23,0.86);color:#e2e8f0;z-index:7;");
  grid.append(canvas, hudBar, overlayMsg);
  // always-on-board fast-forward toggle (top-right) so speed is reachable during any wave
  const ffBtn = el("button", "position:absolute;right:4px;top:3px;z-index:6;display:none;font-family:ui-monospace,Menlo,monospace;font-size:10px;padding:2px 6px;border-radius:6px;border:1px solid rgba(120,120,135,0.55);background:rgba(2,6,23,0.85);color:#e2e8f0;cursor:pointer;", "⏩ 1×");
  grid.appendChild(ffBtn);
  const ctx = canvas.getContext("2d");

  // controls live below the punchcard; only the board overlays it
  const ui = el("div", "font-family:ui-monospace,Menlo,monospace;margin-top:10px;");
  const playBtn = btn("▶ play tower defense");
  playBtn.style.borderColor = "#22c55e"; playBtn.style.color = "#4ade80";
  ui.appendChild(playBtn);

  const panel = el("div", "display:none;flex-direction:column;gap:6px;");
  const shop = el("div", "display:flex;gap:5px;flex-wrap:wrap;");
  const shopBtns = [];
  [["peon", "Peon"], ["frost", "Frost"], ["rapid", "Rapid"]].forEach(([t, label]) => {
    const b = btn(label + " " + TOWERS[t].cost);
    b.dataset.t = t;
    shop.appendChild(b); shopBtns.push(b);
  });
  const controls = el("div", "display:flex;gap:5px;flex-wrap:wrap;");
  const pauseBtn = btn("⏸ pause");
  const resetBtn = btn("↺ reset");
  const exitBtn = btn("✕ exit");
  controls.append(pauseBtn, resetBtn, exitBtn);
  const tip = el("div", "font-size:9px;color:#64748b;", "build on dark tiles · peon & rapid damage, frost slows · ⏩ or press F to fast-forward");
  panel.append(shop, controls, tip);
  ui.appendChild(panel);
  parent.insertBefore(ui, grid.nextSibling);

  // ---- state ----
  let active = false, gold, waveNum, enemies, towers, shots,
      spawnQueue, spawnTimer, betweenTimer, state, paused, speed, selected, hover, killed, escaped;

  function reset() {
    boss = false;
    waves = groupWaves(roster); totalEnemies = roster.length;
    gold = START_GOLD; waveNum = 0;
    enemies = []; towers = []; shots = []; spawnQueue = []; spawnTimer = 0; betweenTimer = null;
    state = "idle"; paused = false; speed = 1; selected = "peon"; hover = null;
    killed = 0; escaped = 0;
    roster.forEach((e) => { e.dot.style.opacity = ""; });
    showOverlay("start"); sync();
  }

  function enter() {
    layout();
    const dpr = Math.min(2, window.devicePixelRatio || 1);
    canvas.width = Math.round(W * dpr); canvas.height = Math.round(H * dpr);
    canvas.style.width = W + "px"; canvas.style.height = H + "px";
    ctx.setTransform(dpr, 0, 0, dpr, 0, 0);
    overlayMsg.style.width = W + "px"; overlayMsg.style.height = H + "px";
    canvas.style.display = "block"; hudBar.style.display = "block"; panel.style.display = "flex";
    ffBtn.style.display = "block"; playBtn.style.display = "none";
    active = true;
    reset();
  }
  function exit() {
    active = false;
    canvas.style.display = "none"; hudBar.style.display = "none"; overlayMsg.style.display = "none";
    ffBtn.style.display = "none"; panel.style.display = "none"; playBtn.style.display = "inline-block";
    roster.forEach((e) => { e.dot.style.opacity = ""; });
  }

  function dim(src, leaked) {
    // keep the punchcard color & size; just fade to show it's been dealt with
    if (src.dot) src.dot.style.opacity = leaked ? "0.12" : "0.32"; // boss dots have no home cell
  }
  function makeEnemy(src) {
    // boss dots move at a flat brisk clip (they're all maxed out, so speedFor would crawl)
    const spd = (boss ? 1.7 : speedFor(src.count)) * waveSpeed(waveNum);
    const hp = boss ? 24 : hpFor(src.count, waveNum);
    return { src, dist: 0, x: pts[0].x, y: pts[0].y, count: src.count,
      spd, hp, maxHp: hp, reward: boss ? 2 : rewardFor(src.count), color: src.color, rad: RAD,
      slowF: 1, slowT: 0, dead: false, leaked: false };
  }
  function beginWave() {
    if (state !== "idle") return;
    spawnQueue = waves[0].list.slice();
    spawnTimer = 0.15; betweenTimer = null;
    state = "running"; paused = false;
    hideOverlay(); sync();
  }
  function startBoss() {
    boss = true;
    const list = bossRoster();
    waves = groupWaves(list); totalEnemies = list.length;
    waveNum = 0; enemies = []; shots = []; betweenTimer = null; killed = 0; escaped = 0;
    spawnQueue = waves[0].list.slice(); spawnTimer = 0.2;
    gold += 200; // reinforcements for the final stand (your towers carry over)
    state = "running"; paused = false;
    hideOverlay(); sync();
  }

  function updateShop() {
    shopBtns.forEach((b) => {
      const spec = TOWERS[b.dataset.t], is = b.dataset.t === selected;
      b.style.borderColor = is ? "#22c55e" : "rgba(120,120,135,0.4)";
      b.style.color = is ? "#4ade80" : gold < spec.cost ? "#64748b" : "#cbd5e1";
      b.style.boxShadow = is ? "inset 0 0 0 1px #22c55e" : "none";
      b.style.opacity = gold < spec.cost ? "0.6" : "1";
    });
  }
  function sync() {
    const shown = state === "idle" ? 0 : state === "won" ? waves.length : Math.min(waveNum + 1, waves.length);
    let line = "💰" + gold + "  🌊" + shown + "/" + waves.length +
      " " + (boss ? "BOSS" : MONTHS[waves[Math.min(waveNum, waves.length - 1)].m]) +
      " &nbsp;<span style=\"color:#4ade80\">●</span>" + enemies.length +
      " &nbsp;(" + Math.max(0, totalEnemies - killed - escaped) + " left)";
    if (state === "running" && betweenTimer !== null) line += " &nbsp;⏳next " + Math.ceil(betweenTimer) + "s";
    hudBar.innerHTML = line;
    pauseBtn.disabled = state !== "running";
    pauseBtn.style.opacity = pauseBtn.disabled ? "0.4" : "1";
    pauseBtn.textContent = paused ? "▶ resume" : "⏸ pause";
    ffBtn.textContent = "⏩ " + speed + "×";
    updateShop();
  }
  function showOverlay(kind) {
    overlayMsg.style.display = "flex"; overlayMsg.innerHTML = "";
    const h = el("div", "font-size:15px;font-weight:600;");
    const p = el("div", "font-size:10px;line-height:1.5;color:#cbd5e1;max-width:195px;");
    const row = el("div", "display:flex;gap:6px;flex-wrap:wrap;justify-content:center;");
    const add = (label, fn, accent) => {
      const b = btn(label);
      if (accent) { b.style.borderColor = "#22c55e"; b.style.color = "#4ade80"; }
      b.onclick = fn; row.appendChild(b);
    };
    if (kind === "start") {
      h.textContent = "Tower defense";
      p.innerHTML = "<b>" + totalEnemies + "</b> commit-days march the track over <b>" + waves.length +
        "</b> waves, each faster than the last. Brighter dots pack more commits and take more hits. Let a single one reach the exit and it's over.";
      add("⚔ line them up", beginWave, true);
    } else if (kind === "won") {
      h.textContent = "You won! 🎉"; h.style.color = "#4ade80";
      p.innerHTML = "Are you ready to fight " + BOSS_NAME + ", the final boss?";
      add("I think so. Wait.. more than 25k commits..?", startBoss, true);
      add("No, but throw it at me anyway", startBoss);
    } else if (kind === "bossWon") {
      h.textContent = "Final boss down"; h.style.color = "#facc15";
      p.innerHTML = "You held the line against " + BOSS_NAME + "'s onslaught. <b>" + BOSS_COMMITS.toLocaleString("en-US") +
        "</b> commits and not one got through.";
      add("↺ play again", reset, true);
    } else {
      h.textContent = "One slipped through 💥"; h.style.color = "#f87171";
      p.innerHTML = "That's all it takes. You crushed <b>" + killed + "</b> before a commit reached the exit.";
      add("↺ try again", reset, true);
    }
    overlayMsg.append(h, p, row);
  }
  function hideOverlay() { overlayMsg.style.display = "none"; overlayMsg.innerHTML = ""; }

  // ---- simulation ----
  function step(dt) {
    const s = dt * speed;
    if (spawnQueue.length) {
      const gap = boss ? 0.18 : Math.max(0.22, SPAWN_GAP - waveNum * 0.025); // waves flood denser as they go
      spawnTimer -= s;
      while (spawnTimer <= 0 && spawnQueue.length) { enemies.push(makeEnemy(spawnQueue.shift())); spawnTimer += gap; }
    }
    for (const e of enemies) {
      let m = 1;
      if (e.slowT > 0) { m = e.slowF; e.slowT -= s; }
      e.dist += e.spd * TILE * m * s;
      if (e.dist >= pathLen) e.leaked = true;
      else { const q = pointAt(e.dist); e.x = q.x; e.y = q.y; }
    }
    const gotThrough = enemies.find((e) => e.leaked);
    if (gotThrough) { escaped++; dim(gotThrough.src, true); state = "lost"; showOverlay("lost"); sync(); return; }
    for (const t of towers) {
      t.cd -= s;
      if (t.cd > 0) continue;
      const spec = TOWERS[t.type], range = spec.range * TILE;
      let best = null, bd = -1, rr = range * range;
      for (const e of enemies) {
        if (e.dead) continue;
        if (spec.slow && e.slowT > 0) continue; // frost won't waste a shot re-chilling
        const dx = e.x - t.x, dy = e.y - t.y;
        if (dx * dx + dy * dy <= rr && e.dist > bd) { best = e; bd = e.dist; }
      }
      if (best) { shots.push({ x: t.x, y: t.y, target: best, color: spec.proj, dmg: spec.dmg, slow: spec.slow, slowT: spec.slowT, v: 340 }); t.cd = spec.cd; }
      else t.cd = 0;
    }
    for (const p of shots) {
      const e = p.target;
      if (!e || e.dead) { p.done = true; continue; }
      const dx = e.x - p.x, dy = e.y - p.y, d = Math.hypot(dx, dy) || 0.001, adv = p.v * s;
      if (d <= adv + e.rad) {
        if (p.slow > 0) { e.slowF = p.slow; e.slowT = Math.max(e.slowT, p.slowT); } // frost chills
        if (p.dmg > 0) {
          e.hp -= p.dmg;
          if (e.hp <= 0 && !e.dead) { e.dead = true; gold += e.reward; killed++; dim(e.src, false); }
        }
        p.done = true;
      } else { p.x += dx / d * adv; p.y += dy / d * adv; }
    }
    enemies = enemies.filter((e) => !e.dead);
    shots = shots.filter((p) => !p.done);
    if (state === "running" && !spawnQueue.length && !enemies.length) {
      if (betweenTimer === null) {
        // wave cleared, bank a small bonus, then auto-launch the next after a short breather
        if (waveNum + 1 >= waves.length) { state = "won"; showOverlay(boss ? "bossWon" : "won"); }
        else { betweenTimer = WAVE_GAP; gold += 8 + (waveNum + 1) * 3; }
      } else {
        betweenTimer -= s;
        if (betweenTimer <= 0) {
          betweenTimer = null;
          waveNum++;
          spawnQueue = waves[waveNum].list.slice();
          spawnTimer = 0.1;
        }
      }
    }
    sync();
  }

  // ---- render ----
  function road() {
    ctx.beginPath(); ctx.moveTo(pts[0].x, pts[0].y);
    for (let i = 1; i < pts.length; i++) ctx.lineTo(pts[i].x, pts[i].y);
    ctx.stroke();
  }
  function draw() {
    ctx.clearRect(0, 0, W, H);
    ctx.fillStyle = "rgba(2,6,23,0.62)"; ctx.fillRect(0, 0, W, H); // scrim: mutes the real dots behind
    ctx.lineJoin = "round"; ctx.lineCap = "round";
    ctx.strokeStyle = "rgba(30,41,59,0.92)"; ctx.lineWidth = TILE * 0.66; road();
    ctx.strokeStyle = "rgba(148,163,184,0.5)"; ctx.lineWidth = 2; ctx.setLineDash([4, 7]); road(); ctx.setLineDash([]);
    ctx.fillStyle = "rgba(148,163,184,0.16)";
    for (const [c, r] of buildable) {
      if (towers.some((t) => t.tc === c && t.tr === r)) continue;
      ctx.beginPath(); ctx.arc(c * TILE + TILE / 2, r * TILE + TILE / 2, 1.4, 0, 7); ctx.fill();
    }
    if (hover && state !== "won" && state !== "lost") {
      const { c, r } = hover;
      if (c >= 0 && c < COLS && r >= 0 && r < ROWS) {
        const occ = blocked.has(keyOf(c, r)) || towers.some((t) => t.tc === c && t.tr === r);
        const spec = TOWERS[selected], ok = !occ && gold >= spec.cost;
        const cx = c * TILE + TILE / 2, cy = r * TILE + TILE / 2;
        ctx.beginPath(); ctx.arc(cx, cy, spec.range * TILE, 0, 7);
        ctx.fillStyle = ok ? "rgba(74,222,128,0.10)" : "rgba(248,113,113,0.10)";
        ctx.strokeStyle = ok ? "rgba(74,222,128,0.5)" : "rgba(248,113,113,0.5)"; ctx.lineWidth = 1.5;
        ctx.fill(); ctx.stroke();
        ctx.fillStyle = ok ? "rgba(74,222,128,0.4)" : "rgba(248,113,113,0.4)";
        ctx.fillRect(c * TILE + 4, r * TILE + 4, TILE - 8, TILE - 8);
      }
    }
    for (const t of towers) {
      const spec = TOWERS[t.type];
      ctx.fillStyle = "rgba(15,23,42,0.95)"; ctx.fillRect(t.tc * TILE + 3, t.tr * TILE + 3, TILE - 6, TILE - 6);
      ctx.strokeStyle = spec.body; ctx.lineWidth = 1.5; ctx.strokeRect(t.tc * TILE + 3, t.tr * TILE + 3, TILE - 6, TILE - 6);
      ctx.beginPath(); ctx.arc(t.x, t.y, TILE * 0.22, 0, 7); ctx.fillStyle = spec.body; ctx.fill();
    }
    for (const p of shots) { ctx.beginPath(); ctx.arc(p.x, p.y, 2.5, 0, 7); ctx.fillStyle = p.color; ctx.fill(); }
    for (const e of enemies) {
      ctx.beginPath(); ctx.arc(e.x, e.y, e.rad, 0, 7); ctx.fillStyle = e.color; ctx.fill();
      if (e.slowT > 0) { ctx.lineWidth = 1.5; ctx.strokeStyle = "#67e8f9"; ctx.stroke(); } // chilled
      if (e.hp < e.maxHp) {
        const bw = e.rad * 2.4, bx = e.x - bw / 2, by = e.y - e.rad - 5;
        ctx.fillStyle = "#0f172a"; ctx.fillRect(bx, by, bw, 2.5);
        ctx.fillStyle = "#22c55e"; ctx.fillRect(bx, by, bw * Math.max(0, e.hp / e.maxHp), 2.5);
      }
    }
    ctx.fillStyle = "#64748b"; ctx.font = "700 9px ui-monospace,monospace";
    ctx.fillText("IN", 3, pts[0].y + 3);
    const last = pts[pts.length - 1];
    ctx.fillText("OUT", Math.min(W - 22, last.x - 8), last.y - 6);
  }

  // ---- input ----
  const toTile = (ev) => {
    const b = canvas.getBoundingClientRect();
    const x = (ev.clientX - b.left) * (W / b.width), y = (ev.clientY - b.top) * (H / b.height);
    return { c: Math.floor(x / TILE), r: Math.floor(y / TILE) };
  };
  canvas.addEventListener("pointermove", (e) => { hover = toTile(e); }, { signal: sig });
  canvas.addEventListener("pointerleave", () => { hover = null; }, { signal: sig });
  canvas.addEventListener("pointerdown", (e) => {
    e.preventDefault();
    if (state === "won" || state === "lost") return;
    const t = toTile(e); hover = t;
    if (t.c < 0 || t.c >= COLS || t.r < 0 || t.r >= ROWS) return;
    if (blocked.has(keyOf(t.c, t.r))) return;
    if (towers.some((w) => w.tc === t.c && w.tr === t.r)) return;
    const spec = TOWERS[selected];
    if (gold < spec.cost) return;
    gold -= spec.cost;
    towers.push({ tc: t.c, tr: t.r, x: t.c * TILE + TILE / 2, y: t.r * TILE + TILE / 2, type: selected, cd: 0 });
    sync();
  }, { signal: sig });
  shopBtns.forEach((b) => b.addEventListener("click", () => { selected = b.dataset.t; updateShop(); }, { signal: sig }));
  playBtn.addEventListener("click", enter, { signal: sig });
  exitBtn.addEventListener("click", exit, { signal: sig });
  pauseBtn.addEventListener("click", () => { if (state === "running") { paused = !paused; sync(); } }, { signal: sig });
  const bumpSpeed = () => { speed = speed >= 4 ? 1 : speed + 1; sync(); };
  ffBtn.addEventListener("click", bumpSpeed, { signal: sig });
  window.addEventListener("keydown", (e) => {
    if (!active) return;
    if (e.key === "f" || e.key === "F") bumpSpeed();
    else if (e.key === " " && state === "running") { e.preventDefault(); paused = !paused; sync(); }
  }, { signal: sig });
  resetBtn.addEventListener("click", reset, { signal: sig });

  // ---- loop ----
  let last = null, raf = 0, alive = true;
  function frame(t) {
    if (!alive) return;
    raf = requestAnimationFrame(frame);
    if (!active) { last = t; return; }
    if (last === null) last = t;
    let dt = (t - last) / 1000; last = t;
    if (dt > 0.05) dt = 0.05;
    if (state === "running" && !paused) step(dt);
    draw();
  }
  raf = requestAnimationFrame(frame);

  // ---- teardown (called before re-binding on the next htmx swap) ----
  return () => {
    alive = false;
    cancelAnimationFrame(raf);
    ac.abort();
    ui.remove();
    canvas.remove(); hudBar.remove(); overlayMsg.remove(); ffBtn.remove();
    grid.style.position = "";
    delete grid.dataset.tdOn;
    roster.forEach((e) => { e.dot.style.opacity = ""; });
  };
}
