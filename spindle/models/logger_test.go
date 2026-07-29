package models

import (
	"encoding/json"
	"os"
	"path/filepath"
	"strings"
	"testing"
)

func testWorkflowId(name string) WorkflowId {
	return WorkflowId{PipelineId: PipelineId{Knot: "knot1", Rkey: "rkey1"}, Name: name}
}

func readDataContents(t *testing.T, path string) []string {
	t.Helper()
	raw, err := os.ReadFile(path)
	if err != nil {
		t.Fatal(err)
	}

	var got []string
	for _, encoded := range strings.Split(strings.TrimSpace(string(raw)), "\n") {
		var line LogLine
		if err := json.Unmarshal([]byte(encoded), &line); err != nil {
			t.Fatalf("decode log line %q: %v", encoded, err)
		}
		got = append(got, line.Content)
	}
	return got
}

func TestDataWriterMasksSecretSplitAcrossWrites(t *testing.T) {
	dir := t.TempDir()
	secret := "hunter2-super-secret-token"
	wid := testWorkflowId("mask")
	logger, err := NewFileWorkflowLogger(dir, wid, []string{secret})
	if err != nil {
		t.Fatal(err)
	}
	w := logger.DataWriter(0, "stdout")

	for _, ch := range strings.Split("prefix "+secret+" suffix", "") {
		if _, err := w.Write([]byte(ch)); err != nil {
			t.Fatal(err)
		}
	}
	if err := logger.Close(); err != nil {
		t.Fatal(err)
	}

	raw, err := os.ReadFile(filepath.Join(dir, wid.String()+".log"))
	if err != nil {
		t.Fatal(err)
	}
	if strings.Contains(string(raw), secret) {
		t.Errorf("log contains raw secret: %s", raw)
	}
	if !strings.Contains(string(raw), "***") {
		t.Errorf("log does not contain masked marker: %s", raw)
	}
	// trailing bytes land in the final flush entry contiguously
	if !strings.Contains(string(raw), "suffix") {
		t.Errorf("log lost trailing output: %s", raw)
	}
	if got := strings.Join(readDataContents(t, filepath.Join(dir, wid.String()+".log")), "\n"); got != "prefix *** suffix" {
		t.Errorf("masked output changed: %q", got)
	}
}

func TestDataWriterMasksSingleFrame(t *testing.T) {
	dir := t.TempDir()
	secret := "hunter2-super-secret-token"
	wid := testWorkflowId("frame")
	logger, err := NewFileWorkflowLogger(dir, wid, []string{secret})
	if err != nil {
		t.Fatal(err)
	}
	w := logger.DataWriter(0, "stdout")
	if _, err := w.Write([]byte("token is " + secret + " ok")); err != nil {
		t.Fatal(err)
	}
	if err := logger.Close(); err != nil {
		t.Fatal(err)
	}

	raw, err := os.ReadFile(filepath.Join(dir, wid.String()+".log"))
	if err != nil {
		t.Fatal(err)
	}
	if strings.Contains(string(raw), secret) {
		t.Errorf("log contains raw secret: %s", raw)
	}
	if !strings.Contains(string(raw), "en is *** ok") {
		t.Errorf("masked entry mangled: %s", raw)
	}
}

func TestDataWriterNoMaskPassthrough(t *testing.T) {
	dir := t.TempDir()
	wid := testWorkflowId("plain")
	logger, err := NewFileWorkflowLogger(dir, wid, nil)
	if err != nil {
		t.Fatal(err)
	}
	w := logger.DataWriter(0, "stdout")
	if _, err := w.Write([]byte("hello")); err != nil {
		t.Fatal(err)
	}
	if _, err := w.Write([]byte(" world")); err != nil {
		t.Fatal(err)
	}
	if err := logger.Close(); err != nil {
		t.Fatal(err)
	}

	raw, err := os.ReadFile(filepath.Join(dir, wid.String()+".log"))
	if err != nil {
		t.Fatal(err)
	}
	if !strings.Contains(string(raw), "hello") || !strings.Contains(string(raw), " world") {
		t.Errorf("log missing output: %s", raw)
	}
}

func TestDataWriterDoesNotSplitSafeFragmentsIntoLogLines(t *testing.T) {
	dir := t.TempDir()
	wid := testWorkflowId("line-boundaries")
	logger, err := NewFileWorkflowLogger(dir, wid, []string{"a-secret-with-a-long-window"})
	if err != nil {
		t.Fatal(err)
	}
	w := logger.DataWriter(0, "stdout")
	want := []string{
		"first line",
		"second line",
		"third line",
		"fourth line",
		"fifth line",
		"sixth line",
		"seventh line",
		"eighth line",
	}
	for _, line := range want {
		if _, err := w.Write([]byte(line + "\n")); err != nil {
			t.Fatal(err)
		}
	}
	if err := logger.Close(); err != nil {
		t.Fatal(err)
	}

	got := readDataContents(t, filepath.Join(dir, wid.String()+".log"))

	if joined := strings.Join(got, "\n"); joined != strings.Join(want, "\n") {
		t.Fatalf("log content was split at masking window:\n got: %q\nwant: %q", joined, strings.Join(want, "\n"))
	}
}

func TestDataWriterMasksMultilineSecret(t *testing.T) {
	dir := t.TempDir()
	secret := "line-one\nline-two"
	wid := testWorkflowId("multiline-mask")
	logger, err := NewFileWorkflowLogger(dir, wid, []string{secret})
	if err != nil {
		t.Fatal(err)
	}
	w := logger.DataWriter(0, "stdout")
	chunk := strings.Repeat("p", 40) + "\nline-one\nline-two\n" + strings.Repeat("t", 30) + "\nsuffix\n"
	if _, err := w.Write([]byte(chunk)); err != nil {
		t.Fatal(err)
	}
	if err := logger.Close(); err != nil {
		t.Fatal(err)
	}

	got := strings.Join(readDataContents(t, filepath.Join(dir, wid.String()+".log")), "\n")
	want := strings.Repeat("p", 40) + "\n***\n***\n" + strings.Repeat("t", 30) + "\nsuffix"
	if got != want {
		t.Fatalf("multiline secret was not masked: %q", got)
	}
}
