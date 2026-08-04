package gitutil

import (
	"bytes"
	"context"
	"fmt"
	"io"
	"mime"
	"net/http"
	"net/url"
	"os/exec"
	"path"
	"slices"
	"strings"
	"syscall"
	"time"
	"unicode"

	"github.com/go-git/go-git/v5/plumbing"
	"github.com/samber/lo"
)

type ArchiveFormat string

const (
	ArchiveTarGz ArchiveFormat = "tar.gz"
	ArchiveZip   ArchiveFormat = "zip"
)

var ArchiveFormats = []ArchiveFormat{ArchiveTarGz, ArchiveZip}

func ParseArchiveFormat(raw string) (ArchiveFormat, error) {
	if format := ArchiveFormat(raw); slices.Contains(ArchiveFormats, format) {
		return format, nil
	}
	return "", fmt.Errorf("only tar.gz and zip formats are supported, got %q", raw)
}

func (f ArchiveFormat) String() string { return string(f) }

func (f ArchiveFormat) contentType() string {
	return lo.Ternary(f == ArchiveZip, "application/zip", "application/gzip")
}

var pathSeparators = strings.NewReplacer("/", "-", `\`, "-")

type RepoName string

type Rev string

const RevHead Rev = "HEAD"

func ParseRev(raw string) (Rev, error) {
	switch {
	case raw == "":
		return "", fmt.Errorf("ref is empty")
	case strings.ContainsFunc(raw, func(c rune) bool { return unicode.IsSpace(c) || unicode.IsControl(c) }):
		return "", fmt.Errorf("ref contains whitespace or a control character: %q", raw)
	case strings.HasPrefix(raw, "-"):
		return "", fmt.Errorf("ref starts with a dash: %q", raw)
	}
	return Rev(raw), nil
}

func RevFromHash(h plumbing.Hash) Rev { return Rev(h.String()) }

func (r Rev) String() string { return string(r) }

func (r Rev) Slug() string { return pathSeparators.Replace(plumbing.ReferenceName(r).Short()) }

func (r Rev) Or(fallback Rev) Rev { return lo.Ternary(r == "", fallback, r) }

func (r Rev) OrHash(h plumbing.Hash) Rev { return r.Or(RevFromHash(h)) }

type ArchivePrefix string

const MaxArchivePrefixLen = 255

func ParseArchivePrefix(raw string) (ArchivePrefix, error) {
	switch {
	case len(raw) > MaxArchivePrefixLen:
		return "", fmt.Errorf("prefix is %d bytes, over the %d byte limit", len(raw), MaxArchivePrefixLen)
	case strings.ContainsFunc(raw, unicode.IsControl):
		return "", fmt.Errorf("prefix contains a control character: %q", raw)
	case strings.Contains(raw, `\`):
		return "", fmt.Errorf("prefix contains a backslash: %q", raw)
	}
	trimmed := strings.Trim(raw, "/")
	if trimmed == "" {
		return "", nil
	}
	cleaned := path.Clean(trimmed)
	if cleaned == "." || cleaned == ".." || strings.HasPrefix(cleaned, "../") {
		return "", fmt.Errorf("prefix escapes the archive root: %q", raw)
	}
	return ArchivePrefix(cleaned), nil
}

func (p ArchivePrefix) String() string { return string(p) }

func (p ArchivePrefix) OrDefault(repo RepoName, rev Rev) ArchivePrefix {
	if p == "" {
		return ArchivePrefix(archiveStem(repo, rev))
	}
	return p
}

func archiveStem(repo RepoName, rev Rev) string {
	stem := pathSeparators.Replace(string(repo)) + "-" + rev.Slug()
	if len(stem) <= MaxArchivePrefixLen {
		return stem
	}
	return strings.ToValidUTF8(stem[:MaxArchivePrefixLen], "")
}

type ArchiveParams struct {
	Rev    Rev
	Format ArchiveFormat
	Prefix ArchivePrefix
}

func ParseArchiveParams(q url.Values) (ArchiveParams, error) {
	p := ArchiveParams{Format: ArchiveTarGz}
	var err error
	if raw := q.Get("ref"); raw != "" {
		if p.Rev, err = ParseRev(raw); err != nil {
			return ArchiveParams{}, err
		}
	}
	if raw := q.Get("format"); raw != "" {
		if p.Format, err = ParseArchiveFormat(raw); err != nil {
			return ArchiveParams{}, err
		}
	}
	if p.Prefix, err = ParseArchivePrefix(q.Get("prefix")); err != nil {
		return ArchiveParams{}, err
	}
	return p, nil
}

func (p ArchiveParams) WithRev(rev Rev) ArchiveParams {
	p.Rev = rev
	return p
}

func (p ArchiveParams) Query(repo string) url.Values {
	return url.Values{
		"repo":   {repo},
		"ref":    {p.Rev.String()},
		"format": {p.Format.String()},
		"prefix": {p.Prefix.String()},
	}
}

func (p ArchiveParams) SetHeaders(h http.Header, repo RepoName) {
	h.Set("Content-Type", p.Format.contentType())
	h.Set("Content-Disposition", mime.FormatMediaType("attachment", map[string]string{
		"filename": archiveStem(repo, p.Rev) + "." + p.Format.String(),
	}))
	h.Set("X-Content-Type-Options", "nosniff")
}

func ImmutableLink(target string) string { return `<` + target + `>; rel="immutable"` }

func ParseImmutableLink(header string) (Rev, error) {
	target := strings.TrimSuffix(strings.TrimPrefix(header, "<"), `>; rel="immutable"`)
	parsed, err := url.Parse(target)
	if err != nil {
		return "", err
	}
	return ParseRev(parsed.Query().Get("ref"))
}

const archiveWaitDelay = 10 * time.Second

func WriteArchive(ctx context.Context, w io.Writer, repoPath string, rev Rev, format ArchiveFormat, prefix ArchivePrefix) error {
	args := []string{"archive", "--format=" + format.String()}
	if prefix != "" {
		args = append(args, "--prefix="+prefix.String()+"/")
	}

	cmd := exec.CommandContext(ctx, "git", append(args, "--", rev.String())...)
	cmd.Dir = repoPath
	cmd.Stdout = w
	stderr := new(bytes.Buffer)
	cmd.Stderr = stderr
	cmd.SysProcAttr = &syscall.SysProcAttr{Setpgid: true}
	cmd.WaitDelay = archiveWaitDelay
	cmd.Cancel = func() error {
		err := syscall.Kill(-cmd.Process.Pid, syscall.SIGKILL)
		return lo.Ternary(err == syscall.ESRCH, nil, err)
	}

	if err := cmd.Run(); err != nil {
		return fmt.Errorf("%w, stderr: %s", err, stderr.String())
	}
	return nil
}
