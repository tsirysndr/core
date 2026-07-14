// atfeed.js — punchcard profile effect
//
// Swaps the punchcard for a small RSS-style feed of the profile owner's
// atproto records: Leaflet documents (both the original pub.leaflet.* and
// the newer site.standard.* lexicons) and Bluesky posts, merged and sorted
// newest-first. The profile owner's DID is read off the page itself (the
// followers span carries it as data-followers-did). Everything is fetched
// client-side from the public PDS; if anything fails, the punchcard is
// simply left alone.

const DID_SELECTOR = "#followers[data-followers-did]";
const PLC_DIRECTORY = "https://plc.directory";

const FEED_LIMIT = 12; // entries shown
const PER_COLLECTION = 25; // records fetched per collection
const SNIPPET_LEN = 140; // max chars for post/description text
const STAGGER_MS = 45; // per-entry entrance delay

const DARK_QUERY = "(prefers-color-scheme: dark)";
const REDUCED_QUERY = "(prefers-reduced-motion: reduce)";

// Collections worth putting in a feed, with how to read each one.
// `docs` sources look up their publication record to build a public URL.
const SOURCES = {
  "pub.leaflet.document": { label: "leaflet", kind: "doc", pubCollection: "pub.leaflet.publication" },
  "site.standard.document": { label: "leaflet", kind: "doc", pubCollection: "site.standard.publication" },
  "app.bsky.feed.post": { label: "bsky", kind: "post" },
};

const PALETTES = {
  light: {
    text: "rgb(28 25 23)",
    muted: "rgb(120 113 108)",
    accent: "rgb(234 88 12)", // RSS orange
    border: "rgb(231 229 228)",
    tagBg: "rgb(245 245 244)",
  },
  dark: {
    text: "rgb(231 229 228)",
    muted: "rgb(168 162 158)",
    accent: "rgb(251 146 60)",
    border: "rgb(68 64 60)",
    tagBg: "rgb(41 37 36)",
  },
};

// --- fetching -------------------------------------------------------------

const getJson = async (url, signal) => {
  const res = await fetch(url, { signal });
  if (!res.ok) throw new Error(`${res.status} ${url}`);
  return res.json();
};

const xrpc = (pds, method, params) =>
  `${pds}/xrpc/${method}?${new URLSearchParams(params)}`;

const resolveIdentity = async (did, signal) => {
  const doc = await getJson(`${PLC_DIRECTORY}/${did}`, signal);
  const svc = (doc.service || []).find(
    (s) => s.id === "#atproto_pds" || s.type === "AtprotoPersonalDataServer",
  );
  if (!svc) throw new Error("no PDS in DID document");
  const aka = (doc.alsoKnownAs || [])[0] || "";
  return {
    pds: svc.serviceEndpoint,
    handle: aka.startsWith("at://") ? aka.slice(5) : did,
  };
};

const listRecords = (pds, did, collection, limit, signal) =>
  getJson(
    xrpc(pds, "com.atproto.repo.listRecords", { repo: did, collection, limit }),
    signal,
  ).then((r) => r.records || []);

// --- record → feed entry ---------------------------------------------------

const rkeyOf = (uri) => uri.split("/").pop();

const truncate = (s) => {
  const t = (s || "").replace(/\s+/g, " ").trim();
  return t.length > SNIPPET_LEN ? `${t.slice(0, SNIPPET_LEN - 1)}…` : t;
};

