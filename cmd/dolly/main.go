package main

import (
	"bytes"
	_ "embed"
	"flag"
	"fmt"
	"image"
	"image/color"
	"image/png"
	"math"
	"os"
	"path/filepath"
	"strconv"
	"strings"
	"text/template"

	"github.com/srwiley/oksvg"
	"github.com/srwiley/rasterx"
	"golang.org/x/image/draw"
	"tangled.org/core/ico"
)

func main() {
	var (
		size         string
		fillColor    string
		output       string
		templatePath string
		kind         string
		favicon      bool
	)

	flag.StringVar(&templatePath, "template", "", "Path to a dolly go-html template file, or a directory of templates")
	flag.StringVar(&size, "size", "512", "Output size as WIDTH (height derived from aspect ratio, e.g., 512) or WIDTHxHEIGHT (e.g., 512x512)")
	flag.StringVar(&fillColor, "color", "#000000", "Fill color in hex format (e.g., #FF5733)")
	flag.StringVar(&output, "output", "dolly.svg", "Output file path (format detected from extension: .svg, .png, or .ico)")
	flag.StringVar(&kind, "kind", "logo", "Asset to generate: logo (dolly only) or logotype (dolly + wordmark)")
	flag.BoolVar(&favicon, "favicon", false, "Embed a prefers-color-scheme style block so the SVG reacts to dark mode (SVG output only)")
	flag.Parse()

	if templatePath == "" {
		fmt.Fprintf(os.Stderr, "Empty template path")
		os.Exit(1)
	}

	if kind != "logo" && kind != "logotype" {
		fmt.Fprintf(os.Stderr, "Invalid kind: %s. Must be logo or logotype\n", kind)
		os.Exit(1)
	}

	width, height, err := parseSize(size)
	if err != nil {
		fmt.Fprintf(os.Stderr, "Error parsing size: %v\n", err)
		os.Exit(1)
	}

	// Detect format from file extension
	ext := strings.ToLower(filepath.Ext(output))
	format := strings.TrimPrefix(ext, ".")

	if format != "svg" && format != "png" && format != "ico" {
		fmt.Fprintf(os.Stderr, "Invalid file extension: %s. Must be .svg, .png, or .ico\n", ext)
		os.Exit(1)
	}

	if fillColor != "currentColor" && !isValidHexColor(fillColor) {
		fmt.Fprintf(os.Stderr, "Invalid color format: %s. Use hex format like #FF5733\n", fillColor)
		os.Exit(1)
	}

	tpl, err := loadTemplates(templatePath)
	if err != nil {
		fmt.Fprintf(os.Stderr, "Failed to load templates from path %s: %v\n", templatePath, err)
		os.Exit(1)
	}

	if favicon && format != "svg" {
		fmt.Fprintf(os.Stderr, "-favicon is only supported for .svg output\n")
		os.Exit(1)
	}

	svgData, err := dolly(tpl, "fragments/dolly/"+kind, fillColor, favicon)
	if err != nil {
		fmt.Fprintf(os.Stderr, "Error generating SVG: %v\n", err)
		os.Exit(1)
	}

	// Derive height from the SVG's aspect ratio when only a width was given
	if height == 0 && format != "svg" {
		height, err = deriveHeight(svgData, width)
		if err != nil {
			fmt.Fprintf(os.Stderr, "Error deriving height: %v\n", err)
			os.Exit(1)
		}
	}

	// Create output directory if it doesn't exist
	dir := filepath.Dir(output)
	if dir != "" && dir != "." {
		if err := os.MkdirAll(dir, 0755); err != nil {
			fmt.Fprintf(os.Stderr, "Error creating output directory: %v\n", err)
			os.Exit(1)
		}
	}

	switch format {
	case "svg":
		err = saveSVG(svgData, output, width, height)
	case "png":
		err = savePNG(svgData, output, width, height)
	case "ico":
		err = saveICO(svgData, output, width, height)
	}

	if err != nil {
		fmt.Fprintf(os.Stderr, "Error saving file: %v\n", err)
		os.Exit(1)
	}

	if format == "svg" {
		// size is irrelevant for svg output; it scales to its viewBox
		fmt.Printf("Successfully generated %s\n", output)
	} else {
		fmt.Printf("Successfully generated %s (%dx%d)\n", output, width, height)
	}
}

