const HUE = 276;
const ACCENT_L = 0.563;
const ACCENT_C = 0.219;
const PALE_L = 0.86;
const PALE_C = 0.035;
const GLOW_LIFT = 0.07;
const GLOW_REACH = 0.85;

const LEVELS = 5;
const GLOW_STEPS = 64;
const LEVEL_CLASSES = [
	"bg-green-200",
	"bg-green-300",
	"bg-green-400",
	"bg-green-500",
];

const BREATH_SCALE = 0.22;
const BREATH_GLOW = 0.5;
const GRAVITY_RADIUS = 130;
const GRAVITY_PULL = 10;
const ATTACK = 0.5;
const RELEASE = 0.07;

const WIDE = "(min-width: 768px)";
const REDUCE = "(prefers-reduced-motion: reduce)";

const COLORS = Array.from({ length: LEVELS }, (_, lv) => {
	const intensity = lv / (LEVELS - 1);
	return Array.from({ length: GLOW_STEPS }, (_, gs) => {
		const glow = gs / (GLOW_STEPS - 1);
		const e = intensity + (1 - intensity) * glow * GLOW_REACH;
		const L = PALE_L + (ACCENT_L - PALE_L) * e + GLOW_LIFT * glow;
		const C = PALE_C + (ACCENT_C - PALE_C) * e;
		return `oklch(${L.toFixed(3)} ${C.toFixed(3)} ${HUE})`;
	});
});

const attach = (grid) => {
	const dots = Array.from(grid.children, (c) => c.firstElementChild).filter(
		Boolean,
	);
	if (!dots.length) return () => {};

	const levels = new Uint8Array(dots.length);
	const hollow = dots.map((dot, i) => {
		levels[i] = LEVEL_CLASSES.findIndex((c) => dot.classList.contains(c)) + 1;
		return dot.classList.contains("border");
	});

	dots.forEach((dot) => {
		dot.style.transition = "none";
	});

	if (matchMedia(REDUCE).matches) {
		dots.forEach((dot, i) => {
			if (!hollow[i]) dot.style.backgroundColor = COLORS[levels[i]][0];
		});
		return () => {};
	}

	const ac = new AbortController();
	const opts = { signal: ac.signal };
	const passive = { passive: true, signal: ac.signal };
	const wide = matchMedia(WIDE);

	const grav = new Float32Array(dots.length);
	const lastGlow = new Int16Array(dots.length).fill(-1);
	const lastTransform = new Array(dots.length).fill("");
	const offsets = new Float32Array(dots.length * 2);

	let cols = wide.matches ? 14 : 28;
	let rect = grid.getBoundingClientRect();
	let mouseX = -9999;
	let mouseY = -9999;
	let raf = 0;
	let running = true;

	dots.forEach((dot) => {
		dot.style.willChange = "transform, background-color";
	});

	const measure = () => {
		cols = wide.matches ? 14 : 28;
		rect = grid.getBoundingClientRect();
		dots.forEach((dot, i) => {
			const r = dot.parentElement.getBoundingClientRect();
			offsets[i * 2] = r.left + r.width / 2 - rect.left;
			offsets[i * 2 + 1] = r.top + r.height / 2 - rect.top;
		});
	};

	const track = () => {
		rect = grid.getBoundingClientRect();
	};

	measure();

	wide.addEventListener("change", measure, opts);
	window.addEventListener("resize", measure, opts);
	window.addEventListener("scroll", track, passive);
	window.addEventListener(
		"pointermove",
		(e) => {
			mouseX = e.clientX;
			mouseY = e.clientY;
		},
		passive,
	);
	document.addEventListener(
		"pointerleave",
		() => {
			mouseX = -9999;
			mouseY = -9999;
		},
		opts,
	);

	const frame = (now) => {
		const t = now / 1000;

		for (let i = 0; i < dots.length; i++) {
			const col = i % cols;
			const row = (i / cols) | 0;

			const w1 = Math.sin(col * 0.38 + row * 0.3 + t * 0.5);
			const w2 = Math.sin(col * 0.18 - row * 0.45 + t * 0.32 + 2.1);
			const breath = (w1 + w2) / 4 + 0.5;

			const dx = mouseX - (rect.left + offsets[i * 2]);
			const dy = mouseY - (rect.top + offsets[i * 2 + 1]);
			const dist = Math.hypot(dx, dy);

			let target = Math.max(0, 1 - dist / GRAVITY_RADIUS);
			target = target * target * (3 - 2 * target);
			grav[i] += (target - grav[i]) * (target > grav[i] ? ATTACK : RELEASE);
			const g = grav[i];

			const pull = g * GRAVITY_PULL;
			const px = dist > 0 ? (dx / dist) * pull : 0;
			const py = dist > 0 ? (dy / dist) * pull : 0;

			const scale = 1 + breath * BREATH_SCALE + g * 0.55;
			const qx = Math.round(px * 4) / 4;
			const qy = Math.round(py * 4) / 4;
			const qs = Math.round(scale * 250) / 250;

			const tf = `translate(${qx}px,${qy}px) scale(${qs})`;
			if (tf !== lastTransform[i]) {
				dots[i].style.transform = tf;
				lastTransform[i] = tf;
			}

			if (hollow[i]) continue;

			const glow = Math.min(1, breath * BREATH_GLOW + g * 0.65);
			const gs = Math.min(GLOW_STEPS - 1, (glow * GLOW_STEPS) | 0);
			if (gs !== lastGlow[i]) {
				dots[i].style.backgroundColor = COLORS[levels[i]][gs];
				lastGlow[i] = gs;
			}
		}

		raf = requestAnimationFrame(frame);
	};

	const io = new IntersectionObserver((entries) => {
		const visible = entries.some((e) => e.isIntersecting);
		if (visible === running) return;
		running = visible;
		if (running) {
			measure();
			raf = requestAnimationFrame(frame);
		} else {
			cancelAnimationFrame(raf);
		}
	});
	io.observe(grid);

	raf = requestAnimationFrame(frame);

	return () => {
		cancelAnimationFrame(raf);
		io.disconnect();
		ac.abort();
	};
};

let teardown = null;
let current = null;

const boot = () => {
	const grid = document.querySelector("[data-punchcard]");
	if (grid === current) return;
	if (teardown) teardown();
	current = grid;
	teardown = grid ? attach(grid) : null;
};

boot();
document.addEventListener("htmx:load", boot);
