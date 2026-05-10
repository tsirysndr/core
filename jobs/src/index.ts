import { marked } from 'marked';
import { getEmojiFlag, countries } from 'countries-list';

export interface Env {
	LINEAR_API_KEY: string;
	LINEAR_TEAM_ID: string;
}

// ---------------------------------------------------------------------------
// Job postings — add a new markdown file in src/postings/ and import it here
// ---------------------------------------------------------------------------
// import softwareEngineer from './postings/software-engineer.md';

interface Posting {
	slug: string;
	title: string;
	location: string;
	type: string;
	salary: string;
	body: string; // raw markdown (frontmatter stripped)
}

function parsePosting(slug: string, raw: string): Posting {
	const fm = raw.match(/^---\n([\s\S]*?)\n---\n([\s\S]*)$/);
	if (!fm) throw new Error(`Posting ${slug} is missing frontmatter`);

	const meta: Record<string, string> = {};
	for (const line of fm[1].split('\n')) {
		const [k, ...rest] = line.split(':');
		if (k && rest.length) meta[k.trim()] = rest.join(':').trim();
	}

	return {
		slug,
		title: meta['title'] ?? slug,
		location: meta['location'] ?? 'Remote',
		type: meta['type'] ?? 'Full-time',
		salary: meta['salary'] ?? '',
		body: fm[2].trim(),
	};
}

const POSTINGS: Posting[] = [
	// parsePosting('software-engineer', softwareEngineer as string),
	// add more here as you create markdown files
];

const POSTINGS_BY_SLUG = new Map(POSTINGS.map((p) => [p.slug, p]));

// ---------------------------------------------------------------------------
// Country list (sorted alphabetically by name, from countries-list)
// ---------------------------------------------------------------------------
const COUNTRIES: { code: string; name: string }[] = Object.entries(countries)
	.map(([code, c]) => ({ code, name: c.name }))
	.sort((a, b) => a.name.localeCompare(b.name));

// ---------------------------------------------------------------------------
// SVG logo
// ---------------------------------------------------------------------------
const DOLLY_SVG = `<svg class="size-7" width="25" height="25" viewBox="0 0 25 25" xmlns="http://www.w3.org/2000/svg">
  <style>.dolly{color:#000}@media(prefers-color-scheme:dark){.dolly{color:#fff}}</style>
  <g transform="translate(-0.42924038,-0.87777209)">
    <path class="dolly" fill="currentColor" style="stroke-width:0.111183" d="m 16.775491,24.987061 c -0.78517,-0.0064 -1.384202,-0.234614 -2.033994,-0.631295 -0.931792,-0.490188 -1.643475,-1.31368 -2.152014,-2.221647 C 11.781409,23.136647 10.701392,23.744942 9.4922931,24.0886 8.9774725,24.238111 8.0757679,24.389777 6.5811304,23.84827 4.4270703,23.124679 2.8580086,20.883331 3.0363279,18.599583 3.0037061,17.652919 3.3488675,16.723769 3.8381157,15.925061 2.5329485,15.224503 1.4686756,14.048584 1.0611184,12.606459 0.81344502,11.816973 0.82385989,10.966486 0.91519098,10.154906 1.2422711,8.2387903 2.6795811,6.5725716 4.5299585,5.9732484 5.2685364,4.290122 6.8802592,3.0349975 8.706276,2.7794663 c 1.2124148,-0.1688264 2.46744,0.084987 3.52811,0.7011837 1.545426,-1.7139736 4.237779,-2.2205077 6.293579,-1.1676231 1.568222,0.7488935 2.689625,2.3113526 2.961888,4.0151464 1.492195,0.5977882 2.749007,1.8168898 3.242225,3.3644951 0.329805,0.9581836 0.340709,2.0135956 0.127128,2.9974286 -0.381606,1.535184 -1.465322,2.842146 -2.868035,3.556463 0.0034,0.273204 0.901506,2.243045 0.751284,3.729647 -0.03281,1.858525 -1.211631,3.619894 -2.846433,4.475452 -0.953967,0.556812 -2.084452,0.546309 -3.120531,0.535398 z m -4.470079,-5.349839 c 1.322246,-0.147248 2.189053,-1.300106 2.862307,-2.338363 0.318287,-0.472954 0.561404,-1.002348 0.803,-1.505815 0.313265,0.287151 0.578698,0.828085 1.074141,0.956909 0.521892,0.162542 1.133743,0.03052 1.45325,-0.443554 0.611414,-1.140449 0.31004,-2.516537 -0.04602,-3.698347 C 18.232844,11.92927 17.945151,11.232927 17.397785,10.751793 17.514522,9.9283111 17.026575,9.0919791 16.332883,8.6609491 15.741721,9.1323278 14.842258,9.1294949 14.271975,8.6252369 13.178927,9.7400102 12.177239,9.7029996 11.209704,8.8195135 10.992255,8.6209543 10.577326,10.031484 9.1211947,9.2324497 8.2846288,9.9333947 7.6359672,10.607693 7.0611981,11.578553 6.5026891,12.62523 5.9177873,13.554793 5.867393,14.69141 c -0.024234,0.66432 0.4948601,1.360337 1.1982269,1.306329 0.702996,0.06277 1.1815208,-0.629091 1.7138087,-0.916491 0.079382,0.927141 0.1688108,1.923227 0.4821259,2.828358 0.3596254,1.171275 1.6262605,1.915695 2.8251855,1.745211 0.08481,-0.0066 0.218672,-0.01769 0.218672,-0.0176 z m 0.686342,-3.497495 c -0.643126,-0.394168 -0.33365,-1.249599 -0.359402,-1.870938 0.064,-0.749774 0.115321,-1.538054 0.452402,-2.221125 0.356724,-0.487008 1.226721,-0.299139 1.265134,0.325689 -0.02558,0.628509 -0.314101,1.25416 -0.279646,1.9057 -0.07482,0.544043 0.05418,1.155133 -0.186476,1.652391 -0.197455,0.275121 -0.599638,0.355105 -0.892012,0.208283 z m -2.808766,-0.358124 c -0.605767,-0.328664 -0.4133176,-1.155655 -0.5083256,-1.73063 0.078762,-0.66567 0.013203,-1.510085 0.5705316,-1.976886 0.545037,-0.380109 1.286917,0.270803 1.029164,0.868384 -0.274913,0.755214 -0.09475,1.580345 -0.08893,2.34609 -0.104009,0.451702 -0.587146,0.691508 -1.002445,0.493042 z"/>
  </g>
</svg>`;

