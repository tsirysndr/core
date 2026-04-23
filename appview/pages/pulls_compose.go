package pages

import "strings"

type Source string

const (
	SourcePatch  Source = "patch"
	SourceBranch Source = "branch"
	SourceFork   Source = "fork"
)

func ParseSource(s string) (Source, bool) {
	switch strings.ToLower(s) {
	case string(SourcePatch):
		return SourcePatch, true
	case string(SourceFork):
		return SourceFork, true
	case string(SourceBranch):
		return SourceBranch, true
	default:
		return "", false
	}
}
