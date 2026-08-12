package tui

import (
	"fmt"
	"testing"
	"time"

	"charm.land/lipgloss/v2"

	"github.com/oogxdd/gather-agent-logs/go/internal/session"
)

func TestFuzzyFilterMatchesMetadataAcrossFields(t *testing.T) {
	m := New([]session.Session{
		testSession(session.Codex, "Fix authentication", "/work/backend", 100, 300),
		testSession(session.Claude, "Polish the header", "/work/frontend", 200, 400),
	}, 0, session.SortUpdated)
	m.input.SetValue("frontend header")
	m.refilter()

	if len(m.filtered) != 1 || m.filtered[0] != 1 {
		t.Fatalf("unexpected matches: %v", m.filtered)
	}
}

func TestSelectionKeepsOnlyVisibleWindow(t *testing.T) {
	items := make([]session.Session, 20)
	for index := range items {
		items[index] = testSession(
			session.Codex,
			fmt.Sprintf("Session %d", index),
			fmt.Sprintf("/work/project-%d", index),
			int64(index),
			int64(index),
		)
	}
	m := New(items, 0, session.SortUpdated)
	m.listHeight = 5
	m.selectPosition(12)
	if m.offset != 8 {
		t.Fatalf("offset = %d, want 8", m.offset)
	}
	m.selectPosition(3)
	if m.offset != 3 {
		t.Fatalf("offset = %d, want 3", m.offset)
	}
}

func TestVimFocusAndScrolling(t *testing.T) {
	m := New([]session.Session{
		testSession(session.Codex, stringsOfLength(800), "/work/project", 100, 200),
	}, 0, session.SortUpdated)
	m.resize(45, 14)
	m.setDetailsContent()

	m.handleBrowseKey("ctrl+w")
	if m.focus != focusDetails {
		t.Fatalf("focus = %v, want details", m.focus)
	}
	m.handleBrowseKey("j")
	m.handleBrowseKey("ctrl+d")
	if m.details.YOffset() == 0 {
		t.Fatal("details did not scroll")
	}
	m.handleBrowseKey("ctrl+w")
	if m.focus != focusSessions {
		t.Fatalf("focus = %v, want sessions", m.focus)
	}
}

func TestDoubleGAndUpperGJumpToEnds(t *testing.T) {
	items := make([]session.Session, 20)
	for index := range items {
		items[index] = testSession(session.Codex, fmt.Sprintf("Session %d", index), "/work", int64(index), int64(index))
	}
	m := New(items, 0, session.SortUpdated)
	m.selectPosition(8)
	m.handleBrowseKey("g")
	if m.selected != 8 {
		t.Fatalf("first g moved selection to %d", m.selected)
	}
	m.handleBrowseKey("g")
	if m.selected != 0 {
		t.Fatalf("gg moved selection to %d", m.selected)
	}
	m.handleBrowseKey("G")
	if m.selected != 19 {
		t.Fatalf("G moved selection to %d", m.selected)
	}
}

func TestSortTogglePreservesSelectedSession(t *testing.T) {
	m := New([]session.Session{
		testSession(session.Codex, "old-created-new-updated", "/work/a", 100, 400),
		testSession(session.Codex, "new-created-old-updated", "/work/b", 300, 350),
	}, 0, session.SortUpdated)
	m.selectPosition(0)
	selected, _ := m.selectedSession()
	m.handleBrowseKey("s")
	after, _ := m.selectedSession()

	if m.sortMode != session.SortCreated {
		t.Fatalf("sort = %s, want created", m.sortMode)
	}
	if selected.ID != after.ID {
		t.Fatalf("selection changed from %q to %q", selected.ID, after.ID)
	}
}

func TestEnterSelectsCurrentSession(t *testing.T) {
	m := New([]session.Session{
		testSession(session.Claude, "Resume me", "/work/resume", 100, 200),
	}, 0, session.SortUpdated)
	if action := m.handleBrowseKey("enter"); action != actionResume {
		t.Fatalf("action = %v, want resume", action)
	}
	m.chooseSelected()
	chosen, ok := m.Selection()
	if !ok || chosen.Agent != session.Claude || chosen.CWD != "/work/resume" {
		t.Fatalf("unexpected selection: %+v, %v", chosen, ok)
	}
}

func TestPanelsStayInsideCalculatedLayout(t *testing.T) {
	m := New([]session.Session{
		testSession(session.Codex, stringsOfLength(800), "/work/project", 100, 200),
	}, 0, session.SortUpdated)
	m.resize(120, 30)

	assertDimensions(t, m.renderSearch(), 120, 3)
	assertDimensions(t, m.renderSessions(), m.listWidth, m.listPanelH)
	assertDimensions(t, m.renderDetails(), m.detailWidth, m.detailPanelH)
}

func testSession(agent session.Agent, title, cwd string, created, updated int64) session.Session {
	return session.Session{
		Agent:   agent,
		ID:      title + "-id",
		Title:   title,
		CWD:     cwd,
		Path:    "/logs/session.jsonl",
		Created: time.Unix(created, 0).UTC(),
		Updated: time.Unix(updated, 0).UTC(),
	}
}

func stringsOfLength(length int) string {
	value := ""
	for len(value) < length {
		value += "long details line "
	}
	return value
}

func assertDimensions(t *testing.T, rendered string, width, height int) {
	t.Helper()
	if got := lipgloss.Width(rendered); got != width {
		t.Fatalf("width = %d, want %d", got, width)
	}
	if got := lipgloss.Height(rendered); got != height {
		t.Fatalf("height = %d, want %d", got, height)
	}
}
