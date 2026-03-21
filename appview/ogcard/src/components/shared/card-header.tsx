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
  return (
    <Row style={{ gap: 16 }}>
      <Avatar src={avatarUrl} size={64} />
      <span style={{ ...TYPOGRAPHY.cardHeader, color: "#000000" }}>
        {ownerHandle} / {repoName}
      </span>
    </Row>
  );
}
