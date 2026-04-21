import type {
  RepositoryCardData,
  IssueCardData,
  PullRequestCardData,
} from "../validation";

const LONG_TITLE =
  "fix critical memory leak in WebSocket connection handler that causes server crashes under high load conditions in production environments";

export const createRepoData = (avatarUrl: string): RepositoryCardData => ({
  type: "repository",
  repoName: "core",
  ownerHandle: "tangled.org",
  stars: 746,
  pulls: 82,
  issues: 176,
  createdAt: "2026-01-29T00:00:00Z",
  avatarUrl,
  languages: [
    { color: "#00ADD8", percentage: 50 },
    { color: "#e34c26", percentage: 30 },
    { color: "#7e7eff", percentage: 10 },
    { color: "#663399", percentage: 5 },
    { color: "#f1e05a", percentage: 5 },
  ],
});

export const createIssueData = (
  avatarUrl: string,
  overrides?: Partial<IssueCardData>,
): IssueCardData => ({
  type: "issue",
  repoName: "core",
  ownerHandle: "tangled.org",
  authorHandle: "oppi.li",
  avatarUrl,
  authorAvatarUrl: avatarUrl,
  title: "feature request: sync fork button",
  issueNumber: 8,
  status: "open",
  labels: [
    { name: "feature", color: "#4639d6" },
    { name: "help-wanted", color: "#008672" },
    { name: "enhancement", color: "#0052cc" },
  ],
  commentCount: 12,
  reactionCount: 5,
  createdAt: "2026-01-29T00:00:00Z",
  ...overrides,
});

export const createPullRequestData = (
  avatarUrl: string,
  overrides?: Partial<PullRequestCardData>,
): PullRequestCardData => ({
  type: "pullRequest",
  repoName: "core",
  ownerHandle: "tangled.org",
  authorHandle: "oppi.li",
  avatarUrl,
  authorAvatarUrl: avatarUrl,
  title: "add author description to README.md",
  pullRequestNumber: 1,
  status: "open",
  filesChanged: 2,
  additions: 116,
  deletions: 59,
  rounds: 3,
  commentCount: 12,
  reactionCount: 31,
  createdAt: "2026-01-29T00:00:00Z",
  ...overrides,
});

export const createLongTitleIssueData = (
  avatarUrl: string,
  overrides?: Partial<IssueCardData>,
): IssueCardData => ({
  ...createIssueData(avatarUrl),
  title: LONG_TITLE,
  ...overrides,
});

export const createLongTitlePullRequestData = (
  avatarUrl: string,
  overrides?: Partial<PullRequestCardData>,
): PullRequestCardData => ({
  ...createPullRequestData(avatarUrl),
  title: LONG_TITLE,
  ...overrides,
});