// ---------------------------------------------------------------------------
// HTML shell
// ---------------------------------------------------------------------------
function page(title: string, body: string): string {
	return `<!DOCTYPE html>
<html lang="en">
<head>
  <meta charset="UTF-8">
  <meta name="viewport" content="width=device-width, initial-scale=1.0">
  <title>${escapeHtml(title)} &middot; jobs at tangled</title>
  <link rel="icon" type="image/svg+xml" href="/favicon.svg">
  <meta property="og:image" content="https://assets.tangled.network/jobs-og.png">
  <meta name="twitter:card" content="summary_large_image">
  <meta name="twitter:image" content="https://assets.tangled.network/jobs-og.png">
  <link rel="preconnect" href="https://rsms.me/">
  <link rel="stylesheet" href="https://rsms.me/inter/inter.css">
  <script src="https://cdn.tailwindcss.com?plugins=typography"></script>
  <script>
    tailwind.config = {
      darkMode: 'media',
      theme: {
        extend: {
          fontFamily: {
            sans: ['"InterVariable"', '"Inter"', 'system-ui', 'sans-serif'],
            mono: ['"IBM Plex Mono"', 'ui-monospace', 'monospace'],
          }
        }
      }
    }
  </script>
  <style>
    html { font-size: 14px; }
    ::selection { background-color: rgba(250,204,21,0.3); }
    @media (prefers-color-scheme: dark) {
      ::selection { background-color: rgba(202,138,4,0.5); color:#fff; }
    }
    a { color: inherit; text-decoration: none; }
    a:hover { text-decoration: underline; }
    label { display:block; font-size:0.875rem; padding:0.5rem 0; color:#111827; }
    @media (prefers-color-scheme: dark) { label { color:#f3f4f6; } }
    .btn-create {
      position:relative; z-index:10; display:inline-flex; min-height:30px;
      cursor:pointer; align-items:center; justify-content:center;
      background:transparent; padding:0.25rem 0.75rem; font-size:0.875rem;
      color:#fff; border:none; font-family:inherit; text-decoration:none;
    }
    .btn-create::before {
      content:''; position:absolute; inset:0; z-index:-10; display:block;
      border-radius:0.25rem; border:1px solid #15803d; background:#16a34a;
      box-shadow:inset 0 -2px 0 0 rgba(0,0,0,.1),0 1px 0 0 rgba(0,0,0,.04);
      transition:all .15s ease-in-out;
    }
    .btn-create:hover { text-decoration: none; }
    .btn-create:hover::before { background:#15803d; border-color:#166534; }
    .btn-create:active::before { box-shadow:inset 0 2px 2px 0 rgba(0,0,0,.1); }
    .btn-create:disabled { cursor:not-allowed; opacity:.5; }
    @media (prefers-color-scheme: dark) {
      .btn-create::before { background:#15803d; border-color:#166534; }
      .btn-create:hover::before { background:#166534; }
    }
  </style>
</head>
<body class="bg-white dark:bg-gray-900 text-gray-900 dark:text-white min-h-screen flex flex-col">

  <header class="max-w-screen-xl mx-auto w-full">
    <nav class="mx-auto space-x-4 px-6 py-2">
      <div class="flex justify-between p-0 items-center">
        <div>
          <a href="/" class="text-2xl no-underline hover:no-underline flex items-center gap-2">
            ${DOLLY_SVG.replace('class="size-7"', 'class="size-8 text-black dark:text-white"')}
            <span class="font-bold text-xl not-italic">tangled</span>
          </a>
        </div>
        </div>
      </div>
    </nav>
  </header>

  ${body}

  <footer class="mt-12 w-full px-6 py-4">
    <div class="max-w-[90ch] mx-auto flex flex-wrap justify-center items-center gap-x-4 gap-y-2 text-sm text-gray-500 dark:text-gray-400">
      <div class="flex items-center justify-center gap-x-2 order-last sm:order-first w-full sm:w-auto">
        <a href="https://tangled.org" class="no-underline hover:no-underline flex items-center">${DOLLY_SVG.replace('class="size-7"', 'class="size-5 text-gray-500 dark:text-gray-400"')}</a>
        <span>&copy; 2026 Tangled Labs Oy.</span>
      </div>
      <a href="https://docs.tangled.org" class="hover:text-gray-900 dark:hover:text-gray-200 hover:underline no-underline">docs</a>
      <a href="https://tangled.org/@tangled.org/core" class="hover:text-gray-900 dark:hover:text-gray-200 hover:underline no-underline">source</a>
      <a href="https://chat.tangled.org" target="_blank" rel="noopener noreferrer" class="hover:text-gray-900 dark:hover:text-gray-200 hover:underline no-underline">discord</a>
      <a href="https://bsky.app/profile/tangled.org" target="_blank" rel="noopener noreferrer" class="hover:text-gray-900 dark:hover:text-gray-200 hover:underline no-underline">bluesky</a>
      <a href="https://x.com/tangled_org" target="_blank" rel="noopener noreferrer" class="hover:text-gray-900 dark:hover:text-gray-200 hover:underline no-underline">twitter (x)</a>
    </div>
  </footer>

</body>
</html>`;
}

