#!/usr/bin/env bash
#
# commit-msg-lint.sh — enforce Go-style commit messages.
#
# Reference: https://go.dev/wiki/CommitMessage
#
#   pkg/path: short summary in the imperative mood
#
#   Optional body explaining what changed and why, wrapped at ~76 columns.
#   The blank line between the summary and the body is required.
#
#   Fixes #123
#
# Usage:
#   commit-msg-lint.sh --file <path>     # lint a single message file (git commit-msg hook)
#   commit-msg-lint.sh --rev  <rev>      # lint the message of one commit
#   commit-msg-lint.sh --range <a> <b>   # lint every commit in a..b (exclusive of a)
#   ... | commit-msg-lint.sh             # lint a message on stdin
#
# Flags:
#   --strict   treat warnings as errors (exit non-zero on warnings too)
#
# Exit status: 0 = clean, 1 = at least one error (or warning under --strict).

set -euo pipefail

# Maximum length of the summary line. Go's guideline is "less than 76".
MAX_SUMMARY_LEN=76

# Bare prefixes that look like Conventional Commits rather than a Go package
# path. These are rejected because the summary should name the package/path
# affected, e.g. `ogre: ...` instead of `fix: ...`.
CONVENTIONAL_TYPES="feat fix chore refactor perf style ci revert"

# Special prefixes that are not repo paths but are conventionally allowed,
# following the Go project (e.g. `all:` for tree-wide changes).
PATH_ALLOWLIST="all"

# Space-separated list of real top-level entries in the repo (dirs and files),
# used to validate that a prefix names an actual path. Populated per run from
# the tree being linted; empty means "couldn't determine, skip path checks".
TOPLEVEL=""

# is_toplevel <name> — true if <name> is a real top-level entry or allowlisted.
is_toplevel() {
	case " $TOPLEVEL " in *" $1 "*) return 0 ;; esac
	case " $PATH_ALLOWLIST " in *" $1 "*) return 0 ;; esac
	return 1
}

# Non-imperative openers (past tense / gerund) that read as "described the
# change" rather than "make the change". Warning only.
NON_IMPERATIVE="added adds adding fixed fixes fixing updated updates updating \
removed removes removing changed changes changing implemented implements \
implementing created creates creating deleted refactored refactoring \
improved improves improving bumped bumps"

STRICT=0
errors=0
warnings=0

err()  { printf '  \033[31merror\033[0m  %s\n' "$1" >&2; errors=$((errors + 1)); }
warn() { printf '  \033[33mwarn\033[0m   %s\n' "$1" >&2; warnings=$((warnings + 1)); }

