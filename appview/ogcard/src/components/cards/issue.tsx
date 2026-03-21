import { Card, Row, Col } from "../shared/layout";
import { TangledLogo } from "../shared/logo";
import { IssueStatusBadge } from "../shared/status-badge";
import { CardHeader } from "../shared/card-header";
import { LabelList } from "../shared/label-pill";
import { FooterStats } from "../shared/footer-stats";
import { TYPOGRAPHY } from "../shared/constants";
import type { IssueCardData } from "../../validation";

export function IssueCard(data: IssueCardData) {
  return (
    <Card style={{ justifyContent: "space-between" }}>
      <Col style={{ gap: 48 }}>
        <Col style={{ gap: 32 }}>
          <Row style={{ justifyContent: "space-between" }}>
            <CardHeader
              avatarUrl={data.avatarUrl}
              ownerHandle={data.ownerHandle}
              repoName={data.repoName}
            />
            <IssueStatusBadge status={data.status} />
          </Row>

          <div
            style={{
              ...TYPOGRAPHY.title,
              color: "#000000",
              display: "block",
              lineClamp: `2 "... #${data.issueNumber}"`,
            }}>
            {data.title}
          </div>
        </Col>

        <LabelList labels={data.labels} />
      </Col>

      <Row
        style={{
          alignItems: "flex-end",
          justifyContent: "space-between",
        }}>
        <FooterStats
          createdAt={data.createdAt}
          reactionCount={data.reactionCount}
          commentCount={data.commentCount}
        />
        <TangledLogo />
      </Row>
    </Card>
  );
}