// ---------------------------------------------------------------------------
// Pages
// ---------------------------------------------------------------------------
function listingsPage(): string {
	const rows = POSTINGS.map(
		(p) => `
    <a href="/${p.slug}" class="no-underline hover:no-underline group flex items-center justify-between gap-4 px-6 py-4 hover:bg-gray-50 hover:dark:bg-gray-800/50 transition-colors">
      <div>
        <div class="font-medium text-gray-900 dark:text-white group-hover:underline">${escapeHtml(p.title)}</div>
        <div class="text-sm text-gray-500 dark:text-gray-400 mt-0.5">${escapeHtml(p.location)} · ${escapeHtml(p.type)}${p.salary ? ` · ${escapeHtml(p.salary)}` : ''}</div>
      </div>
      <span class="text-gray-400 dark:text-gray-500 shrink-0">→</span>
    </a>`,
	).join('');

	const body = `
  <main class="max-w-[90ch] mx-auto w-full px-4 py-10 flex-1">
    <header class="mb-10">
      <h1 class="text-3xl font-bold dark:text-white mb-2">Open positions</h1>
      <p class="text-gray-500 dark:text-gray-400 max-w-prose">
      We're a lean, globally distributed team working on building the next-generation of social coding.
      We work remotely and try to meet once a year in-person.
      </p>
    </header>

    ${
			POSTINGS.length === 0
				? `<p class="p-10 italic text-gray-500 dark:text-gray-400">No open positions right now; check back soon.</p>`
				: `<div class="rounded border border-gray-200 dark:border-gray-700 divide-y divide-gray-200 dark:divide-gray-700">${rows}</div>`
		}
  </main>`;

	return page('open positions', body);
}

