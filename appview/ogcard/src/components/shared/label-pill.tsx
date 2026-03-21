import { Row } from "./layout";
import { COLORS, TYPOGRAPHY } from "./constants";

interface LabelPillProps {
  name: string;
  color: string;
}

function LabelPill({ name, color }: LabelPillProps) {
  return (
    <Row
      style={{
        gap: 16,
        padding: "16px 28px",
        borderRadius: 18,
        backgroundColor: "#fff",
        border: `4px solid ${COLORS.label.border}`,
      }}>
      <div
        style={{
          width: 24,
          height: 24,
          borderRadius: "50%",
          backgroundColor: color,
        }}
      />
      <span style={{ ...TYPOGRAPHY.body, color: COLORS.label.text }}>
        {name}
      </span>
    </Row>
  );
}

interface LabelListProps {
  labels: Array<{ name: string; color: string }>;
  max?: number;
}

export function LabelList({ labels, max = 5 }: LabelListProps) {
  if (labels.length === 0) return null;

  return (
    <Row style={{ gap: 12 }}>
      {labels.slice(0, max).map((label, i) => (
        <LabelPill key={i} name={label.name} color={label.color} />
      ))}
    </Row>
  );
}
