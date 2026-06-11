import { Row } from "./layout";
import { Avatar } from "./avatar";
import { TYPOGRAPHY } from "./constants";

interface CardHeaderProps {
  avatarUrl: string;
  ownerHandle: string;
  repoName: string;
}

export function CardHeader({
  avatarUrl,
  ownerHandle,
  repoName,
}: CardHeaderProps) {
  const text = `${ownerHandle} / ${repoName}`;
  const BASE_SIZE = TYPOGRAPHY.cardHeader.fontSize;
  const MAX_CHARS = 28;
  const fontSize =
    text.length > MAX_CHARS
      ? Math.max(28, Math.floor((BASE_SIZE * MAX_CHARS) / text.length))
      : BASE_SIZE;

  return (
    <Row style={{ gap: 16 }}>
      <Avatar src={avatarUrl} size={64} />
      <span style={{ ...TYPOGRAPHY.cardHeader, fontSize, color: "#000000" }}>
        {ownerHandle} / {repoName}
      </span>
    </Row>
  );
}
