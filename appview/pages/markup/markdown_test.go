package markup

import (
	"bytes"
	"strings"
	"testing"

	"tangled.org/core/appview/pages/markup/sanitizer"
)

func TestMermaidExtension(t *testing.T) {
	tests := []struct {
		name        string
		markdown    string
		contains    string
		notContains string
	}{
		{
			name:        "mermaid block produces pre.mermaid",
			markdown:    "```mermaid\ngraph TD\n    A-->B\n```",
			contains:    `<pre class="mermaid">`,
			notContains: `<code class="language-mermaid"`,
		},
		{
			name:     "mermaid block contains diagram source",
			markdown: "```mermaid\ngraph TD\n    A-->B\n```",
			contains: "graph TD",
		},
		{
			name:     "non-mermaid code block is not affected",
			markdown: "```go\nfunc main() {}\n```",
			contains: `<pre class="chroma">`,
		},
	}

	for _, tt := range tests {
		t.Run(tt.name, func(t *testing.T) {
			md := NewMarkdown("tangled.org")

			var buf bytes.Buffer
			if err := md.Convert([]byte(tt.markdown), &buf); err != nil {
				t.Fatalf("failed to convert markdown: %v", err)
			}

			result := buf.String()
			if !strings.Contains(result, tt.contains) {
				t.Errorf("expected output to contain:\n%s\ngot:\n%s", tt.contains, result)
			}
			if tt.notContains != "" && strings.Contains(result, tt.notContains) {
				t.Errorf("expected output NOT to contain:\n%s\ngot:\n%s", tt.notContains, result)
			}
		})
	}
}

func TestMathExtension(t *testing.T) {
	tests := []struct {
		name        string
		markdown    string
		contains    string
		notContains string
	}{
		{
			name:     "inline math produces span with mathjax delimiters",
			markdown: "the famous $E = mc^2$ equation",
			contains: `<span class="math inline">\(E = mc^2\)</span>`,
		},
		{
			name:     "block math produces display span",
			markdown: "$$\n\\frac{a}{b}\n$$",
			contains: `<span class="math display">\[`,
		},
		{
			name:        "underscores inside math are not treated as emphasis",
			markdown:    "$a_1 + a_2$",
			contains:    `\(a_1 + a_2\)`,
			notContains: "<em>",
		},
		{
			name:        "non-math dollar usage is left alone",
			markdown:    "it costs $5 today",
			notContains: `class="math`,
		},
		{
			// regression: two currency amounts must not be parsed as one
			// inline math span (the "$5 and $" .. "10" case).
			name:        "currency pair is not math",
			markdown:    "it costs $5 today and $10 tomorrow",
			contains:    "it costs $5 today and $10 tomorrow",
			notContains: `class="math`,
		},
		{
			// regression: single-line $$...$$ must keep both the math and the
			// trailing prose.
			name:     "single-line block keeps trailing prose",
			markdown: "$$x^2$$ and then prose",
			contains: "and then prose",
		},
		{
			// math content with < / & must be escaped so the sanitizer keeps
			// the span and MathJax reads the literal source.
			name:     "angle brackets in math are escaped",
			markdown: "$a < b$",
			contains: `\(a &lt; b\)`,
		},
	}

	for _, tt := range tests {
		t.Run(tt.name, func(t *testing.T) {
			md := NewMarkdown("tangled.org")

			var buf bytes.Buffer
			if err := md.Convert([]byte(tt.markdown), &buf); err != nil {
				t.Fatalf("failed to convert markdown: %v", err)
			}

			result := buf.String()
			if tt.contains != "" && !strings.Contains(result, tt.contains) {
				t.Errorf("expected output to contain:\n%s\ngot:\n%s", tt.contains, result)
			}
			if tt.notContains != "" && strings.Contains(result, tt.notContains) {
				t.Errorf("expected output NOT to contain:\n%s\ngot:\n%s", tt.notContains, result)
			}
		})
	}
}

// The sanitizer must preserve the carrier spans that MathJax renders client-side.
func TestMathSurvivesSanitizer(t *testing.T) {
	md := NewMarkdown("tangled.org")

	var buf bytes.Buffer
	if err := md.Convert([]byte("inline $x^2$ and block\n\n$$\ny^2\n$$"), &buf); err != nil {
		t.Fatalf("failed to convert markdown: %v", err)
	}

	out := sanitizer.SanitizeDefault(buf.String())

	for _, want := range []string{`class="math inline"`, `class="math display"`} {
		if !strings.Contains(out, want) {
			t.Errorf("sanitizer stripped math span; expected %q in:\n%s", want, out)
		}
	}
}

