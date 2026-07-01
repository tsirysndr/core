package extension

import (
	"bytes"

	"github.com/gohugoio/hugo-goldmark-extensions/passthrough"
	"github.com/yuin/goldmark"
	"github.com/yuin/goldmark/ast"
	"github.com/yuin/goldmark/renderer"
	"github.com/yuin/goldmark/util"
)

// MathExt renders LaTeX math for client-side typesetting.
//
// Detection is delegated to the Hugo passthrough extension, which correctly
// handles single-line "$$...$$" display blocks and protects markdown inside
// math (e.g. "$a_1$" is not emphasis).
// We override passthrough's verbatim renderers so each span is wrapped in
// <span class="math inline|display"> carrying the raw LaTeX with MathJax
// \( \) / \[ \] delimiters. MathJax (loaded on demand in layouts/base.html)
// typesets only these spans, so surrounding prose is never scanned — which is
// what keeps a stray "$" in body text from being interpreted as math.
var MathExt = &mathExt{}

type mathExt struct{}

func (e *mathExt) Extend(m goldmark.Markdown) {
	// Installs the passthrough parsers (and its own verbatim renderers, which
	// we override below).
	passthrough.New(passthrough.Config{
		InlineDelimiters: []passthrough.Delimiters{
			{Open: "$", Close: "$"},
			{Open: `\(`, Close: `\)`},
		},
		BlockDelimiters: []passthrough.Delimiters{
			{Open: "$$", Close: "$$"},
			{Open: `\[`, Close: `\]`},
		},
	}).Extend(m)

	// Higher priority than passthrough's default renderers (priority 100), so
	// ours win for the passthrough node kinds.
	m.Renderer().AddOptions(renderer.WithNodeRenderers(
		util.Prioritized(&mathRenderer{}, 1),
	))
}

type mathRenderer struct{}

func (r *mathRenderer) RegisterFuncs(reg renderer.NodeRendererFuncRegisterer) {
	reg.Register(passthrough.KindPassthroughInline, r.renderInline)
	reg.Register(passthrough.KindPassthroughBlock, r.renderBlock)
}

func (r *mathRenderer) renderInline(w util.BufWriter, source []byte, n ast.Node, entering bool) (ast.WalkStatus, error) {
	if !entering {
		return ast.WalkSkipChildren, nil
	}
	node := n.(*passthrough.PassthroughInline)
	open, closing := node.Delimiters.Open, node.Delimiters.Close
	raw := node.Segment.Value(source)
	inner := raw[len(open) : len(raw)-len(closing)]

	// Currency guard for "$"-delimited inline math (Pandoc's rules): a "$...$"
	// run is not math if the inner text is empty or space-padded, or if the
	// closing "$" is immediately followed by a digit (e.g. "$5 and $10"). In
	// those cases emit the run verbatim so MathJax never sees it.
	if open == "$" {
		var after byte
		if node.Segment.Stop < len(source) {
			after = source[node.Segment.Stop]
		}
		if len(inner) == 0 || inner[0] == ' ' || inner[len(inner)-1] == ' ' || isASCIIDigit(after) {
			w.Write(raw)
			return ast.WalkSkipChildren, nil
		}
	}

	w.WriteString(`<span class="math inline">\(`)
	w.Write(util.EscapeHTML(inner))
	w.WriteString(`\)</span>`)
	return ast.WalkSkipChildren, nil
}

func (r *mathRenderer) renderBlock(w util.BufWriter, source []byte, n ast.Node, entering bool) (ast.WalkStatus, error) {
	if !entering {
		return ast.WalkSkipChildren, nil
	}
	node := n.(*passthrough.PassthroughBlock)
	open, closing := node.Delimiters.Open, node.Delimiters.Close

	var buf bytes.Buffer
	for i := 0; i < node.Lines().Len(); i++ {
		seg := node.Lines().At(i)
		buf.Write(seg.Value(source))
	}

	inner := bytes.TrimSpace(buf.Bytes())
	inner = bytes.TrimPrefix(inner, []byte(open))
	inner = bytes.TrimSuffix(inner, []byte(closing))
	inner = bytes.TrimSpace(inner)

	w.WriteString(`<p><span class="math display">\[`)
	w.Write(util.EscapeHTML(inner))
	w.WriteString(`\]</span></p>`)
	return ast.WalkSkipChildren, nil
}

func isASCIIDigit(b byte) bool { return b >= '0' && b <= '9' }