# lint_message <label> <message>
#
# Validates a single commit message. <label> is used only in diagnostics.
lint_message() {
	local label="$1" msg="$2"
	local before="$errors"

	# Read the message into an array of lines.
	local -a lines=()
	while IFS= read -r line || [ -n "$line" ]; do
		lines+=("$line")
	done <<<"$msg"

	local summary="${lines[0]:-}"

	# Skip messages that git/tools generate and that don't follow the format:
	# merges, reverts, and rebase fixup/squash markers.
	case "$summary" in
		"Merge "* | "Revert "* | "fixup! "* | "squash! "* | "amend! "*)
			return 0
			;;
	esac

	# Ignore trailing comment/diff lines that `git commit` appends to the
	# editor buffer (lines starting with '#').
	local -a body=()
	local i
	for ((i = 0; i < ${#lines[@]}; i++)); do
		[[ "${lines[$i]}" == \#* ]] && continue
		body+=("${lines[$i]}")
	done
	lines=("${body[@]}")
	summary="${lines[0]:-}"

	if [ -z "${summary// /}" ]; then
		err "$label: empty commit message"
		return 0
	fi

	# --- summary line checks -------------------------------------------------

	# Must have a "prefix: summary" shape.
	if [[ "$summary" != *": "* ]]; then
		err "$label: summary must be \"pkg/path: short description\" (missing \"<prefix>: \")"
	else
		local prefix="${summary%%: *}"
		local rest="${summary#*: }"
		local flagged_prefix=0

		# The first path segment of the prefix: everything up to the first
		# '/', ',' or '{'. e.g. "spindle/microvm" -> "spindle",
		# "api,lexicons" -> "api", "appview/{config,state}" -> "appview".
		local first_seg="${prefix%%[/,{]*}"

		# Reject bare Conventional-Commit type prefixes (feat/fix/...) that
		# aren't package paths. A '/' means it's a real path, so allow it.
		if [[ "$prefix" != */* ]]; then
			local lower_prefix
			lower_prefix="$(printf '%s' "$prefix" | tr '[:upper:]' '[:lower:]')"
			# Strip a Conventional-Commit scope suffix, e.g. "fix(og)" -> "fix".
			lower_prefix="${lower_prefix%%(*}"
			local t
			for t in $CONVENTIONAL_TYPES; do
				if [ "$lower_prefix" = "$t" ]; then
					err "$label: \"$prefix:\" looks like a Conventional Commit; name the affected package/path instead (e.g. \"ogre: ...\")"
					flagged_prefix=1
					break
				fi
			done
		fi

		# The prefix must name a real path in the repo: a Go package, or the
		# actual top-level dir. We validate the first segment against the set
		# of top-level entries — this accepts package abbreviations the repo
		# already uses (e.g. "spindle/microvm" for spindle/engines/microvm)
		# while rejecting invented roots like "workflows/rust" (the real path
		# is ".tangled/workflows").
		if [ "$flagged_prefix" -eq 0 ] && [ -n "$TOPLEVEL" ] && ! is_toplevel "$first_seg"; then
			err "$label: \"$first_seg\" is not a path in the repo; use the affected Go package or the real top-level path (e.g. \".tangled/workflows: ...\")"
		fi

		if [ -z "${rest// /}" ]; then
			err "$label: summary has no description after \"$prefix:\""
		else
			# Description should be lowercase and imperative.
			local first_word="${rest%% *}"
			local first_char="${rest:0:1}"
			if [[ "$first_char" =~ [A-Z] ]]; then
				warn "$label: description should start lowercase (\"$first_word\")"
			fi
			local lower_word
			lower_word="$(printf '%s' "$first_word" | tr '[:upper:]' '[:lower:]')"
			local w
			for w in $NON_IMPERATIVE; do
				if [ "$lower_word" = "$w" ]; then
					warn "$label: use the imperative mood (\"$lower_word\" -> imperative form)"
					break
				fi
			done
		fi
	fi

	# No trailing period on the summary.
	if [[ "$summary" == *. ]]; then
		err "$label: summary must not end with a period"
	fi

	# Length: Go suggests under 76, but this isn't strictly enforced, so warn.
	if [ "${#summary}" -gt "$MAX_SUMMARY_LEN" ]; then
		warn "$label: summary is ${#summary} chars (prefer under $MAX_SUMMARY_LEN)"
	fi

	# --- body checks ---------------------------------------------------------

	# If there's more than one line, the second must be blank.
	if [ "${#lines[@]}" -gt 1 ] && [ -n "${lines[1]// /}" ]; then
		err "$label: leave a blank line between the summary and the body"
	fi

	if [ "$errors" -eq "$before" ]; then
		printf '  \033[32mok\033[0m     %s\n' "$label" >&2
	fi
}

# --- argument handling -------------------------------------------------------

mode="stdin"
a=""
b=""

while [ "$#" -gt 0 ]; do
	case "$1" in
		--strict) STRICT=1; shift ;;
		--file)   mode="file";  a="${2:-}"; shift 2 ;;
		--rev)    mode="rev";   a="${2:-}"; shift 2 ;;
		--range)  mode="range"; a="${2:-}"; b="${3:-}"; shift 3 ;;
		-h | --help)
			sed -n '2,30p' "$0"; exit 0 ;;
		*) shift ;;
	esac
done

# load_toplevel <ref> — populate TOPLEVEL from a tree, best-effort.
load_toplevel() {
	TOPLEVEL="$(git ls-tree --name-only "$1" 2>/dev/null | tr '\n' ' ')" || TOPLEVEL=""
}

# load_toplevel_local — top-level entries for local linting (hook): the union
# of what's committed at HEAD and what's on disk, so a commit that introduces a
# new top-level dir can reference it without a false positive.
load_toplevel_local() {
	local root committed ondisk
	root="$(git rev-parse --show-toplevel 2>/dev/null)" || { TOPLEVEL=""; return; }
	committed="$(git ls-tree --name-only HEAD 2>/dev/null || true)"
	ondisk="$(ls -A "$root" 2>/dev/null | grep -vxE '\.git|\.jj' || true)"
	TOPLEVEL="$(printf '%s\n%s\n' "$committed" "$ondisk" | sort -u | tr '\n' ' ')"
}

case "$mode" in
	file)
		load_toplevel_local
		lint_message "commit message" "$(cat "$a")"
		;;
	stdin)
		load_toplevel_local
		lint_message "(stdin)" "$(cat)"
		;;
	rev)
		load_toplevel "$a"
		lint_message "$(git rev-parse --short "$a")" "$(git log -1 --format=%B "$a")"
		;;
	range)
		load_toplevel "$b"
		revs="$(git rev-list --no-merges "$a..$b")"
		if [ -z "$revs" ]; then
			echo "commit-msg-lint: no commits in range $a..$b" >&2
			exit 0
		fi
		while IFS= read -r rev; do
			[ -z "$rev" ] && continue
			label="$(git rev-parse --short "$rev"): $(git log -1 --format=%s "$rev")"
			lint_message "$label" "$(git log -1 --format=%B "$rev")"
		done <<<"$revs"
		;;
esac

# --- summary -----------------------------------------------------------------

if [ "$errors" -gt 0 ] || { [ "$STRICT" -eq 1 ] && [ "$warnings" -gt 0 ]; }; then
	printf '\ncommit-msg-lint: \033[31m%d error(s), %d warning(s)\033[0m\n' "$errors" "$warnings" >&2
	printf 'See https://go.dev/wiki/CommitMessage — format: "pkg/path: short imperative summary"\n' >&2
	exit 1
fi

if [ "$warnings" -gt 0 ]; then
	printf '\ncommit-msg-lint: %d warning(s)\n' "$warnings" >&2
fi
exit 0
