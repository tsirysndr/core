/**
 * Cloudflare Workers runtime implementation
 * Uses ?module suffix for WASM imports as required by Wrangler
 */
import type { FontData, SatoriFn, ResvgClass } from "./types";

import inter400 from "@fontsource/inter/files/inter-latin-400-normal.woff";
import inter500 from "@fontsource/inter/files/inter-latin-500-normal.woff";
import inter600 from "@fontsource/inter/files/inter-latin-600-normal.woff";

let satoriFn: SatoriFn | null = null;
let resvgInitialized = false;
let Resvg: ResvgClass | null = null;

export async function initSatori(): Promise<SatoriFn> {
  if (satoriFn) return satoriFn;

  const { default: satori, init } = await import("satori/standalone");
  const wasmModule = (await import("satori/yoga.wasm?module")).default;
  await init(wasmModule);
  satoriFn = satori;

  return satoriFn;
}

export async function initResvg(): Promise<ResvgClass> {
  if (resvgInitialized) return Resvg!;

  const { Resvg: ResvgClass, initWasm } = await import("@resvg/resvg-wasm");
  const wasmModule = (await import("@resvg/resvg-wasm/index_bg.wasm?module"))
    .default;
  await initWasm(wasmModule);

  Resvg = ResvgClass;
  resvgInitialized = true;
  return Resvg;
}

export async function loadFonts(): Promise<FontData[]> {
  return [
    {
      name: "Inter",
      data: inter400 as ArrayBuffer,
      weight: 400,
      style: "normal",
    },
    {
      name: "Inter",
      data: inter500 as ArrayBuffer,
      weight: 500,
      style: "normal",
    },
    {
      name: "Inter",
      data: inter600 as ArrayBuffer,
      weight: 600,
      style: "normal",
    },
  ];
}
