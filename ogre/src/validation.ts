import { z } from "zod";

const hexColor = /^#[0-9A-Fa-f]{6}$/;

const languageSchema = z.object({
  color: z.string().regex(hexColor),
  percentage: z.number().min(0).max(100),
});

export const repositoryCardSchema = z.object({
  type: z.literal("repository"),
  repoName: z.string().min(1).max(100),
  ownerHandle: z.string().min(1).max(100),
  stars: z.number().int().min(0).max(1000000),
  pulls: z.number().int().min(0).max(100000),
  issues: z.number().int().min(0).max(100000),
  createdAt: z.string().max(100),
  avatarUrl: z.string().url(),
  languages: z.array(languageSchema).max(5),
});

export const issueCardSchema = z.object({
  type: z.literal("issue"),
  repoName: z.string().min(1).max(100),
  ownerHandle: z.string().min(1).max(100),
  authorHandle: z.string().min(1).max(100),
  avatarUrl: z.string().url(),
  authorAvatarUrl: z.string().url(),
  title: z.string().min(1).max(500),
  issueNumber: z.number().int().positive(),
  status: z.enum(["open", "closed"]),
  labels: z
    .array(
      z.object({
        name: z.string().max(50),
        color: z.string().regex(hexColor),
      }),
    )
    .max(10),
  commentCount: z.number().int().min(0),
  reactionCount: z.number().int().min(0),
  createdAt: z.string(),
});

export const pullRequestCardSchema = z.object({
  type: z.literal("pullRequest"),
  repoName: z.string().min(1).max(100),
  ownerHandle: z.string().min(1).max(100),
  authorHandle: z.string().min(1).max(100),
  avatarUrl: z.string().url(),
  authorAvatarUrl: z.string().url(),
  title: z.string().min(1).max(500),
  pullRequestNumber: z.number().int().positive(),
  status: z.enum(["open", "closed", "merged"]),
  filesChanged: z.number().int().min(0),
  additions: z.number().int().min(0),
  deletions: z.number().int().min(0),
  rounds: z.number().int().min(1),
  // reviews: z.number().int().min(0), // TODO: implement review tracking
  commentCount: z.number().int().min(0),
  reactionCount: z.number().int().min(0),
  createdAt: z.string(),
});

export const cardPayloadSchema = z.discriminatedUnion("type", [
  repositoryCardSchema,
  issueCardSchema,
  pullRequestCardSchema,
]);

export type Language = z.infer<typeof languageSchema>;
export type RepositoryCardData = z.infer<typeof repositoryCardSchema>;
export type IssueCardData = z.infer<typeof issueCardSchema>;
export type PullRequestCardData = z.infer<typeof pullRequestCardSchema>;
