// Intersex-Inclusive Progress Pride palette, arranged as left-to-right bands.
const prideColors = [
  "#E22016", "#F28917", "#F5E524", "#7BB82A", "#2C5B84", "#6D2380",
  "#000000", "#945516", "#7BCCE5", "#F4AEC8", "#FFFFFF", "#FFD817",
];
const particleShapes = ["triangle", "diamond", "dot", "hexagon"];
const shapePaths = {
  triangle: "polygon(50% 0, 100% 100%, 0 100%)",
  diamond: "polygon(50% 0, 100% 50%, 50% 100%, 0 50%)",
  hexagon: "polygon(25% 6.7%, 75% 6.7%, 100% 50%, 75% 93.3%, 25% 93.3%, 0 50%)",
};

const wideScreen = matchMedia("(min-width: 768px)");
const reducedMotion = matchMedia("(prefers-reduced-motion: reduce)");
let currentGrid = null;
let stop = null;

function animate(grid) {
  const dots = Array.from(grid.children, (cell) => cell.firstElementChild).filter(Boolean);
  if (!dots.length) return () => {};

  const originalStyles = dots.map((dot) => dot.getAttribute("style"));
  const originalGridStyle = grid.getAttribute("style");
  const maxRadius = 5;
  const accelerationX = new Float32Array(dots.length);
  const accelerationY = new Float32Array(dots.length);
  const effects = new Map();
  const fireworkTimers = new Set();
  const devourTimers = new Set();
  let audioContext = null;
  const collisionNotes = [587, 659, 440, 659, 740, 880, 784, 740, 587, 659, 440, 440, 440, 494, 587, 587,
    587, 659, 440, 659, 740, 880, 784, 740, 587, 659, 440, 440, 440, 494, 587, 587,
      0, 494, 554, 587, 587, 659, 554, 494, 440,   0,   0, 494, 494, 554, 587, 494,
    440, 880,   0, 880, 659,   0, 494, 494, 554, 587, 494, 587, 659,   0,   0, 554,
    494, 440,   0,   0, 494, 494, 554, 587, 494, 440, 659, 659, 659, 740, 659,   0,
    587, 659, 740, 587, 659, 659, 659, 740, 659, 440,   0, 494, 554, 587, 494,   0,
    659, 740, 659, 440, 494, 587, 494, 740, 740, 659, 440, 494, 587, 494, 659, 659,
    587, 554, 494, 440, 494, 587, 494, 587, 659, 554, 494, 440, 440, 440, 659, 587,
    440, 494, 587, 494, 740, 740, 659, 440, 494, 587, 494, 880, 554, 587, 554, 494,
    440, 494, 587, 494, 587, 659, 554, 494, 440, 440, 659, 587,   0,   0, 494, 587,
    494, 587, 659,   0,   0, 554, 494, 440,   0,   0, 494, 494, 554, 587, 494, 440,
      0, 880, 880, 659, 740, 659, 587,   0, 440, 494, 554, 587, 494,   0, 554, 494,
    440,   0, 494, 494, 554, 587, 494, 440,   0,   0, 659, 659, 740, 659, 587, 587,
    659, 740, 659, 659, 659, 740, 659, 440, 440,   0, 440, 494, 554, 587, 494,   0,
    659, 740, 659, 440, 494, 587, 494, 740, 740, 659, 440, 494, 587, 494, 659, 659,
    587, 554, 494, 440, 494, 587, 494, 587, 659, 554, 494, 440, 440, 659, 587, 440,
    494, 587, 494, 740, 740, 659, 440, 494, 587, 494, 880, 554, 587, 554, 494, 440,
    494, 587, 494, 587, 659, 554, 494, 440, 440, 659, 587, 440, 494, 587, 494, 740,
    740, 659, 440, 494, 587, 494, 880, 554, 587, 554, 494, 440, 494, 587, 494, 587,
    659, 554, 494, 440, 440, 659, 587, 440, 494, 587, 494, 740, 740, 659, 440, 494,
    587, 494, 880, 554, 587, 554, 494, 440, 494, 587, 494, 587, 659, 554, 494, 440,
    440, 659, 587,   0];
  let collisionNoteIndex = 0;
  let particles = [];
  let aliveCount = dots.length;
  let fireworksShown = false;
  let cols = 0;
  let halfWidth = 1;
  let halfHeight = 1;
  let frame = 0;
  let resizeFrame = 0;
  let lastTime = 0;
  let visible = true;
  let mouse = { x: 0, y: 0, active: false };
  let singularity = null;

  Object.assign(grid.style, {
    backgroundColor: "rgba(7, 8, 24, 0.2)",
    backgroundImage: "radial-gradient(ellipse at center, rgba(109, 35, 128, 0.34), transparent 68%)",
    backgroundRepeat: "no-repeat",
    boxShadow: "inset 0 0 20px rgba(109, 35, 128, 0.25)",
    cursor: "none",
  });

  const blackHole = document.createElement("div");
  const makeEye = () => {
    const eye = document.createElement("div");
    const pupil = document.createElement("div");
    Object.assign(eye.style, {
      position: "absolute",
      top: "7px",
      width: "10px",
      height: "10px",
      borderRadius: "50%",
      background: "white",
      overflow: "hidden",
    });
    Object.assign(pupil.style, {
      position: "absolute",
      left: "3px",
      top: "3px",
      width: "5px",
      height: "5px",
      borderRadius: "50%",
      background: "#111",
    });
    eye.append(pupil);
    return { eye, pupil };
  };
  const leftEye = makeEye();
  const rightEye = makeEye();
  const smile = document.createElement("div");
  leftEye.eye.style.left = "5px";
  rightEye.eye.style.right = "5px";
  Object.assign(smile.style, {
    position: "absolute",
    left: "8px",
    top: "19px",
    width: "16px",
    height: "6px",
    borderBottom: "2px solid white",
    borderRadius: "0 0 60% 60%",
    transform: "rotate(9deg)",
  });
  Object.assign(blackHole.style, {
    position: "fixed",
    width: "32px",
    height: "32px",
    borderRadius: "50%",
    background: "radial-gradient(circle at 42% 38%, #242424 0 9%, #050505 34%, #000 70%)",
    boxShadow: "0 0 10px rgba(109, 35, 128, 0.55)",
    pointerEvents: "none",
    display: "none",
    zIndex: "10002",
  });
  blackHole.dataset.profileFx = "black-hole";
  blackHole.append(leftEye.eye, rightEye.eye, smile);
  document.body.append(blackHole);

  const paintBlackHole = (time) => {
    const location = singularity || mouse;
    if (!singularity && !mouse.active) {
      blackHole.style.display = "none";
      return;
    }
    const wobbleX = Math.sin(time / 90) * 1.4;
    const wobbleY = Math.cos(time / 120) * 1.2;
    blackHole.style.display = "block";
    blackHole.style.left = `${location.screenX}px`;
    blackHole.style.top = `${location.screenY}px`;
    blackHole.style.transform = `translate(-50%, -50%) rotate(${(Math.sin(time / 280) * 4).toFixed(1)}deg)`;
    leftEye.pupil.style.transform = `translate(${wobbleX.toFixed(1)}px, ${wobbleY.toFixed(1)}px)`;
    rightEye.pupil.style.transform = `translate(${(-wobbleX * 0.7).toFixed(1)}px, ${(wobbleY * 0.8).toFixed(1)}px)`;
  };

  const getAudioContext = () => (audioContext?.state === "running" ? audioContext : null);

  const playTone = (frequency, duration, delay, volume) => {
    const context = getAudioContext();
    if (!context) return;
    const start = context.currentTime + delay;
    const oscillator = context.createOscillator();
    const gain = context.createGain();
    oscillator.type = "sine";
    oscillator.frequency.setValueAtTime(frequency, start);
    gain.gain.setValueAtTime(0.0001, start);
    gain.gain.exponentialRampToValueAtTime(volume, start + 0.015);
    gain.gain.exponentialRampToValueAtTime(0.0001, start + duration);
    oscillator.connect(gain).connect(context.destination);
    oscillator.start(start);
    oscillator.stop(start + duration + 0.02);
  };

  const playPop = () => {
    const frequency = collisionNotes[collisionNoteIndex];
    collisionNoteIndex = (collisionNoteIndex + 1) % collisionNotes.length;
    // Zero or a negative value is a rest; advance the sequence without a tone.
    if (!Number.isFinite(frequency) || frequency <= 0) return;
    const context = getAudioContext();
    if (!context) return;
    const start = context.currentTime;
    const oscillator = context.createOscillator();
    const gain = context.createGain();
    oscillator.type = "sine";
    oscillator.frequency.setValueAtTime(frequency, start);
    oscillator.frequency.exponentialRampToValueAtTime(frequency * 0.82, start + 0.12);
    gain.gain.setValueAtTime(0.0001, start);
    gain.gain.exponentialRampToValueAtTime(0.05, start + 0.01);
    gain.gain.exponentialRampToValueAtTime(0.0001, start + 0.14);
    oscillator.connect(gain).connect(context.destination);
    oscillator.start(start);
    oscillator.stop(start + 0.16);
  };

  const playTada = () => {
    playTone(523.25, 0.24, 0, 0.045);
    playTone(659.25, 0.24, 0.13, 0.045);
    playTone(783.99, 0.55, 0.26, 0.06);
  };

  const unlockAudio = () => {
    if (!audioContext) {
      const AudioContext = window.AudioContext || window.webkitAudioContext;
      if (!AudioContext) return;
      audioContext = new AudioContext();
    }
    if (audioContext.state === "suspended") audioContext.resume().catch(() => {});
  };

  const moveMouse = (event) => {
    if (singularity) return;
    const gridRect = grid.getBoundingClientRect();
    mouse = {
      x: event.clientX - gridRect.left - halfWidth,
      y: event.clientY - gridRect.top - halfHeight,
      screenX: event.clientX,
      screenY: event.clientY,
      active: true,
    };
    paintBlackHole(performance.now());
  };

  const leaveMouse = () => {
    if (!singularity) mouse = { ...mouse, active: false };
    paintBlackHole(performance.now());
  };

  const initialRadius = (dot) => {
    if (dot.classList.contains("size-[4px]")) return 1;
    if (dot.classList.contains("size-[7px]")) return 3.5;
    if (dot.classList.contains("size-[6px]")) return 3;
    return 2.5;
  };

  const paintParticle = (particle) => {
    const dot = dots[particle.index];
    const diameter = (particle.baseRadius * 2).toFixed(2);
    const path = shapePaths[particle.shape];
    const color = dot.style.backgroundColor || prideColors[particle.colorIndex];
    const glow = (1.5 + Math.min(particle.radius, maxRadius) * 0.8).toFixed(1);
    dot.style.width = `${diameter}px`;
    dot.style.height = `${diameter}px`;
    dot.style.minWidth = `${diameter}px`;
    dot.style.minHeight = `${diameter}px`;
    dot.style.maxWidth = "none";
    dot.style.maxHeight = "none";
    dot.style.aspectRatio = "1 / 1";
    dot.style.borderRadius = particle.shape === "dot" ? "50%" : "0";
    dot.style.clipPath = path || "none";
    dot.style.webkitClipPath = path || "none";
    dot.style.filter = `saturate(1.45) brightness(1.18) drop-shadow(0 0 ${glow}px ${color})`;
    dot.style.flexShrink = "0";
    dot.style.opacity = particle.radius < 1.5 ? "0.45" : "1";
    dot.style.visibility = particle.alive ? "visible" : "hidden";
  };

  const showBlast = (particle) => {
    const gridRect = grid.getBoundingClientRect();
    const color = dots[particle.index].style.backgroundColor;
    const blast = document.createElement("div");
    const size = Math.max(8, particle.radius * 4);

    Object.assign(blast.style, {
      position: "fixed",
      left: `${gridRect.left + gridRect.width / 2 + particle.x}px`,
      top: `${gridRect.top + gridRect.height / 2 + particle.y}px`,
      width: `${size}px`,
      height: `${size}px`,
      border: `1px solid ${color}`,
      borderRadius: "9999px",
      boxShadow: `0 0 8px ${color}`,
      pointerEvents: "none",
      transform: "translate(-50%, -50%) scale(0.3)",
      transition: "transform 280ms ease-out, opacity 280ms ease-out",
      opacity: "1",
      zIndex: "9999",
    });

    document.body.append(blast);
    requestAnimationFrame(() => {
      blast.style.transform = "translate(-50%, -50%) scale(2.8)";
      blast.style.opacity = "0";
    });
    const timer = setTimeout(() => {
      effects.delete(blast);
      blast.remove();
    }, 300);
    effects.set(blast, timer);
  };

  const showCongrats = (left, top) => {
    const message = document.createElement("div");
    message.dataset.profileFx = "congrats";
    message.textContent = "Congrats!";
    Object.assign(message.style, {
      position: "fixed",
      left: `${left}px`,
      top: `${top - 28}px`,
      color: "white",
      fontFamily: "ui-rounded, system-ui, sans-serif",
      fontSize: "clamp(22px, 4vw, 38px)",
      fontWeight: "800",
      letterSpacing: "0.04em",
      pointerEvents: "none",
      textShadow: "0 0 8px #7BCCE5, 0 0 18px #6D2380",
      transform: "translate(-50%, -50%) scale(0.5)",
      transition: "transform 500ms cubic-bezier(.15,.8,.25,1), opacity 600ms ease-out",
      opacity: "0",
      zIndex: "10000",
    });
    document.body.append(message);
    requestAnimationFrame(() => {
      message.style.transform = "translate(-50%, -50%) scale(1)";
      message.style.opacity = "1";
    });
    effects.set(message, []);
  };

  const devourPage = (target) => {
    const letters = [];
    const textNodes = [];
    const walker = document.createTreeWalker(document.body, NodeFilter.SHOW_TEXT, {
      acceptNode(node) {
        const parent = node.parentElement;
        if (!parent || !node.nodeValue.trim()) return NodeFilter.FILTER_REJECT;
        if (parent.closest("[data-punchcard], [data-profile-fx], script, style, noscript, textarea, select, option")) {
          return NodeFilter.FILTER_REJECT;
        }
        return NodeFilter.FILTER_ACCEPT;
      },
    });

    while (walker.nextNode()) {
      const node = walker.currentNode;
      const parent = node.parentElement;
      const text = node.nodeValue;
      const style = getComputedStyle(parent);
      const start = letters.length;

      for (let index = 0; index < text.length; index += 1) {
        if (/\s/.test(text[index])) continue;
        const range = document.createRange();
        range.setStart(node, index);
        range.setEnd(node, index + 1);
        const rect = range.getBoundingClientRect();
        if (!rect.width && !rect.height) continue;
        letters.push({
          character: text[index],
          left: rect.left,
          top: rect.top,
          width: rect.width,
          height: rect.height,
          font: style.font,
          color: style.color,
          order: letters.length,
        });
      }
      if (letters.length > start) textNodes.push({ node, delay: start * 18 });
    }

    textNodes.forEach(({ node, delay }) => {
      const timer = setTimeout(() => {
        devourTimers.delete(timer);
        node.nodeValue = "";
      }, delay);
      devourTimers.add(timer);
    });

    letters.forEach((letter) => {
      const glyph = document.createElement("span");
      const delay = letter.order * 18;
      glyph.dataset.profileFx = "devoured-letter";
      glyph.textContent = letter.character;
      Object.assign(glyph.style, {
        position: "fixed",
        left: `${letter.left}px`,
        top: `${letter.top}px`,
        width: `${Math.max(1, letter.width)}px`,
        height: `${Math.max(1, letter.height)}px`,
        color: letter.color,
        font: letter.font,
        lineHeight: `${Math.max(1, letter.height)}px`,
        pointerEvents: "none",
        transformOrigin: "center",
        transition: "transform 1.5s cubic-bezier(.1,.8,.2,1), opacity 1.5s ease-in",
        zIndex: "10001",
      });
      document.body.append(glyph);

      const startTimer = setTimeout(() => {
        devourTimers.delete(startTimer);
        const dx = target.screenX - (letter.left + letter.width / 2);
        const dy = target.screenY - (letter.top + letter.height / 2);
        glyph.style.transform = `translate(${dx.toFixed(1)}px, ${dy.toFixed(1)}px) scale(0.08) rotate(${(Math.random() * 720 - 360).toFixed(0)}deg)`;
        glyph.style.opacity = "0";
      }, delay + 80);
      const removeTimer = setTimeout(() => {
        devourTimers.delete(removeTimer);
        effects.delete(glyph);
        glyph.remove();
      }, delay + 1650);
      devourTimers.add(startTimer);
      devourTimers.add(removeTimer);
      effects.set(glyph, [startTimer, removeTimer]);
    });
  };

  const becomeSingularity = (particle) => {
    const gridRect = grid.getBoundingClientRect();
    singularity = {
      screenX: gridRect.left + gridRect.width / 2 + particle.x,
      screenY: gridRect.top + gridRect.height / 2 + particle.y,
    };
    mouse = { ...mouse, active: false };
    particle.vx = 0;
    particle.vy = 0;
    dots[particle.index].style.visibility = "hidden";
    paintBlackHole(performance.now());
    devourPage(singularity);
  };

  const showFireworks = (particle) => {
    const gridRect = grid.getBoundingClientRect();
    const left = gridRect.left + gridRect.width / 2 + particle.x;
    const top = gridRect.top + gridRect.height / 2 + particle.y;

    const launchWave = (wave) => {
      const sparks = wave === 0 ? 28 : 18;
      for (let index = 0; index < sparks; index += 1) {
        const spark = document.createElement("div");
        const angle = (Math.PI * 2 * index) / sparks + (Math.random() - 0.5) * 0.25;
        const distance = 38 + Math.random() * 85;
        const color = prideColors[(index + wave * 3) % prideColors.length];
        Object.assign(spark.style, {
          position: "fixed",
          left: `${left}px`,
          top: `${top}px`,
          width: "4px",
          height: "4px",
          backgroundColor: color,
          borderRadius: "50%",
          boxShadow: `0 0 6px ${color}`,
          pointerEvents: "none",
          transform: "translate(-50%, -50%)",
          transition: "transform 1.1s cubic-bezier(.15,.8,.25,1)",
          zIndex: "9999",
        });
        document.body.append(spark);
        requestAnimationFrame(() => {
          spark.style.transform = `translate(-50%, -50%) translate(${(Math.cos(angle) * distance).toFixed(1)}px, ${(Math.sin(angle) * distance).toFixed(1)}px)`;
        });
        effects.set(spark, []);
      }
    };

    playTada();
    showCongrats(gridRect.left + gridRect.width / 2, gridRect.top + gridRect.height / 2);
    for (let wave = 0; wave < 7; wave += 1) {
      const timer = setTimeout(() => {
        fireworkTimers.delete(timer);
        launchWave(wave);
      }, wave * 800);
      fireworkTimers.add(timer);
    }
  };

  const reset = () => {
    if (singularity) return;
    // Set every base size before measuring. Absorption only uses transform scale,
    // so changing a shape later cannot shift the grid beneath the physics mesh.
    const seeds = dots.map((dot, index) => {
      const radius = initialRadius(dot);
      return {
        index,
        baseRadius: radius,
        radius,
        mass: (radius * radius) / 3,
        colorIndex: Math.floor(((index % cols) * prideColors.length) / cols),
        shape: particleShapes[Math.floor(Math.random() * particleShapes.length)],
        alive: true,
      };
    });
    seeds.forEach(paintParticle);

    const gridRect = grid.getBoundingClientRect();
    halfWidth = Math.max(1, gridRect.width / 2);
    halfHeight = Math.max(1, gridRect.height / 2);
    particles = seeds.map((particle) => {
      const cellRect = dots[particle.index].parentElement.getBoundingClientRect();
      const originX = cellRect.left - gridRect.left + cellRect.width / 2 - halfWidth;
      const originY = cellRect.top - gridRect.top + cellRect.height / 2 - halfHeight;
      const spin = 0.25 + (Math.random() - 0.5) * 0.08;
      return {
        ...particle,
        originX,
        originY,
        x: originX,
        y: originY,
        vx: -originY * spin,
        vy: originX * spin,
      };
    });
    aliveCount = particles.length;
    fireworksShown = false;
    lastTime = 0;
  };

  const layout = () => {
    cols = wideScreen.matches ? 14 : 28;
    dots.forEach((dot, index) => {
      const column = index % cols;
      dot.style.backgroundColor = prideColors[Math.floor((column * prideColors.length) / cols)];
      dot.style.transition = "none";
      dot.style.transformOrigin = "center";
      dot.style.willChange = "transform";
    });
    reset();
  };

  const absorb = (winner, loser) => {
    const totalMass = winner.mass + loser.mass;
    winner.x = (winner.x * winner.mass + loser.x * loser.mass) / totalMass;
    winner.y = (winner.y * winner.mass + loser.y * loser.mass) / totalMass;
    winner.vx = (winner.vx * winner.mass + loser.vx * loser.mass) / totalMass;
    winner.vy = (winner.vy * winner.mass + loser.vy * loser.mass) / totalMass;
    winner.radius = Math.min(maxRadius, Math.hypot(winner.radius, loser.radius));
    winner.mass = (winner.radius * winner.radius) / 3;
    winner.colorIndex = (winner.colorIndex + loser.colorIndex + 1 + Math.floor(Math.random() * 3)) % prideColors.length;
    winner.shape = particleShapes[Math.floor(Math.random() * particleShapes.length)];
    dots[winner.index].style.backgroundColor = prideColors[winner.colorIndex];
    loser.alive = false;
    aliveCount -= 1;
    paintParticle(winner);
    paintParticle(loser);
    showBlast(winner);
    playPop();
    if (aliveCount === 1 && !fireworksShown) {
      fireworksShown = true;
      becomeSingularity(winner);
      showFireworks(winner);
    }
  };

  const tick = (time) => {
    frame = 0;
    if (!visible) return;

    const seconds = lastTime ? Math.min((time - lastTime) / 1000, 1 / 30) : 0;
    lastTime = time;
    paintBlackHole(time);
    accelerationX.fill(0);
    accelerationY.fill(0);
    const attractionBoost = aliveCount < 5 ? 1 + ((5 - aliveCount) / 4) * 3 : 1;

    for (let i = 0; i < particles.length; i += 1) {
      const a = particles[i];
      if (!a.alive) continue;

      if (mouse.active) {
        const dx = mouse.x - a.x;
        const dy = mouse.y - a.y;
        const distanceSquared = dx * dx + dy * dy + 576;
        const pull = 200000 / (distanceSquared * Math.sqrt(distanceSquared));
        accelerationX[i] += dx * pull;
        accelerationY[i] += dy * pull;
      }

      for (let j = i + 1; j < particles.length; j += 1) {
        const b = particles[j];
        if (!b.alive) continue;

        const dx = b.x - a.x;
        const dy = b.y - a.y;
        const collisionDistance = a.radius + b.radius;
        if (dx * dx + dy * dy < collisionDistance * collisionDistance) {
          const winner = a.radius >= b.radius ? a : b;
          const loser = winner === a ? b : a;
          absorb(winner, loser);
          if (loser === a) break;
          continue;
        }

        // Every dot pulls on every other dot; larger combined dots have more mass.
        const distanceSquared = dx * dx + dy * dy + 324;
        const pull = (560 * attractionBoost) / (distanceSquared * Math.sqrt(distanceSquared));
        accelerationX[i] += dx * pull * b.mass;
        accelerationY[i] += dy * pull * b.mass;
        accelerationX[j] -= dx * pull * a.mass;
        accelerationY[j] -= dy * pull * a.mass;
      }
    }

    particles.forEach((particle, index) => {
      if (!particle.alive) return;

      const overflow = Math.max(
        0,
        (Math.abs(particle.x) - halfWidth) / halfWidth,
        (Math.abs(particle.y) - halfHeight) / halfHeight,
      );
      if (overflow > 0) {
        // The farther it escapes the punchcard, the stronger its pull back to centre.
        const pullBack = 0.12 * overflow + 0.9 * overflow * overflow;
        particle.vx -= particle.x * pullBack * seconds;
        particle.vy -= particle.y * pullBack * seconds;
      }

      particle.vx = (particle.vx + accelerationX[index] * seconds) * 0.995;
      particle.vy = (particle.vy + accelerationY[index] * seconds) * 0.995;
      particle.x += particle.vx * seconds;
      particle.y += particle.vy * seconds;
      const scale = particle.radius / particle.baseRadius;
      dots[index].style.transform = `translate(${(particle.x - particle.originX).toFixed(2)}px, ${(particle.y - particle.originY).toFixed(2)}px) scale(${scale.toFixed(3)})`;
    });

    frame = requestAnimationFrame(tick);
  };

  const resize = () => {
    if (resizeFrame) return;
    resizeFrame = requestAnimationFrame(() => {
      resizeFrame = 0;
      reset();
    });
  };

  layout();
  wideScreen.addEventListener("change", layout);
  window.addEventListener("resize", resize);
  grid.addEventListener("pointerdown", unlockAudio);
  grid.addEventListener("pointermove", moveMouse, { passive: true });
  grid.addEventListener("pointerleave", leaveMouse);

  const observer = new IntersectionObserver((entries) => {
    visible = entries.some((entry) => entry.isIntersecting);
    if (visible && !frame) frame = requestAnimationFrame(tick);
    else if (!visible && frame) {
      cancelAnimationFrame(frame);
      frame = 0;
    }
  });
  observer.observe(grid);
  frame = requestAnimationFrame(tick);

  return () => {
    cancelAnimationFrame(frame);
    observer.disconnect();
    cancelAnimationFrame(resizeFrame);
    wideScreen.removeEventListener("change", layout);
    window.removeEventListener("resize", resize);
    grid.removeEventListener("pointerdown", unlockAudio);
    grid.removeEventListener("pointermove", moveMouse);
    grid.removeEventListener("pointerleave", leaveMouse);
    blackHole.remove();
    fireworkTimers.forEach((timer) => clearTimeout(timer));
    devourTimers.forEach((timer) => clearTimeout(timer));
    effects.forEach((timers, effect) => {
      (Array.isArray(timers) ? timers : [timers]).forEach((timer) => clearTimeout(timer));
      effect.remove();
    });
    if (originalGridStyle === null) grid.removeAttribute("style");
    else grid.setAttribute("style", originalGridStyle);
    dots.forEach((dot, index) => {
      if (originalStyles[index] === null) dot.removeAttribute("style");
      else dot.setAttribute("style", originalStyles[index]);
    });
  };
}

function mount() {
  const grid = document.querySelector("[data-punchcard]");
  if (grid === currentGrid && stop && !reducedMotion.matches) return;

  if (stop) stop();
  stop = null;
  currentGrid = grid;

  if (!grid || reducedMotion.matches) return;
  stop = animate(grid);
}

mount();
document.addEventListener("htmx:load", mount);
reducedMotion.addEventListener("change", mount);
