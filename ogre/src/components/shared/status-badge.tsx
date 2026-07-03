import { Row } from "./layout";
import {
  CircleDot,
  Ban,
  GitPullRequest,
  GitPullRequestClosed,
  GitMerge,
} from "../../icons/lucide";
import { COLORS, TYPOGRAPHY } from "./constants";

function capitalize(text: string) {
  return text.charAt(0).toUpperCase() + text.slice(1);
}

const STATUS_CONFIG = {
  open: {
    Icon: CircleDot,
    bg: COLORS.status.open.bg,
    text: COLORS.status.open.text,
  },
  closed: {
    Icon: Ban,
    bg: COLORS.status.closed.bg,
    text: COLORS.status.closed.text,
  },
  merged: {
    Icon: GitMerge,
    bg: COLORS.status.merged.bg,
    text: COLORS.status.merged.text,
  },
} as const;

interface StatusBadgeProps {
  status: "open" | "closed" | "merged";
}

export function StatusBadge({ status }: StatusBadgeProps) {
  const config =
    status === "merged"
      ? STATUS_CONFIG.merged
      : status === "closed"
        ? STATUS_CONFIG.closed
        : STATUS_CONFIG.open;
  const Icon = config.Icon;

  return (
    <Row
      style={{
        gap: 12,
        padding: "14px 26px 14px 24px",
        borderRadius: 18,
        backgroundColor: config.bg,
      }}>
      <Icon size={48} color={config.text} />
      <span style={{ ...TYPOGRAPHY.status, color: config.text }}>{capitalize(status)}</span>
    </Row>
  );
}

export function IssueStatusBadge({ status }: { status: "open" | "closed" }) {
  const config =
    status === "closed" ? STATUS_CONFIG.closed : STATUS_CONFIG.open;
  const Icon = config.Icon;

  return (
    <Row
      style={{
        gap: 12,
        padding: "14px 26px 14px 24px",
        borderRadius: 18,
        backgroundColor: config.bg,
      }}>
      <Icon size={48} color={config.text} />
      <span style={{ ...TYPOGRAPHY.status, color: config.text }}>{capitalize(status)}</span>
    </Row>
  );
}
