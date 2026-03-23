import { h } from "preact";
import iconNodes from "lucide-static/icon-nodes.json";

interface IconProps {
  size?: number;
  color?: string;
  strokeWidth?: number;
}

type IconNodeEntry = [string, Record<string, string | number>];

function createIcon(name: string) {
  const nodes = (iconNodes as unknown as Record<string, IconNodeEntry[]>)[name];
  if (!nodes) throw new Error(`Icon "${name}" not found`);

  return function Icon({
    size = 24,
    color = "currentColor",
    strokeWidth = 2,
  }: IconProps = {}) {
    return h(
      "svg",
      {
        xmlns: "http://www.w3.org/2000/svg",
        width: size,
        height: size,
        viewBox: "0 0 24 24",
        fill: "none",
        stroke: color,
        strokeWidth,
        strokeLinecap: "round" as const,
        strokeLinejoin: "round" as const,
      },
      nodes.map(([tag, attrs], i) => h(tag, { key: i, ...attrs })),
    );
  };
}

export const Star = createIcon("star");
export const GitPullRequest = createIcon("git-pull-request");
export const GitPullRequestClosed = createIcon("git-pull-request-closed");
export const GitMerge = createIcon("git-merge");
export const CircleDot = createIcon("circle-dot");
export const Calendar = createIcon("calendar");
export const MessageSquare = createIcon("message-square");
export const MessageSquareCode = createIcon("message-square-code");
export const Ban = createIcon("ban");
export const SmilePlus = createIcon("smile-plus");
export const FileDiff = createIcon("file-diff");
export const RefreshCw = createIcon("refresh-cw");

export type LucideIcon = typeof Star;
