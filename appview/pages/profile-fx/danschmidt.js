// A lane-crossing dodge game on the Tangled punchcard.
// Guide the chicken from one edge of the grid to the other without getting
// clipped by the moving hazards in the "traffic" lanes. Arrow keys / WASD
// once the grid is focused, or tap in the direction you want to move.
// Reduced motion gets a turn-based variant: hazards only move when you do.
//
// Levels alternate direction: odd levels climb to the top, even levels
// descend to the bottom, and so on forever, getting faster each time.
// Losing all your lives turns the whole grid into a plate of drumsticks.
//
// Tune the constants below to change difficulty and pacing.

const COLS_WIDE = 14;
const COLS_NARROW = 28;
const WIDE_QUERY = "(min-width: 768px)";
const REDUCED_MOTION_QUERY = "(prefers-reduced-motion: reduce)";

const LIVES_START = 3;
const BASE_SPEED = 0.7; // cells/sec for the easiest danger lane
const SPEED_RAMP = 0.06; // extra cells/sec per lane closer to the goal
const SAFE_ROW_INTERVAL = 3; // every Nth interior row is a resting lane
const CAR_DENSITY_DIVISOR = 11; // larger = fewer cars per lane at level 1
const CARS_PER_LEVEL = 0.5; // extra cars per lane added every ~2 levels
const MAX_CARS_PER_LANE = 6; // cap so late levels don't get absurdly crowded
const WIN_PAUSE_MS = 1600;
const HIT_FLASH_MS = 180;

const PLAYER_COLOR = "hsl(45 100% 55%)";
const PLAYER_GLYPH = "🐔";
const PLAYER_GLYPH_SIZE = "16px"; // tune relative to your actual dot size
const HIT_COLOR = "hsl(355 85% 55%)";
const HAZARD_GLYPH = "🚗";
const HAZARD_GLYPH_SIZE = "16px"; // tune relative to your actual dot size
const HAZARD_BG = "hsla(330, 85%, 62%, 0.55)";
const HAZARD_BG_REVERSE = "hsla(280, 80%, 62%, 0.55)";
const GAME_OVER_GLYPH = "🍗";
const GAME_OVER_GLYPH_SIZE = "16px";
const GRASS_HUE = 100;
const GRASS_HUE_JITTER = 18;
const GRASS_LIGHTNESS_MIN = 36;
const GRASS_LIGHTNESS_MAX = 58;
const GRASS_SCALE_MIN = 0.65;
const GRASS_SCALE_MAX = 1.3;

function getCols() {
  return matchMedia(WIDE_QUERY).matches ? COLS_WIDE : COLS_NARROW;
}

