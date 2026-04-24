import { test, describe, beforeAll } from "bun:test";
import { writeFileSync, mkdirSync, readFileSync } from "fs";
import { join } from "path";
import { h, type VNode } from "preact";
import { renderCard } from "../lib/render";
import { RepositoryCard } from "../components/cards/repository";
import { IssueCard } from "../components/cards/issue";
import { PullRequestCard } from "../components/cards/pull-request";
import {
  repositoryCardSchema,
  issueCardSchema,
  pullRequestCardSchema,
} from "../validation";
import {
  createRepoData,
  createIssueData,
  createPullRequestData,
  createLongTitleIssueData,
  createLongTitlePullRequestData,
} from "./fixtures";

const outputDir = join(process.cwd(), "output");
let avatarDataUri: string;

const loadAvatar = (): string => {
  const avatarPath = join(
    process.cwd(),
    "src",
    "__tests__",
    "assets",
    "avatar.jpg",
  );
  const avatarBase64 = readFileSync(avatarPath).toString("base64");
  return `data:image/jpeg;base64,${avatarBase64}`;
};

beforeAll(() => {
  mkdirSync(outputDir, { recursive: true });
  avatarDataUri = loadAvatar();
});

const savePng = (filename: string, buffer: Uint8Array) => {
  writeFileSync(join(outputDir, filename), buffer);
};

const saveSvg = (filename: string, svg: string) => {
  writeFileSync(join(outputDir, filename), svg);
};

const renderAndSave = async <P>(component: VNode<P>, filename: string) => {
  const { svg, png } = await renderCard(component as VNode);
  saveSvg(filename.replace(".png", ".svg"), svg);
  savePng(filename, png);
};

describe("repository card", () => {
  test("renders repository card", async () => {
    const data = createRepoData(avatarDataUri);
    const validated = repositoryCardSchema.parse(data);
    await renderAndSave(h(RepositoryCard, validated), "repository-card.png");
  });
});

describe("issue cards", () => {
  test("renders open issue", async () => {
    const data = createIssueData(avatarDataUri);
    const validated = issueCardSchema.parse(data);
    await renderAndSave(h(IssueCard, validated), "issue-card.png");
  });

  test("renders closed issue", async () => {
    const data = createIssueData(avatarDataUri, {
      issueNumber: 5,
      status: "closed",
      labels: [{ name: "wontfix", color: "#6a737d" }],
      reactionCount: 2,
    });
    const validated = issueCardSchema.parse(data);
    await renderAndSave(h(IssueCard, validated), "issue-card-closed.png");
  });

  test("renders issue with long title", async () => {
    const data = createLongTitleIssueData(avatarDataUri, {
      issueNumber: 42,
    });
    const validated = issueCardSchema.parse(data);
    await renderAndSave(h(IssueCard, validated), "issue-card-long-title.png");
  });

  test("renders issue with long author handle", async () => {
    const data = createIssueData(avatarDataUri, {
      authorHandle: "very.very.long.tangled.org",
    });
    const validated = issueCardSchema.parse(data);
    await renderAndSave(
      h(IssueCard, validated),
      "issue-card-long-handle.png",
    );
  });
});

describe("pull request cards", () => {
  test("renders open pull request", async () => {
    const data = createPullRequestData(avatarDataUri);
    const validated = pullRequestCardSchema.parse(data);
    await renderAndSave(h(PullRequestCard, validated), "pull-request-card.png");
  });

  test("renders merged pull request", async () => {
    const data = createPullRequestData(avatarDataUri, {
      pullRequestNumber: 2,
      status: "merged",
      title: "Implement OAuth2 authentication flow",
      filesChanged: 5,
      additions: 342,
      deletions: 28,
    });
    const validated = pullRequestCardSchema.parse(data);
    await renderAndSave(
      h(PullRequestCard, validated),
      "pull-request-card-merged.png",
    );
  });

  test("renders closed pull request", async () => {
    const data = createPullRequestData(avatarDataUri, {
      pullRequestNumber: 3,
      status: "closed",
      title: "WIP: Experimental feature",
    });
    const validated = pullRequestCardSchema.parse(data);
    await renderAndSave(
      h(PullRequestCard, validated),
      "pull-request-card-closed.png",
    );
  });

  test("renders pull request with long title", async () => {
    const data = createLongTitlePullRequestData(avatarDataUri, {
      pullRequestNumber: 42,
    });
    const validated = pullRequestCardSchema.parse(data);
    await renderAndSave(
      h(PullRequestCard, validated),
      "pull-request-card-long-title.png",
    );
  });

  test("renders pull request with long author handle", async () => {
    const data = createPullRequestData(avatarDataUri, {
      authorHandle: "very.very.long.tangled.org",
    });
    const validated = pullRequestCardSchema.parse(data);
    await renderAndSave(
      h(PullRequestCard, validated),
      "pull-request-card-long-handle.png",
    );
  });
});
