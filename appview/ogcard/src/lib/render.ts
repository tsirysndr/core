import type { VNode } from "preact";
import { initSatori, initResvg, loadFonts } from "@tangled/ogcard-runtime";
import type { ResvgClass } from "@tangled/ogcard-runtime/types";

let satoriFn: typeof import("satori").default | null = null;
let Resvg: ResvgClass | null = null;
let fontsLoaded = false;
let cachedFonts: Awaited<ReturnType<typeof loadFonts>> | null = null;

export interface RenderResult {
  svg: string;
  png: Uint8Array;
}

export async function renderCard(component: VNode): Promise<RenderResult> {
  if (!satoriFn) {
    satoriFn = await initSatori();
  }

  if (!Resvg) {
    Resvg = await initResvg();
  }

  if (!fontsLoaded) {
    cachedFonts = await loadFonts();
    fontsLoaded = true;
  }

  const svg = await satoriFn(component as any, {
    width: 1200,
    height: 630,
    fonts: cachedFonts!,
    embedFont: true,
  });

  const resvg = new Resvg!(svg, {
    fitTo: { mode: "width", value: 1200 },
  });

  const pngData = resvg.render();

  return {
    svg,
    png: pngData.asPng(),
  };
}
