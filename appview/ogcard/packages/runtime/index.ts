/**
 * Bun/Node.js runtime implementation
 * Uses filesystem APIs to load WASM and fonts
 */
import { readFile } from "node:fs/promises";
import { createRequire } from "node:module";
import type { FontData, SatoriFn, ResvgClass } from "./types";

const require = createRequire(import.meta.url);

let satoriFn: SatoriFn | null = null;
let resvgInitialized = false;
let Resvg: ResvgClass | null = null;

export async function initSatori(): Promise<SatoriFn> {
  if (satoriFn) return satoriFn;

  const { default: satori } = await import("satori");
  satoriFn = satori;

  return satoriFn;
}

export async function initResvg(): Promise<ResvgClass> {
  if (resvgInitialized) return Resvg!;

  const { Resvg: ResvgClass, initWasm } = await import("@resvg/resvg-wasm");
  const wasmPath = require.resolve("@resvg/resvg-wasm/index_bg.wasm");
  const wasmBuffer = await readFile(wasmPath);
  await initWasm(wasmBuffer);

  Resvg = ResvgClass;
  resvgInitialized = true;
  return Resvg;
}

export async function loadFonts(): Promise<FontData[]> {
  // In Bun, .woff imports return a Module object with `default` being the file path
  const inter400Module = await import(
    "@fontsource/inter/files/inter-latin-400-normal.woff"
  );
  const inter500Module = await import(
    "@fontsource/inter/files/inter-latin-500-normal.woff"
  );
  const inter600Module = await import(
    "@fontsource/inter/files/inter-latin-600-normal.woff"
  );

  const inter400Path = (inter400Module as { default: string }).default;
  const inter500Path = (inter500Module as { default: string }).default;
  const inter600Path = (inter600Module as { default: string }).default;

  const [buf400, buf500, buf600] = await Promise.all([
    readFile(inter400Path),
    readFile(inter500Path),
    readFile(inter600Path),
  ]);

  return [
    {
      name: "Inter",
      data: buf400.buffer.slice(
        buf400.byteOffset,
        buf400.byteOffset + buf400.byteLength,
      ),
      weight: 400,
      style: "normal",
    },
    {
      name: "Inter",
      data: buf500.buffer.slice(
        buf500.byteOffset,
        buf500.byteOffset + buf500.byteLength,
      ),
      weight: 500,
      style: "normal",
    },
    {
      name: "Inter",
      data: buf600.buffer.slice(
        buf600.byteOffset,
        buf600.byteOffset + buf600.byteLength,
      ),
      weight: 600,
      style: "normal",
    },
  ];
}
