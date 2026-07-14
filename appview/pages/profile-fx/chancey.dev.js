const punchcard = document.querySelector("[data-punchcard]");

if (punchcard) {
  const reduceMotion = matchMedia("(prefers-reduced-motion: reduce)");
  const wideScreen = matchMedia("(min-width: 768px)");

  const purple = "#b57edc";
  const white = "#fff";
  const idleBackground = `linear-gradient(90deg, ${purple} 0 50%, ${white} 50% 100%)`;

  const activityCache = new WeakMap();

  let dots = [];
  let frame = 0;
  let hovered = false;
  let pointerX = 0.5;
  let pointerY = 0.5;
  let coinX = NaN;
  let coinY = NaN;
  let velocityX = 0;
  let velocityY = 0;
  let coinMix = 0;
  let evaporateStart = null;
  let evaporateFrom = 0;
  let clickSpinStart = -Infinity;
  let clickSpinDirection = 1;
  let coalesceStart = -Infinity;
  let explosionStart = -Infinity;
  let explosionPower = 1;
  let explosionX = 0;
  let explosionY = 0;
  let joltStart = -Infinity;
  let joltX = 0;
  let joltY = 0;
  let joltDirection = 1;
  let suppressGatherUntil = 0;
  let waitForReenterAfterExplosion = false;
  let lastTime = 0;
  let layout = { cols: 28, rows: 1 };

  function columnCount() {
    return wideScreen.matches ? 14 : 28;
  }

  function clamp(value, min, max) {
    return Math.min(max, Math.max(min, value));
  }

  function lerp(a, b, t) {
    return a + (b - a) * t;
  }

  function ease(t) {
    return t * t * (3 - 2 * t);
  }

  function smoothstep(edge0, edge1, value) {
    const t = clamp((value - edge0) / (edge1 - edge0), 0, 1);
    return t * t * (3 - 2 * t);
  }

  function ring(distance, front, width, energy = 1) {
    return (1 - smoothstep(0, width, Math.abs(distance - front))) * energy;
  }

  function heldSpin(raw, hold) {
    return raw - Math.sin(raw * 2) * hold * 0.5;
  }

  function occasionalSpin(now, activity, phase) {
    const active = 0.16;
    const period = lerp(9800, 5600, activity);
    const cycle = ((now + phase * 1400) % period) / period;

    if (cycle >= active) return Math.PI * 2;

    return ease(cycle / active) * Math.PI * 2;
  }

  function numberFromLabel(text) {
    const match = text?.match(
      /(\d+(?:\.\d+)?)\s+(?:commit|commits|contribution|contributions|change|changes)/i,
    );

    return match ? Number(match[1]) : null;
  }

  function explicitActivity(dot, wrapper) {
    for (const element of [dot, wrapper]) {
      for (const key of ["count", "commits", "contributions", "value", "level", "intensity"]) {
        const value = element.dataset?.[key];
        if (value !== undefined && value !== "" && !Number.isNaN(Number(value))) {
          return Number(value);
        }
      }

      const label =
        `${element.getAttribute("aria-label") || ""} ${element.getAttribute("title") || ""}`;
      const labelValue = numberFromLabel(label);
      if (labelValue !== null) return labelValue;

      const className = typeof element.className === "string" ? element.className : "";
      const classValue = className.match(/(?:level|count|activity|intensity)-?(\d+)/i);
      if (classValue) return Number(classValue[1]);
    }

    return null;
  }

  function colorActivity(color) {
    const match = color.match(/rgba?\(([\d.]+)[,\s]+([\d.]+)[,\s]+([\d.]+)(?:[,\s/]+([\d.]+))?\)/i);
    if (!match) return 0;

    const r = Number(match[1]);
    const g = Number(match[2]);
    const b = Number(match[3]);
    const a = match[4] === undefined ? 1 : Number(match[4]);
    if (a <= 0) return 0;

    const max = Math.max(r, g, b);
    const min = Math.min(r, g, b);
    const saturation = max === 0 ? 0 : (max - min) / max;
    const luminance = (0.2126 * r + 0.7152 * g + 0.0722 * b) / 255;
    const greenBias = clamp((g - Math.max(r, b)) / 160, 0, 1);

    return clamp((saturation * 0.45 + greenBias * 0.55) * (0.55 + (1 - luminance) * 0.65), 0, 1);
  }

  function readActivity(dot, wrapper) {
    if (activityCache.has(dot)) return activityCache.get(dot);

    const signal = {
      explicit: explicitActivity(dot, wrapper),
      color: colorActivity(getComputedStyle(dot).backgroundColor),
    };

    activityCache.set(dot, signal);
    return signal;
  }

  function refreshLayout() {
    layout.cols = columnCount();
    layout.rows = Math.ceil(dots.length / layout.cols) || 1;
  }

  function setup() {
    const cols = columnCount();

    const items = Array.from(punchcard.children)
      .map((wrapper, index) => {
        const dot = wrapper.firstElementChild;
        if (!dot) return null;

        return {
          dot,
          wrapper,
          signal: readActivity(dot, wrapper),
          col: index % cols,
          row: Math.floor(index / cols),
          phase: index * 0.43,
        };
      })
      .filter(Boolean);

    const maxExplicit = Math.max(0, ...items.map((item) => item.signal.explicit || 0));
    const maxColor = Math.max(0.001, ...items.map((item) => item.signal.color || 0));

    dots = items.map((item) => {
      const activity =
        item.signal.explicit !== null
          ? maxExplicit > 0
            ? Math.log1p(item.signal.explicit) / Math.log1p(maxExplicit)
            : 0
          : item.signal.color > 0.015
            ? item.signal.color / maxColor
            : 0;

      item.wrapper.style.perspective = "90px";

      item.dot.style.transition = "none";
      item.dot.style.borderRadius = "50%";
      item.dot.style.transformOrigin = "50% 50%";
      item.dot.style.backfaceVisibility = "visible";
      item.dot.style.willChange = "transform, opacity, background, box-shadow, filter";
      item.dot.style.background = idleBackground;

      return {
        ...item,
        activity: clamp(activity, 0, 1),
      };
    });

    refreshLayout();

    if (!Number.isFinite(coinX)) coinX = (layout.cols - 1) / 2;
    if (!Number.isFinite(coinY)) coinY = (layout.rows - 1) / 2;
  }

  function radius() {
    return Math.max(2.8, Math.min(layout.cols * 0.3, layout.rows * 0.24));
  }

  function bounds() {
    return {
      minX: 0,
      maxX: layout.cols - 1,
      minY: 0,
      maxY: layout.rows - 1,
    };
  }

  function setPointer(event) {
    const rect = punchcard.getBoundingClientRect();

    pointerX = clamp((event.clientX - rect.left) / rect.width, 0, 1);
    pointerY = clamp((event.clientY - rect.top) / rect.height, 0, 1);
  }

  function evaporateCoin() {
    evaporateStart = performance.now();
    evaporateFrom = Math.max(coinMix, 0.28);

    const speed = Math.hypot(velocityX, velocityY);
    if (speed < 2.8) {
      const angle = speed > 0.2 ? Math.atan2(velocityY, velocityX) : -0.72;
      velocityX = Math.cos(angle) * 3.5;
      velocityY = Math.sin(angle) * 3.5;
    }
  }

  function triggerCoalesce(now = performance.now()) {
    coalesceStart = now - 90;
    evaporateStart = null;
    coinMix = Math.max(coinMix, 0.78);
  }

  function triggerSpin(now = performance.now()) {
    clickSpinStart = now;
    clickSpinDirection *= -1;
    evaporateStart = null;
    coinMix = Math.max(coinMix, 0.62);
  }

  function triggerExplosion(now = performance.now(), power = 1) {
    const { minX, maxX, minY, maxY } = bounds();

    explosionStart = now;
    explosionPower = power;
    explosionX = Number.isFinite(coinX) ? coinX : clamp(pointerX * (layout.cols - 1), minX, maxX);
    explosionY = Number.isFinite(coinY) ? coinY : clamp(pointerY * (layout.rows - 1), minY, maxY);
    suppressGatherUntil = now + 1050 + power * 260;
    waitForReenterAfterExplosion = hovered;
    if (hovered) coalesceStart = Infinity;
    evaporateStart = null;
    evaporateFrom = Math.max(coinMix, 0.95);
    coinMix = evaporateFrom;

    const angle = Math.atan2(velocityY || -0.45, velocityX || 0.9);
    velocityX = Math.cos(angle) * (4.8 + power * 0.7);
    velocityY = Math.sin(angle) * (4.8 + power * 0.7);
  }

  function triggerJolt(now = performance.now()) {
    const { minX, maxX, minY, maxY } = bounds();

    joltStart = now;
    joltX = clamp(pointerX * (layout.cols - 1), minX, maxX);
    joltY = clamp(pointerY * (layout.rows - 1), minY, maxY);
    joltDirection *= -1;
  }

  function resumeCoalesceAfterReenter(now = performance.now()) {
    if (!waitForReenterAfterExplosion) return false;

    waitForReenterAfterExplosion = false;
    coalesceStart = Math.max(now, suppressGatherUntil);
    if (now >= suppressGatherUntil) coinMix = Math.max(coinMix, 0.78);
    return true;
  }

  function paintStill() {
    for (const { dot, activity } of dots) {
      dot.style.background = idleBackground;
      dot.style.opacity = `${0.45 + activity * 0.55}`;
      dot.style.transform = `scale(${0.72 + activity * 0.42})`;
      dot.style.boxShadow = "none";
      dot.style.filter = "none";
    }
  }

  function animate(now) {
    const dt = lastTime ? clamp((now - lastTime) / 1000, 0.001, 0.04) : 0.016;
    lastTime = now;

    const r = radius();
    const { minX, maxX, minY, maxY } = bounds();
    const targetX = clamp(pointerX * (layout.cols - 1), minX, maxX);
    const targetY = clamp(pointerY * (layout.rows - 1), minY, maxY);
    const explosionDuration = 1050 + explosionPower * 250;
    const explosionT = clamp((now - explosionStart) / explosionDuration, 0, 1);
    const exploding = now - explosionStart >= 0 && explosionT < 1;
    const joltDuration = 620;
    const joltT = clamp((now - joltStart) / joltDuration, 0, 1);
    const jolting = now - joltStart >= 0 && joltT < 1;
    const evaporateDuration = 1650;
    const evaporateT =
      evaporateStart !== null ? clamp((now - evaporateStart) / evaporateDuration, 0, 1) : 1;
    const evaporating = evaporateStart !== null && evaporateT < 1;
    const effectiveHovered =
      hovered && !exploding && !waitForReenterAfterExplosion && now >= suppressGatherUntil;

    const previousX = coinX;
    const previousY = coinY;

    if (effectiveHovered) {
      evaporateStart = null;

      const follow = 1 - Math.exp(-9.5 * dt);
      coinX += (targetX - coinX) * follow;
      coinY += (targetY - coinY) * follow;

      velocityX = (coinX - previousX) / dt;
      velocityY = (coinY - previousY) / dt;

      coinMix += (1 - coinMix) * (1 - Math.exp(-18 * dt));
    } else {
      if (exploding) {
        coinMix = evaporateFrom * (1 - smoothstep(0.06, 0.74, explosionT));
      } else if (waitForReenterAfterExplosion) {
        coinMix = 0;
        evaporateStart = null;
      } else if (evaporating) {
        const dissolve = smoothstep(0.04, 0.96, evaporateT);
        coinMix = evaporateFrom * (1 - dissolve);
        if (evaporateT >= 1) evaporateStart = null;
      } else if (evaporateStart !== null) {
        coinMix = 0;
        evaporateStart = null;
      } else {
        coinMix += (0 - coinMix) * (1 - Math.exp(-3.5 * dt));
      }

      if (coinMix > 0.01) {
        coinX += velocityX * dt;
        coinY += velocityY * dt;

        if (coinX < minX || coinX > maxX) {
          coinX = clamp(coinX, minX, maxX);
          velocityX *= -0.9;
        }

        if (coinY < minY || coinY > maxY) {
          coinY = clamp(coinY, minY, maxY);
          velocityY *= -0.9;
        }

        const driftDamping = Math.exp(-0.22 * dt);
        velocityX *= driftDamping;
        velocityY *= driftDamping;
      }
    }

    coinX = clamp(coinX, minX, maxX);
    coinY = clamp(coinY, minY, maxY);

    const mix = ease(coinMix);
    const rippleEnergy = Math.sin(mix * Math.PI);
    const gridReach = Math.hypot(layout.cols, layout.rows) + r;
    const coalesceDuration = 520;
    const coalesceT = clamp((now - coalesceStart) / coalesceDuration, 0, 1);
    const coalescing = effectiveHovered && now - coalesceStart >= 0 && coalesceT < 1;
    const coalesceEnergy = coalescing ? Math.pow(1 - coalesceT, 0.38) : 0;

    const hopCycle = (now / 1580) % 1;
    const hopArc = Math.sin(hopCycle * Math.PI);
    const hopLift = Math.pow(hopArc, 0.86) * Math.min(1.15, r * 0.24) * mix;

    const centerX = coinX;
    const centerY = clamp(coinY - hopLift, minY, maxY);

    const clickSpinT = clamp((now - clickSpinStart) / 820, 0, 1);
    const clickSpinActive = now - clickSpinStart >= 0 && clickSpinT < 1;
    const clickSpinEase = 1 - Math.pow(1 - clickSpinT, 3);
    const clickSpinPop = clickSpinActive ? Math.sin(clickSpinT * Math.PI) : 0;
    const clickSpin = clickSpinActive ? clickSpinDirection * Math.PI * 6 * clickSpinEase : 0;

    const rawSpin = hopCycle * Math.PI * 2 + clickSpin;
    const spin = heldSpin(rawSpin, lerp(0.64, 0.18, clickSpinPop));
    const face = Math.abs(Math.cos(spin));
    const faceHold = Math.pow(face, 0.38);
    const edgeFlash = 1 - face;
    const widthScale = 0.2 + faceHold * 0.8;
    const heightScale = 1 + edgeFlash * 0.06;
    const flipped = Math.cos(spin) < 0;
    const coinBrightness = 0.94 + faceHold * 0.13 + edgeFlash * 0.12 + clickSpinPop * 0.16;
    const explosionReach = (gridReach + r) * (0.92 + explosionPower * 0.12);
    const explosionFront = explosionT * explosionReach - r * 0.35;
    const explosionEnergy = exploding ? Math.pow(1 - explosionT, 0.55) * explosionPower : 0;
    const joltReach = layout.cols + layout.rows;
    const joltFront = joltT * joltReach - 1;
    const joltEnergy = jolting ? Math.pow(1 - joltT, 0.65) : 0;
    const evaporateFront = evaporateT * gridReach - r * 0.2;
    const evaporateEnergy = evaporating ? Math.pow(1 - evaporateT, 0.42) : 0;
    const coalesceFront = (1 - coalesceT) * gridReach;

    for (const item of dots) {
      const { dot, col, row, phase, activity } = item;

      const idleSpin = heldSpin(occasionalSpin(now, activity, phase), 0.72);
      const idleFace = Math.pow(Math.abs(Math.cos(idleSpin)), 0.42);
      const idleScale = 0.66 + activity * 0.48 + idleFace * (0.04 + activity * 0.05);
      const idleOpacity = 0.42 + activity * 0.58;
      const idleGlow = (1 - idleFace) * (0.07 + activity * 0.22);

      const localX = (col - centerX) / widthScale;
      const localY = (row - centerY) / heightScale;
      const distance = Math.hypot(localX, localY);
      const coinShape = 1 - smoothstep(r - 0.65, r + 0.35, distance);
      const coinMass = mix * coinShape;

      const fieldDistance = Math.hypot(col - centerX, row - centerY);
      const ripple = Math.sin(fieldDistance * 1.15 - now * 0.0065) * rippleEnergy;
      const transferFront = mix * gridReach;
      const coalesceRing = coalescing ? ring(fieldDistance, coalesceFront, 2.5, coalesceEnergy) : 0;
      const coalesceAbsorb =
        coalescing
          ? smoothstep(coalesceFront - 1.8, coalesceFront + 1.8, fieldDistance)
          : 0;
      const fieldAbsorb =
        coalescing
          ? coalesceAbsorb
          : mix > 0.94
            ? 1
            : 1 - smoothstep(transferFront - 2.2, transferFront + 2.2, fieldDistance);
      const transfer = clamp(mix * (coinShape + (1 - coinShape) * fieldAbsorb), 0, 1);
      const idleMass = clamp(1 - transfer, 0, 1);
      const totalMass = idleMass + coinMass;

      const explosionDistance = Math.hypot(col - explosionX, row - explosionY);
      const explosionRing = exploding ? ring(explosionDistance, explosionFront, 2.4, explosionEnergy) : 0;
      const joltDistance =
        joltDirection > 0
          ? col - joltX + (row - joltY) * 0.45
          : joltX - col + (row - joltY) * 0.45;
      const joltRing = jolting ? ring(joltDistance, joltFront, 1.1, joltEnergy) : 0;
      const explosionAfterglow =
        exploding
          ? (1 - smoothstep(explosionFront - 3.4, explosionFront + 0.2, explosionDistance)) *
            explosionEnergy
          : 0;
      const evaporateRing =
        evaporating ? ring(fieldDistance, evaporateFront, 2.1, evaporateEnergy * coinShape) : 0;
      const evaporateSpark =
        evaporating
          ? Math.max(0, Math.sin(phase * 11.3 + evaporateT * 34)) * evaporateEnergy * coinShape
          : 0;

      const edge = distance / r;
      const rim = edge > 0.78;
      const leftHalf = flipped ? localX > 0 : localX < 0;
      const onSeam = Math.abs(localX) < 0.38 && edge < 0.88;

      const coinBackground = onSeam
        ? `linear-gradient(90deg, ${purple}, ${white})`
        : leftHalf
          ? purple
          : white;

      const explosionBackground = Math.sin(phase + explosionT * 22) > 0 ? purple : white;
      const coinScale = (rim ? 1.5 : 1.28) + clickSpinPop * (rim ? 0.16 : 0.1);
      const idleWeightedScale = idleScale + ripple * idleMass * 0.035;
      const scale =
        totalMass > 0.001
          ? (idleWeightedScale * idleMass + coinScale * coinMass) / totalMass
          : idleWeightedScale;
      const spinAmount = idleSpin;
      const burstRing = Math.max(explosionRing, coalesceRing, evaporateRing, joltRing);
      const burstScale =
        explosionRing * (0.42 + activity * 0.22 + clickSpinPop * 0.12) +
        joltRing * 0.14 +
        coalesceRing * (0.3 + activity * 0.18) +
        evaporateRing * 0.38 +
        evaporateSpark * 0.22;
      const burstOpacity =
        explosionRing * 0.95 +
        explosionAfterglow * 0.2 +
        joltRing * 0.36 +
        coalesceRing * 0.75 +
        evaporateRing * 0.7 +
        evaporateSpark * 0.38;

      dot.style.background =
        burstRing > Math.max(coinMass, idleMass) * 0.28
          ? explosionBackground
          : coinMass > idleMass * 0.72
            ? coinBackground
            : idleBackground;
      dot.style.opacity = `${clamp(idleOpacity * idleMass + coinMass + burstOpacity, 0, 1)}`;
      dot.style.transform = `rotateY(${spinAmount}rad) scale(${scale + burstScale})`;
      dot.style.filter = `brightness(${lerp(0.9 + idleFace * 0.13 + activity * 0.08, coinBrightness, coinMass) + burstRing * 0.45 + evaporateSpark * 0.25}) saturate(${lerp(1, 1.1, coinMass) + burstRing * 0.18})`;
      dot.style.boxShadow =
        burstRing > 0.16
          ? `0 0 ${8 + burstRing * 14}px rgba(181, 126, 220, ${0.22 + burstRing * 0.32})`
          : coinMass > 0.2
          ? rim
            ? "0 0 8px rgba(181, 126, 220, 0.3)"
            : "0 0 4px rgba(181, 126, 220, 0.18)"
          : `0 0 ${idleGlow * 8}px rgba(181, 126, 220, ${idleGlow})`;
    }

    frame = requestAnimationFrame(animate);
  }

  function start() {
    if (frame) cancelAnimationFrame(frame);

    frame = 0;
    lastTime = 0;
    setup();

    if (reduceMotion.matches) {
      paintStill();
    } else {
      frame = requestAnimationFrame(animate);
    }
  }

  punchcard.addEventListener("pointerenter", (event) => {
    const wasHovered = hovered;
    hovered = true;
    setPointer(event);
    const resumed = !wasHovered && resumeCoalesceAfterReenter();
    if (!wasHovered && !resumed) triggerCoalesce();
  });

  punchcard.addEventListener("pointermove", setPointer);

  punchcard.addEventListener("pointerleave", () => {
    hovered = false;
    evaporateCoin();
  });

  punchcard.addEventListener("click", (event) => {
    const now = performance.now();

    setPointer(event);
    if (waitForReenterAfterExplosion) {
      triggerJolt(now);
      return;
    }

    if (event.detail % 3 === 0) {
      triggerSpin(now);
      triggerExplosion(now, 1.75);
    } else {
      triggerExplosion(now, 1);
    }
  });

  start();

  wideScreen.addEventListener("change", start);
  reduceMotion.addEventListener("change", start);
  addEventListener("resize", refreshLayout);
}
