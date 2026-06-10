package main

import (
	"bytes"
	_ "embed"
	"flag"
	"fmt"
	"image"
	"image/color"
	"image/png"
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
		favicon      bool
	)

	flag.StringVar(&templatePath, "template", "", "Path to dolly go-html template")
	flag.StringVar(&size, "size", "512x512", "Output size in format WIDTHxHEIGHT (e.g., 512x512)")
	flag.StringVar(&fillColor, "color", "#000000", "Fill color in hex format (e.g., #FF5733)")
	flag.StringVar(&output, "output", "dolly.svg", "Output file path (format detected from extension: .svg, .png, or .ico)")
	flag.BoolVar(&favicon, "favicon", false, "Embed a prefers-color-scheme style block so the SVG reacts to dark mode (SVG output only)")
	flag.Parse()

	if templatePath == "" {
		fmt.Fprintf(os.Stderr, "Empty template path")
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

	tpl, err := os.ReadFile(templatePath)
	if err != nil {
		fmt.Fprintf(os.Stderr, "Failed to read template from path %s: %v\n", templatePath, err)
		os.Exit(1)
	}

	if favicon && format != "svg" {
		fmt.Fprintf(os.Stderr, "-favicon is only supported for .svg output\n")
		os.Exit(1)
	}

	svgData, err := dolly(string(tpl), fillColor, favicon)
	if err != nil {
		fmt.Fprintf(os.Stderr, "Error generating SVG: %v\n", err)
		os.Exit(1)
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

	fmt.Printf("Successfully generated %s (%dx%d)\n", output, width, height)
}

func dolly(tplString, hexColor string, favicon bool) ([]byte, error) {
	tpl, err := template.New("dolly").Parse(tplString)
	if err != nil {
		return nil, err
	}

	var svgData bytes.Buffer
	if err := tpl.ExecuteTemplate(&svgData, "fragments/dolly/logo", map[string]any{
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

func parseSize(size string) (int, int, error) {
	parts := strings.Split(size, "x")
	if len(parts) != 2 {
		return 0, 0, fmt.Errorf("invalid size format, use WIDTHxHEIGHT")
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
