const styles = `
	[data-punchcard] {
		.bg-green-100, .dark\\:bg-green-100 { --w-size: 20%; }
		.bg-green-200, .dark\\:bg-green-200 { --w-size: 30%; }
		.bg-green-300, .dark\\:bg-green-300 { --w-size: 40%; }
		.bg-green-400, .dark\\:bg-green-400 { --w-size: 50%; }
		.bg-green-500, .dark\\:bg-green-500 { --w-size: 60%; }
		.bg-green-600, .dark\\:bg-green-600 { --w-size: 70%; }
		.bg-green-700, .dark\\:bg-green-700 { --w-size: 80%; }
		.bg-green-800, .dark\\:bg-green-800 { --w-size: 90%; }
		.bg-green-900, .dark\\:bg-green-900 { --w-size: 100%; }

		> div > div {
			background-color: color-mix(in srgb, #2160ec var(--w-size), light-dark(white, black));
			transition: transform 0.2s ease-out;
			will-change: transform;
		}
	}
`;

const sheet = new CSSStyleSheet();
sheet.replaceSync(styles);
document.adoptedStyleSheets = [sheet];

const RADIUS = 60;
const MAX_SCALE = 2.5;

const grid = document.querySelector('[data-punchcard]');
const dots = Array.from(grid.querySelectorAll(':scope > div > div'));

function mousemove(e) {
	for (const dot of dots) {
		const rect = dot.getBoundingClientRect();
		const cx = rect.left + rect.width / 2;
		const cy = rect.top + rect.height / 2;
		const dist = Math.hypot(e.clientX - cx, e.clientY - cy);
		const scale = 1 + (MAX_SCALE - 1) * Math.max(0, 1 - dist / RADIUS);
		dot.style.transform = `scale(${scale})`;
	}
}

function mouseleave() {
	for (const dot of dots) {
		dot.style.transform = 'scale(1)';
	}
}

function toggleAnimation(enabled) {
	if (enabled) {
		grid.addEventListener('mousemove', mousemove);
		grid.addEventListener('mouseleave', mouseleave);
	} else {
		grid.removeEventListener('mousemove', mousemove);
		grid.removeEventListener('mouseleave', mouseleave);
	}
}

const motion = window.matchMedia('(prefers-reduced-motion: reduce)');
toggleAnimation(!motion.matches);
motion.addEventListener('change', (event) => toggleAnimation(!event.matches));
