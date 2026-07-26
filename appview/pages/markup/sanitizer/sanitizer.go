package sanitizer

import (
	"maps"
	"regexp"
	"slices"
	"strings"

	"github.com/alecthomas/chroma/v2"
	"github.com/microcosm-cc/bluemonday"
)

// shared policies built once at init; safe for concurrent use per bluemonday docs
var (
	sharedDefaultPolicy     *bluemonday.Policy
	sharedDescriptionPolicy *bluemonday.Policy
	sharedLogsPolicy        *bluemonday.Policy
)

func init() {
	sharedDefaultPolicy = buildDefaultPolicy()
	sharedDescriptionPolicy = buildDescriptionPolicy()
	sharedLogsPolicy = buildLogsPolicy()
}

func SanitizeDefault(html string) string {
	return sharedDefaultPolicy.Sanitize(html)
}
func SanitizeDescription(html string) string {
	return sharedDescriptionPolicy.Sanitize(html)
}
func SanitizeLogs(html string) string {
	return sharedLogsPolicy.Sanitize(html)
}

func buildDefaultPolicy() *bluemonday.Policy {
	policy := bluemonday.UGCPolicy()

	// Allow generally safe attributes
	generalSafeAttrs := []string{
		"abbr", "accept", "accept-charset",
		"accesskey", "action", "align", "alt",
		"aria-describedby", "aria-hidden", "aria-label", "aria-labelledby",
		"axis", "border", "cellpadding", "cellspacing", "char",
		"charoff", "charset", "checked",
		"clear", "cols", "colspan", "color",
		"compact", "coords", "datetime", "dir",
		"disabled", "enctype", "for", "frame",
		"headers", "height", "hreflang",
		"hspace", "ismap", "label", "lang",
		"maxlength", "media", "method",
		"multiple", "name", "nohref", "noshade",
		"nowrap", "open", "prompt", "readonly", "rel", "rev",
		"rows", "rowspan", "rules", "scope",
		"selected", "shape", "size", "span",
		"start", "summary", "tabindex", "target",
		"title", "type", "usemap", "valign", "value",
		"vspace", "width", "itemprop",
	}

	generalSafeElements := []string{
		"h1", "h2", "h3", "h4", "h5", "h6", "h7", "h8", "br", "b", "i", "strong", "em", "a", "pre", "code", "img", "tt",
		"div", "ins", "del", "sup", "sub", "p", "ol", "ul", "table", "thead", "tbody", "tfoot", "blockquote", "label",
		"dl", "dt", "dd", "kbd", "q", "samp", "var", "hr", "ruby", "rt", "rp", "li", "tr", "td", "th", "s", "strike", "summary",
		"details", "caption", "figure", "figcaption",
		"abbr", "bdo", "cite", "dfn", "mark", "small", "span", "time", "video", "wbr",
	}

	policy.AllowAttrs(generalSafeAttrs...).OnElements(generalSafeElements...)

	// video
	policy.AllowAttrs("src", "autoplay", "controls").OnElements("video")

	// picture/source for modern image formats (avif, webp, etc.)
	policy.AllowAttrs("srcset", "type", "media").OnElements("source")

	// marker the editor uses to preview an uncommitted blob image; see markdown.go
	policy.AllowAttrs("data-blob-cid").OnElements("img", "source")

	// checkboxes
	policy.AllowAttrs("type").Matching(regexp.MustCompile(`^checkbox$`)).OnElements("input")
	policy.AllowAttrs("checked", "disabled", "data-source-position").OnElements("input")

	// for code blocks
	policy.AllowAttrs("class").Matching(regexp.MustCompile(`chroma|mermaid`)).OnElements("pre")
	policy.AllowAttrs("class").Matching(regexp.MustCompile(`anchor|footnote-ref|footnote-backref`)).OnElements("a")
	policy.AllowAttrs("class").Matching(regexp.MustCompile(`heading`)).OnElements("h1", "h2", "h3", "h4", "h5", "h6", "h7", "h8")
	policy.AllowAttrs("class").Matching(regexp.MustCompile(strings.Join(slices.Collect(maps.Values(chroma.StandardTypes)), "|"))).OnElements("span")

	// at-mentions
	policy.AllowAttrs("class").Matching(regexp.MustCompile(`mention`)).OnElements("a")

	// centering content
	policy.AllowElements("center")

	policy.AllowAttrs("align", "style", "width", "height").Globally()
	policy.AllowStyles(
		"margin",
		"padding",
		"text-align",
		"font-weight",
		"text-decoration",
		"padding-left",
		"padding-right",
		"padding-top",
		"padding-bottom",
		"margin-left",
		"margin-right",
		"margin-top",
		"margin-bottom",
	)

	// math: the math extension emits <span class="math inline|display"> wrapping
	// the raw LaTeX (delimited by \( \) / \[ \]). MathJax renders it client-side,
	// so the sanitizer only needs to preserve these carrier spans.
	policy.AllowAttrs("class").Matching(regexp.MustCompile(`^math (inline|display)$`)).OnElements("span")

	// raw MathML: markdown is rendered with html.WithUnsafe(), so hand-authored
	// <math>...</math> in source passes through to here. Browsers render
	// presentation MathML natively, so preserve the elements and their attributes.
	mathAttrs := []string{
		"accent", "columnalign", "columnlines", "columnspan", "dir", "display",
		"displaystyle", "encoding", "fence", "form", "largeop", "linebreak",
		"linethickness", "lspace", "mathcolor", "mathsize", "mathvariant", "minsize",
		"movablelimits", "notation", "rowalign", "rspace", "rowspacing", "rowspan",
		"scriptlevel", "stretchy", "symmetric", "title", "voffset", "width",
	}
	mathElements := []string{
		"annotation", "math", "menclose", "merror", "mfrac", "mi", "mmultiscripts",
		"mn", "mo", "mover", "mpadded", "mprescripts", "mroot", "mrow", "mspace",
		"msqrt", "mstyle", "msub", "msubsup", "msup", "mtable", "mtd", "mtext",
		"mtr", "munder", "munderover", "semantics",
	}
	policy.AllowNoAttrs().OnElements(mathElements...)
	policy.AllowAttrs(mathAttrs...).OnElements(mathElements...)

	// goldmark-callout
	policy.AllowAttrs("data-callout").OnElements("details")

	return policy
}

func buildDescriptionPolicy() *bluemonday.Policy {
	policy := bluemonday.NewPolicy()
	policy.AllowStandardURLs()

	// allow italics and bold.
	policy.AllowElements("i", "b", "em", "strong")

	// allow code.
	policy.AllowElements("code")

	// allow links
	policy.AllowAttrs("href", "target", "rel").OnElements("a")

	return policy
}

func buildLogsPolicy() *bluemonday.Policy {
	policy := bluemonday.NewPolicy()

	policy.AllowElements("p", "span")

	// allow italics and bold
	policy.AllowElements("i", "b", "em", "strong")

	// allow fg/bg classes from terminal-to-html
	policy.AllowAttrs("class").Matching(regexp.MustCompile(`term-*`)).OnElements("span")

	return policy
}
