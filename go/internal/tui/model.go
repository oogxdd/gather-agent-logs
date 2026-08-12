package tui

import (
	"fmt"
	"image/color"
	"strings"
	"time"
	"unicode/utf8"

	"charm.land/bubbles/v2/textinput"
	"charm.land/bubbles/v2/viewport"
	tea "charm.land/bubbletea/v2"
	"charm.land/lipgloss/v2"
	"github.com/sahilm/fuzzy"

	"github.com/oogxdd/gather-agent-logs/go/internal/session"
)

type mode uint8

const (
	modeBrowse mode = iota
	modeSearch
)

type focus uint8

const (
	focusSessions focus = iota
	focusDetails
)

type action uint8

const (
	actionContinue action = iota
	actionQuit
	actionResume
	actionSearch
)

var (
	cyan       = lipgloss.Color("6")
	magenta    = lipgloss.Color("5")
	muted      = lipgloss.Color("8")
	selectedBG = lipgloss.Color("8")
)

// Model is the Bubble Tea state for the session picker.
type Model struct {
	sessions   []session.Session
	filtered   []int
	searchable []string
	input      textinput.Model
	details    viewport.Model
	mode       mode
	focus      focus
	sortMode   session.SortMode
	pendingG   bool
	selected   int
	offset     int
	listHeight int
	skipped    int
	width      int
	height     int

	sideBySide   bool
	listWidth    int
	detailWidth  int
	listPanelH   int
	detailPanelH int

	chosen *session.Session
}

// New creates a picker containing already-discovered session metadata.
func New(items []session.Session, skipped int, sortMode session.SortMode) Model {
	input := textinput.New()
	input.Prompt = "/ "
	input.Placeholder = "Press / to search title, project, path, or session ID"
	input.CharLimit = 512

	details := viewport.New()
	details.SoftWrap = true
	details.FillHeight = true

	m := Model{
		sessions: items,
		input:    input,
		details:  details,
		mode:     modeBrowse,
		focus:    focusSessions,
		sortMode: sortMode,
		selected: -1,
		skipped:  skipped,
	}
	m.rebuildSearchable()
	m.refilter()
	m.resize(120, 32)
	return m
}

func (m Model) Init() tea.Cmd { return nil }

func (m Model) Update(message tea.Msg) (tea.Model, tea.Cmd) {
	switch msg := message.(type) {
	case tea.WindowSizeMsg:
		m.resize(msg.Width, msg.Height)
		return m, nil
	case tea.KeyPressMsg:
		key := msg.Keystroke()
		if key == "ctrl+c" {
			return m, tea.Quit
		}
		if m.mode == modeSearch {
			return m.updateSearch(msg, key)
		}
		switch m.handleBrowseKey(key) {
		case actionQuit:
			return m, tea.Quit
		case actionResume:
			m.chooseSelected()
			return m, tea.Quit
		case actionSearch:
			return m, m.input.Focus()
		default:
			return m, nil
		}
	}
	return m, nil
}

func (m Model) updateSearch(msg tea.KeyPressMsg, key string) (tea.Model, tea.Cmd) {
	switch key {
	case "esc":
		m.input.Reset()
		m.input.Blur()
		m.mode = modeBrowse
		m.refilter()
		return m, nil
	case "enter":
		m.chooseSelected()
		if m.chosen != nil {
			return m, tea.Quit
		}
		return m, nil
	case "tab", "shift+tab":
		m.input.Blur()
		m.mode = modeBrowse
		m.focus = focusSessions
		return m, nil
	}

	before := m.input.Value()
	var cmd tea.Cmd
	m.input, cmd = m.input.Update(msg)
	if m.input.Value() != before {
		m.refilter()
	}
	return m, cmd
}

func (m Model) View() tea.View {
	top := m.renderSearch()
	list := m.renderSessions()
	details := m.renderDetails()
	body := lipgloss.JoinVertical(lipgloss.Left, list, details)
	if m.sideBySide {
		body = lipgloss.JoinHorizontal(lipgloss.Top, list, details)
	}
	view := tea.NewView(lipgloss.JoinVertical(lipgloss.Left, top, body, m.renderHelp()))
	view.AltScreen = true
	return view
}

