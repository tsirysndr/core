//go:build linux

package microvm

import (
	"fmt"
	"strings"
)

type argBuilder struct {
	args []string
}

func newArgBuilder(capacity int) argBuilder {
	return argBuilder{
		args: make([]string, 0, capacity),
	}
}

func (b *argBuilder) Add(args ...string) *argBuilder {
	b.args = append(b.args, args...)
	return b
}

func (b *argBuilder) Flag(name string) *argBuilder {
	b.args = append(b.args, name)
	return b
}

func (b *argBuilder) Opt(name, value string) *argBuilder {
	b.args = append(b.args, name, value)
	return b
}

func (b *argBuilder) Optf(name, format string, values ...any) *argBuilder {
	return b.Opt(name, fmt.Sprintf(format, values...))
}

func (b *argBuilder) Args() []string {
	args := make([]string, len(b.args))
	copy(args, b.args)
	return args
}

type optionBuilder struct {
	parts []string
}

func newOptionBuilder(capacity int) optionBuilder {
	return optionBuilder{
		parts: make([]string, 0, capacity),
	}
}

func (b *optionBuilder) Add(parts ...string) *optionBuilder {
	b.parts = append(b.parts, parts...)
	return b
}

func (b *optionBuilder) KV(key, value string) *optionBuilder {
	b.parts = append(b.parts, key+"="+value)
	return b
}

func (b *optionBuilder) KVf(key, format string, values ...any) *optionBuilder {
	return b.KV(key, fmt.Sprintf(format, values...))
}

func (b optionBuilder) String() string {
	return strings.Join(b.parts, ",")
}
