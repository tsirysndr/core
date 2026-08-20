package dagger

import (
	"fmt"

	"tangled.org/core/spindle/models"
)

// installDaggerStep puts the cli the workflow asked for on PATH.
//
// A workflow pins one with `version:`, an operator with
// SPINDLE_DAGGER_PIPELINES_VERSION, and with neither the installer fetches the
// latest release. An image that already ships the wanted version is left
// alone, which is also how an operator's own image (pinned with
// SPINDLE_DAGGER_PIPELINES_IMAGE) avoids a download it does not need.
func installDaggerStep() Step {
	cmd := scriptPreamble + fmt.Sprintf(`
want="${%[1]s:-}"
want="${want#v}"

current=""
if command -v dagger >/dev/null 2>&1; then
	current="$(dagger version 2>/dev/null | { read -r _ v _ && echo "${v#v}"; } || true)"
fi

if [ -n "$current" ] && { [ -z "$want" ] || [ "$want" = "$current" ]; }; then
	echo "dagger $current is already installed"
	exit 0
fi

echo "installing dagger ${want:-latest}"
if [ -n "$want" ]; then
	curl -fsSL %[2]s | BIN_DIR="$cli_dir" DAGGER_VERSION="$want" sh
else
	curl -fsSL %[2]s | BIN_DIR="$cli_dir" sh
fi

if [ ! -x "$cli_dir/dagger" ]; then
	echo "the dagger installer did not produce a cli at $cli_dir/dagger" >&2
	exit 1
fi
"$cli_dir"/dagger version`, versionEnv, installerURL)

	return Step{
		name:    "Install Dagger",
		kind:    models.StepKindSystem,
		command: cmd,
	}
}

// detectModuleStep works out which directory holds the workflow's dagger
// module and records it for later steps.
//
// An explicit `module:` key wins. Otherwise we look for the two layouts dagger
// itself produces: `dagger init` writes dagger.json at the repository root and
// puts the SDK sources under .dagger, while a module initialised inside a
// subdirectory keeps its own dagger.json next to those sources. `dagger call
// -m` wants the directory containing dagger.json, so the root layout resolves
// to "." even though .dagger is what the user sees.
func detectModuleStep() Step {
	cmd := scriptPreamble + fmt.Sprintf(`
if ! command -v dagger >/dev/null 2>&1; then
	echo "the dagger cli is not present in this image" >&2
	echo "set SPINDLE_DAGGER_PIPELINES_IMAGE, or add dagger to the workflow's dependencies" >&2
	exit 1
fi

mod="${%s:-}"
if [ -n "$mod" ]; then
	if [ ! -f "$mod/dagger.json" ]; then
		echo "no dagger.json in \"$mod\" (set by the workflow's 'module' field)" >&2
		exit 1
	fi
elif [ -f dagger.json ]; then
	mod="."
elif [ -f .dagger/dagger.json ]; then
	mod=".dagger"
elif [ -d .dagger ]; then
	# a .dagger with no dagger.json anywhere is an unbuilt module; let dagger
	# produce the real diagnostic rather than guessing at one here
	mod=".dagger"
else
	echo "no dagger module found in this repository" >&2
	echo "expected a dagger.json at the root or a .dagger directory" >&2
	echo "point at one explicitly with the workflow's 'module' field" >&2
	exit 1
fi

printf '%%s' "$mod" > "$module_file"
echo "dagger module: $mod"
dagger version`, moduleEnv)

	return Step{
		name:    "Detect Dagger module",
		kind:    models.StepKindSystem,
		command: cmd,
	}
}

func linkFunctionsStep() Step {
	cmd := scriptPreamble + `
mod="$(cat "$module_file")"

if ! dagger --progress plain functions -m "$mod" > "$functions_file"; then
	echo "failed to list the functions of dagger module \"$mod\"" >&2
	exit 1
fi
cat "$functions_file"

count=0
while read -r name rest; do
	case "$name" in
		"" | Name | -*) continue ;;
		*[!A-Za-z0-9_-]*) continue ;;
	esac

	printf '#!/bin/sh\nexec dagger --progress plain call -m %s %s "$@"\n' "$mod" "$name" > "$shim_dir/$name"
	chmod +x "$shim_dir/$name"
	count=$((count + 1))
done < "$functions_file"

if [ "$count" -eq 0 ]; then
	echo "dagger module \"$mod\" exposes no functions" >&2
	exit 1
fi
echo "linked $count dagger function(s); call them by name from any step"`

	return Step{
		name:    "Link Dagger functions",
		kind:    models.StepKindSystem,
		command: cmd,
	}
}

const installerURL = "https://dl.dagger.io/dagger/install.sh"

var scriptPreamble = fmt.Sprintf(`set -eu
dagger_dir="${%s:-%s}"
shim_dir="$dagger_dir/bin"
cli_dir="$dagger_dir/cli"
module_file="$dagger_dir/module"
functions_file="$dagger_dir/functions"
mkdir -p "$shim_dir" "$cli_dir"
`, dirEnv, daggerDir)
