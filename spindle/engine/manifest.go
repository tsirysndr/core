package engine

import (
	"errors"
	"fmt"
	"reflect"
	"strconv"
	"strings"

	"gopkg.in/yaml.v3"
	"tangled.org/core/workflow"
)

// how many lines of context to show on above / below of an offending line.
const frameContext = 3

type manifestError struct {
	line int
	msg  string
}

func (e *manifestError) Error() string { return e.msg }

// codeFrame renders the lines around `line` with a gutter and a `>` marker on
// the offending line, eg.
//
//	  4 | image: alpine
//	> 5 | registre:
//	  6 |   nixpkgs: github:nixos/nixpkgs/nixos-unstable
func codeFrame(raw string, line int) string {
	lines := strings.Split(raw, "\n")
	if line < 1 || line > len(lines) {
		return ""
	}
	start := max(line-frameContext, 1)
	end := min(line+frameContext, len(lines))
	width := len(strconv.Itoa(end))

	var b strings.Builder
	for n := start; n <= end; n++ {
		marker := "  "
		if n == line {
			marker = "> "
		}
		fmt.Fprintf(&b, "%s%*d | %s\n", marker, width, n, lines[n-1])
	}
	return strings.TrimRight(b.String(), "\n")
}

var genericWorkflowKeys = ignoredKeys(reflect.TypeFor[workflow.Workflow]())

// ignoredKeys is the set of yaml keys we ignore on field checks for a struct.
// real, parseable keys come straight from the tags (via fieldsByYAMLName); on
// top of those we tolerate `yaml:"-"` fields by their conventional spelling.
// those have no yaml key of their own (the program fills them in itself, eg.
// `name` from the filename, `raw` from the file bytes), but users sometimes
// write one in the body anyway, and that's harmless rather than a typo.
func ignoredKeys(t reflect.Type) map[string]bool {
	if t.Kind() == reflect.Pointer {
		t = t.Elem()
	}
	keys := make(map[string]bool)
	for k := range fieldsByYAMLName(t) {
		keys[k] = true
	}
	for i := 0; i < t.NumField(); i++ {
		f := t.Field(i)
		if tag, _, _ := strings.Cut(f.Tag.Get("yaml"), ","); tag == "-" {
			keys[strings.ToLower(f.Name)] = true
		}
	}
	return keys
}

// this exists because yaml.v3 reports mismatches as "cannot unmarshal !!seq into
// map[string]interface {}", which is kind of confusing, even if it outputs a line.
// so we use reflection, walk the node tree alongside the schema type, and point
// at the field that's actually mis-shaped.
//
// returns nil when nothing is structurally wrong.
func DescribeManifestError(raw string, schema any) error {
	var doc yaml.Node
	if err := yaml.Unmarshal([]byte(raw), &doc); err != nil {
		return nil
	}
	if len(doc.Content) == 0 {
		return nil
	}
	err := checkNode(doc.Content[0], reflect.TypeOf(schema), "", genericWorkflowKeys)
	var me *manifestError
	if !errors.As(err, &me) {
		return err // nil
	}
	if frame := codeFrame(raw, me.line); frame != "" {
		return fmt.Errorf("%s\n\n%s", me.msg, frame)
	}
	return errors.New(me.msg)
}

// checkNode walks a yaml node against the type it's expected to decode into,
// recursing through structs, maps and slices. allowExtra names keys that are
// valid at this level despite not being in the struct (only the root uses it).
func checkNode(node *yaml.Node, t reflect.Type, path string, allowExtra map[string]bool) error {
	if node.Kind == yaml.AliasNode && node.Alias != nil {
		node = node.Alias
	}
	if t == nil {
		return nil
	}
	if t.Kind() == reflect.Pointer {
		t = t.Elem()
	}
	// `any` accepts anything (eg. registry values) so we can't check more
	if t.Kind() == reflect.Interface {
		return nil
	}
	// an empty value (eg. `registry:` with nothing under it) is harmless
	if node.Kind == yaml.ScalarNode && (node.Tag == "!!null" || node.Value == "") {
		return nil
	}

	want, ok := yamlKindForType(t)
	if !ok {
		return nil
	}
	if node.Kind != want {
		return &manifestError{line: node.Line, msg: fmt.Sprintf(
			"%s must be %s, but got %s (line %d)",
			describePath(path), yamlKindName(want), yamlKindName(node.Kind), node.Line)}
	}

	switch t.Kind() {
	case reflect.Struct:
		fields := fieldsByYAMLName(t)
		for i := 0; i+1 < len(node.Content); i += 2 {
			key, val := node.Content[i], node.Content[i+1]
			ft, ok := fields[key.Value]
			if !ok {
				// a struct has a fixed set of fields, so anything else is a typo.
				// (maps, take arbitrary user-defined keys and don't count)
				if allowExtra[key.Value] {
					continue
				}
				return &manifestError{line: key.Line, msg: fmt.Sprintf(
					"unknown field %s (line %d)",
					describePath(joinKey(path, key.Value)), key.Line)}
			}
			if err := checkNode(val, ft, joinKey(path, key.Value), nil); err != nil {
				return err
			}
		}
	case reflect.Map:
		for i := 0; i+1 < len(node.Content); i += 2 {
			key, val := node.Content[i], node.Content[i+1]
			if err := checkNode(val, t.Elem(), joinKey(path, key.Value), nil); err != nil {
				return err
			}
		}
	case reflect.Slice, reflect.Array:
		for idx, val := range node.Content {
			if err := checkNode(val, t.Elem(), fmt.Sprintf("%s[%d]", path, idx), nil); err != nil {
				return err
			}
		}
	}
	return nil
}

// fieldsByYAMLName maps a struct's yaml keys to their field types, mirroring how
// yaml.v3 resolves keys: explicit tag name, else the lowercased field name.
func fieldsByYAMLName(t reflect.Type) map[string]reflect.Type {
	fields := make(map[string]reflect.Type)
	for i := 0; i < t.NumField(); i++ {
		f := t.Field(i)
		name, _, _ := strings.Cut(f.Tag.Get("yaml"), ",")
		if name == "-" {
			continue
		}
		if name == "" {
			name = strings.ToLower(f.Name)
		}
		fields[name] = f.Type
	}
	return fields
}

func joinKey(path, key string) string {
	if path == "" {
		return key
	}
	return path + "." + key
}

func describePath(path string) string {
	if path == "" {
		return "the manifest"
	}
	return "`" + path + "`"
}

func yamlKindForType(t reflect.Type) (yaml.Kind, bool) {
	switch t.Kind() {
	case reflect.Pointer:
		return yamlKindForType(t.Elem())
	case reflect.Map, reflect.Struct:
		return yaml.MappingNode, true
	case reflect.Slice, reflect.Array:
		return yaml.SequenceNode, true
	case reflect.String, reflect.Bool,
		reflect.Int, reflect.Int8, reflect.Int16, reflect.Int32, reflect.Int64,
		reflect.Uint, reflect.Uint8, reflect.Uint16, reflect.Uint32, reflect.Uint64,
		reflect.Float32, reflect.Float64:
		return yaml.ScalarNode, true
	default:
		return 0, false
	}
}

func yamlKindName(k yaml.Kind) string {
	switch k {
	case yaml.MappingNode:
		return "a mapping"
	case yaml.SequenceNode:
		return "a list"
	case yaml.ScalarNode:
		return "a scalar value"
	case yaml.AliasNode:
		return "an alias"
	default:
		return "an unknown value"
	}
}