// Selection returns the session chosen with Enter.
func (m Model) Selection() (session.Session, bool) {
	if m.chosen == nil {
		return session.Session{}, false
	}
	return *m.chosen, true
}

func (m *Model) handleBrowseKey(key string) action {
	if key == "g" {
		if m.pendingG {
			m.pendingG = false
			m.jumpStart()
		} else {
			m.pendingG = true
		}
		return actionContinue
	}
	m.pendingG = false

	switch key {
	case "q", "esc":
		return actionQuit
	case "enter":
		if m.selected >= 0 {
			return actionResume
		}
	case "/":
		m.mode = modeSearch
		return actionSearch
	case "c":
		if m.input.Value() != "" {
			m.input.Reset()
			m.refilter()
		}
	case "s":
		m.toggleSort()
	case "ctrl+w", "tab", "shift+tab":
		m.toggleFocus()
	case "h", "left":
		m.focus = focusSessions
	case "l", "right":
		m.focus = focusDetails
	case "j", "down":
		m.scrollFocused(1)
	case "k", "up":
		m.scrollFocused(-1)
	case "ctrl+d":
		m.scrollFocused(m.halfPageStep())
	case "ctrl+u":
		m.scrollFocused(-m.halfPageStep())
	case "ctrl+f", "pgdown":
		m.scrollFocused(m.pageStep())
	case "ctrl+b", "pgup":
		m.scrollFocused(-m.pageStep())
	case "home":
		m.jumpStart()
	case "end", "G":
		m.jumpEnd()
	case "H":
		if m.focus == focusSessions {
			m.selectPosition(m.offset)
		}
	case "M":
		if m.focus == focusSessions {
			m.selectPosition(m.offset + m.listHeight/2)
		}
	case "L":
		if m.focus == focusSessions {
			m.selectPosition(m.offset + max(m.listHeight-1, 0))
		}
	}
	return actionContinue
}

func (m *Model) resize(width, height int) {
	m.width = max(width, 20)
	m.height = max(height, 8)
	bodyHeight := max(m.height-4, 4)
	m.sideBySide = m.width >= 105
	if m.sideBySide {
		m.listWidth = m.width * 68 / 100
		m.detailWidth = m.width - m.listWidth
		m.listPanelH = bodyHeight
		m.detailPanelH = bodyHeight
	} else {
		m.listWidth = m.width
		m.detailWidth = m.width
		m.detailPanelH = min(9, max(4, bodyHeight/2))
		m.listPanelH = bodyHeight - m.detailPanelH
	}
	m.listHeight = max(m.listPanelH-3, 1)
	m.input.SetWidth(max(m.width-32, 10))
	m.details.SetWidth(max(m.detailWidth-2, 1))
	m.details.SetHeight(max(m.detailPanelH-3, 1))
	m.ensureSelectionVisible()
}

func (m *Model) rebuildSearchable() {
	m.searchable = make([]string, len(m.sessions))
	for index := range m.sessions {
		m.searchable[index] = m.sessions[index].SearchText()
	}
}

func (m *Model) refilter() {
	query := strings.TrimSpace(m.input.Value())
	m.filtered = m.filtered[:0]
	if query == "" {
		for index := range m.sessions {
			m.filtered = append(m.filtered, index)
		}
	} else {
		for _, match := range fuzzy.FindNoSort(query, m.searchable) {
			m.filtered = append(m.filtered, match.Index)
		}
	}
	m.offset = 0
	if len(m.filtered) == 0 {
		m.selected = -1
		m.setDetailsContent()
		return
	}
	m.selected = 0
	m.setDetailsContent()
}

func (m *Model) toggleSort() {
	selectedID := ""
	if item, ok := m.selectedSession(); ok {
		selectedID = item.ID
	}
	m.sortMode = m.sortMode.Toggle()
	session.Sort(m.sessions, m.sortMode)
	m.rebuildSearchable()
	m.refilter()
	if selectedID != "" {
		for position, index := range m.filtered {
			if m.sessions[index].ID == selectedID {
				m.selectPosition(position)
				break
			}
		}
	}
}

