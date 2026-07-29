package models

import (
	"encoding/base64"
	"strings"
)

// SecretMask replaces secret values in strings with "***".
type SecretMask struct {
	replacer *strings.Replacer
	// length of the longest secret. writers keep the last window-1
	// bytes unflushed so a secret split across writes can still match
	// whole
	window int
}

// NewSecretMask creates a mask for the given secret values.
// Also registers base64-encoded variants of each secret.
func NewSecretMask(values []string) *SecretMask {
	var pairs []string
	add := func(value string) {
		if value != "" {
			pairs = append(pairs, value, "***")
		}
	}

	for _, value := range values {
		if value == "" {
			continue
		}

		add(value)
		// mask each non-empty line of a multiline secret
		// output may split a secret over multiple log lines...
		for _, line := range strings.FieldsFunc(value, func(r rune) bool {
			return r == '\r' || r == '\n'
		}) {
			add(line)
		}

		b64 := base64.StdEncoding.EncodeToString([]byte(value))
		if b64 != value {
			add(b64)
		}

		b64NoPad := strings.TrimRight(b64, "=")
		if b64NoPad != b64 && b64NoPad != value {
			add(b64NoPad)
		}
	}

	if len(pairs) == 0 {
		return nil
	}

	window := 0
	for i := 0; i < len(pairs); i += 2 {
		window = max(window, len(pairs[i]))
	}

	return &SecretMask{
		replacer: strings.NewReplacer(pairs...),
		window:   window,
	}
}

// trailing bytes a streaming caller must keep unflushed so a secret
// spanning a write boundary still matches
func (m *SecretMask) Window() int {
	if m == nil {
		return 0
	}
	if m.window <= 1 {
		return 0
	}
	return m.window - 1
}

// Mask replaces all registered secret values with "***".
func (m *SecretMask) Mask(input string) string {
	if m == nil || m.replacer == nil {
		return input
	}
	return m.replacer.Replace(input)
}
