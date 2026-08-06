package xrpc

import (
	"fmt"
	"net/http"
	"runtime/debug"

	"tangled.org/core/api/tangled"
	"tangled.org/core/consts"
)

// version is set during build time.
var version string

var knotCapabilities = []string{
	string(consts.CapKnotACL),
	string(consts.CapRepoDidInput),
}

func (x *Xrpc) Version(w http.ResponseWriter, r *http.Request) {
	if version == "" {
		info, ok := debug.ReadBuildInfo()
		if !ok {
			http.Error(w, "failed to read build info", http.StatusInternalServerError)
			return
		}

		modVer := info.Main.Version
		if modVer == "" || modVer == "(devel)" {
			modVer = "(devel)"
		}

		var sha string
		var modified bool
		for _, setting := range info.Settings {
			switch setting.Key {
			case "vcs.revision":
				sha = setting.Value
			case "vcs.modified":
				modified = setting.Value == "true"
			}
		}

		if sha == "" {
			version = modVer
		} else if modified {
			version = fmt.Sprintf("%s (%s with modifications)", modVer, sha)
		} else {
			version = fmt.Sprintf("%s (%s)", modVer, sha)
		}
	}

	response := tangled.KnotVersion_Output{
		Version:      version,
		Capabilities: knotCapabilities,
	}

	x.writeJson(w, response)
}