func (m *Model) selectedSession() (session.Session, bool) {
	if m.selected < 0 || m.selected >= len(m.filtered) {
		return session.Session{}, false
	}
	index := m.filtered[m.selected]
	if index < 0 || index >= len(m.sessions) {
		return session.Session{}, false
	}
	return m.sessions[index], true
}

func (m *Model) chooseSelected() {
	if item, ok := m.selectedSession(); ok {
		copy := item
		m.chosen = &copy
	}
}

func (m *Model) toggleFocus() {
	if m.focus == focusSessions {
		m.focus = focusDetails
	} else {
		m.focus = focusSessions
	}
}

func (m *Model) pageStep() int {
	if m.focus == focusSessions {
		return max(m.listHeight, 1)
	}
	return max(m.details.Height(), 1)
}

func (m *Model) halfPageStep() int { return max(m.pageStep()/2, 1) }

func (m *Model) scrollFocused(delta int) {
	if m.focus == focusSessions {
		m.moveSelection(delta)
		return
	}
	if delta > 0 {
		m.details.ScrollDown(delta)
	} else {
		m.details.ScrollUp(-delta)
	}
}

func (m *Model) jumpStart() {
	if m.focus == focusSessions {
		m.selectPosition(0)
	} else {
		m.details.GotoTop()
	}
}

func (m *Model) jumpEnd() {
	if m.focus == focusSessions {
		m.selectPosition(len(m.filtered) - 1)
	} else {
		m.details.GotoBottom()
	}
}

func (m *Model) moveSelection(delta int) {
	if len(m.filtered) == 0 {
		m.selected = -1
		return
	}
	m.selectPosition(min(max(m.selected+delta, 0), len(m.filtered)-1))
}

func (m *Model) selectPosition(position int) {
	if len(m.filtered) == 0 {
		m.selected = -1
		m.offset = 0
		m.setDetailsContent()
		return
	}
	position = min(max(position, 0), len(m.filtered)-1)
	changed := m.selected != position
	m.selected = position
	m.ensureSelectionVisible()
	if changed {
		m.setDetailsContent()
	}
}

func (m *Model) ensureSelectionVisible() {
	if m.selected < 0 {
		m.offset = 0
		return
	}
	height := max(m.listHeight, 1)
	if m.selected < m.offset {
		m.offset = m.selected
	} else if m.selected >= m.offset+height {
		m.offset = m.selected + 1 - height
	}
	m.offset = min(m.offset, max(len(m.filtered)-height, 0))
}

func (m *Model) setDetailsContent() {
	item, ok := m.selectedSession()
	if !ok {
		m.details.SetContent("No sessions match the current search.")
		m.details.GotoTop()
		return
	}
	m.details.SetContent(strings.Join([]string{
		"Agent   " + item.Agent.Label(),
		"Created " + item.Created.UTC().Format("2006-01-02 15:04 UTC"),
		"Updated " + item.Updated.UTC().Format("2006-01-02 15:04 UTC"),
		"Project " + item.CWD,
		"ID      " + item.ID,
		"",
		item.Title,
		"",
		"Log     " + item.Path,
	}, "\n"))
	m.details.GotoTop()
}

func (m Model) renderSearch() string {
	innerWidth := max(m.width-2, 1)
	query := m.input.Value()
	if query == "" && m.mode == modeBrowse {
		query = "Press / to search title, project, path, or session ID"
	} else {
		query = "/ " + query
		if m.mode == modeSearch {
			query += "▏"
		}
	}
	count := fmt.Sprintf("%d of %d · sort: %s", len(m.filtered), len(m.sessions), m.sortMode)
	line := joinSides("agent-resume  "+query, count, innerWidth)
	borderColor := muted
	if m.mode == modeSearch {
		borderColor = cyan
	}
	return panel(innerWidth+2, 3, borderColor, line)
}