function jobPage(posting: Posting): string {
	const bodyHtml = marked.parse(posting.body) as string;

	const body = `
  <main class="max-w-[90ch] mx-auto w-full px-4 py-10 flex-1">
    <div class="mb-2">
      <a href="/" class="text-sm text-gray-500 dark:text-gray-400 hover:underline no-underline">← All positions</a>
    </div>

    <header class="mb-8 not-prose">
      <p class="text-sm text-gray-500 dark:text-gray-400 mb-1">${escapeHtml(posting.location)} · ${escapeHtml(posting.type)}${posting.salary ? ` · ${escapeHtml(posting.salary)}` : ''}</p>
      <h1 class="text-2xl font-bold dark:text-white">${escapeHtml(posting.title)}</h1>
    </header>

    <div class="prose dark:prose-invert prose-headings:no-underline text-[15px] w-full max-w-none mb-10">
      ${bodyHtml}
    </div>

    <div class="border-t border-gray-200 dark:border-gray-700 pt-8">
      <h2 class="font-bold text-lg dark:text-white mb-1">Apply for this role</h2>
      <p class="text-base text-gray-500 dark:text-gray-400 mb-6">We read every application; if there's a fit, we'll be in touch via email.</p>
      ${applyForm(posting.slug)}
    </div>
  </main>`;

	return page(posting.title, body);
}

function applyForm(slug: string, error?: string): string {
	const errorHtml = error
		? `<div class="mb-6 rounded border border-red-200 dark:border-red-800 bg-red-50 dark:bg-red-900/20 px-4 py-3 text-sm text-red-600 dark:text-red-400">${escapeHtml(error)}</div>`
		: '';

	const inputClass = 'block w-full rounded p-3 bg-gray-50 dark:bg-gray-800 dark:text-white border border-gray-300 dark:border-gray-600 focus:outline-none focus:ring-1 focus:ring-gray-400 dark:focus:ring-gray-500';
	const selectClass = `${inputClass} appearance-none`;

	return `
    ${errorHtml}
    <form method="POST" action="/${slug}/apply" id="apply-form" class="space-y-4" enctype="multipart/form-data">
      <div class="grid grid-cols-1 sm:grid-cols-2 gap-4">
        <div>
          <label for="first_name">First name <span class="text-red-400">*</span></label>
          <input type="text" id="first_name" name="first_name" required placeholder="Jane" autocomplete="given-name" class="${inputClass}">
        </div>
        <div>
          <label for="last_name">Last name <span class="text-red-400">*</span></label>
          <input type="text" id="last_name" name="last_name" required placeholder="Smith" autocomplete="family-name" class="${inputClass}">
        </div>
      </div>

      <div class="grid grid-cols-1 sm:grid-cols-2 gap-4">
        <div>
          <label for="country">Country <span class="text-red-400">*</span></label>
          <select id="country" name="country" required class="${selectClass}">
            <option value="">Select a country…</option>
            ${COUNTRIES.map((c) => `<option value="${escapeHtml(c.name)}">${escapeHtml(c.name)} ${getEmojiFlag(c.code as Parameters<typeof getEmojiFlag>[0])}</option>`).join('')}
          </select>
        </div>
        <div>
          <label for="city">City <span class="text-red-400">*</span></label>
          <input type="text" id="city" name="city" required placeholder="Your city" autocomplete="address-level2" class="${inputClass}">
        </div>
      </div>

      <div>
        <label for="email">Email <span class="text-red-400">*</span></label>
        <input type="email" id="email" name="email" required placeholder="you@example.com" autocomplete="email" class="${inputClass}">
      </div>

      <div class="grid grid-cols-1 sm:grid-cols-2 gap-4">
        <div>
          <label for="portfolio">Portfolio <span class="text-gray-400 font-normal">(optional)</span></label>
          <input type="url" id="portfolio" name="portfolio" placeholder="https://…" class="${inputClass}">
        </div>
        <div>
          <label for="linkedin">LinkedIn <span class="text-gray-400 font-normal">(optional)</span></label>
          <input type="url" id="linkedin" name="linkedin" placeholder="https://linkedin.com/in/…" class="${inputClass}">
        </div>
      </div>

      <div>
        <label for="resume">Résumé / CV <span class="text-red-400">*</span> <span class="text-gray-400 font-normal">(PDF)</span></label>
        <input type="file" id="resume" name="resume" accept=".pdf,application/pdf" required class="${inputClass}">
      </div>

      <div>
        <label for="cover">Cover letter <span class="text-red-400">*</span></label>
        <textarea id="cover" name="cover" required rows="10" placeholder="Tell us about yourself and why you want to work at Tangled." class="${inputClass}"></textarea>
      </div>

      <div class="flex items-center gap-4 pt-2">
        <button type="submit" class="btn-create" id="submit-btn">Submit application →</button>
      </div>
      <p class="text-sm text-gray-400 dark:text-gray-500 mt-6">By submitting this form you agree to your data being processed by our subprocessors: Cloudflare and Linear.</p>
    </form>
    <script>
      document.getElementById('apply-form').addEventListener('submit', function() {
        var btn = document.getElementById('submit-btn');
        btn.disabled = true;
        btn.textContent = 'Submitting…';
      });
    </script>`;
}

