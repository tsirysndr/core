package models

import (
	"os"
	"path/filepath"
	"strings"
	"testing"
)

func testWorkflowId(name string) WorkflowId {
	return WorkflowId{PipelineId: PipelineId{Knot: "knot1", Rkey: "rkey1"}, Name: name}
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
