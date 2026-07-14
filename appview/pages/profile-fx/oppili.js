// Brickblaster — punchcard edition
// Vanilla ES module. No imports, no build step.

const HS_KEY    = 'brickblaster-hs';
const WQ        = '(min-width: 768px)';
const BALL_D    = 10;
const BALL_R    = BALL_D / 2;
const PAD_H     = 6;
const PAD_GAP   = 10;
const PAD_CELLS = 5;

const lsGet = k => { try { return localStorage.getItem(k); } catch (e) { return null; } };
const lsSet = (k, v) => { try { localStorage.setItem(k, v); } catch (e) {} };

// True if the dot has a contribution colour (not gray).
// Uses HSL saturation: gray dots have s < 0.25, green contribution dots are much higher.
const isGreen = (el) => {
  const c = getComputedStyle(el).backgroundColor;
  const m = c.match(/[\d.]+/g);
  if (!m || m.length < 3) return false;
  const r = +m[0] / 255, g = +m[1] / 255, b = +m[2] / 255;
  const mx = Math.max(r, g, b), mn = Math.min(r, g, b);
  if (mx === mn) return false;
  const l = (mx + mn) / 2;
  return (mx - mn) / (l > 0.5 ? 2 - mx - mn : mx + mn) > 0.25;
};

