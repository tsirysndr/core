import { Card, Row, Col } from "../shared/layout";
import { TangledLogo } from "../shared/logo";
import { StatusBadge } from "../shared/status-badge";
import { CardHeader } from "../shared/card-header";
import { FooterStats } from "../shared/footer-stats";
import { FileDiff, RefreshCw } from "../../icons/lucide";
import { COLORS, TYPOGRAPHY } from "../shared/constants";
import { pluralize } from "../../lib/pluralize";
import type { PullRequestCardData } from "../../validation";

interface FilesChangedPillProps {
  filesChanged: number;
  additions: number;
  deletions: number;
}

function FilesChangedPill({
  filesChanged,
  additions,
  deletions,
}: FilesChangedPillProps) {
  return (
    <Row
      style={{
        overflow: "hidden",
        borderRadius: 18,
        backgroundColor: "#fff",
        border: `4px solid ${COLORS.label.border}`,
      }}>
      <Row
        style={{
          gap: 16,
          padding: "16px 28px",
        }}>
        <FileDiff size={34} color="#202020" />
        <span style={{ ...TYPOGRAPHY.body, color: "#202020" }}>
          {filesChanged} {pluralize("file", filesChanged)}
        </span>
      </Row>
      <Row style={{ gap: 0 }}>
        <Row
          style={{
            padding: "16px 10px 16px 11px",
            backgroundColor: COLORS.diff.additions.bg,
          }}>
          <span
            style={{ ...TYPOGRAPHY.body, color: COLORS.diff.additions.text }}>
            +{additions}
          </span>
        </Row>
        <Row
          style={{
            padding: "16px 16px 16px 11px",
            backgroundColor: COLORS.diff.deletions.bg,
          }}>
          <span
            style={{ ...TYPOGRAPHY.body, color: COLORS.diff.deletions.text }}>
            -{deletions}
          </span>
        </Row>
      </Row>
    </Row>
  );
}

interface MetricPillProps {
  value: number;
  label: string;
}

function RoundsPill({ value, label }: MetricPillProps) {
  return (
    <Row
      style={{
        gap: 16,
        padding: "16px 28px",
        borderRadius: 18,
        backgroundColor: "#fff",
        border: `4px solid ${COLORS.label.border}`,
      }}>
      <RefreshCw size={36} color="#202020" />
      <span style={{ ...TYPOGRAPHY.body, color: "#202020" }}>
        {value} {label}
      </span>
    </Row>
  );
}

export function PullRequestCard(data: PullRequestCardData) {
  return (
    <Card style={{
        justifyContent: "space-between",
        paddingBottom: 36,
    }}>
      <Col style={{ gap: 48 }}>
        <Col style={{ gap: 32 }}>
          <Row style={{ justifyContent: "space-between" }}>
            <CardHeader
              avatarUrl={data.avatarUrl}
              ownerHandle={data.ownerHandle}
              repoName={data.repoName}
            />
            <StatusBadge status={data.status} />
          </Row>

          <span
            style={{
              ...TYPOGRAPHY.title,
              color: "#000000",
              display: "block",
              lineClamp: `2 "... #${data.pullRequestNumber}"`,
            }}>
            {data.title}
          </span>
        </Col>

        <Row style={{ gap: 16 }}>
          <FilesChangedPill
            filesChanged={data.filesChanged}
            additions={data.additions}
            deletions={data.deletions}
          />
          <RoundsPill value={data.rounds} label={data.rounds <= 1 ? `round` : `rounds`} />
        </Row>
      </Col>

      <Row
        style={{
          alignItems: "center",
          justifyContent: "space-between",
        }}>
        <FooterStats
          createdAt={data.createdAt}
          authorHandle={data.authorHandle}
          authorAvatarUrl={data.authorAvatarUrl}
          reactionCount={data.reactionCount}
          commentCount={data.commentCount}
        />
        <TangledLogo />
      </Row>
    </Card>
  );
}
