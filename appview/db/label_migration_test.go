package db

import (
	"database/sql"
	"path/filepath"
	"testing"
)

func TestDropLabelOpsIndexedColumn(t *testing.T) {
	path := filepath.Join(t.TempDir(), "legacy.db")
	conn, err := sql.Open("sqlite3", path+"?_foreign_keys=1")
	if err != nil {
		t.Fatalf("open: %v", err)
	}
	defer conn.Close()

	legacy := `
		create table label_definitions (
			id integer primary key autoincrement,
			did text not null,
			rkey text not null,
			at_uri text generated always as ('at://' || did || '/' || 'sh.tangled.label.definition' || '/' || rkey) stored,
			name text not null,
			unique (at_uri)
		);
		create table label_ops (
			id integer primary key autoincrement,
			did text not null,
			rkey text not null,
			at_uri text generated always as ('at://' || did || '/' || 'sh.tangled.label.op' || '/' || rkey) stored,
			subject text not null,
			operation text not null check (operation in ("add", "del")),
			operand_key text not null,
			operand_value text not null,
			performed text not null default (strftime('%Y-%m-%dT%H:%M:%SZ', 'now')),
			indexed text not null default (strftime('%Y-%m-%dT%H:%M:%SZ', 'now')),
			foreign key (operand_key) references label_definitions (at_uri) on delete cascade,
			unique (did, rkey, subject, operand_key, operand_value)
		);
	`
	if _, err := conn.Exec(legacy); err != nil {
		t.Fatalf("legacy schema: %v", err)
	}

	defUri := "at://did:plc:boltless/sh.tangled.label.definition/prio"
	if _, err := conn.Exec(
		`insert into label_definitions (did, rkey, name) values (?, ?, ?)`,
		"did:plc:boltless", "prio", "priority",
	); err != nil {
		t.Fatalf("seed def: %v", err)
	}
	if _, err := conn.Exec(
		`insert into label_ops (did, rkey, subject, operation, operand_key, operand_value) values (?, ?, ?, ?, ?, ?)`,
		"did:plc:boltless", "op1", "at://did:plc:boltless/sh.tangled.repo.issue/issue1", "add", defUri, "high",
	); err != nil {
		t.Fatalf("seed op: %v", err)
	}

	indexedColumns := func() int {
		var count int
		if err := conn.QueryRow(
			`select count(*) from pragma_table_info('label_ops') where name = 'indexed'`,
		).Scan(&count); err != nil {
			t.Fatalf("pragma_table_info: %v", err)
		}
		return count
	}

	if indexedColumns() != 1 {
		t.Fatalf("legacy table must carry the indexed column before the migration")
	}
	if _, err := conn.Exec(`alter table label_ops drop column indexed`); err != nil {
		t.Fatalf("drop column indexed must succeed against the real table shape: %v", err)
	}
	if indexedColumns() != 0 {
		t.Fatalf("indexed column must be gone after the drop")
	}

	var gotAtUri, gotVal string
	if err := conn.QueryRow(
		`select at_uri, operand_value from label_ops where did = ? and rkey = ?`,
		"did:plc:boltless", "op1",
	).Scan(&gotAtUri, &gotVal); err != nil {
		t.Fatalf("row must survive the drop: %v", err)
	}
	if want := "at://did:plc:boltless/sh.tangled.label.op/op1"; gotAtUri != want {
		t.Fatalf("generated at_uri must survive the drop: got %q want %q", gotAtUri, want)
	}
	if gotVal != "high" {
		t.Fatalf("operand value must survive the drop: got %q", gotVal)
	}
}
