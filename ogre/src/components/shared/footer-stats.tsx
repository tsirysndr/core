import { Row } from "./layout";
import { Calendar, MessageSquare, SmilePlus } from "../../icons/lucide";
import { StatItem } from "./stat-item";
import { Avatar } from "./avatar";
import { TYPOGRAPHY } from "./constants";

// Handles longer than this cause the footer to overflow when combined with
// other stats, so we drop the less-important reaction count first, then the
// comment count once the handle grows longer still.
const LONG_HANDLE_THRESHOLD = 20;
const VERY_LONG_HANDLE_THRESHOLD = 28;

interface FooterStatsProps {
  createdAt: string;
  authorHandle?: string;
  authorAvatarUrl?: string;
  reactionCount?: number;
  commentCount?: number;
}

export function FooterStats({
  createdAt,
  authorHandle,
  authorAvatarUrl,
  reactionCount,
  commentCount,
}: FooterStatsProps) {
  const formattedDate = new Intl.DateTimeFormat("en-GB", {
    day: "numeric",
    month: "short",
    year: "numeric",
  }).format(new Date(createdAt));

  const handleLength = authorHandle?.length ?? 0;
  // Long handles crowd the footer. Drop reactions first; drop comments too
  // for extremely long handles to prevent overflow past the tangled logo.
  const isLongHandle = handleLength > LONG_HANDLE_THRESHOLD;
  const isVeryLongHandle = handleLength > VERY_LONG_HANDLE_THRESHOLD;
  const gap = isLongHandle ? 40 : 64;
  const hideReactions = isLongHandle;
  const hideComments = isVeryLongHandle;

  return (
    <Row style={{ gap }}>
      {authorHandle && authorAvatarUrl ? (
        <Row style={{ gap: 16, alignItems: "center" }}>
          <Avatar src={authorAvatarUrl} size={40} />
          <span
            style={{
              ...TYPOGRAPHY.body,
              color: "#404040",
              maxWidth: 480,
              overflow: "hidden",
              textOverflow: "ellipsis",
              whiteSpace: "nowrap",
            }}>
            {authorHandle}
          </span>
        </Row>
      ) : null}
      <StatItem Icon={Calendar} value={formattedDate} />
      {reactionCount && !hideReactions ? (
        <StatItem Icon={SmilePlus} value={reactionCount} />
      ) : null}
      {commentCount && !hideComments ? (
        <StatItem Icon={MessageSquare} value={commentCount} />
      ) : null}
    </Row>
  );
}
