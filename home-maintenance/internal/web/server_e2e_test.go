package web

import (
	"context"
	"encoding/json"
	"io"
	"net/http"
	"net/http/httptest"
	"net/url"
	"path/filepath"
	"regexp"
	"strings"
	"sync"
	"testing"
	"time"

	"github.com/jessfraz/home-maintenance/internal/catalog"
	"github.com/jessfraz/home-maintenance/internal/history"
	"github.com/jessfraz/home-maintenance/internal/homeassistant"
	"github.com/jessfraz/home-maintenance/internal/maintenance"
)

func TestItemCompletionEndToEnd(t *testing.T) {
	t.Parallel()

	ha := newFakeHomeAssistant()
	haServer := httptest.NewServer(ha)
	t.Cleanup(haServer.Close)
	haClient, err := homeassistant.NewClient(haServer.URL+"/api", "test-token", nil)
	if err != nil {
		t.Fatalf("NewClient() error = %v", err)
	}
	catalogValue, err := catalog.Decode(strings.NewReader(`{
      "title": "Example maintenance",
      "timezone": "America/New_York",
      "todo_entity": "todo.example_maintenance",
      "items": [{
        "id": "example-filter",
        "name": "Example air filter",
        "ha_summary": "Service example air filter",
        "schedule": {"days": 37},
        "initial_due": "2031-02-17",
        "specs": [{"label": "Size", "value": "DEMO-SIZE"}]
      }]
    }`))
	if err != nil {
		t.Fatalf("Decode() error = %v", err)
	}
	store, err := history.Open(filepath.Join(t.TempDir(), "history.db"))
	if err != nil {
		t.Fatalf("Open() history error = %v", err)
	}
	t.Cleanup(func() {
		if err := store.Close(); err != nil {
			t.Errorf("Close() history error = %v", err)
		}
	})
	service, err := maintenance.New(catalogValue, store, haClient)
	if err != nil {
		t.Fatalf("New() maintenance error = %v", err)
	}
	if err := service.Refresh(context.Background()); err != nil {
		t.Fatalf("Refresh() error = %v", err)
	}
	server, err := New(Config{
		Service:        service,
		CSRFSecretPath: filepath.Join(t.TempDir(), "csrf.key"),
		Development:    true,
	})
	if err != nil {
		t.Fatalf("New() web error = %v", err)
	}
	webServer := httptest.NewServer(server.Handler())
	t.Cleanup(webServer.Close)

	homeResponse, err := http.Get(webServer.URL + "/")
	if err != nil {
		t.Fatalf("GET home error = %v", err)
	}
	homeBody := readBody(t, homeResponse)
	if !strings.Contains(homeBody, `<h1 class="next-name">Example air filter</h1>`) ||
		strings.Contains(homeBody, "What needs attention next") {
		t.Fatalf("home hierarchy body = %q", homeBody)
	}

	itemResponse, err := http.Get(webServer.URL + "/items/example-filter")
	if err != nil {
		t.Fatalf("GET item error = %v", err)
	}
	itemBody := readBody(t, itemResponse)
	if itemResponse.StatusCode != http.StatusOK || !strings.Contains(itemBody, "DEMO-SIZE") {
		t.Fatalf("GET item status = %d, body = %q", itemResponse.StatusCode, itemBody)
	}
	csrf := hiddenValue(t, itemBody, "csrf")
	uid := hiddenValue(t, itemBody, "uid")

	client := &http.Client{
		CheckRedirect: func(*http.Request, []*http.Request) error {
			return http.ErrUseLastResponse
		},
	}
	completionResponse, err := client.PostForm(
		webServer.URL+"/items/example-filter/complete",
		url.Values{"csrf": {csrf}, "uid": {uid}},
	)
	if err != nil {
		t.Fatalf("POST completion error = %v", err)
	}
	completionResponse.Body.Close()
	if completionResponse.StatusCode != http.StatusSeeOther {
		t.Fatalf("POST completion status = %d", completionResponse.StatusCode)
	}
	location := completionResponse.Header.Get("Location")
	if !strings.Contains(location, "?completed=") {
		t.Fatalf("POST completion location = %q", location)
	}

	resultResponse, err := http.Get(webServer.URL + location)
	if err != nil {
		t.Fatalf("GET completion result error = %v", err)
	}
	resultBody := readBody(t, resultResponse)
	if !strings.Contains(resultBody, "Completed by Demo User") || !strings.Contains(resultBody, "next due") {
		t.Fatalf("completion result body = %q", resultBody)
	}
	if strings.Contains(resultBody, "Mark maintenance done") {
		t.Fatalf("completion result immediately offers another completion: %q", resultBody)
	}
	latest, found, err := store.Latest(context.Background(), "example-filter")
	if err != nil {
		t.Fatalf("Latest() error = %v", err)
	}
	if !found || latest.UserName != "Demo User" || latest.Phase != history.PhaseScheduled {
		t.Fatalf("Latest() = %#v, found = %v", latest, found)
	}

	staleResponse, err := client.PostForm(
		webServer.URL+"/items/example-filter/complete",
		url.Values{"csrf": {csrf}, "uid": {uid}},
	)
	if err != nil {
		t.Fatalf("POST stale completion error = %v", err)
	}
	staleBody := readBody(t, staleResponse)
	if staleResponse.StatusCode != http.StatusConflict || staleBody != "Could not complete maintenance. Refresh and try again.\n" {
		t.Fatalf("POST stale completion status = %d, body = %q", staleResponse.StatusCode, staleBody)
	}
}

