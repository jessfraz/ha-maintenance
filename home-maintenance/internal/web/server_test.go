package web

import (
	"context"
	"io"
	"log/slog"
	"net/http"
	"net/http/httptest"
	"path/filepath"
	"strings"
	"testing"
	"time"

	"github.com/jessfraz/home-maintenance/internal/catalog"
	"github.com/jessfraz/home-maintenance/internal/history"
	"github.com/jessfraz/home-maintenance/internal/homeassistant"
	"github.com/jessfraz/home-maintenance/internal/maintenance"
)

func TestRelativeDateUsesCalendarUnits(t *testing.T) {
	t.Parallel()
	location := time.FixedZone("PST", -8*60*60)
	tests := []struct {
		name   string
		target time.Time
		now    time.Time
		want   string
	}{
		{
			name:   "today",
			target: time.Date(2026, 9, 3, 0, 0, 0, 0, location),
			now:    time.Date(2026, 9, 3, 23, 59, 0, 0, location),
			want:   "today",
		},
		{
			name:   "future years months and days",
			target: time.Date(2028, 11, 8, 0, 0, 0, 0, location),
			now:    time.Date(2026, 9, 3, 0, 0, 0, 0, location),
			want:   "in 2 years, 2 months, 5 days",
		},
		{
			name:   "past month boundary",
			target: time.Date(2026, 1, 31, 0, 0, 0, 0, location),
			now:    time.Date(2026, 2, 28, 0, 0, 0, 0, location),
			want:   "1 month ago",
		},
	}
	for _, test := range tests {
		t.Run(test.name, func(t *testing.T) {
			if got := relativeDate(test.target, test.now); got != test.want {
				t.Fatalf("relativeDate() = %q, want %q", got, test.want)
			}
		})
	}
}

func TestProgressClampsOutsideInterval(t *testing.T) {
	t.Parallel()
	start := time.Date(2026, 1, 1, 0, 0, 0, 0, time.UTC)
	due := time.Date(2026, 1, 11, 0, 0, 0, 0, time.UTC)
	if got, ok := progress(start, due, start.AddDate(0, 0, 5)); !ok || got != 50 {
		t.Fatalf("progress() midpoint = %d, %v", got, ok)
	}
	if got, ok := progress(start, due, due.AddDate(0, 0, 2)); !ok || got != 100 {
		t.Fatalf("progress() overdue = %d, %v", got, ok)
	}
}

func TestCivilDatesIgnoreDaylightSavingHourChanges(t *testing.T) {
	t.Parallel()
	location, err := time.LoadLocation("America/New_York")
	if err != nil {
		t.Fatal(err)
	}
	start := time.Date(2027, 3, 13, 0, 0, 0, 0, location)
	middle := time.Date(2027, 3, 14, 0, 0, 0, 0, location)
	end := time.Date(2027, 3, 15, 0, 0, 0, 0, location)

	if got := civilDaysBetween(start, end); got != 2 {
		t.Fatalf("civilDaysBetween() = %d, want 2", got)
	}
	if got := relativeDate(end, start); got != "in 2 days" {
		t.Fatalf("relativeDate() = %q", got)
	}
	if got := dueClass(end, start, 1); got != "upcoming" {
		t.Fatalf("dueClass() = %q", got)
	}
	if got, ok := progress(start, end, middle); !ok || got != 50 {
		t.Fatalf("progress() = %d, %v", got, ok)
	}
}