const ensureHttps = (u) => (/^https?:\/\//.test(u) ? u : `https://${u}`);

// Public URL for a document, via its publication's url/base_path.
const docUrl = (record, pubMap) => {
  const v = record.value;
  if (typeof v.url === "string") return ensureHttps(v.url);
  const base = pubMap.get(v.publication);
  if (base) return `${ensureHttps(base).replace(/\/+$/, "")}/${rkeyOf(record.uri)}`;
  return `https://pdsls.dev/at://${record.uri.slice(5)}`; // browse the raw record
};

const toEntry = (record, source, pubMap, did) => {
  const v = record.value || {};
  const when = new Date(v.publishedAt || v.createdAt || 0);
  if (source.kind === "doc") {
    return {
      when,
      label: source.label,
      title: v.title || v.name || "(untitled document)",
      snippet: truncate(v.description),
      href: docUrl(record, pubMap),
    };
  }
  // Bluesky post: skip replies, use the text as the body.
  if (v.reply) return null;
  return {
    when,
    label: source.label,
    title: truncate(v.text) || "(media post)",
    snippet: "",
    href: `https://witchsky.app/profile/${did}/post/${rkeyOf(record.uri)}`,
  };
};

const loadFeed = async (did, signal) => {
  const identity = await resolveIdentity(did, signal);
  const { pds } = identity;

  // Only query collections that actually exist in the repo.
  const info = await getJson(
    xrpc(pds, "com.atproto.repo.describeRepo", { repo: did }),
    signal,
  );
  const present = new Set(info.collections || []);
  const wanted = Object.keys(SOURCES).filter((c) => present.has(c));
  if (wanted.length === 0) throw new Error("no feed-worthy collections");

  // Publication records (small collections) → map at-uri → base url.
  const pubCollections = [
    ...new Set(
      wanted
        .map((c) => SOURCES[c].pubCollection)
        .filter((c) => c && present.has(c)),
    ),
  ];
  const pubMap = new Map();
  const pubResults = await Promise.allSettled(
    pubCollections.map((c) => listRecords(pds, did, c, 50, signal)),
  );
  pubResults.forEach((r) => {
    if (r.status !== "fulfilled") return;
    r.value.forEach((rec) => {
      const base = rec.value?.url || rec.value?.base_path;
      if (base) pubMap.set(rec.uri, base);
    });
  });

  // The records themselves; PDS returns newest-first by default.
  const results = await Promise.allSettled(
    wanted.map((c) => listRecords(pds, did, c, PER_COLLECTION, signal)),
  );
  const entries = results.flatMap((r, i) =>
    r.status === "fulfilled"
      ? r.value
          .map((rec) => toEntry(rec, SOURCES[wanted[i]], pubMap, did))
          .filter(Boolean)
      : [],
  );
  entries.sort((a, b) => b.when - a.when);
  return { identity, entries: entries.slice(0, FEED_LIMIT) };
};

// --- rendering --------------------------------------------------------------

const relativeDate = (d) => {
  const s = (Date.now() - d.getTime()) / 1000;
  if (!Number.isFinite(s) || s < 0 || s > 30 * 86400) {
    return d.toLocaleDateString(undefined, { month: "short", day: "numeric", year: "numeric" });
  }
  if (s < 3600) return `${Math.max(1, Math.floor(s / 60))}m ago`;
  if (s < 86400) return `${Math.floor(s / 3600)}h ago`;
  return `${Math.floor(s / 86400)}d ago`;
};

const el = (tag, styles, text) => {
  const node = document.createElement(tag);
  Object.assign(node.style, styles);
  if (text != null) node.textContent = text; // records are untrusted: text only
  return node;
};

const render = (grid, { identity, entries }, colors, animate, height) => {
  const root = el("div", {
    fontSize: "13px",
    lineHeight: "1.45",
    color: colors.text,
    height: height ? `${height}px` : "auto",
    overflowY: "auto",
    paddingRight: "4px",
    boxSizing: "border-box",
  });
  root.setAttribute("data-punchcard-feed", "");

  const header = el("div", {
    display: "flex",
    alignItems: "center",
    gap: "6px",
    padding: "2px 0 8px",
    borderBottom: `1px solid ${colors.border}`,
    fontFamily: "ui-monospace, monospace",
    fontSize: "12px",
    color: colors.muted,
  });
  header.appendChild(el("span", {}, `@${identity.handle} · ${entries.length} entries`));
  root.appendChild(header);

  entries.forEach((entry, i) => {
    const row = el("div", {
      padding: "8px 0",
      borderBottom: `1px solid ${colors.border}`,
    });

    const meta = el("div", {
      display: "flex",
      alignItems: "baseline",
      gap: "8px",
      fontFamily: "ui-monospace, monospace",
      fontSize: "11px",
      color: colors.muted,
    });
    meta.appendChild(el("span", {}, relativeDate(entry.when)));
    meta.appendChild(
      el(
        "span",
        {
          background: colors.tagBg,
          border: `1px solid ${colors.border}`,
          borderRadius: "9999px",
          padding: "0 7px",
        },
        entry.label,
      ),
    );
    row.appendChild(meta);

    const link = el("a", {
      display: "block",
      marginTop: "2px",
      color: colors.text,
      textDecoration: "none",
      fontWeight: "550",
      overflowWrap: "anywhere",
    });
    link.href = entry.href;
    link.target = "_blank";
    link.rel = "noopener noreferrer";
    link.textContent = entry.title;
    link.addEventListener("pointerenter", () => {
      link.style.color = colors.accent;
      link.style.textDecoration = "underline";
    });
    link.addEventListener("pointerleave", () => {
      link.style.color = colors.text;
      link.style.textDecoration = "none";
    });
    row.appendChild(link);

    if (entry.snippet) {
      row.appendChild(el("div", { color: colors.muted, marginTop: "1px" }, entry.snippet));
    }

    if (animate) {
      row.style.opacity = "0";
      row.style.transform = "translateY(4px)";
      row.style.transition = "opacity 300ms ease, transform 300ms ease";
      row.style.transitionDelay = `${i * STAGGER_MS}ms`;
      requestAnimationFrame(() =>
        requestAnimationFrame(() => {
          row.style.opacity = "1";
          row.style.transform = "none";
        }),
      );
    }
    root.appendChild(row);
  });

  return root;
};

// --- lifecycle ---------------------------------------------------------------

const attach = (grid) => {
  // The profile page carries the owner's DID on the followers count.
  const did = document.querySelector(DID_SELECTOR)?.dataset.followersDid;
  if (!did || !did.startsWith("did:")) return () => {};

  const ac = new AbortController();
  const dark = matchMedia(DARK_QUERY);
  const reduced = matchMedia(REDUCED_QUERY).matches;

  let feedEl = null;
  let data = null;
  let gridHeight = 0; // measured before the grid is hidden
  const savedDisplay = grid.style.display;

  const mount = (animate) => {
    if (!data) return;
    if (!feedEl) gridHeight = grid.getBoundingClientRect().height;
    const colors = dark.matches ? PALETTES.dark : PALETTES.light;
    const next = render(grid, data, colors, animate && !reduced, gridHeight);
    if (feedEl) {
      feedEl.replaceWith(next);
    } else {
      grid.insertAdjacentElement("afterend", next);
      grid.style.display = "none"; // punchcard stays intact underneath
    }
    feedEl = next;
  };

  // Re-render with the other palette when the color scheme flips.
  dark.addEventListener("change", () => mount(false), { signal: ac.signal });

  loadFeed(did, ac.signal)
    .then((d) => {
      if (ac.signal.aborted || d.entries.length === 0) return;
      data = d;
      mount(true);
    })
    .catch(() => {
      /* network/CORS/empty repo: leave the punchcard as it was */
    });

  return () => {
    ac.abort();
    if (feedEl) feedEl.remove();
    grid.style.display = savedDisplay;
  };
};

let cleanup = null;
let current = null;

const init = () => {
  const grid = document.querySelector("[data-punchcard]");
  if (grid === current) return;
  if (cleanup) cleanup();
  current = grid;
  cleanup = grid ? attach(grid) : null;
};

init();
document.addEventListener("htmx:load", init);