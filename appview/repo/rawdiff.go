package repo

import (
	"fmt"
	"strings"
	"time"

	"tangled.org/core/types"
)

// renderUnifiedDiff reconstructs a unified diff from a NiceDiff.
func renderUnifiedDiff(nd *types.NiceDiff) string {
	if nd == nil {
		return ""
	}
	var sb strings.Builder
	for _, d := range nd.Diff {
		oldName := d.Name.Old
		newName := d.Name.New
		if oldName == "" {
			oldName = newName
		}
		if newName == "" {
			newName = oldName
		}

		fmt.Fprintf(&sb, "diff --git a/%s b/%s\n", oldName, newName)
		switch {
		case d.IsNew:
			fmt.Fprintf(&sb, "new file mode 100644\n")
			fmt.Fprintf(&sb, "--- /dev/null\n")
			fmt.Fprintf(&sb, "+++ b/%s\n", newName)
		case d.IsDelete:
			fmt.Fprintf(&sb, "deleted file mode 100644\n")
			fmt.Fprintf(&sb, "--- a/%s\n", oldName)
			fmt.Fprintf(&sb, "+++ /dev/null\n")
		case d.IsRename:
			fmt.Fprintf(&sb, "rename from %s\n", oldName)
			fmt.Fprintf(&sb, "rename to %s\n", newName)
			fmt.Fprintf(&sb, "--- a/%s\n", oldName)
			fmt.Fprintf(&sb, "+++ b/%s\n", newName)
		default:
			fmt.Fprintf(&sb, "--- a/%s\n", oldName)
			fmt.Fprintf(&sb, "+++ b/%s\n", newName)
		}

		for i := range d.TextFragments {
			sb.WriteString(d.TextFragments[i].String())
		}
	}
	return sb.String()
}

// renderFormatPatch reconstructs an email-style format-patch from a NiceDiff.
func renderFormatPatch(nd *types.NiceDiff) string {
	if nd == nil {
		return ""
	}
	c := nd.Commit

	// subject: first line of commit message
	subject := c.Message
	if i := strings.IndexByte(subject, '\n'); i >= 0 {
		subject = subject[:i]
	}

	// body: rest of message after first line
	body := ""
	if i := strings.Index(c.Message, "\n\n"); i >= 0 {
		body = strings.TrimRight(c.Message[i+2:], "\n")
	}

	date := c.Author.When.UTC().Format(time.RFC1123Z)

	var sb strings.Builder
	fmt.Fprintf(&sb, "From %s Mon Sep 17 00:00:00 2001\n", c.Hash.String())
	fmt.Fprintf(&sb, "From: %s <%s>\n", c.Author.Name, c.Author.Email)
	fmt.Fprintf(&sb, "Date: %s\n", date)
	fmt.Fprintf(&sb, "Subject: [PATCH] %s\n", subject)
	sb.WriteString("\n")
	if body != "" {
		sb.WriteString(body)
		sb.WriteString("\n")
	}
	sb.WriteString("---\n")

	// stat summary
	for _, d := range nd.Diff {
		name := d.Name.New
		if name == "" {
			name = d.Name.Old
		}
		stats := d.Stats()
		fmt.Fprintf(&sb, " %s | %d %s\n", name, stats.Insertions+stats.Deletions,
			strings.Repeat("+", int(stats.Insertions))+strings.Repeat("-", int(stats.Deletions)))
	}
	fmt.Fprintf(&sb, " %d file(s) changed, %d insertion(s)(+), %d deletion(s)(-)\n\n",
		nd.Stat.FilesChanged, nd.Stat.Insertions, nd.Stat.Deletions)

	sb.WriteString(renderUnifiedDiff(nd))
	sb.WriteString("\n--\ntangled.sh\n")
	return sb.String()
}
