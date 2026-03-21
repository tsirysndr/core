import { Row, Col } from "./layout";
import { TYPOGRAPHY } from "./constants";
import {
  Star,
  GitPullRequest,
  CircleDot,
  type LucideIcon,
} from "../../icons/lucide";

interface MetricsProps {
  stars: number;
  pulls: number;
  issues: number;
}

// Display stars, pulls, issues with Lucide icons
export function Metrics({ stars, pulls, issues }: MetricsProps) {
  return (
    <Row style={{ gap: 56, alignItems: "flex-start" }}>
      <MetricItem value={stars} label="stars" Icon={Star} />
      <MetricItem value={pulls} label="pulls" Icon={GitPullRequest} />
      <MetricItem value={issues} label="issues" Icon={CircleDot} />
    </Row>
  );
}

interface MetricItemProps {
  value: number;
  label: string;
  Icon: LucideIcon;
}

function MetricItem({ value, label, Icon }: MetricItemProps) {
  return (
    <Col style={{ gap: 12 }}>
      <Row style={{ gap: 12, alignItems: "center" }}>
        <span style={TYPOGRAPHY.metricValue}>{value}</span>
        <Icon size={48} />
      </Row>
      <span style={{ ...TYPOGRAPHY.label, opacity: 0.75 }}>{label}</span>
    </Col>
  );
}
