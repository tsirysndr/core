package pages

import (
	"bytes"
	"context"
	"crypto/hmac"
	"crypto/sha256"
	"encoding/hex"
	"errors"
	"fmt"
	"html"
	"html/template"
	"log"
	"math"
	"math/rand"
	"net/url"
	"path/filepath"
	"reflect"
	"strings"
	"time"

	"github.com/alecthomas/chroma/v2"
	chromahtml "github.com/alecthomas/chroma/v2/formatters/html"
	"github.com/alecthomas/chroma/v2/lexers"
	"github.com/alecthomas/chroma/v2/styles"
	"github.com/bluesky-social/indigo/atproto/syntax"
	"github.com/dustin/go-humanize"
	"github.com/dustin/go-humanize/english"
	"github.com/go-enry/go-enry/v2"
	"github.com/yuin/goldmark"
	emoji "github.com/yuin/goldmark-emoji"
	"tangled.org/core/appview/cache"
	"tangled.org/core/appview/db"
	"tangled.org/core/appview/models"
	"tangled.org/core/appview/oauth"
	"tangled.org/core/appview/pages/markup"
	"tangled.org/core/crypto"
	"tangled.org/core/idresolver"
)

type tab map[string]string

func (p *Pages) funcMap() template.FuncMap {
	return template.FuncMap{
		"split": func(s string) []string {
			return strings.Split(s, "\n")
		},
		"capitalize": func(s string) string {
			if s == "" {
				return s
			}
			return strings.ToUpper(s[:1]) + s[1:]
		},
		"trimPrefix": func(s, prefix string) string {
			return strings.TrimPrefix(s, prefix)
		},
		"join": func(elems []string, sep string) string {
			return strings.Join(elems, sep)
		},
		"contains": func(s string, target string) bool {
			return strings.Contains(s, target)
		},
		"stripPort": func(hostname string) string {
			if strings.Contains(hostname, ":") {
				return strings.Split(hostname, ":")[0]
			}
			return hostname
		},
		"mapContains": func(m any, key any) bool {
			mapValue := reflect.ValueOf(m)
			if mapValue.Kind() != reflect.Map {
				return false
			}
			keyValue := reflect.ValueOf(key)
			return mapValue.MapIndex(keyValue).IsValid()
		},
		"resolve": func(s string) string {
			return p.DisplayHandle(context.Background(), s)
		},
		"resolver": func() *idresolver.Resolver {
			return p.resolver
		},
		"primaryHandle": func(s string) string {
			return primaryHandle(p.resolver, s)
		},
		"resolvePds": func(s string) string {
			identity, err := p.resolver.ResolveIdent(context.Background(), s)
			if err != nil {
				return ""
			}
			return identity.PDSEndpoint()
		},
		"ownerSlashRepo": func(repo *models.Repo) string {
			ownerId, err := p.resolver.ResolveIdent(context.Background(), repo.Did)
			if err != nil {
				return repo.RepoIdentifier()
			}
			handle := ownerId.Handle
			if handle != "" && !handle.IsInvalidHandle() {
				return string(handle) + "/" + repo.Slug()
			}
			return repo.RepoIdentifier()
		},
		"truncateAt30": func(s string) string {
			if len(s) <= 30 {
				return s
			}
			return s[:30] + "…"
		},
		// short prefix of a commit hash or jj change id, safe on short input
		"shortId": func(s string) string {
			if len(s) <= 8 {
				return s
			}
			return s[:8]
		},
		"splitOn": func(s, sep string) []string {
			return strings.Split(s, sep)
		},
		"string": func(v any) string {
			return fmt.Sprint(v)
		},
		"int64": func(a int) int64 {
			return int64(a)
		},
		"add": func(a, b int) int {
			return a + b
		},
		"now": func() time.Time {
			return time.Now()
		},
		// the absolute state of go templates
		"add64": func(a, b int64) int64 {
			return a + b
		},
		"sub": func(a, b int) int {
			return a - b
		},
		"mul": func(a, b int) int {
			return a * b
		},
		"div": func(a, b int) int {
			return a / b
		},
		"mod": func(a, b int) int {
			return a % b
		},
		"randInt": func(bound int) int {
			return rand.Intn(bound)
		},
		"f64": func(a int) float64 {
			return float64(a)
		},
		"addf64": func(a, b float64) float64 {
			return a + b
		},
		"subf64": func(a, b float64) float64 {
			return a - b
		},
		"mulf64": func(a, b float64) float64 {
			return a * b
		},
		"divf64": func(a, b float64) float64 {
			if b == 0 {
				return 0
			}
			return a / b
		},
		"negf64": func(a float64) float64 {
			return -a
		},
		"cond": func(cond any, a, b string) string {
			if cond == nil {
				return b
			}

			if boolean, ok := cond.(bool); boolean && ok {
				return a
			}

			return b
		},
		"assoc": func(values ...string) ([][]string, error) {
			if len(values)%2 != 0 {
				return nil, fmt.Errorf("invalid assoc call, must have an even number of arguments")
			}
			pairs := make([][]string, 0)
			for i := 0; i < len(values); i += 2 {
				pairs = append(pairs, []string{values[i], values[i+1]})
			}
			return pairs, nil
		},
		"append": func(s []any, values ...any) []any {
			s = append(s, values...)
			return s
		},
		// scale numerics over 1000 to 1k
		"scaleFmt": func(n any) string {
			var v float64
			switch x := n.(type) {
			case int:
				v = float64(x)
			case int32:
				v = float64(x)
			case int64:
				v = float64(x)
			case float64:
				v = x
			default:
				return fmt.Sprintf("%v", n)
			}
			if v < 1000 {
				return fmt.Sprintf("%d", int(v))
			}
			k := v / 1000
			if k < 10 {
				return fmt.Sprintf("%.1fk", k)
			}
			return fmt.Sprintf("%dk", int(k))
		},
		"commaFmt":   humanize.Comma,
		"plural":     english.Plural,
		"relTimeFmt": humanize.Time,
		"shortRelTimeFmt": func(t time.Time) string {
			return humanize.CustomRelTime(t, time.Now(), "", "", []humanize.RelTimeMagnitude{
				{D: time.Second, Format: "now", DivBy: time.Second},
				{D: 2 * time.Second, Format: "1s %s", DivBy: 1},
				{D: time.Minute, Format: "%ds %s", DivBy: time.Second},
				{D: 2 * time.Minute, Format: "1min %s", DivBy: 1},
				{D: time.Hour, Format: "%dmin %s", DivBy: time.Minute},
				{D: 2 * time.Hour, Format: "1hr %s", DivBy: 1},
				{D: humanize.Day, Format: "%dhrs %s", DivBy: time.Hour},
				{D: 2 * humanize.Day, Format: "1d %s", DivBy: 1},
				{D: 20 * humanize.Day, Format: "%dd %s", DivBy: humanize.Day},
				{D: 8 * humanize.Week, Format: "%dw %s", DivBy: humanize.Week},
				{D: humanize.Year, Format: "%dmo %s", DivBy: humanize.Month},
				{D: 18 * humanize.Month, Format: "1y %s", DivBy: 1},
				{D: 2 * humanize.Year, Format: "2y %s", DivBy: 1},
				{D: humanize.LongTime, Format: "%dy %s", DivBy: humanize.Year},
				{D: math.MaxInt64, Format: "a long while %s", DivBy: 1},
			})
		},
		"shortTimeFmt": func(t time.Time) string {
			return t.Format("Jan 2, 2006")
		},
		"longTimeFmt": func(t time.Time) string {
			return t.Format("Jan 2, 2006, 3:04 PM MST")
		},
		"iso8601DateTimeFmt": func(t time.Time) string {
			return t.Format("2006-01-02T15:04:05-07:00")
		},
		"iso8601DurationFmt": func(duration time.Duration) string {
			days := int64(duration.Hours() / 24)
			hours := int64(math.Mod(duration.Hours(), 24))
			minutes := int64(math.Mod(duration.Minutes(), 60))
			seconds := int64(math.Mod(duration.Seconds(), 60))
			return fmt.Sprintf("P%dD%dH%dM%dS", days, hours, minutes, seconds)
		},
		"durationFmt": func(duration time.Duration) string {
			return durationFmt(duration, [4]string{"d", "h", "m", "s"})
		},
		"longDurationFmt": func(duration time.Duration) string {
			return durationFmt(duration, [4]string{"days", "hours", "minutes", "seconds"})
		},
		"byteFmt": humanize.Bytes,
		"length": func(slice any) int {
			v := reflect.ValueOf(slice)
			if v.Kind() == reflect.Slice || v.Kind() == reflect.Array {
				return v.Len()
			}
			return 0
		},
		"splitN": func(s, sep string, n int) []string {
			return strings.SplitN(s, sep, n)
		},
		"escapeHtml": func(s string) template.HTML {
			if s == "" {
				return template.HTML("<br>")
			}
			return template.HTML(s)
		},
		"unescapeHtml": func(s string) string {
			return html.UnescapeString(s)
		},
		"nl2br": func(text string) template.HTML {
			return template.HTML(strings.ReplaceAll(template.HTMLEscapeString(text), "\n", "<br>"))
		},
		"unwrapText": func(text string) string {
			paragraphs := strings.Split(text, "\n\n")

			for i, p := range paragraphs {
				lines := strings.Split(p, "\n")
				paragraphs[i] = strings.Join(lines, " ")
			}

			return strings.Join(paragraphs, "\n\n")
		},
		"sequence": func(n int) []struct{} {
			return make([]struct{}, n)
		},
		// take atmost N items from this slice
		"take": func(slice any, n int) any {
			v := reflect.ValueOf(slice)
			if v.Kind() != reflect.Slice && v.Kind() != reflect.Array {
				return nil
			}
			if v.Len() == 0 {
				return nil
			}
			return v.Slice(0, min(n, v.Len())).Interface()
		},
		"markdown": func(text string) template.HTML {
			rctx := p.rctx.Clone()
			rctx.RendererType = markup.RendererTypeDefault
			htmlString := rctx.RenderMarkdown(text)
			sanitized := rctx.SanitizeDefault(htmlString)
			return template.HTML(sanitized)
		},
		"description": func(text string) template.HTML {
			rctx := p.rctx.Clone()
			rctx.RendererType = markup.RendererTypeDefault
			htmlString := rctx.RenderMarkdownWith(text, goldmark.New(
				goldmark.WithExtensions(
					emoji.Emoji,
				),
			))
			sanitized := rctx.SanitizeDescription(htmlString)
			return template.HTML(sanitized)
		},
		"readme": func(text string) template.HTML {
			rctx := p.rctx.Clone()
			rctx.RendererType = markup.RendererTypeRepoMarkdown
			htmlString := rctx.RenderMarkdown(text)
			sanitized := rctx.SanitizeDefault(htmlString)
			return template.HTML(sanitized)
		},
		"code": func(content, path string) string {
			var style *chroma.Style = styles.Get("catpuccin-latte")
			formatter := chromahtml.New(
				chromahtml.InlineCode(false),
				chromahtml.WithLineNumbers(true),
				chromahtml.WithLinkableLineNumbers(true, "L"),
				chromahtml.Standalone(false),
				chromahtml.WithClasses(true),
			)

			lexer := lexers.Get(filepath.Base(path))
			if lexer == nil {
				if firstLine, _, ok := strings.Cut(content, "\n"); ok && strings.HasPrefix(firstLine, "#!") {
					// extract interpreter from shebang (handles "#!/usr/bin/env nu", "#!/usr/bin/nu", etc.)
					fields := strings.Fields(firstLine[2:])
					if len(fields) > 0 {
						interp := filepath.Base(fields[len(fields)-1])
						lexer = lexers.Get(interp)
					}
				}
			}
			if lexer == nil {
				lexer = lexers.Analyse(content)
			}
			if lexer == nil {
				lexer = lexers.Fallback
			}

			iterator, err := lexer.Tokenise(nil, content)
			if err != nil {
				p.logger.Error("chroma tokenize", "err", "err")
				return ""
			}

			var code bytes.Buffer
			err = formatter.Format(&code, style, iterator)
			if err != nil {
				p.logger.Error("chroma format", "err", "err")
				return ""
			}

			return code.String()
		},
		"trimUriScheme": func(text string) string {
			text = strings.TrimPrefix(text, "https://")
			text = strings.TrimPrefix(text, "http://")
			return text
		},
		"isNil": func(t any) bool {
			// returns false for other "zero" values
			return t == nil
		},
		"hasPrefix": strings.HasPrefix,
		"list": func(args ...any) []any {
			return args
		},
		"dict": func(values ...any) (map[string]any, error) {
			if len(values)%2 != 0 {
				return nil, errors.New("invalid dict call")
			}
			dict := make(map[string]any, len(values)/2)
			for i := 0; i < len(values); i += 2 {
				key, ok := values[i].(string)
				if !ok {
					return nil, errors.New("dict keys must be strings")
				}
				dict[key] = values[i+1]
			}
			return dict, nil
		},
		"queryParams": func(params ...any) (url.Values, error) {
			if len(params)%2 != 0 {
				return nil, errors.New("invalid queryParams call")
			}
			vals := make(url.Values, len(params)/2)
			for i := 0; i < len(params); i += 2 {
				key, ok := params[i].(string)
				if !ok {
					return nil, errors.New("queryParams keys must be strings")
				}
				v, ok := params[i+1].(string)
				if !ok {
					return nil, errors.New("queryParams values must be strings")
				}
				vals.Add(key, v)
			}
			return vals, nil
		},
		"deref": func(v any) any {
			val := reflect.ValueOf(v)
			if val.Kind() == reflect.Pointer && !val.IsNil() {
				return val.Elem().Interface()
			}
			return nil
		},
		"i": func(name string, classes ...string) template.HTML {
			data, err := p.icon(name, classes)
			if err != nil {
				log.Printf("icon %s does not exist", name)
				data, _ = p.icon("airplay", classes)
			}
			return template.HTML(data)
		},
		"cssContentHash": p.CssContentHash,
		"pathEscape": func(s string) string {
			return url.PathEscape(s)
		},
		"pathUnescape": func(s string) string {
			u, _ := url.PathUnescape(s)
			return u
		},
		"safeUrl": func(s string) template.URL {
			return template.URL(s)
		},
		"tinyAvatar": func(handle string) string {
			return p.AvatarUrl(handle, "tiny")
		},
		"fullAvatar": func(handle string) string {
			return p.AvatarUrl(handle, "")
		},
		"placeholderAvatar": func(size string) template.HTML {
			sizeClass := "size-6"
			iconSize := "size-4"
			switch size {
			case "tiny":
				sizeClass = "size-6"
				iconSize = "size-4"
			case "small":
				sizeClass = "size-8"
				iconSize = "size-5"
			default:
				sizeClass = "size-12"
				iconSize = "size-8"
			}
			icon, _ := p.icon("user-round", []string{iconSize, "text-gray-400", "dark:text-gray-500"})
			return template.HTML(fmt.Sprintf(`<div class="%s rounded-full bg-gray-200 dark:bg-gray-700 flex items-center justify-center flex-shrink-0">%s</div>`, sizeClass, icon))
		},
		"profileAvatarUrl": func(profile *models.Profile, size string) string {
			if profile != nil {
				return p.AvatarUrl(profile.Did, size)
			}
			return ""
		},
		"langColor": enry.GetColor,
		"reverse": func(s any) any {
			if s == nil {
				return nil
			}

			v := reflect.ValueOf(s)

			if v.Kind() != reflect.Slice {
				return s
			}

			length := v.Len()
			reversed := reflect.MakeSlice(v.Type(), length, length)

			for i := range length {
				reversed.Index(i).Set(v.Index(length - 1 - i))
			}

			return reversed.Interface()
		},
		"normalizeForHtmlId": func(s string) string {
			normalized := strings.ReplaceAll(s, ":", "_")
			normalized = strings.ReplaceAll(normalized, ".", "_")
			return normalized
		},
		"sshFingerprint": func(pubKey string) string {
			fp, err := crypto.SSHFingerprint(pubKey)
			if err != nil {
				return "error"
			}
			return fp
		},
		"otherAccounts": func(activeDid string, accounts []oauth.AccountInfo) []oauth.AccountInfo {
			result := make([]oauth.AccountInfo, 0, len(accounts))
			for _, acc := range accounts {
				if acc.Did != activeDid {
					result = append(result, acc)
				}
			}
			return result
		},
		"isGenerated": func(path string) bool {
			return enry.IsGenerated(path, nil)
		},
		// NOTE(boltless): I know... I hate doing this too
		"asReactionMapMap": func(dict any) map[syntax.ATURI]map[models.ReactionKind]models.ReactionDisplayData {
			if dict == nil {
				return make(map[syntax.ATURI]map[models.ReactionKind]models.ReactionDisplayData)
			}
			m, _ := dict.(map[syntax.ATURI]map[models.ReactionKind]models.ReactionDisplayData)
			return m
		},
		"asReactionStatusMapMap": func(dict any) map[syntax.ATURI]map[models.ReactionKind]bool {
			if dict == nil {
				log.Println("returning empty map")
				return make(map[syntax.ATURI]map[models.ReactionKind]bool)
			}
			m, _ := dict.(map[syntax.ATURI]map[models.ReactionKind]bool)
			return m
		},
		// constant values used to define a template
		"const": func() map[string]any {
			return map[string]any{
				"OrderedReactionKinds": models.OrderedReactionKinds,
				// would be great to have ordered maps right about now
				"UserSettingsTabs": []tab{
					{"Name": "profile", "Label": "Profile", "Icon": "user"},
					{"Name": "keys", "Label": "Keys", "Icon": "key"},
					{"Name": "emails", "Label": "Emails", "Icon": "mail"},
					{"Name": "notifications", "Label": "Notifications", "Icon": "bell"},
					{"Name": "knots", "Label": "Knots", "Icon": "volleyball"},
					{"Name": "spindles", "Label": "Spindles", "Icon": "spool"},
					{"Name": "sites", "Label": "Sites", "Icon": "globe"},
				},
				"RepoSettingsTabs": []tab{
					{"Name": "general", "Label": "General", "Icon": "sliders-horizontal"},
					{"Name": "access", "Label": "Access", "Icon": "users"},
					{"Name": "pipelines", "Label": "Pipelines", "Icon": "layers-2"},
					{"Name": "hooks", "Label": "Hooks", "Icon": "webhook"},
					{"Name": "sites", "Label": "Sites", "Icon": "globe"},
				},
				"PdsUserDomain": p.pdsCfg.UserDomain,
			}
		},
		"did": func(s string) syntax.DID {
			// cast to DID
			return syntax.DID(s)
		},
	}
}

