import { Row } from "./layout";
import { TYPOGRAPHY } from "./constants";
import type { LucideIcon } from "../../icons/lucide";

interface StatItemProps {
  Icon: LucideIcon;
  value: string | number;
}

export function StatItem({ Icon, value }: StatItemProps) {
  return (
    <Row style={{ gap: 16 }}>
      <Icon size={36} color="#404040" />
      <span style={{ ...TYPOGRAPHY.body, color: "#404040" }}>{value}</span>
    </Row>
  );
}
