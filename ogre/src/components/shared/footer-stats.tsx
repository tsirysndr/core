import { Row } from "./layout";
import { Calendar, MessageSquare, SmilePlus } from "../../icons/lucide";
import { StatItem } from "./stat-item";
import { Avatar } from "./avatar";
import { TYPOGRAPHY } from "./constants";

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
    const showReactions = handleLength <= 16;
    const showComments = handleLength <= 11;

    return (
        <Row style={{ gap: 40 }}>
            {authorHandle && authorAvatarUrl ? (
                <Row style={{ gap: 16, alignItems: "center" }}>
                    <Avatar src={authorAvatarUrl} size={40} />
                    <span
                        style={{
                            ...TYPOGRAPHY.body,
                            color: "#404040",
                            maxWidth: 400,
                            lineClamp: 1,
                            display: "block",
                        }}>
                        {authorHandle}
                    </span>
                </Row>
            ) : null}
            <StatItem Icon={Calendar} value={formattedDate} />
            {showReactions && reactionCount ? (
                <StatItem Icon={SmilePlus} value={reactionCount} />
            ) : null}
            {showComments && commentCount ? (
                <StatItem Icon={MessageSquare} value={commentCount} />
            ) : null}
        </Row>
    );
}