func primaryHandle(r *idresolver.Resolver, s string) string {
	identity, err := r.ResolveIdent(context.Background(), s)
	if err != nil || identity.Handle.IsInvalidHandle() {
		return "handle.invalid"
	}
	return identity.Handle.String()
}

func (p *Pages) DisplayHandle(ctx context.Context, did string) string {
	if p.db != nil {
		if h := cache.LookupPreferredHandle(ctx, p.rdb, p.db, did); h != "" {
			return h
		}
	}
	if id, err := p.resolver.ResolveIdent(ctx, did); err == nil && !id.Handle.IsInvalidHandle() {
		return id.Handle.String()
	}
	return did
}

func (p *Pages) AvatarUrl(actor, size string) string {
	actor = strings.TrimPrefix(actor, "@")

	identity, err := p.resolver.ResolveIdent(context.Background(), actor)
	var did string
	if err != nil {
		did = actor
	} else {
		did = identity.DID.String()
	}

	secret := p.avatar.SharedSecret
	if secret == "" {
		return ""
	}
	h := hmac.New(sha256.New, []byte(secret))
	h.Write([]byte(did))
	signature := hex.EncodeToString(h.Sum(nil))

	// Get avatar CID for cache busting
	version := ""
	if p.db != nil {
		profile, err := db.GetProfile(p.db, did)
		if err == nil && profile != nil && profile.Avatar != "" {
			// Use first 8 chars of avatar CID as version
			if len(profile.Avatar) > 8 {
				version = profile.Avatar[:8]
			} else {
				version = profile.Avatar
			}
		}
	}

	baseUrl := fmt.Sprintf("%s/%s/%s", p.avatar.Host, signature, did)
	if size != "" {
		if version != "" {
			return fmt.Sprintf("%s?size=%s&v=%s", baseUrl, size, version)
		}
		return fmt.Sprintf("%s?size=%s", baseUrl, size)
	}
	if version != "" {
		return fmt.Sprintf("%s?v=%s", baseUrl, version)
	}

	return baseUrl
}