func loadTemplates(path string) (*template.Template, error) {
	info, err := os.Stat(path)
	if err != nil {
		return nil, err
	}

	if info.IsDir() {
		return template.ParseGlob(filepath.Join(path, "*.html"))
	}

	return template.ParseFiles(path)
}

func dolly(tpl *template.Template, name, hexColor string, favicon bool) ([]byte, error) {
	var svgData bytes.Buffer
	if err := tpl.ExecuteTemplate(&svgData, name, map[string]any{
		"FillColor": hexColor,
		"Classes":   "",
		"Favicon":   favicon,
	}); err != nil {
		return nil, err
	}

	return svgData.Bytes(), nil
}

func svgToImage(svgData []byte, w, h int) (image.Image, error) {
	icon, err := oksvg.ReadIconStream(bytes.NewReader(svgData))
	if err != nil {
		return nil, fmt.Errorf("error parsing SVG: %v", err)
	}

	icon.SetTarget(0, 0, float64(w), float64(h))
	rgba := image.NewRGBA(image.Rect(0, 0, w, h))
	draw.Draw(rgba, rgba.Bounds(), &image.Uniform{color.Transparent}, image.Point{}, draw.Src)
	scanner := rasterx.NewScannerGV(w, h, rgba, rgba.Bounds())
	raster := rasterx.NewDasher(w, h, scanner)
	icon.Draw(raster, 1.0)

	return rgba, nil
}

// parseSize parses WIDTH or WIDTHxHEIGHT. A height of 0 means "derive
// from the SVG's aspect ratio".
func parseSize(size string) (int, int, error) {
	if !strings.Contains(size, "x") {
		width, err := strconv.Atoi(size)
		if err != nil {
			return 0, 0, fmt.Errorf("invalid width: %v", err)
		}
		if width <= 0 {
			return 0, 0, fmt.Errorf("width must be positive")
		}
		return width, 0, nil
	}

	parts := strings.Split(size, "x")
	if len(parts) != 2 {
		return 0, 0, fmt.Errorf("invalid size format, use WIDTH or WIDTHxHEIGHT")
	}

	width, err := strconv.Atoi(parts[0])
	if err != nil {
		return 0, 0, fmt.Errorf("invalid width: %v", err)
	}

	height, err := strconv.Atoi(parts[1])
	if err != nil {
		return 0, 0, fmt.Errorf("invalid height: %v", err)
	}

	if width <= 0 || height <= 0 {
		return 0, 0, fmt.Errorf("width and height must be positive")
	}

	return width, height, nil
}

func deriveHeight(svgData []byte, width int) (int, error) {
	icon, err := oksvg.ReadIconStream(bytes.NewReader(svgData))
	if err != nil {
		return 0, fmt.Errorf("error parsing SVG: %v", err)
	}

	if icon.ViewBox.W <= 0 || icon.ViewBox.H <= 0 {
		return 0, fmt.Errorf("SVG has an invalid viewBox (%gx%g)", icon.ViewBox.W, icon.ViewBox.H)
	}

	return int(math.Round(float64(width) * icon.ViewBox.H / icon.ViewBox.W)), nil
}

func isValidHexColor(hex string) bool {
	if len(hex) != 7 || hex[0] != '#' {
		return false
	}
	_, err := strconv.ParseUint(hex[1:], 16, 32)
	return err == nil
}

func saveSVG(svgData []byte, filepath string, _, _ int) error {
	return os.WriteFile(filepath, svgData, 0644)
}

func savePNG(svgData []byte, filepath string, width, height int) error {
	img, err := svgToImage(svgData, width, height)
	if err != nil {
		return err
	}

	f, err := os.Create(filepath)
	if err != nil {
		return err
	}
	defer f.Close()

	return png.Encode(f, img)
}

func saveICO(svgData []byte, filepath string, width, height int) error {
	img, err := svgToImage(svgData, width, height)
	if err != nil {
		return err
	}

	icoData, err := ico.ImageToIco(img)
	if err != nil {
		return err
	}

	return os.WriteFile(filepath, icoData, 0644)
}
