package placement

func IsNativeArchitecture(imageArch, goArch string) bool {
	normalize := func(arch string) string {
		switch arch {
		case "x86_64", "amd64":
			return "amd64"
		case "aarch64", "arm64":
			return "arm64"
		default:
			return arch
		}
	}
	return imageArch != "" && normalize(imageArch) == normalize(goArch)
}
