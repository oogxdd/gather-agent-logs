package session

import (
	"os"
	"path/filepath"
	"testing"
	"time"
)

func TestParseCodexTimestampsAndTitle(t *testing.T) {
	path := writeFixture(t, "rollout-019ff21e-4824-70d2-8cb6-57e5b1aebefb.jsonl", `
{"timestamp":"2026-01-01T10:00:00Z","type":"session_meta","payload":{"id":"019ff21e-4824-70d2-8cb6-57e5b1aebefb","cwd":"/work/repo","timestamp":"2026-01-01T10:00:00Z"}}
{"timestamp":"2026-01-01T10:01:00Z","type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"# AGENTS.md instructions\ninternal"}]}}
{"timestamp":"2026-01-01T10:02:00Z","type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"Fix the auth race"}]}}
{"timestamp":"2026-01-01T10:04:00Z","type":"response_item","payload":{"type":"message","role":"assistant","content":[]}}
{"timestamp":"2026-01-01T10:05:00Z","type":"response_item","payload":{"type":"custom_tool_call"}}
`)

	item, err := parseFile(path, Codex)
	if err != nil {
		t.Fatal(err)
	}
	if item.Title != "Fix the auth race" || item.CWD != "/work/repo" {
		t.Fatalf("unexpected metadata: %+v", item)
	}
	assertTime(t, item.Created, "2026-01-01T10:00:00Z")
	assertTime(t, item.Updated, "2026-01-01T10:04:00Z")
}

func TestParseClaudeTimestampsAndTitle(t *testing.T) {
	path := writeFixture(t, "86e1a7f3-2e99-483f-b743-64e0003c58c8.jsonl", `
{"type":"queue-operation","timestamp":"2026-02-01T10:00:00Z","sessionId":"86e1a7f3-2e99-483f-b743-64e0003c58c8"}
{"type":"user","timestamp":"2026-02-01T10:01:00Z","sessionId":"86e1a7f3-2e99-483f-b743-64e0003c58c8","cwd":"/work/repo","message":{"role":"user","content":"Ship the CLI"}}
{"type":"assistant","timestamp":"2026-02-01T10:03:00Z","sessionId":"86e1a7f3-2e99-483f-b743-64e0003c58c8","cwd":"/work/repo","message":{"role":"assistant","content":"Done"}}
{"type":"last-prompt","timestamp":"2026-02-01T10:05:00Z","sessionId":"86e1a7f3-2e99-483f-b743-64e0003c58c8"}
`)

	item, err := parseFile(path, Claude)
	if err != nil {
		t.Fatal(err)
	}
	if item.Title != "Ship the CLI" || item.CWD != "/work/repo" {
		t.Fatalf("unexpected metadata: %+v", item)
	}
	assertTime(t, item.Created, "2026-02-01T10:00:00Z")
	assertTime(t, item.Updated, "2026-02-01T10:03:00Z")
}

func TestSortModes(t *testing.T) {
	items := []Session{
		{ID: "old-created-new-updated", Created: time.Unix(100, 0), Updated: time.Unix(400, 0)},
		{ID: "new-created-old-updated", Created: time.Unix(300, 0), Updated: time.Unix(350, 0)},
	}
	Sort(items, SortCreated)
	if items[0].ID != "new-created-old-updated" {
		t.Fatalf("created sort: %+v", items)
	}
	Sort(items, SortUpdated)
	if items[0].ID != "old-created-new-updated" {
		t.Fatalf("updated sort: %+v", items)
	}
}

func writeFixture(t *testing.T, name, contents string) string {
	t.Helper()
	path := filepath.Join(t.TempDir(), name)
	if err := os.WriteFile(path, []byte(contents), 0o600); err != nil {
		t.Fatal(err)
	}
	return path
}

func assertTime(t *testing.T, got time.Time, expected string) {
	t.Helper()
	want, err := time.Parse(time.RFC3339, expected)
	if err != nil {
		t.Fatal(err)
	}
	if !got.Equal(want) {
		t.Fatalf("got %s, want %s", got, want)
	}
}