function successPage(firstName: string, posting: Posting): string {
	const body = `
  <main class="max-w-[90ch] mx-auto w-full px-4 py-16 text-center">
    <div class="mb-6 text-5xl">🎉</div>
    <h1 class="text-2xl font-bold dark:text-white mb-3">Application received</h1>
    <p class="text-gray-500 dark:text-gray-400 max-w-md mx-auto mb-8">
      Thanks, ${escapeHtml(firstName)}! We've received your application for <strong class="text-gray-700 dark:text-gray-300">${escapeHtml(posting.title)}</strong> and will be in touch soon.
    </p>
    <a href="/" class="btn-create">← View all positions</a>
  </main>`;

	return page('application received', body);
}

// ---------------------------------------------------------------------------
// Linear
// ---------------------------------------------------------------------------

// Upload a file to Linear's asset storage and return the public asset URL.
async function uploadToLinear(env: Env, file: File): Promise<string> {
	const query = `
    mutation FileUpload($contentType: String!, $filename: String!, $size: Int!) {
      fileUpload(contentType: $contentType, filename: $filename, size: $size) {
        uploadFile {
          uploadUrl
          assetUrl
          headers { key value }
        }
      }
    }`;

	const metaResp = await fetch('https://api.linear.app/graphql', {
		method: 'POST',
		headers: { Authorization: env.LINEAR_API_KEY, 'Content-Type': 'application/json' },
		body: JSON.stringify({
			query,
			variables: { contentType: file.type, filename: file.name, size: file.size },
		}),
	});

	const meta = (await metaResp.json()) as {
		data?: { fileUpload?: { uploadFile?: { uploadUrl: string; assetUrl: string; headers: { key: string; value: string }[] } } };
		errors?: unknown[];
	};

	const uploadFile = meta.data?.fileUpload?.uploadFile;
	if (!uploadFile || meta.errors) {
		throw new Error(`Linear fileUpload error: ${JSON.stringify(meta.errors ?? meta)}`);
	}

	const uploadHeaders: Record<string, string> = { 'Content-Type': file.type };
	for (const { key, value } of uploadFile.headers) uploadHeaders[key] = value;

	const uploadResp = await fetch(uploadFile.uploadUrl, {
		method: 'PUT',
		headers: uploadHeaders,
		body: await file.arrayBuffer(),
	});

	if (!uploadResp.ok) {
		throw new Error(`Resume upload failed: ${uploadResp.status} ${uploadResp.statusText}`);
	}

	return uploadFile.assetUrl;
}

