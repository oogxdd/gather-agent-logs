package session

import (
	"bufio"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"os"
	"path/filepath"
	"slices"
	"strings"
	"time"
	"unicode/utf8"
)

type Agent string

const (
	Codex  Agent = "codex"
	Claude Agent = "claude"
)

func (a Agent) Label() string {
	if a == Codex {
		return "Codex"
	}
	return "Claude"
}

func (a Agent) Executable() string { return string(a) }

type SortMode string

const (
	SortCreated SortMode = "created"
	SortUpdated SortMode = "updated"
)

func ParseSortMode(value string) (SortMode, error) {
	mode := SortMode(value)
	if mode != SortCreated && mode != SortUpdated {
		return "", fmt.Errorf("invalid sort mode %q (want created or updated)", value)
	}
	return mode, nil
}

func (s SortMode) Toggle() SortMode {
	if s == SortCreated {
		return SortUpdated
	}
	return SortCreated
}

func (s SortMode) Timestamp(item Session) time.Time {
	if s == SortCreated {
		return item.Created
	}
	return item.Updated
}

type Session struct {
	Agent   Agent
	ID      string
	Title   string
	CWD     string
	Path    string
	Created time.Time
	Updated time.Time
}

func (s Session) Project() string {
	cleaned := filepath.Clean(s.CWD)
	project := filepath.Base(cleaned)
	if project == "." || project == string(filepath.Separator) {
		return s.CWD
	}
	return project
}

func (s Session) SearchText() string {
	return strings.Join([]string{string(s.Agent), s.ID, s.Project(), s.CWD, s.Title}, " ")
}

type Options struct {
	CodexDir      string
	ClaudeDir     string
	IncludeCodex  bool
	IncludeClaude bool
}

type Result struct {
	Sessions []Session
	Skipped  int
}

func Discover(options Options) Result {
	var result Result
	if options.IncludeCodex {
		discoverAgent(options.CodexDir, Codex, &result)
	}
	if options.IncludeClaude {
		discoverAgent(options.ClaudeDir, Claude, &result)
	}
	return result
}

func discoverAgent(root string, agent Agent, result *Result) {
	if root == "" {
		return
	}
	info, err := os.Stat(root)
	if err != nil || !info.IsDir() {
		return
	}

	err = filepath.WalkDir(root, func(path string, entry os.DirEntry, walkErr error) error {
		if walkErr != nil {
			result.Skipped++
			return nil
		}
		if entry.IsDir() {
			if agent == Claude && entry.Name() == "subagents" && path != root {
				return filepath.SkipDir
			}
			return nil
		}
		if entry.Type()&os.ModeSymlink != 0 || filepath.Ext(path) != ".jsonl" {
			return nil
		}
		if agent == Claude && strings.HasPrefix(strings.TrimSuffix(entry.Name(), ".jsonl"), "agent-") {
			return nil
		}

		item, parseErr := parseFile(path, agent)
		if parseErr != nil {
			result.Skipped++
			return nil
		}
		result.Sessions = append(result.Sessions, item)
		return nil
	})
	if err != nil {
		result.Skipped++
	}
}

func Sort(items []Session, mode SortMode) {
	slices.SortStableFunc(items, func(left, right Session) int {
		if order := mode.Timestamp(right).Compare(mode.Timestamp(left)); order != 0 {
			return order
		}
		if order := right.Updated.Compare(left.Updated); order != 0 {
			return order
		}
		if order := strings.Compare(string(left.Agent), string(right.Agent)); order != 0 {
			return order
		}
		return strings.Compare(left.ID, right.ID)
	})
}

type record struct {
	Type        string          `json:"type"`
	Timestamp   string          `json:"timestamp"`
	SessionID   string          `json:"sessionId"`
	CWD         string          `json:"cwd"`
	CustomTitle string          `json:"customTitle"`
	Payload     json.RawMessage `json:"payload"`
	Message     struct {
		Role    string          `json:"role"`
		Content json.RawMessage `json:"content"`
	} `json:"message"`
}

type codexPayload struct {
	Type      string          `json:"type"`
	Role      string          `json:"role"`
	ID        string          `json:"id"`
	SessionID string          `json:"session_id"`
	CWD       string          `json:"cwd"`
	Timestamp string          `json:"timestamp"`
	Message   string          `json:"message"`
	Content   json.RawMessage `json:"content"`
}