func TestProductionIngressBoundaryAndIdentity(t *testing.T) {
	t.Parallel()
	server := testProductionServer(t)
	handler := server.Handler()

	health := httptest.NewRequest(http.MethodGet, "/health", nil)
	health.RemoteAddr = "127.0.0.1:1234"
	healthRecorder := httptest.NewRecorder()
	handler.ServeHTTP(healthRecorder, health)
	if healthRecorder.Code != http.StatusOK || healthRecorder.Body.String() != "ok\n" {
		t.Fatalf("health response = %d, %q", healthRecorder.Code, healthRecorder.Body.String())
	}

	outside := httptest.NewRequest(http.MethodGet, "/", nil)
	outside.RemoteAddr = "192.0.2.1:1234"
	outsideRecorder := httptest.NewRecorder()
	handler.ServeHTTP(outsideRecorder, outside)
	if outsideRecorder.Code != http.StatusForbidden {
		t.Fatalf("outside ingress status = %d", outsideRecorder.Code)
	}

	missingIdentity := httptest.NewRequest(http.MethodGet, "/", nil)
	missingIdentity.RemoteAddr = "172.30.32.2:1234"
	missingIdentityRecorder := httptest.NewRecorder()
	handler.ServeHTTP(missingIdentityRecorder, missingIdentity)
	if missingIdentityRecorder.Code != http.StatusUnauthorized {
		t.Fatalf("missing identity status = %d", missingIdentityRecorder.Code)
	}

	inside := httptest.NewRequest(http.MethodGet, "/", nil)
	inside.RemoteAddr = "172.30.32.2:1234"
	inside.Header.Set("X-Remote-User-Id", "user-1")
	inside.Header.Set("X-Remote-User-Display-Name", "Demo User")
	inside.Header.Set("X-Ingress-Path", "/api/hassio_ingress/session")
	insideRecorder := httptest.NewRecorder()
	handler.ServeHTTP(insideRecorder, inside)
	if insideRecorder.Code != http.StatusOK {
		t.Fatalf("inside ingress status = %d", insideRecorder.Code)
	}
	body := insideRecorder.Body.String()
	if !strings.Contains(body, "/api/hassio_ingress/session/items/example-filter") ||
		!strings.Contains(body, "/api/hassio_ingress/session/assets/style.css?v=") {
		t.Fatalf("ingress-prefixed body = %q", body)
	}
	if insideRecorder.Header().Get("Cache-Control") != "no-store" {
		t.Fatalf("dynamic Cache-Control = %q", insideRecorder.Header().Get("Cache-Control"))
	}
	if insideRecorder.Header().Get("Content-Security-Policy") == "" {
		t.Fatal("Content-Security-Policy is empty")
	}

	asset := httptest.NewRequest(http.MethodGet, "/assets/style.css?v=test", nil)
	asset.RemoteAddr = "172.30.32.2:1234"
	assetRecorder := httptest.NewRecorder()
	handler.ServeHTTP(assetRecorder, asset)
	if assetRecorder.Code != http.StatusOK {
		t.Fatalf("asset status = %d", assetRecorder.Code)
	}
	if got := assetRecorder.Header().Get("Cache-Control"); got != "public, max-age=31536000, immutable" {
		t.Fatalf("asset Cache-Control = %q", got)
	}
}

type emptyTodo struct{}

func (emptyTodo) GetTodoItems(context.Context, string) ([]homeassistant.TodoItem, error) {
	return nil, nil
}

func (emptyTodo) CompleteTodoItem(context.Context, string, string) error { return nil }

func (emptyTodo) UpdateTodoItem(context.Context, string, string, string, string, string) error {
	return nil
}

func (emptyTodo) AddTodoItem(context.Context, string, string, string, string) error { return nil }

func testProductionServer(t *testing.T) *Server {
	t.Helper()
	catalogValue, err := catalog.Decode(strings.NewReader(`{
      "title": "Example maintenance",
      "timezone": "America/New_York",
      "todo_entity": "todo.example_maintenance",
      "items": [{
        "id": "example-filter",
        "name": "Example air filter",
        "ha_summary": "Service example air filter",
        "schedule": {"days": 37},
        "initial_due": "2031-02-17"
      }]
    }`))
	if err != nil {
		t.Fatal(err)
	}
	store, err := history.Open(filepath.Join(t.TempDir(), "history.db"))
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() {
		if err := store.Close(); err != nil {
			t.Errorf("Close() history error = %v", err)
		}
	})
	service, err := maintenance.New(catalogValue, store, emptyTodo{})
	if err != nil {
		t.Fatal(err)
	}
	server, err := New(Config{
		Service:        service,
		CSRFSecretPath: filepath.Join(t.TempDir(), "csrf.key"),
		Logger:         slog.New(slog.NewTextHandler(io.Discard, nil)),
		IngressProxyIP: "172.30.32.2",
	})
	if err != nil {
		t.Fatal(err)
	}
	return server
}
