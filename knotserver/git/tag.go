package git

import (
	"fmt"
	"strconv"
	"strings"
	"time"

	"github.com/go-git/go-git/v5/plumbing"
	"github.com/go-git/go-git/v5/plumbing/object"
)

type TagsOptions struct {
	Limit   int
	Offset  int
	Pattern string
}

// numTagFields is the number of fields per record produced by the for-each-ref
// format string built in Tags.
const numTagFields = 11

// parseTagRecord parses a single for-each-ref output record into an object.Tag.
//
// Returns (object.Tag{}, false, nil) when the record is too short and should be
// skipped. Returns (object.Tag{}, false, err) when the object type is
// unrecognised. Returns (tag, true, nil) on success.
func parseTagRecord(line string) (object.Tag, bool, error) {
	parts := strings.SplitN(strings.TrimSpace(line), fieldSeparator, numTagFields)
	if len(parts) < 6 {
		return object.Tag{}, false, nil
	}

	tagName := parts[0]
	objectHash := parts[1]
	objectType := parts[2]
	targetHash := parts[3] // dereferenced object hash (empty for lightweight tags)
	// targetType := parts[4] // dereferenced object type (empty for lightweight tags)
	taggerName := parts[5]
	taggerEmail := parts[6]
	taggerDate := parts[7]
	subject := parts[8]
	body := parts[9]
	signature := parts[10]

	// combine subject and body for the message
	var message string
	if subject != "" && body != "" {
		message = subject + "\n\n" + body
	} else if subject != "" {
		message = subject
	} else {
		message = body
	}

	// parse creation time
	var createdAt time.Time
	if unix, err := strconv.ParseInt(taggerDate, 10, 64); err == nil {
		createdAt = time.Unix(unix, 0)
	}

	// parse object type
	typ, err := plumbing.ParseObjectType(objectType)
	if err != nil {
		return object.Tag{}, false, err
	}

	// strip email separators
	taggerEmail = strings.TrimSuffix(strings.TrimPrefix(taggerEmail, "<"), ">")

	return object.Tag{
		Hash: plumbing.NewHash(objectHash),
		Name: tagName,
		Tagger: object.Signature{
			Name:  taggerName,
			Email: taggerEmail,
			When:  createdAt,
		},
		Message:      message,
		PGPSignature: signature,
		TargetType:   typ,
		Target:       plumbing.NewHash(targetHash),
	}, true, nil
}

func (g *GitRepo) Tags(opts *TagsOptions) ([]object.Tag, error) {
	if opts == nil {
		opts = &TagsOptions{}
	}

	if opts.Pattern == "" {
		opts.Pattern = "refs/tags"
	}

	fields := []string{
		"refname:short",
		"objectname",
		"objecttype",
		"*objectname",
		"*objecttype",
		"taggername",
		"taggeremail",
		"taggerdate:unix",
		"contents:subject",
		"contents:body",
		"contents:signature",
	}

	var outFormat strings.Builder
	outFormat.WriteString("--format=")
	for i, f := range fields {
		if i != 0 {
			outFormat.WriteString(fieldSeparator)
		}
		fmt.Fprintf(&outFormat, "%%(%s)", f)
	}
	outFormat.WriteString("")
	outFormat.WriteString(recordSeparator)

	args := []string{outFormat.String(), "--sort=-creatordate"}

	// only add the count if the limit is a non-zero value,
	// if it is zero, get as many tags as we can
	if opts.Limit > 0 {
		args = append(args, fmt.Sprintf("--count=%d", opts.Offset+opts.Limit))
	}

	args = append(args, opts.Pattern)

	output, err := g.forEachRef(args...)
	if err != nil {
		return nil, fmt.Errorf("failed to get tags: %w", err)
	}

	records := strings.Split(strings.TrimSpace(string(output)), recordSeparator)
	if len(records) == 1 && records[0] == "" {
		return nil, nil
	}

	startIdx := opts.Offset
	if startIdx >= len(records) {
		return nil, nil
	}

	endIdx := len(records)
	if opts.Limit > 0 {
		endIdx = min(startIdx+opts.Limit, len(records))
	}

	records = records[startIdx:endIdx]
	tags := make([]object.Tag, 0, len(records))

	for _, line := range records {
		tag, ok, err := parseTagRecord(line)
		if err != nil {
			return nil, err
		}
		if !ok {
			continue
		}
		tags = append(tags, tag)
	}

	return tags, nil
}