function setupGame(grid) {
  const cells = Array.from(grid.children, (c) => c.firstElementChild).filter(Boolean);
  const total = cells.length;
  if (total === 0) return () => {};

  const reducedMotion = matchMedia(REDUCED_MOTION_QUERY).matches;
  const wideQuery = matchMedia(WIDE_QUERY);

  let cols = getCols();
  let rows = Math.ceil(total / cols);
  let homeRow = total % cols === 0 ? rows - 1 : rows - 2;
  let prevKey = new Array(total).fill("");
  let lanes = [];
  let player = { row: 0, col: 0 };
  let level = 1;
  let bestLevel = 1;
  let lives = LIVES_START;
  let gameOver = false;
  let justWon = false;
  let resolving = false;
  let raf = 0;
  let lastTs = 0;
  let visible = true;
  let flashTimeoutId = 0;
  let winTimeoutId = 0;
  let playerElIdx = -1;
  let hazardIdxs = new Set();
  let audioCtx = null;

  cells.forEach((el) => {
    el.style.transformOrigin = "center";
    if (!reducedMotion) {
      el.style.transition = "background-color 0.12s ease, transform 0.12s ease";
    }
  });

  grid.tabIndex = 0;
  grid.setAttribute("aria-label", "Dodge game: use arrow keys or tap to cross");

  const status = document.createElement("div");
  status.style.fontSize = "11px";
  status.style.lineHeight = "1.4";
  status.style.marginTop = "6px";
  status.style.opacity = "0.75";
  status.style.fontFamily = "inherit";
  status.setAttribute("aria-live", "polite");
  grid.insertAdjacentElement("afterend", status);

  let grassColor = new Array(total);
  let grassScale = new Array(total);
  function buildGrassField() {
    for (let i = 0; i < total; i++) {
      const hue = GRASS_HUE + (Math.random() * 2 - 1) * GRASS_HUE_JITTER;
      const light = GRASS_LIGHTNESS_MIN + Math.random() * (GRASS_LIGHTNESS_MAX - GRASS_LIGHTNESS_MIN);
      grassColor[i] = `hsl(${hue.toFixed(0)} 55% ${light.toFixed(0)}%)`;
      grassScale[i] = GRASS_SCALE_MIN + Math.random() * (GRASS_SCALE_MAX - GRASS_SCALE_MIN);
    }
  }
  buildGrassField();

  function colsInRow(row) {
    return row === rows - 1 ? total - cols * (rows - 1) : cols;
  }

  // Odd levels climb from the bottom (homeRow) to the top (0).
  // Even levels descend from the top (0) back to the bottom (homeRow).
  function goalRow() {
    return level % 2 === 1 ? 0 : homeRow;
  }
  function startRow() {
    return level % 2 === 1 ? homeRow : 0;
  }

  function laneTypeFor(row) {
    if (row === goalRow()) return "goal";
    if (row === startRow()) return "home";
    if (row % SAFE_ROW_INTERVAL === 0) return "safe";
    return "danger";
  }

  function buildLanes() {
    lanes = new Array(rows);
    for (let r = 0; r <= homeRow; r++) {
      const type = laneTypeFor(r);
      if (type !== "danger") {
        lanes[r] = { type };
        continue;
      }
      const distFromGoal = Math.abs(r - goalRow());
      const baseCars = Math.max(1, Math.floor(cols / CAR_DENSITY_DIVISOR));
      const bonusCars = Math.floor((level - 1) * CARS_PER_LEVEL);
      lanes[r] = {
        type,
        dir: r % 2 === 0 ? 1 : -1,
        speed: BASE_SPEED + (homeRow - distFromGoal) * SPEED_RAMP + (level - 1) * 0.08,
        carCount: Math.min(MAX_CARS_PER_LANE, baseCars + bonusCars),
        offset: Math.random() * cols,
      };
    }
  }

  function carColumnsFor(lane) {
    const spacing = cols / lane.carCount;
    const out = [];
    for (let k = 0; k < lane.carCount; k++) {
      const pos = ((lane.offset + k * spacing) % cols + cols) % cols;
      out.push(Math.round(pos) % cols);
    }
    return out;
  }

  function resetPlayer() {
    const row = startRow();
    const maxCol = colsInRow(row) - 1;
    player = { row, col: Math.floor(maxCol / 2) };
  }

  function applyStyle(i, bg, scale) {
    const key = bg + "|" + scale;
    if (prevKey[i] === key) return;
    prevKey[i] = key;
    const el = cells[i];
    el.style.backgroundColor = bg;
    el.style.transform = scale === 1 ? "" : `scale(${scale})`;
  }

  function clearCell(el) {
    el.style.backgroundColor = "";
    el.style.transform = "";
    el.textContent = "";
    el.style.fontSize = "";
    el.style.display = "";
    el.style.alignItems = "";
    el.style.justifyContent = "";
  }

  function paintPlayer(idx) {
    if (playerElIdx !== -1 && playerElIdx !== idx) {
      clearCell(cells[playerElIdx]);
      prevKey[playerElIdx] = "";
    }
    const el = cells[idx];
    el.style.backgroundColor = PLAYER_COLOR;
    el.style.display = "flex";
    el.style.alignItems = "center";
    el.style.justifyContent = "center";
    el.style.fontSize = PLAYER_GLYPH_SIZE;
    el.textContent = PLAYER_GLYPH;
    prevKey[idx] = "player";
    playerElIdx = idx;
  }

  function paintHazard(i, dir) {
    const el = cells[i];
    el.style.backgroundColor = dir === 1 ? HAZARD_BG_REVERSE : HAZARD_BG;
    el.style.display = "flex";
    el.style.alignItems = "center";
    el.style.justifyContent = "center";
    el.style.fontSize = HAZARD_GLYPH_SIZE;
    el.style.transform = dir === 1 ? "scaleX(-1)" : "";
    el.textContent = HAZARD_GLYPH;
    prevKey[i] = "hazard|" + dir;
  }

  function clearHazard(i) {
    clearCell(cells[i]);
    prevKey[i] = "";
  }

  function renderGameOver() {
    for (let i = 0; i < total; i++) {
      const el = cells[i];
      el.style.backgroundColor = "";
      el.style.transform = "";
      el.style.display = "flex";
      el.style.alignItems = "center";
      el.style.justifyContent = "center";
      el.style.fontSize = GAME_OVER_GLYPH_SIZE;
      el.textContent = GAME_OVER_GLYPH;
      prevKey[i] = "gameover";
    }
    playerElIdx = -1;
    hazardIdxs = new Set();
  }

  function render() {
    if (gameOver) {
      renderGameOver();
      return;
    }
    const newHazards = new Map();
    for (let r = 0; r <= homeRow; r++) {
      const lane = lanes[r];
      const rowCols = colsInRow(r);
      const carCols = lane.type === "danger" ? carColumnsFor(lane) : null;
      for (let c = 0; c < rowCols; c++) {
        const i = r * cols + c;
        if (carCols && carCols.includes(c)) {
          newHazards.set(i, lane.dir);
        } else {
          applyStyle(i, grassColor[i], grassScale[i]);
        }
      }
    }
    for (const i of hazardIdxs) {
      if (!newHazards.has(i)) clearHazard(i);
    }
    for (const [i, dir] of newHazards) {
      paintHazard(i, dir);
    }
    hazardIdxs = new Set(newHazards.keys());

    const pIdx = player.row * cols + player.col;
    paintPlayer(pIdx);
  }

  function updateStatus() {
    if (gameOver) {
      status.textContent = `Game over — reached level ${level} (best ${bestLevel}). Click the grid to try again.`;
      return;
    }
    if (justWon) {
      status.textContent = `Level ${level} complete! Best ${bestLevel}`;
      return;
    }
    const filled = "●".repeat(lives);
    const empty = "○".repeat(Math.max(0, LIVES_START - lives));
    const dirLabel = level % 2 === 1 ? "climb up" : "climb down";
    const hint = reducedMotion ? "tap/arrows (turn-based)" : "arrows or tap";
    status.textContent = `Level ${level}: ${dirLabel} · ${filled}${empty} · Best ${bestLevel} — ${hint}`;
  }

  function ensureAudio() {
    const AC = window.AudioContext || window.webkitAudioContext;
    if (!AC) return null;
    if (!audioCtx) audioCtx = new AC();
    if (audioCtx.state === "suspended") audioCtx.resume();
    return audioCtx;
  }

  function playTone(freq, duration, type, gainLevel) {
    const ctx = ensureAudio();
    if (!ctx) return;
    const osc = ctx.createOscillator();
    const gain = ctx.createGain();
    osc.type = type;
    osc.frequency.value = freq;
    gain.gain.value = gainLevel;
    gain.gain.exponentialRampToValueAtTime(0.0001, ctx.currentTime + duration);
    osc.connect(gain).connect(ctx.destination);
    osc.start();
    osc.stop(ctx.currentTime + duration);
  }

  function sfxHop() {
    playTone(520, 0.07, "square", 0.07);
  }

  function sfxHit() {
    playTone(120, 0.25, "sawtooth", 0.12);
  }

  function sfxWin() {
    [523, 659, 784, 1047].forEach((freq, i) => {
      setTimeout(() => playTone(freq, 0.15, "square", 0.09), i * 90);
    });
  }

  function sfxGameOver() {
    playTone(200, 0.35, "sawtooth", 0.1);
    setTimeout(() => playTone(140, 0.4, "sawtooth", 0.1), 150);
  }

  function checkCollision() {
    if (gameOver || resolving) return false;
    const lane = lanes[player.row];
    if (lane.type !== "danger") return false;
    if (!carColumnsFor(lane).includes(player.col)) return false;

    resolving = true;
    lives--;
    sfxHit();
    const hitIdx = player.row * cols + player.col;
    clearCell(cells[hitIdx]);
    cells[hitIdx].style.backgroundColor = HIT_COLOR;
    cells[hitIdx].style.transform = "scale(1.5)";
    prevKey[hitIdx] = "hit";

    const finalize = () => {
      resolving = false;
      if (lives <= 0) {
        gameOver = true;
        cancelAnimationFrame(raf);
        raf = 0;
        sfxGameOver();
      } else {
        resetPlayer();
      }
      updateStatus();
      render();
    };

    if (reducedMotion) {
      finalize();
    } else {
      flashTimeoutId = setTimeout(finalize, HIT_FLASH_MS);
    }
    return true;
  }

  function win() {
    clearTimeout(winTimeoutId);
    resolving = true;
    justWon = true;
    sfxWin();
    updateStatus();
    render();
    winTimeoutId = setTimeout(advanceLevel, WIN_PAUSE_MS);
  }

  function advanceLevel() {
    level++;
    bestLevel = Math.max(bestLevel, level);
    justWon = false;
    resolving = false;
    buildLanes();
    resetPlayer();
    prevKey.fill("");
    updateStatus();
    render();
    if (!reducedMotion) {
      lastTs = 0;
      if (raf === 0 && visible) raf = requestAnimationFrame(frame);
    }
  }

  function advanceLanesOneStep() {
    for (let r = 0; r <= homeRow; r++) {
      const lane = lanes[r];
      if (lane.type !== "danger") continue;
      const steps = Math.max(1, Math.round(lane.speed));
      lane.offset = ((lane.offset + lane.dir * steps) % cols + cols) % cols;
    }
  }

  function advanceLanesContinuous(dt) {
    for (let r = 0; r <= homeRow; r++) {
      const lane = lanes[r];
      if (lane.type !== "danger") continue;
      lane.offset = ((lane.offset + lane.dir * lane.speed * dt) % cols + cols) % cols;
    }
  }

  function tryMove(dr, dc) {
    if (gameOver) {
      newGame();
      return;
    }
    if (resolving) return;
    const nr = player.row + dr;
    const nc = player.col + dc;
    if (nr < 0 || nr > homeRow) return;
    const maxCol = colsInRow(nr) - 1;
    if (nc < 0 || nc > maxCol) return;

    player.row = nr;
    player.col = nc;
    sfxHop();
    if (nr === goalRow()) {
      win();
      return;
    }
    let hit = checkCollision();
    if (!hit && reducedMotion) {
      advanceLanesOneStep();
      hit = checkCollision();
    }
    render();
    updateStatus();
  }

  function frame(ts) {
    const dt = lastTs ? (ts - lastTs) / 1000 : 0;
    lastTs = ts;
    if (!gameOver && !resolving) {
      advanceLanesContinuous(dt);
      checkCollision();
    }
    render();
    if (visible && !gameOver) {
      raf = requestAnimationFrame(frame);
    } else {
      raf = 0;
    }
  }

  function newGame() {
    level = 1;
    lives = LIVES_START;
    gameOver = false;
    justWon = false;
    resolving = false;
    playerElIdx = -1;
    hazardIdxs = new Set();
    cells.forEach(clearCell);
    prevKey.fill("");
    buildLanes();
    resetPlayer();
    updateStatus();
    render();
    if (!reducedMotion) {
      lastTs = 0;
      if (raf === 0 && visible) raf = requestAnimationFrame(frame);
    }
  }

  function onKeyDown(e) {
    const moves = {
      ArrowUp: [-1, 0], ArrowDown: [1, 0], ArrowLeft: [0, -1], ArrowRight: [0, 1],
      w: [-1, 0], s: [1, 0], a: [0, -1], d: [0, 1],
      W: [-1, 0], S: [1, 0], A: [0, -1], D: [0, 1],
    };
    const mv = moves[e.key];
    if (!mv) return;
    e.preventDefault();
    tryMove(mv[0], mv[1]);
  }

  function onClick(e) {
    grid.focus({ preventScroll: true });
    if (gameOver) {
      newGame();
      return;
    }
    const rect = grid.getBoundingClientRect();
    const colF = ((e.clientX - rect.left) / rect.width) * cols;
    const rowF = ((e.clientY - rect.top) / rect.height) * rows;
    const dc = colF - (player.col + 0.5);
    const dr = rowF - (player.row + 0.5);
    if (Math.abs(dc) > Math.abs(dr)) tryMove(0, dc > 0 ? 1 : -1);
    else tryMove(dr > 0 ? 1 : -1, 0);
  }

  grid.addEventListener("keydown", onKeyDown);
  grid.addEventListener("click", onClick);

  function rebuild() {
    cancelAnimationFrame(raf);
    raf = 0;
    cols = getCols();
    rows = Math.ceil(total / cols);
    homeRow = total % cols === 0 ? rows - 1 : rows - 2;
    prevKey = new Array(total).fill("");
    newGame();
  }
  wideQuery.addEventListener("change", rebuild);

  let io = null;
  if (!reducedMotion) {
    io = new IntersectionObserver((entries) => {
      const nowVisible = entries.some((e) => e.isIntersecting);
      if (nowVisible === visible) return;
      visible = nowVisible;
      if (visible && !gameOver && raf === 0) {
        lastTs = 0;
        raf = requestAnimationFrame(frame);
      } else if (!visible) {
        cancelAnimationFrame(raf);
        raf = 0;
      }
    });
    io.observe(grid);
  }

  newGame();

  return () => {
    cancelAnimationFrame(raf);
    clearTimeout(flashTimeoutId);
    clearTimeout(winTimeoutId);
    grid.removeEventListener("keydown", onKeyDown);
    grid.removeEventListener("click", onClick);
    wideQuery.removeEventListener("change", rebuild);
    if (io) io.disconnect();
    grid.removeAttribute("tabindex");
    grid.removeAttribute("aria-label");
    cells.forEach(clearCell);
    if (audioCtx) audioCtx.close();
    status.remove();
  };
}

let currentGrid = null;
let cleanup = null;
function init() {
  const grid = document.querySelector("[data-punchcard]");
  if (grid === currentGrid) return;
  if (cleanup) cleanup();
  currentGrid = grid;
  cleanup = grid ? setupGame(grid) : null;
}
init();
document.addEventListener("htmx:load", init);