async function getOrCreateLabel(env: Env, name: string): Promise<string | null> {
	try {
		const searchQuery = `
      query Labels($teamId: ID!) {
        issueLabels(filter: { team: { id: { eq: $teamId } } }) {
          nodes { id name }
        }
      }`;

		const searchResp = await fetch('https://api.linear.app/graphql', {
			method: 'POST',
			headers: { Authorization: env.LINEAR_API_KEY, 'Content-Type': 'application/json' },
			body: JSON.stringify({ query: searchQuery, variables: { teamId: env.LINEAR_TEAM_ID } }),
		});

		const searchResult = (await searchResp.json()) as {
			data?: { issueLabels?: { nodes: { id: string; name: string }[] } };
		};

		const existing = searchResult.data?.issueLabels?.nodes.find((l) => l.name === name);
		if (existing) return existing.id;

		const createQuery = `
      mutation LabelCreate($input: IssueLabelCreateInput!) {
        issueLabelCreate(input: $input) {
          success
          issueLabel { id }
        }
      }`;

		const createResp = await fetch('https://api.linear.app/graphql', {
			method: 'POST',
			headers: { Authorization: env.LINEAR_API_KEY, 'Content-Type': 'application/json' },
			body: JSON.stringify({ query: createQuery, variables: { input: { name, teamId: env.LINEAR_TEAM_ID } } }),
		});

		const created = (await createResp.json()) as {
			data?: { issueLabelCreate?: { success: boolean; issueLabel: { id: string } } };
		};

		return created.data?.issueLabelCreate?.issueLabel?.id ?? null;
	} catch (err) {
		console.error('Failed to get/create label:', err);
		return null;
	}
}

async function createLinearIssue(
	env: Env,
	posting: Posting,
	data: {
		firstName: string;
		lastName: string;
		email: string;
		country: string;
		city: string;
		portfolio: string;
		linkedin: string;
		cover: string;
		resume: File | null;
	},
): Promise<void> {
	const fullName = `${data.firstName} ${data.lastName}`;
	const title = fullName;

	const [resumeUrl, labelId] = await Promise.all([
		data.resume ? uploadToLinear(env, data.resume) : Promise.resolve(null),
		getOrCreateLabel(env, posting.title),
	]);

	const description = [
		`**Name:** ${fullName}`,
		`**Email:** ${data.email}`,
		`**Location:** ${data.city}, ${data.country}`,
		`**Role:** ${posting.title}`,
		data.portfolio ? `**Portfolio:** ${data.portfolio}` : null,
		data.linkedin ? `**LinkedIn:** ${data.linkedin}` : null,
		resumeUrl ? `**Résumé:** [${data.resume!.name}](${resumeUrl})` : null,
		'',
		'---',
		'',
		'## Cover letter',
		'',
		data.cover,
	]
		.filter((l): l is string => l !== null)
		.join('\n');

	const query = `
    mutation CreateIssue($input: IssueCreateInput!) {
      issueCreate(input: $input) {
        success
        issue { id identifier }
      }
    }`;

	const input: Record<string, unknown> = { title, description, teamId: env.LINEAR_TEAM_ID };
	if (labelId) input.labelIds = [labelId];

	const resp = await fetch('https://api.linear.app/graphql', {
		method: 'POST',
		headers: { Authorization: env.LINEAR_API_KEY, 'Content-Type': 'application/json' },
		body: JSON.stringify({ query, variables: { input } }),
	});

	const result = (await resp.json()) as { data?: { issueCreate?: { success: boolean } }; errors?: unknown[] };

	if (result.errors || !result.data?.issueCreate?.success) {
		throw new Error(`Linear error: ${JSON.stringify(result.errors ?? result)}`);
	}
}