func TestAtExtension_Rendering(t *testing.T) {
	tests := []struct {
		name     string
		markdown string
		expected string
	}{
		{
			name:     "renders simple at mention",
			markdown: "Hello @user.tngl.sh!",
			expected: `<p>Hello <a href="/user.tngl.sh" class="mention">@user.tngl.sh</a>!</p>`,
		},
		{
			name:     "renders multiple at mentions",
			markdown: "Hi @alice.tngl.sh and @bob.example.com",
			expected: `<p>Hi <a href="/alice.tngl.sh" class="mention">@alice.tngl.sh</a> and <a href="/bob.example.com" class="mention">@bob.example.com</a></p>`,
		},
		{
			name:     "renders at mention in parentheses",
			markdown: "Check this out (@user.tngl.sh)",
			expected: `<p>Check this out (<a href="/user.tngl.sh" class="mention">@user.tngl.sh</a>)</p>`,
		},
		{
			name:     "does not render email",
			markdown: "Contact me at test@example.com",
			expected: `<p>Contact me at <a href="mailto:test@example.com">test@example.com</a></p>`,
		},
		{
			name:     "renders at mention with hyphen",
			markdown: "Follow @user-name.tngl.sh",
			expected: `<p>Follow <a href="/user-name.tngl.sh" class="mention">@user-name.tngl.sh</a></p>`,
		},
		{
			name:     "renders at mention with numbers",
			markdown: "@user123.test456.social",
			expected: `<p><a href="/user123.test456.social" class="mention">@user123.test456.social</a></p>`,
		},
		{
			name:     "at mention at start of line",
			markdown: "@user.tngl.sh is cool",
			expected: `<p><a href="/user.tngl.sh" class="mention">@user.tngl.sh</a> is cool</p>`,
		},
	}

	for _, tt := range tests {
		t.Run(tt.name, func(t *testing.T) {
			md := NewMarkdown("tangled.org")

			var buf bytes.Buffer
			if err := md.Convert([]byte(tt.markdown), &buf); err != nil {
				t.Fatalf("failed to convert markdown: %v", err)
			}

			result := buf.String()
			if result != tt.expected+"\n" {
				t.Errorf("expected:\n%s\ngot:\n%s", tt.expected, result)
			}
		})
	}
}

func TestAtExtension_WithOtherMarkdown(t *testing.T) {
	tests := []struct {
		name     string
		markdown string
		contains string
	}{
		{
			name:     "at mention with bold",
			markdown: "**Hello @user.tngl.sh**",
			contains: `<strong>Hello <a href="/user.tngl.sh" class="mention">@user.tngl.sh</a></strong>`,
		},
		{
			name:     "at mention with italic",
			markdown: "*Check @user.tngl.sh*",
			contains: `<em>Check <a href="/user.tngl.sh" class="mention">@user.tngl.sh</a></em>`,
		},
		{
			name:     "at mention in list",
			markdown: "- Item 1\n- @user.tngl.sh\n- Item 3",
			contains: `<a href="/user.tngl.sh" class="mention">@user.tngl.sh</a>`,
		},
		{
			name:     "at mention in link",
			markdown: "[@regnault.dev](https://regnault.dev)",
			contains: `<a href="https://regnault.dev">@regnault.dev</a>`,
		},
		{
			name:     "at mention in link again",
			markdown: "[check out @regnault.dev](https://regnault.dev)",
			contains: `<a href="https://regnault.dev">check out @regnault.dev</a>`,
		},
		{
			name:     "at mention in link again, multiline",
			markdown: "[\ncheck out @regnault.dev](https://regnault.dev)",
			contains: "<a href=\"https://regnault.dev\">\ncheck out @regnault.dev</a>",
		},
	}

	for _, tt := range tests {
		t.Run(tt.name, func(t *testing.T) {
			md := NewMarkdown("tangled.org")

			var buf bytes.Buffer
			if err := md.Convert([]byte(tt.markdown), &buf); err != nil {
				t.Fatalf("failed to convert markdown: %v", err)
			}

			result := buf.String()
			if !bytes.Contains([]byte(result), []byte(tt.contains)) {
				t.Errorf("expected output to contain:\n%s\ngot:\n%s", tt.contains, result)
			}
		})
	}
}
