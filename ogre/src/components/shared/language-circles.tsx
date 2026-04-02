import type { Language } from "../../validation";

interface LanguageCirclesProps {
  languages: Language[];
  width: number;
}

export function LanguageCircles({ languages, width }: LanguageCirclesProps) {
  const MAX_RADIUS = width || 100;

  const sortedLanguages = [...languages]
    .sort((a, b) => b.percentage - a.percentage)
    .slice(0, 5);

  let cumulativePercentage = 0;
  const circles: { color: string; radius: number }[] = [];

  for (const lang of sortedLanguages) {
    // Radius decreases as we go inward, but ring area is proportional to percentage
    // Using sqrt to make area (πr²) proportional to remaining percentage
    const radius = Math.max(
      1,
      Math.round(MAX_RADIUS * Math.sqrt(1 - cumulativePercentage / 100) * 100) /
        100,
    );
    circles.push({ color: lang.color, radius });
    cumulativePercentage += lang.percentage;
  }

  return (
    <svg
      width={MAX_RADIUS}
      height={MAX_RADIUS}
      viewBox={`0 0 ${MAX_RADIUS * 2} ${MAX_RADIUS * 2}`}>
      {circles.map((circle, i) => (
        <circle
          key={i}
          cx="50%"
          cy="50%"
          r={circle.radius}
          fill={circle.color}
        />
      ))}
    </svg>
  );
}
