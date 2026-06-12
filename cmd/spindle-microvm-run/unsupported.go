//go:build !linux

package main

import (
	"fmt"
	"os"
)

func main() {
	fmt.Fprintf(os.Stderr, "spindle-microvm-run is only supported on Linux\n")
	os.Exit(-1)
}
