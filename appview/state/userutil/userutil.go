package userutil

import (
	"bufio"
	"log/slog"
	"os"
	"regexp"
	"strings"
)

var (
	handleRegex = regexp.MustCompile(`^([a-zA-Z0-9]([a-zA-Z0-9-]{0,61}[a-zA-Z0-9])?\.)+[a-zA-Z]([a-zA-Z0-9-]{0,61}[a-zA-Z0-9])?$`)
	didRegex    = regexp.MustCompile(`^did:[a-z]+:[a-zA-Z0-9._:%-]*[a-zA-Z0-9._-]$`)
)

func IsHandle(s string) bool {
	// ref: https://atproto.com/specs/handle
	return handleRegex.MatchString(s)
}

// IsDid checks if the given string is a standard DID.
func IsDid(s string) bool {
	return didRegex.MatchString(s)
}

func UnflattenDid(s string) string {
	if !IsFlattenedDid(s) {
		return s
	}

	return strings.Replace(s, "-", ":", 2)
}

// IsFlattenedDid checks if the given string is a flattened DID.
func IsFlattenedDid(s string) bool {
	// Check if the string starts with "did-"
	if !strings.HasPrefix(s, "did-") {
		return false
	}

	// Reconstruct as a standard DID format using Replace
	// Example: "did-plc-xyz-abc" becomes "did:plc:xyz-abc"
	reconstructed := strings.Replace(s, "-", ":", 2)

	return didRegex.MatchString(reconstructed)
}

// FlattenDid converts a DID to a flattened format.
// A flattened DID is a DID with the :s swapped to -s to satisfy certain
// application requirements, such as Go module naming conventions.
func FlattenDid(s string) string {
	if IsDid(s) {
		return strings.Replace(s, ":", "-", 2)
	}
	return s
}

var subdomainRegex = regexp.MustCompile(`^[a-z0-9]([a-z0-9-]{2,61}[a-z0-9])?$`)

func IsValidSubdomain(name string) bool {
	return len(name) >= 4 && len(name) <= 63 && subdomainRegex.MatchString(name)
}

// LoadDisallowedNicknames reads a newline-separated list of reserved nicknames
// from filepath (blank lines and #-comments ignored). An empty filepath or a
// read error yields an empty set. Invalid entries are logged and skipped.
func LoadDisallowedNicknames(filepath string, logger *slog.Logger) map[string]bool {
	disallowed := make(map[string]bool)

	if filepath == "" {
		logger.Warn("no disallowed nicknames file configured")
		return disallowed
	}

	file, err := os.Open(filepath)
	if err != nil {
		logger.Warn("failed to open disallowed nicknames file", "file", filepath, "error", err)
		return disallowed
	}
	defer file.Close()

	scanner := bufio.NewScanner(file)
	lineNum := 0
	for scanner.Scan() {
		lineNum++
		line := strings.TrimSpace(scanner.Text())
		if line == "" || strings.HasPrefix(line, "#") {
			continue
		}

		nickname := strings.ToLower(line)
		if IsValidSubdomain(nickname) {
			disallowed[nickname] = true
		} else {
			logger.Warn("invalid nickname format in disallowed nicknames file",
				"file", filepath, "line", lineNum, "nickname", nickname)
		}
	}

	if err := scanner.Err(); err != nil {
		logger.Error("error reading disallowed nicknames file", "file", filepath, "error", err)
	}

	logger.Info("loaded disallowed nicknames", "count", len(disallowed), "file", filepath)
	return disallowed
}