func (m Model) renderSessions() string {
	innerWidth := max(m.listWidth-2, 1)
	header := tableLine("Agent", titleCase(string(m.sortMode)), "Project", "First prompt", innerWidth)
	lines := []string{lipgloss.NewStyle().Foreground(muted).Render(header)}
	end := min(m.offset+m.listHeight, len(m.filtered))
	for position := m.offset; position < end; position++ {
		item := m.sessions[m.filtered[position]]
		row := tableLine(item.Agent.Label(), age(m.sortMode.Timestamp(item)), item.Project(), item.Title, innerWidth)
		if position == m.selected {
			row = lipgloss.NewStyle().Background(selectedBG).Foreground(lipgloss.Color("15")).Bold(true).Render(row)
		} else if item.Agent == session.Codex {
			row = lipgloss.NewStyle().Foreground(cyan).Render(row)
		} else {
			row = lipgloss.NewStyle().Foreground(magenta).Render(row)
		}
		lines = append(lines, row)
	}
	color := muted
	if m.focus == focusSessions {
		color = cyan
	}
	return panel(m.listWidth, m.listPanelH, color, strings.Join(lines, "\n"))
}

func (m Model) renderDetails() string {
	title := "Details"
	if m.skipped > 0 {
		title = fmt.Sprintf("Details · %d skipped", m.skipped)
	}
	content := title + "\n" + m.details.View()
	color := muted
	if m.focus == focusDetails {
		color = cyan
	}
	return panel(m.detailWidth, m.detailPanelH, color, content)
}

func (m Model) renderHelp() string {
	help := "SESSIONS  j/k move  gg/G ends  Ctrl+d/u page  Ctrl+w/l pane  s sort  / search  Enter resume"
	if m.mode == modeSearch {
		help = "type to filter  Enter resume  Tab keep filter  Esc clear"
	} else if m.focus == focusDetails {
		help = "DETAILS  j/k scroll  gg/G ends  Ctrl+d/u page  Ctrl+w/h pane  s sort  Enter resume  q quit"
	}
	return lipgloss.NewStyle().Foreground(muted).Render(fit(help, m.width))
}

func panel(width, height int, borderColor color.Color, content string) string {
	return lipgloss.NewStyle().
		Border(lipgloss.RoundedBorder()).
		BorderForeground(borderColor).
		Width(max(width, 3)).
		Height(max(height, 3)).
		Render(content)
}

func tableLine(agent, stamp, project, title string, width int) string {
	agentWidth := min(7, max(width/6, 1))
	timeWidth := min(10, max(width/5, 1))
	projectWidth := min(22, max(width/4, 1))
	titleWidth := max(width-agentWidth-timeWidth-projectWidth-3, 1)
	return strings.Join([]string{
		fitPad(agent, agentWidth), fitPad(stamp, timeWidth),
		fitPad(project, projectWidth), fitPad(title, titleWidth),
	}, " ")
}

func joinSides(left, right string, width int) string {
	right = fit(right, width)
	remaining := width - lipgloss.Width(right) - 1
	if remaining <= 0 {
		return fit(right, width)
	}
	left = fit(left, remaining)
	return left + strings.Repeat(" ", max(remaining-lipgloss.Width(left)+1, 1)) + right
}

func fitPad(value string, width int) string {
	value = fit(value, width)
	return value + strings.Repeat(" ", max(width-lipgloss.Width(value), 0))
}

func fit(value string, width int) string {
	if width <= 0 {
		return ""
	}
	if lipgloss.Width(value) <= width {
		return value
	}
	if width == 1 {
		return "…"
	}
	var builder strings.Builder
	for len(value) > 0 {
		r, size := utf8.DecodeRuneInString(value)
		candidate := builder.String() + string(r)
		if lipgloss.Width(candidate) > width-1 {
			break
		}
		builder.WriteRune(r)
		value = value[size:]
	}
	return builder.String() + "…"
}

func age(timestamp time.Time) string {
	duration := time.Since(timestamp)
	if duration < 0 || duration < time.Minute {
		return "now"
	}
	if duration < time.Hour {
		return fmt.Sprintf("%dm ago", int(duration.Minutes()))
	}
	if duration < 24*time.Hour {
		return fmt.Sprintf("%dh ago", int(duration.Hours()))
	}
	if duration < 30*24*time.Hour {
		return fmt.Sprintf("%dd ago", int(duration.Hours()/24))
	}
	return timestamp.UTC().Format("2006-01-02")
}

func titleCase(value string) string {
	if value == "" {
		return value
	}
	return strings.ToUpper(value[:1]) + value[1:]
}
