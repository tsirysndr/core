import type { Language } from "../../validation";

interface LanguageCirclesProps {
  languages: Language[];
}

const MAX_RADIUS = 380;

function percentageToThickness(percentage: number): number {
  return (percentage / 100) * MAX_RADIUS;
}

export function LanguageCircles({ languages }: LanguageCirclesProps) {
  const sortedLanguages = [...languages]
    .sort((a, b) => b.percentage - a.percentage)
    .slice(0, 5)
    .reverse();

  let cumulativeRadius = 0;

  return (
    <div
      style={{
        position: "absolute",
        right: -MAX_RADIUS,
        top: -MAX_RADIUS,
        width: MAX_RADIUS * 2,
        height: MAX_RADIUS * 2,
        display: "flex",
      }}>
      {sortedLanguages.map((lang, i) => {
        const thickness = percentageToThickness(lang.percentage);
        const contentSize = cumulativeRadius * 2;

        cumulativeRadius += thickness;

        return (
          <div
            key={i}
            style={{
              position: "absolute",
              left: "50%",
              top: "50%",
              transform: "translate(-50%, -50%)",
              width: contentSize,
              height: contentSize,
              borderRadius: "50%",
              border: `${thickness}px solid ${lang.color}`,
              boxSizing: "content-box",
            }}
          />
        );
      })}
    </div>
  );
}
