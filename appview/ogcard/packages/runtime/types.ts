export interface FontData {
  name: string;
  data: ArrayBuffer;
  weight: 100 | 200 | 300 | 400 | 500 | 600 | 700 | 800 | 900;
  style: "normal" | "italic";
}

export type SatoriFn = typeof import("satori").default;

export type ResvgClass = typeof import("@resvg/resvg-wasm").Resvg;
