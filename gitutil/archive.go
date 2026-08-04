package gitutil

import (
	"bytes"
	"cmp"
	"context"
	"crypto/sha256"
	"encoding/hex"
	"fmt"
	"io"
	"mime"
	"net/http"
	"net/url"
	"os/exec"
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

const sha1HexLen, sha256HexLen, hexDigits = 40, 64, "0123456789abcdef"

func (r Rev) IsObjectID() bool {
	return slices.Contains([]int{sha1HexLen, sha256HexLen}, len(r)) &&
		strings.TrimLeft(string(r), hexDigits) == ""
}

func (r Rev) String() string { return string(r) }

func (r Rev) Slug() string { return pathSeparators.Replace(plumbing.ReferenceName(r).Short()) }

func (r Rev) Or(fallback Rev) Rev { return lo.Ternary(r == "", fallback, r) }

type ArchivePrefix string

const MaxArchivePrefixLen = 255

func ParseArchivePrefix(raw string) (ArchivePrefix, error) {
	switch {
	case strings.ContainsFunc(raw, unicode.IsControl):
		return "", fmt.Errorf("prefix contains a control character: %q", raw)
	case strings.Contains(raw, `\`):
		return "", fmt.Errorf("prefix contains a backslash: %q", raw)
	}
	components := lo.Filter(strings.Split(raw, "/"), func(component string, _ int) bool {
		return component != "" && component != "."
	})
	if slices.Contains(components, "..") {
		return "", fmt.Errorf("prefix escapes the archive root: %q", raw)
	}
	prefix := ArchivePrefix(strings.Join(components, "/"))
	if len(prefix) > MaxArchivePrefixLen {
		return "", fmt.Errorf("prefix is %d bytes, over the %d byte limit", len(prefix), MaxArchivePrefixLen)
	}
	return prefix, nil
}

func (p ArchivePrefix) String() string { return string(p) }

func archiveStem(repo RepoName, rev Rev) ArchivePrefix {
	stem := strings.ToValidUTF8(pathSeparators.Replace(string(repo))+"-"+rev.Slug(), "�")
	return ArchivePrefix(strings.ToValidUTF8(stem[:min(len(stem), MaxArchivePrefixLen)], ""))
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

type ServedArchive struct {
	rev    Rev
	format ArchiveFormat
	prefix ArchivePrefix
}

func (p ArchiveParams) Serve(repo RepoName) ServedArchive {
	return ServedArchive{p.Rev, p.Format, cmp.Or(p.Prefix, archiveStem(repo, p.Rev))}
}

func (a ServedArchive) WithRev(rev Rev) ServedArchive {
	a.rev = rev
	return a
}

func (a ServedArchive) Query(repo string) url.Values {
	return url.Values{
		"repo":   {repo},
		"ref":    {a.rev.String()},
		"format": {a.format.String()},
		"prefix": {a.prefix.String()},
	}
}

func (a ServedArchive) SuffixURL() string {
	return fmt.Sprintf("%s.%s?%s", url.PathEscape(a.rev.String()), a.format,
		url.Values{"prefix": {a.prefix.String()}}.Encode())
}

func (a ServedArchive) SetHeaders(h http.Header) {
	h.Set("Content-Type", a.format.contentType())
	h.Set("Content-Disposition", mime.FormatMediaType("attachment", map[string]string{
		"filename": pathSeparators.Replace(a.prefix.String()) + "." + a.format.String(),
	}))
	h.Set("X-Content-Type-Options", "nosniff")
}

const archiveETagDomain = "gitutil.archive.v1"

type RepoIdentity string

func (a ServedArchive) ETag(repo RepoIdentity) string {
	digest := sha256.Sum256([]byte(strings.Join(
		[]string{archiveETagDomain, string(repo), a.rev.String(), a.format.String(), a.prefix.String()}, "\x00",
	)))
	return `"` + hex.EncodeToString(digest[:]) + `"`
}

func (a ServedArchive) ServeNotModified(w http.ResponseWriter, r *http.Request, repo RepoIdentity) bool {
	etag := a.ETag(repo)
	w.Header().Set("Etag", etag)
	w.Header().Set("Cache-Control", "no-cache")
	if !ETagMatches(r.Header, etag) {
		return false
	}
	w.WriteHeader(http.StatusNotModified)
	return true
}

func ETagMatches(h http.Header, etag string) bool {
	offered := strings.Split(strings.Join(h.Values("If-None-Match"), ","), ",")
	return lo.SomeBy(offered, func(candidate string) bool {
		trimmed := strings.TrimSpace(candidate)
		return trimmed == "*" || strings.TrimPrefix(trimmed, "W/") == etag
	})
}

func ForwardHeaders(dst, src http.Header, keys ...string) {
	lo.ForEach(keys, func(key string, _ int) {
		if values := src.Values(key); len(values) > 0 {
			dst[http.CanonicalHeaderKey(key)] = slices.Clone(values)
		}
	})
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

type ResponseBody struct {
	inner   http.ResponseWriter
	started bool
}

func NewResponseBody(w http.ResponseWriter) *ResponseBody { return &ResponseBody{inner: w} }

func (b *ResponseBody) Write(p []byte) (int, error) {
	n, err := b.inner.Write(p)
	b.started = b.started || n > 0
	return n, err
}

func (b *ResponseBody) Fail() {
	if b.started {
		panic(http.ErrAbortHandler)
	}
	header := b.inner.Header()
	lo.ForEach([]string{"Content-Length", "Content-Type", "Content-Disposition", "Etag", "Link"},
		func(key string, _ int) { header.Del(key) })
	header.Set("Cache-Control", "no-store")
	b.inner.WriteHeader(http.StatusInternalServerError)
}

const archiveWaitDelay = 10 * time.Second

func WriteArchive(ctx context.Context, w io.Writer, repoPath string, archive ServedArchive) error {
	ctx, cancel := context.WithCancel(ctx)
	defer cancel()

	cmd := exec.CommandContext(ctx, "git", "archive",
		"--format="+archive.format.String(),
		"--prefix="+archive.prefix.String()+"/",
		"--", archive.rev.String())
	cmd.Dir = repoPath
	stderr := new(bytes.Buffer)
	cmd.Stderr = stderr
	cmd.SysProcAttr = &syscall.SysProcAttr{Setpgid: true}
	cmd.WaitDelay = archiveWaitDelay
	cmd.Cancel = func() error {
		err := syscall.Kill(-cmd.Process.Pid, syscall.SIGKILL)
		return lo.Ternary(err == syscall.ESRCH, nil, err)
	}

	stdout, err := cmd.StdoutPipe()
	if err != nil {
		return err
	}
	if err := cmd.Start(); err != nil {
		return err
	}
	if _, err := io.Copy(w, stdout); err != nil {
		cancel()
		_ = cmd.Wait()
		return fmt.Errorf("writing the archive body: %w", err)
	}
	if err := cmd.Wait(); err != nil {
		return fmt.Errorf("%w, stderr: %s", err, stderr.String())
	}
	return nil
}