const run = (N) => {
  if (matchMedia('(prefers-reduced-motion: reduce)').matches) return () => {};

  const ac  = new AbortController();
  const mq  = matchMedia(WQ);
  const sig = { signal: ac.signal };

  const getCols  = () => mq.matches ? 14 : 28;
  const resetDot = el => {
    el.style.backgroundColor = '';
    el.style.transform       = '';
    el.style.opacity         = '';
  };

  const readGrid = () => {
    const dots = Array.from(N.children, w => w.firstElementChild).filter(Boolean);
    const cols = getCols();
    return { dots, cols, rows: Math.ceil(dots.length / cols) };
  };

  let G = readGrid();

  G.dots.forEach(el => {
    el.style.transition      = 'none';
    el.style.transformOrigin = 'center';
    el.style.willChange      = 'transform, opacity, background-color';
  });

  const origPos    = N.style.position;
  N.style.position = 'relative';
  N.style.cursor   = 'default';

  // Ball element — inside N, can visually overflow below
  const ballEl = document.createElement('div');
  Object.assign(ballEl.style, {
    position:        'absolute',
    width:           BALL_D + 'px',
    height:          BALL_D + 'px',
    borderRadius:    '50%',
    backgroundColor: '#3b82f6',
    boxShadow:       '0 0 6px #3b82f680',
    pointerEvents:   'none',
    zIndex:          '10',
    display:         'none',
  });
  N.appendChild(ballEl);

  // Paddle — block element below N
  const padWrap = document.createElement('div');
  padWrap.style.cssText = `position:relative; height:${PAD_H}px; margin-top:${PAD_GAP}px;`;
  N.parentNode.insertBefore(padWrap, N.nextSibling);

  const padEl = document.createElement('div');
  padEl.style.cssText = `position:absolute; height:${PAD_H}px; border-radius:3px; background:#3b82f6; box-shadow:0 0 6px #3b82f680; top:0;`;
  padWrap.appendChild(padEl);

  // HUD
  const hud = document.createElement('div');
  Object.assign(hud.style, {
    fontSize:      '11px',
    fontFamily:    'ui-monospace, monospace',
    color:         '#6b7280',
    marginBottom:  '6px',
    userSelect:    'none',
    letterSpacing: '0.04em',
  });
  N.parentNode.insertBefore(hud, N);

  // State
  let phase    = 'idle';
  let score    = 0;
  let hs       = +(lsGet(HS_KEY) || 0);
  let brickMap = []; // which dots are green/collidable
  let alive    = []; // which bricks haven't been destroyed yet
  let bx = 0, by = 0, bvx = 0, bvy = 0;
  let padX = 0;
  let padXReady = false;

  // Arrow keys
  const keys = { left: false, right: false };
  window.addEventListener('keydown', e => {
    if (e.key === 'ArrowLeft')  { keys.left  = true;  if (phase === 'playing') e.preventDefault(); }
    if (e.key === 'ArrowRight') { keys.right = true;  if (phase === 'playing') e.preventDefault(); }
  }, sig);
  window.addEventListener('keyup', e => {
    if (e.key === 'ArrowLeft')  keys.left  = false;
    if (e.key === 'ArrowRight') keys.right = false;
  }, sig);

  const getMetrics = () => {
    const gridW = N.offsetWidth;
    const gridH = N.offsetHeight;
    const cellW = gridW / G.cols;
    const cellH = gridH / G.rows;
    const padW     = cellW * PAD_CELLS;
    const paddleY  = gridH + PAD_GAP + PAD_H / 2;
    const padSpeed  = Math.max(100, gridW / 0.6);
    const ballSpeed = Math.max(80,  gridH / 1.2);
    return { gridW, gridH, cellW, cellH, padW, paddleY, padSpeed, ballSpeed };
  };

  const showHud = () => {
    if (phase === 'idle')         hud.textContent = `BRICKBLASTER  |  BEST: ${hs}  |  click to start`;
    else if (phase === 'playing') hud.textContent = `SCORE: ${score}  |  BEST: ${hs}`;
    else                          hud.textContent = `GAME OVER  |  SCORE: ${score}  |  BEST: ${hs}  |  click to replay`;
  };

  const startGame = () => {
    score    = 0;
    brickMap = G.dots.map(el => isGreen(el)); // snapshot which dots are green
    alive    = brickMap.slice();

    const m = getMetrics();
    if (!padXReady) { padX = m.gridW / 2; padXReady = true; }

    const a = Math.PI * (0.3 + Math.random() * 0.4); // 54–126°, always upward
    bx  = padX;
    by  = m.gridH * 0.7;
    bvx = Math.cos(a) * m.ballSpeed;
    bvy = -Math.sin(a) * m.ballSpeed;

    phase = 'playing';
    ballEl.style.display = 'block';
    showHud();
  };

  const endGame = () => {
    if (score > hs) { hs = score; lsSet(HS_KEY, hs); }
    phase = 'gameover';
    ballEl.style.display = 'none';
    showHud();
  };

  const update = (dt, m) => {
    if (phase !== 'playing') return;

    const hw = m.padW / 2;
    if (keys.left)  padX = Math.max(hw, padX - m.padSpeed * dt);
    if (keys.right) padX = Math.min(m.gridW - hw, padX + m.padSpeed * dt);

    bx += bvx * dt;
    by += bvy * dt;

    // Wall bounces
    if (bx < BALL_R)           { bx = BALL_R;           bvx =  Math.abs(bvx); }
    if (bx > m.gridW - BALL_R) { bx = m.gridW - BALL_R; bvx = -Math.abs(bvx); }
    if (by < BALL_R)           { by = BALL_R;            bvy =  Math.abs(bvy); }

    // Ball falls past paddle → game over
    if (by > m.paddleY + PAD_H + BALL_R) { endGame(); return; }

    // Paddle collision
    const padTop = m.paddleY - PAD_H / 2 - BALL_R;
    if (bvy > 0 && by >= padTop && by <= m.paddleY + PAD_H) {
      if (Math.abs(bx - padX) <= hw + BALL_R) {
        by = padTop;
        const offset = (bx - padX) / hw;
        const speed  = Math.hypot(bvx, bvy);
        bvx = offset * speed * 0.8;
        const sp = Math.hypot(bvx, bvy) || speed;
        bvx = (bvx / sp) * speed;
        bvy = -Math.abs((bvy / sp) * speed);
      }
    }

    // Brick collision — only green (brickMap) dots that are still alive
    const HIT_R = BALL_R + Math.min(m.cellW, m.cellH) * 0.4;
    const c0 = Math.round(bx / m.cellW - 0.5);
    const r0 = Math.round(by / m.cellH - 0.5);
    let hitDone = false;
    for (let dr = -1; dr <= 1 && !hitDone; dr++) {
      for (let dc = -1; dc <= 1 && !hitDone; dc++) {
        const c = c0 + dc, r = r0 + dr;
        if (c < 0 || c >= G.cols || r < 0 || r >= G.rows) continue;
        const idx = r * G.cols + c;
        if (!alive[idx]) continue;
        const dotX = (c + 0.5) * m.cellW;
        const dotY = (r + 0.5) * m.cellH;
        const dist = Math.hypot(bx - dotX, by - dotY);
        if (dist >= HIT_R) continue;

        alive[idx] = false;
        score += 1;
        if (score > hs) { hs = score; lsSet(HS_KEY, hs); }
        showHud();

        const nx = (bx - dotX) / (dist || 1);
        const ny = (by - dotY) / (dist || 1);
        const dp = bvx * nx + bvy * ny;
        if (dp < 0) { bvx -= 2 * dp * nx; bvy -= 2 * dp * ny; }
        hitDone = true;
      }
    }

    if (!alive.some(a => a)) endGame();
  };

  const render = (m) => {
    if (!padXReady && m.gridW > 0) { padX = m.gridW / 2; padXReady = true; }

    const hw = m.padW / 2;
    const cx = Math.max(hw, Math.min(m.gridW - hw, padX));

    padEl.style.left  = (cx - hw) + 'px';
    padEl.style.width = m.padW + 'px';

    if (phase === 'playing') {
      ballEl.style.left = (bx - BALL_R) + 'px';
      ballEl.style.top  = (by - BALL_R) + 'px';
    }

    G.dots.forEach((el, i) => {
      // Idle or non-brick gray dot: restore natural appearance
      if (phase === 'idle' || !brickMap[i]) { resetDot(el); return; }

      if (alive[i]) {
        // Alive green brick: keep natural color, scale up so it reads as a target
        el.style.backgroundColor = '';
        el.style.transform       = 'scale(1.5)';
        el.style.opacity         = '';
      } else {
        // Destroyed: shrink and turn gray
        el.style.backgroundColor = '#6b7280';
        el.style.transform       = 'scale(0.6)';
        el.style.opacity         = '';
      }
    });
  };

  N.addEventListener('pointerdown', e => {
    if (e.button !== 0) return;
    if (phase !== 'playing') startGame();
  }, sig);

  let raf = 0, lastT = 0, visible = true;

  const frame = t => {
    const dt = lastT ? Math.min((t - lastT) / 1000, 0.05) : 0;
    lastT = t;
    const m = getMetrics();
    update(dt, m);
    render(m);
    if (visible) raf = requestAnimationFrame(frame);
  };

  const io = new IntersectionObserver(entries => {
    const v = entries.some(e => e.isIntersecting);
    if (v === visible) return;
    visible = v;
    if (visible) { lastT = 0; raf = requestAnimationFrame(frame); }
    else cancelAnimationFrame(raf);
  });
  io.observe(N);

  raf = requestAnimationFrame(frame);

  mq.addEventListener('change', () => {
    G = readGrid();
    G.dots.forEach(el => {
      el.style.transition      = 'none';
      el.style.transformOrigin = 'center';
      el.style.willChange      = 'transform, opacity, background-color';
    });
    padXReady = false;
    phase = 'idle';
    ballEl.style.display = 'none';
    G.dots.forEach(resetDot);
    showHud();
  }, sig);

  showHud();

  return () => {
    cancelAnimationFrame(raf);
    io.disconnect();
    ac.abort();
    hud.remove();
    ballEl.remove();
    padWrap.remove();
    N.style.position = origPos;
    N.style.cursor   = '';
    G.dots.forEach(resetDot);
  };
};

// Bootstrap — mirrors orrery.js pattern, survives htmx navigation
let _cleanup = null;
let _el      = null;

const _setup = () => {
  const el = document.querySelector('[data-punchcard]');
  if (el === _el) return;
  if (_cleanup) _cleanup();
  _el      = el;
  _cleanup = el ? run(el) : null;
};

_setup();
document.addEventListener('htmx:load', _setup);