func readBody(t *testing.T, response *http.Response) string {
	t.Helper()
	defer response.Body.Close()
	body, err := io.ReadAll(response.Body)
	if err != nil {
		t.Fatalf("ReadAll() error = %v", err)
	}
	return string(body)
}

func hiddenValue(t *testing.T, body, name string) string {
	t.Helper()
	pattern := regexp.MustCompile(`name="` + regexp.QuoteMeta(name) + `" value="([^"]+)"`)
	match := pattern.FindStringSubmatch(body)
	if len(match) != 2 {
		t.Fatalf("hidden input %q not found", name)
	}
	return match[1]
}

type fakeHomeAssistant struct {
	mu      sync.Mutex
	items   []homeassistant.TodoItem
	counter int
}

func newFakeHomeAssistant() *fakeHomeAssistant {
	return &fakeHomeAssistant{
		items: []homeassistant.TodoItem{{
			Summary: "Service example air filter",
			UID:     "todo-1",
			Status:  "needs_action",
			Due:     "2031-02-17",
		}},
		counter: 1,
	}
}

func (ha *fakeHomeAssistant) ServeHTTP(response http.ResponseWriter, request *http.Request) {
	ha.mu.Lock()
	defer ha.mu.Unlock()
	if request.Header.Get("Authorization") != "Bearer test-token" {
		http.Error(response, "unauthorized", http.StatusUnauthorized)
		return
	}
	var payload map[string]json.RawMessage
	if err := json.NewDecoder(request.Body).Decode(&payload); err != nil {
		http.Error(response, err.Error(), http.StatusBadRequest)
		return
	}
	switch request.URL.Path {
	case "/api/services/todo/get_items":
		response.Header().Set("Content-Type", "application/json")
		_ = json.NewEncoder(response).Encode(map[string]any{
			"service_response": map[string]any{
				"todo.example_maintenance": map[string]any{"items": ha.items},
			},
		})
	case "/api/services/todo/update_item":
		ha.update(payload)
		response.WriteHeader(http.StatusOK)
	case "/api/services/todo/add_item":
		ha.add(payload)
		response.WriteHeader(http.StatusOK)
	default:
		http.NotFound(response, request)
	}
}

func (ha *fakeHomeAssistant) update(payload map[string]json.RawMessage) {
	uid := rawString(payload["item"])
	for index := range ha.items {
		if ha.items[index].UID != uid {
			continue
		}
		if status := rawString(payload["status"]); status != "" {
			ha.items[index].Status = status
			if status == "completed" {
				completed := time.Now().UTC()
				ha.items[index].Completed = &completed
			}
		}
		if summary := rawString(payload["rename"]); summary != "" {
			ha.items[index].Summary = summary
		}
		if due := rawString(payload["due_date"]); due != "" {
			ha.items[index].Due = due
		}
		if description := rawString(payload["description"]); description != "" {
			ha.items[index].Description = description
		}
	}
}

func (ha *fakeHomeAssistant) add(payload map[string]json.RawMessage) {
	ha.counter++
	ha.items = append(ha.items, homeassistant.TodoItem{
		Summary:     rawString(payload["item"]),
		UID:         "todo-" + time.Date(2000, 1, ha.counter, 0, 0, 0, 0, time.UTC).Format("02"),
		Status:      "needs_action",
		Due:         rawString(payload["due_date"]),
		Description: rawString(payload["description"]),
	})
}

func rawString(value json.RawMessage) string {
	var result string
	_ = json.Unmarshal(value, &result)
	return result
}