// ---------------------------------------------------------------------------
// Router
// ---------------------------------------------------------------------------
export default {
	async fetch(request: Request, env: Env): Promise<Response> {
		const url = new URL(request.url);
		const parts = url.pathname.replace(/^\//, '').split('/');

		// GET /favicon.svg
		if (request.method === 'GET' && url.pathname === '/favicon.svg') {
			return new Response(DOLLY_SVG, { headers: { 'Content-Type': 'image/svg+xml' } });
		}

		// GET / — listings
		if (request.method === 'GET' && url.pathname === '/') {
			return html(listingsPage());
		}

		// GET /:slug — job posting
		if (request.method === 'GET' && parts.length === 1 && parts[0]) {
			const posting = POSTINGS_BY_SLUG.get(parts[0]);
			if (!posting) return notFound();
			return html(jobPage(posting));
		}

		// POST /:slug/apply — submit application
		if (request.method === 'POST' && parts.length === 2 && parts[1] === 'apply') {
			const posting = POSTINGS_BY_SLUG.get(parts[0]);
			if (!posting) return notFound();

			let formData: FormData;
			try {
				formData = await request.formData();
			} catch {
				return html(jobPage(posting), 400);
			}

			const firstName = (formData.get('first_name') as string | null)?.trim() ?? '';
			const lastName = (formData.get('last_name') as string | null)?.trim() ?? '';
			const email = (formData.get('email') as string | null)?.trim() ?? '';
			const country = (formData.get('country') as string | null)?.trim() ?? '';
			const city = (formData.get('city') as string | null)?.trim() ?? '';
			const portfolio = (formData.get('portfolio') as string | null)?.trim() ?? '';
			const linkedin = (formData.get('linkedin') as string | null)?.trim() ?? '';
			const cover = (formData.get('cover') as string | null)?.trim() ?? '';
			const resumeEntry = formData.get('resume');
			const resume =
				resumeEntry instanceof File && resumeEntry.size > 0 && resumeEntry.type === 'application/pdf'
					? resumeEntry
					: null;

			if (!firstName || !lastName || !email || !country || !city || !cover || !resume) {
				const bodyHtml = marked.parse(posting.body) as string;
				const body = `
          <main class="max-w-[90ch] mx-auto w-full px-4 py-10 flex-1">
            <div class="mb-2">
              <a href="/" class="text-sm text-gray-500 dark:text-gray-400 hover:underline no-underline">← All positions</a>
            </div>
            <header class="mb-8 not-prose">
              <p class="text-sm text-gray-500 dark:text-gray-400 mb-1">${escapeHtml(posting.location)} · ${escapeHtml(posting.type)}${posting.salary ? ` · ${escapeHtml(posting.salary)}` : ''}</p>
              <h1 class="text-2xl font-bold dark:text-white">${escapeHtml(posting.title)}</h1>
            </header>
            <div class="prose dark:prose-invert prose-headings:no-underline text-[15px] w-full max-w-none mb-10">${bodyHtml}</div>
            <div class="border-t border-gray-200 dark:border-gray-700 pt-8">
              <h2 class="font-bold text-lg dark:text-white mb-1">Apply for this role</h2>
              <p class="text-base text-gray-500 dark:text-gray-400 mb-6">We read every application; if there's a fit, we'll be in touch via email.</p>
              ${applyForm(posting.slug, 'Please fill in all required fields.')}
            </div>
          </main>`;
				return html(page(posting.title, body), 400);
			}

			try {
				await createLinearIssue(env, posting, { firstName, lastName, email, country, city, portfolio, linkedin, cover, resume });
				return html(successPage(firstName, posting));
			} catch (err) {
				console.error('Linear issue creation failed:', err);
				const bodyHtml = marked.parse(posting.body) as string;
				const body = `
          <main class="max-w-[90ch] mx-auto w-full px-4 py-10 flex-1">
            <div class="mb-2">
              <a href="/" class="text-sm text-gray-500 dark:text-gray-400 hover:underline no-underline">← All positions</a>
            </div>
            <header class="mb-8 not-prose">
              <p class="text-sm text-gray-500 dark:text-gray-400 mb-1">${escapeHtml(posting.location)} · ${escapeHtml(posting.type)}${posting.salary ? ` · ${escapeHtml(posting.salary)}` : ''}</p>
              <h1 class="text-2xl font-bold dark:text-white">${escapeHtml(posting.title)}</h1>
            </header>
            <div class="prose dark:prose-invert prose-headings:no-underline text-[15px] w-full max-w-none mb-10">${bodyHtml}</div>
            <div class="border-t border-gray-200 dark:border-gray-700 pt-8">
              <h2 class="font-bold text-lg dark:text-white mb-1">Apply for this role</h2>
              <p class="text-base text-gray-500 dark:text-gray-400 mb-6">We read every application; if there's a fit, we'll be in touch via email.</p>
              ${applyForm(posting.slug, 'Something went wrong submitting your application. Please try again.')}
            </div>
          </main>`;
				return html(page(posting.title, body), 500);
			}
		}

		return notFound();
	},
} satisfies ExportedHandler<Env>;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------
function html(body: string, status = 200): Response {
	return new Response(body, { status, headers: { 'Content-Type': 'text/html; charset=utf-8' } });
}

function notFound(): Response {
	return new Response('not found', { status: 404 });
}

function escapeHtml(str: string): string {
	return str.replace(/&/g, '&amp;').replace(/</g, '&lt;').replace(/>/g, '&gt;').replace(/"/g, '&quot;').replace(/'/g, '&#39;');
}
