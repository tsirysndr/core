package microvm

import (
	"bufio"
	"fmt"
	"io"
	"path/filepath"
	"regexp"
	"strconv"
	"strings"
)

type narinfo struct {
	StorePath string
	URL       string
	NarHash   string
	NarSize   int64
}

const (
	maxNarinfoSize    = 1 << 20 // 1 MiB
	storePrefix       = "/nix/store/"
	maxNarinfoLineLen = maxNarinfoSize
)

var nixStorePathBaseRe = regexp.MustCompile(`^[0-9abcdfghijklmnpqrsvwxyz]{32}-[^/]+$`)

// parseNarinfo parses and validates a narinfo body.
//   - required fields must be present
//   - StorePath must be under /nix/store/
//   - URL must be a relative, traversal-safe path referencing a NAR in the
//     same staging cache
//   - NarSize must be a non-negative integer
func parseNarinfo(r io.Reader) (*narinfo, error) {
	lr := io.LimitReader(r, maxNarinfoSize+1)
	scanner := bufio.NewScanner(lr)
	scanner.Buffer(make([]byte, 4096), maxNarinfoLineLen)

	var info narinfo
	for scanner.Scan() {
		line := scanner.Text()
		if line == "" {
			continue
		}
		key, value, ok := strings.Cut(line, ":")
		if !ok {
			return nil, fmt.Errorf("invalid narinfo line %q", line)
		}
		key = strings.TrimSpace(key)
		value = strings.TrimSpace(value)

		switch key {
		case "StorePath":
			info.StorePath = value
		case "URL":
			info.URL = value
		case "NarHash":
			info.NarHash = value
		case "NarSize":
			n, err := strconv.ParseInt(value, 10, 64)
			if err != nil {
				return nil, fmt.Errorf("invalid NarSize %q: %w", value, err)
			}
			info.NarSize = n
		}
	}
	if err := scanner.Err(); err != nil {
		return nil, fmt.Errorf("read narinfo: %w", err)
	}

	if err := validateNarinfo(&info); err != nil {
		return nil, err
	}
	return &info, nil
}

func validateNarinfo(info *narinfo) error {
	if info.StorePath == "" {
		return fmt.Errorf("narinfo missing StorePath")
	}
	if _, _, err := parseStorePath(info.StorePath); err != nil {
		return fmt.Errorf("invalid StorePath: %w", err)
	}
	if info.URL == "" {
		return fmt.Errorf("narinfo missing URL")
	}
	if strings.HasPrefix(info.URL, "/") || strings.Contains(info.URL, "..") {
		return fmt.Errorf("narinfo URL %q is not a safe relative path", info.URL)
	}
	if !strings.HasPrefix(info.URL, "nar/") {
		return fmt.Errorf("narinfo URL %q must reference a staged nar/ object", info.URL)
	}
	name := strings.TrimPrefix(info.URL, "nar/")
	if name == "" || name == "." || name != filepath.Base(name) || strings.Contains(name, "/") {
		return fmt.Errorf("narinfo URL %q is not a safe nar object path", info.URL)
	}
	if info.NarHash == "" {
		return fmt.Errorf("narinfo missing NarHash")
	}
	if info.NarSize < 0 {
		return fmt.Errorf("narinfo NarSize must be non-negative")
	}
	return nil
}

func parseStorePath(path string) (hash string, name string, err error) {
	if !strings.HasPrefix(path, storePrefix) {
		return "", "", fmt.Errorf("store path %q does not start with %q", path, storePrefix)
	}

	base := strings.TrimPrefix(path, storePrefix)
	if base == "" || strings.Contains(base, "/") {
		return "", "", fmt.Errorf("store path %q has invalid base name", path)
	}
	if !nixStorePathBaseRe.MatchString(base) {
		return "", "", fmt.Errorf("store path %q is not a valid nix store path", path)
	}

	hash, name, ok := strings.Cut(base, "-")
	if !ok || hash == "" || name == "" {
		return "", "", fmt.Errorf("store path %q is missing hash or name", path)
	}
	return hash, name, nil
}