func parseFile(path string, agent Agent) (Session, error) {
	file, err := os.Open(path)
	if err != nil {
		return Session{}, err
	}
	defer file.Close()

	info, err := file.Stat()
	if err != nil {
		return Session{}, err
	}
	item := Session{Agent: agent, Path: path, Created: info.ModTime(), Updated: info.ModTime()}
	var firstTimestamp, lastMessageTimestamp time.Time
	scanner := bufio.NewScanner(file)
	buffer := make([]byte, 64*1024)
	scanner.Buffer(buffer, 16*1024*1024)
	for scanner.Scan() {
		line := strings.TrimLeft(scanner.Text(), "\x00")
		if line == "" {
			continue
		}
		var value record
		if err := json.Unmarshal([]byte(line), &value); err != nil {
			continue
		}
		stamp := parseTimestamp(value.Timestamp)
		if firstTimestamp.IsZero() && !stamp.IsZero() {
			firstTimestamp = stamp
		}
		if agent == Codex {
			parseCodexRecord(value, stamp, &item, &firstTimestamp, &lastMessageTimestamp)
		} else {
			parseClaudeRecord(value, stamp, &item, &lastMessageTimestamp)
		}
	}
	if err := scanner.Err(); err != nil && !errors.Is(err, io.EOF) {
		return Session{}, err
	}
	if item.ID == "" {
		item.ID = idFromFilename(path)
	}
	if item.ID == "" {
		return Session{}, errors.New("session has no id")
	}
	if !firstTimestamp.IsZero() {
		item.Created = firstTimestamp
	}
	if !lastMessageTimestamp.IsZero() {
		item.Updated = lastMessageTimestamp
	} else {
		item.Updated = item.Created
	}
	if item.Title == "" {
		if item.Agent == Claude {
			item.Title = "Untitled Claude Code session"
		} else {
			item.Title = "Untitled Codex session"
		}
	}
	return item, nil
}

func parseCodexRecord(value record, stamp time.Time, item *Session, created, updated *time.Time) {
	var payload codexPayload
	if len(value.Payload) == 0 || json.Unmarshal(value.Payload, &payload) != nil {
		return
	}
	switch value.Type {
	case "session_meta":
		if payload.ID != "" {
			item.ID = payload.ID
		} else if payload.SessionID != "" {
			item.ID = payload.SessionID
		}
		item.CWD = payload.CWD
		if metaStamp := parseTimestamp(payload.Timestamp); !metaStamp.IsZero() {
			*created = metaStamp
		}
	case "response_item":
		if payload.Type != "message" || (payload.Role != "user" && payload.Role != "assistant") {
			return
		}
		if !stamp.IsZero() {
			*updated = stamp
		}
		if payload.Role == "user" && item.Title == "" {
			item.Title = contentTitle(payload.Content)
		}
	case "event_msg":
		if payload.Type != "user_message" && payload.Type != "agent_message" {
			return
		}
		if !stamp.IsZero() {
			*updated = stamp
		}
		if payload.Type == "user_message" && item.Title == "" {
			item.Title = cleanTitle(payload.Message)
		}
	}
}

func parseClaudeRecord(value record, stamp time.Time, item *Session, updated *time.Time) {
	if item.ID == "" {
		item.ID = value.SessionID
	}
	if item.CWD == "" {
		item.CWD = value.CWD
	}
	if value.Type == "custom-title" && item.Title == "" {
		item.Title = cleanTitle(value.CustomTitle)
	}
	if value.Type != "user" && value.Type != "assistant" {
		return
	}
	if !stamp.IsZero() {
		*updated = stamp
	}
	if value.Type == "user" && item.Title == "" {
		item.Title = contentTitle(value.Message.Content)
	}
}

func contentTitle(raw json.RawMessage) string {
	if len(raw) == 0 {
		return ""
	}
	var text string
	if json.Unmarshal(raw, &text) == nil {
		return cleanTitle(text)
	}
	var parts []struct {
		Type string `json:"type"`
		Text string `json:"text"`
	}
	if json.Unmarshal(raw, &parts) != nil {
		return ""
	}
	for _, part := range parts {
		if part.Type == "text" || part.Type == "input_text" {
			if title := cleanTitle(part.Text); title != "" {
				return title
			}
		}
	}
	return ""
}

func cleanTitle(value string) string {
	cleaned := strings.Join(strings.Fields(value), " ")
	for _, prefix := range []string{
		"# AGENTS.md instructions", "<environment_context>", "<permissions instructions>",
		"<system-reminder>", "<local-command-caveat>", "<local-command-stdout>",
		"<local-command-stderr>", "<command-name>", "<recommended_plugins>",
		"<skills_instructions>", "<apps_instructions>", "<plugins_instructions>",
		"<collaboration_mode>", "<multi_agent_mode>",
	} {
		if strings.HasPrefix(cleaned, prefix) {
			return ""
		}
	}
	if utf8.RuneCountInString(cleaned) <= 180 {
		return cleaned
	}
	runes := []rune(cleaned)
	return string(runes[:180]) + "…"
}

func parseTimestamp(value string) time.Time {
	stamp, err := time.Parse(time.RFC3339Nano, value)
	if err != nil {
		return time.Time{}
	}
	return stamp.UTC()
}

func idFromFilename(path string) string {
	stem := strings.TrimSuffix(filepath.Base(path), filepath.Ext(path))
	if len(stem) < 36 {
		return ""
	}
	candidate := stem[len(stem)-36:]
	for index, char := range candidate {
		if index == 8 || index == 13 || index == 18 || index == 23 {
			if char != '-' {
				return ""
			}
			continue
		}
		if !strings.ContainsRune("0123456789abcdefABCDEF", char) {
			return ""
		}
	}
	return candidate
}
