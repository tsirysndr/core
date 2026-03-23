import type { ComponentChildren } from "preact";

interface StyleProps {
  style?: Record<string, string | number>;
  children?: ComponentChildren;
}

export function Card({ children, style }: StyleProps) {
  return (
    <div
      style={{
        width: 1200,
        height: 630,
        background: "white",
        display: "flex",
        flexDirection: "column",
        padding: 48,
        ...style,
      }}>
      {children}
    </div>
  );
}

export function Row({ children, style }: StyleProps) {
  return (
    <div
      style={{
        display: "flex",
        flexDirection: "row",
        alignItems: "center",
        ...style,
      }}>
      {children}
    </div>
  );
}

export function Col({ children, style }: StyleProps) {
  return (
    <div style={{ display: "flex", flexDirection: "column", ...style }}>
      {children}
    </div>
  );
}
