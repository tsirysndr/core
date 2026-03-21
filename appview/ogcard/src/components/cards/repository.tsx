import { Card, Row, Col } from "../shared/layout";
import { Avatar } from "../shared/avatar";
import { LanguageCircles } from "../shared/language-circles";
import { Metrics } from "../shared/metrics";
import { TangledLogo } from "../shared/logo";
import { FooterStats } from "../shared/footer-stats";
import { TYPOGRAPHY } from "../shared/constants";
import type { RepositoryCardData } from "../../validation";

export function RepositoryCard(data: RepositoryCardData) {
  return (
    <Card>
      <LanguageCircles languages={data.languages} />

      <Col style={{ gap: 64 }}>
        <Col style={{ gap: 24 }}>
          <span style={{ ...TYPOGRAPHY.repoName, color: "#000000" }}>
            {data.repoName}
          </span>

          <Row style={{ gap: 16 }}>
            <Avatar src={data.avatarUrl} size={64} />
            <span style={{ ...TYPOGRAPHY.ownerHandle, color: "#000000" }}>
              {data.ownerHandle}
            </span>
          </Row>
        </Col>

        <Metrics stars={data.stars} pulls={data.pulls} issues={data.issues} />
      </Col>

      <Row
        style={{
          alignItems: "flex-end",
          justifyContent: "space-between",
          flexGrow: 1,
        }}>
        <FooterStats createdAt={data.createdAt} />

        <TangledLogo />
      </Row>
    </Card>
  );
}
