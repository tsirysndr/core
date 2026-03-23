export const COLORS = {
  text: "#000000",
  textSecondary: "#7D7D7D",
  icon: "#404040",
  status: {
    open: { bg: "#16A34A", text: "#ffffff" },
    closed: { bg: "#1f2937", text: "#ffffff" },
    merged: { bg: "#7C3AED", text: "#ffffff" },
  },
  label: {
    text: "#202020",
    border: "#E6E6E6",
  },
  diff: {
    additions: { bg: "#dcfce7", text: "#15803d" },
    deletions: { bg: "#fee2e2", text: "#b91c1c" },
  },
} as const;

export const TYPOGRAPHY = {
  title: { fontFamily: "Inter", fontSize: 64, fontWeight: 600 },
  repoName: { fontFamily: "Inter", fontSize: 144, fontWeight: 600 },
  ownerHandle: { fontFamily: "Inter", fontSize: 48, fontWeight: 500 },
  cardHeader: { fontFamily: "Inter", fontSize: 48, fontWeight: 500 },
  status: { fontFamily: "Inter", fontSize: 48, fontWeight: 500 },
  metricValue: { fontFamily: "Inter", fontSize: 48, fontWeight: 500 },
  body: { fontFamily: "Inter", fontSize: 36, fontWeight: 400 },
  meta: { fontFamily: "Inter", fontSize: 32, fontWeight: 400 },
  label: { fontFamily: "Inter", fontSize: 24, fontWeight: 400 },
} as const;