func (p *Pages) icon(name string, classes []string) (template.HTML, error) {
	iconPath := filepath.Join("static", "icons", name)

	if filepath.Ext(name) == "" {
		iconPath += ".svg"
	}

	data, err := Files.ReadFile(iconPath)
	if err != nil {
		return "", fmt.Errorf("icon %s not found: %w", name, err)
	}

	// Convert SVG data to string
	svgStr := string(data)

	svgTagEnd := strings.Index(svgStr, ">")
	if svgTagEnd == -1 {
		return "", fmt.Errorf("invalid SVG format for icon %s", name)
	}

	classTag := ` class="` + strings.Join(classes, " ") + `"`

	modifiedSVG := svgStr[:svgTagEnd] + classTag + svgStr[svgTagEnd:]
	return template.HTML(modifiedSVG), nil
}

func durationFmt(duration time.Duration, names [4]string) string {
	days := int64(duration.Hours() / 24)
	hours := int64(math.Mod(duration.Hours(), 24))
	minutes := int64(math.Mod(duration.Minutes(), 60))
	seconds := int64(math.Mod(duration.Seconds(), 60))

	chunks := []struct {
		name   string
		amount int64
	}{
		{names[0], days},
		{names[1], hours},
		{names[2], minutes},
		{names[3], seconds},
	}

	parts := []string{}

	for _, chunk := range chunks {
		if chunk.amount != 0 {
			parts = append(parts, fmt.Sprintf("%d%s", chunk.amount, chunk.name))
		}
	}

	return strings.Join(parts, " ")
}
