import { Row } from "./layout";
import { Calendar, MessageSquare, SmilePlus } from "../../icons/lucide";
import { StatItem } from "./stat-item";

interface FooterStatsProps {
  createdAt: string;
  reactionCount?: number;
  commentCount?: number;
}

export function FooterStats({
  createdAt,
  reactionCount,
  commentCount,
}: FooterStatsProps) {
  const formattedDate = new Intl.DateTimeFormat("en-GB", {
    day: "numeric",
    month: "short",
    year: "numeric",
  }).format(new Date(createdAt));

  return (
    <Row style={{ gap: 64 }}>
      <StatItem Icon={Calendar} value={formattedDate} />
      {reactionCount ? (
        <StatItem Icon={SmilePlus} value={reactionCount} />
      ) : null}
      {commentCount ? (
        <StatItem Icon={MessageSquare} value={commentCount} />
      ) : null}
    </Row>
  );
}
