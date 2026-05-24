package pipelines

import (
	"strings"
	"testing"
)

func TestAnsiState_SingleLine(t *testing.T) {
	tests := []struct {
		name        string
		input       string
		wantContain string // substring expected in rendered HTML
	}{
		{"bold", "\033[1mBold\033[0m", "term-fg1"},
		{"red", "\033[31mRed\033[0m", "term-fg31"},
		{"green", "\033[32mGreen\033[0m", "term-fg32"},
		{"yellow", "\033[33mYellow\033[0m", "term-fg33"},
		{"blue", "\033[34mBlue\033[0m", "term-fg34"},
		{"magenta", "\033[35mMagenta\033[0m", "term-fg35"},
		{"cyan", "\033[36mCyan\033[0m", "term-fg36"},
		{"bold green", "\033[1;32mBold Green\033[0m", "term-fg32"},
		{"red background", "\033[41m Red background \033[0m", "term-bg41"},
		{"256 orange", "\033[38;5;208mANSI 256 orange\033[0m", "term-fgx208"},
		// true color is stripped
		{"true color", "\033[38;2;255;100;0mTrue color orange\033[0m", "True color orange"},
	}
	for _, tt := range tests {
		t.Run(tt.name, func(t *testing.T) {
			a := NewAnsiState()
			got := string(a.Render(tt.input))
			t.Logf("%s → %s", tt.name, got)
			if !strings.Contains(got, tt.wantContain) {
				t.Errorf("expected output to contain %q, got: %s", tt.wantContain, got)
			}
		})
	}
}

func TestAnsiState_MultiLine_CarryOver(t *testing.T) {
	// red opened on line 1 without reset — line 2 should carry it over.
	a := NewAnsiState()

	line1 := a.Render("\033[31mstart of red")
	if !strings.Contains(string(line1), "term-fg31") {
		t.Errorf("line1: expected term-fg31, got: %s", line1)
	}

	line2 := a.Render("still red\033[0m")
	t.Logf("line2 → %s", line2)
	if !strings.Contains(string(line2), "term-fg31") {
		t.Errorf("line2: expected carry-over term-fg31, got: %s", line2)
	}

	// after the reset, line 3 should have no colour.
	line3 := a.Render("plain text")
	t.Logf("line3 → %s", line3)
	if strings.Contains(string(line3), "term-fg31") {
		t.Errorf("line3: expected no term-fg31 after reset, got: %s", line3)
	}
}

func TestAnsiState_MultiLine_StackedSequences(t *testing.T) {
	// bold opened line 1, red added line 2 — both should carry to line 2.
	a := NewAnsiState()

	a.Render("\033[1mBold opened")

	line2 := a.Render("\033[31mRed added")
	t.Logf("line2 → %s", line2)
	if !strings.Contains(string(line2), "term-fg1") {
		t.Errorf("line2: expected carried-over term-fg1, got: %s", line2)
	}
	if !strings.Contains(string(line2), "term-fg31") {
		t.Errorf("line2: expected term-fg31, got: %s", line2)
	}

	a.Render("\033[0mReset")

	line4 := a.Render("plain")
	t.Logf("line4 → %s", line4)
	if strings.Contains(string(line4), "term-") {
		t.Errorf("line4: expected no term- classes after reset, got: %s", line4)
	}
}

func TestAnsiState_Reset_ClearsStack(t *testing.T) {
	a := NewAnsiState()
	a.Render("\033[31m\033[1m\033[33mMultiple opens")
	if len(a.stack) != 3 {
		t.Errorf("expected stack depth 3, got %d", len(a.stack))
	}
	a.Render("\033[0mReset line")
	if len(a.stack) != 0 {
		t.Errorf("expected empty stack after reset, got %d: %v", len(a.stack), a.stack)
	}
}

func TestAnsiState_NoAnsi(t *testing.T) {
	a := NewAnsiState()
	got := string(a.Render("plain text no escapes"))
	t.Logf("got → %s", got)
	if strings.Contains(got, "term-") {
		t.Errorf("expected no term- classes for plain text, got: %s", got)
	}
}

func TestAnsiState_Sanitizer_XSS(t *testing.T) {
	tests := []struct {
		name           string
		input          string
		shuoldBeAbsent string // must not appear literally (unescaped) in output
	}{
		{
			name:           "script tag not executable",
			input:          "<script>alert(1)</script>",
			shuoldBeAbsent: "<script>",
		},
		{
			name:           "img onerror not executable",
			input:          `<img src=x onerror="alert(1)">`,
			shuoldBeAbsent: "<img",
		},
		{
			name:           "ansi with embedded script not executable",
			input:          "\033[31m<script>alert(1)</script>\033[0m",
			shuoldBeAbsent: "<script>",
		},
		{
			name:           "only term- classes survive on span",
			input:          "\033[31mcolored\033[0m",
			shuoldBeAbsent: "onclick",
		},
	}
	for _, tt := range tests {
		t.Run(tt.name, func(t *testing.T) {
			a := NewAnsiState()
			got := string(a.Render(tt.input))
			t.Logf("%s → %s", tt.name, got)
			if strings.Contains(got, tt.shuoldBeAbsent) {
				t.Errorf("expected %q to be absent (unescaped), got: %s", tt.shuoldBeAbsent, got)
			}
		})
	}
}
