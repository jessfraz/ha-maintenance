package maintenance

import (
	"context"
	"errors"
	"strings"
	"testing"
	"time"

	"github.com/jessfraz/home-maintenance/internal/catalog"
	"github.com/jessfraz/home-maintenance/internal/history"
	"github.com/jessfraz/home-maintenance/internal/homeassistant"
)

type memoryHistory struct {
	events []history.Completion
}

func (store *memoryHistory) Begin(_ context.Context, value history.Completion) (history.Completion, bool, error) {
	for _, event := range store.events {
		if event.ItemID == value.ItemID && event.SourceUID == value.SourceUID {
			return event, false, nil
		}
	}
	value.ID = int64(len(store.events) + 1)
	value.Phase = history.PhasePending
	store.events = append(store.events, value)
	return value, true, nil
}

func (store *memoryHistory) MarkCompleted(_ context.Context, id int64) error {
	store.events[id-1].Phase = history.PhaseCompleted
	return nil
}

func (store *memoryHistory) MarkScheduled(_ context.Context, id int64, uid string) error {
	store.events[id-1].Phase = history.PhaseScheduled
	store.events[id-1].NextUID = uid
	return nil
}

func (store *memoryHistory) Latest(_ context.Context, itemID string) (history.Completion, bool, error) {
	for index := len(store.events) - 1; index >= 0; index-- {
		event := store.events[index]
		if event.ItemID == itemID && (event.Phase == history.PhaseCompleted || event.Phase == history.PhaseScheduled) {
			return event, true, nil
		}
	}
	return history.Completion{}, false, nil
}

func (store *memoryHistory) ByID(_ context.Context, id int64) (history.Completion, error) {
	return store.events[id-1], nil
}

func (store *memoryHistory) Unfinished(_ context.Context) ([]history.Completion, error) {
	var values []history.Completion
	for _, event := range store.events {
		if event.Phase == history.PhasePending || event.Phase == history.PhaseCompleted {
			values = append(values, event)
		}
	}
	return values, nil
}

type memoryTodo struct {
	items          []homeassistant.TodoItem
	completedCalls int
	nextUID        int
	getErr         error
}

func (todo *memoryTodo) GetTodoItems(context.Context, string) ([]homeassistant.TodoItem, error) {
	if todo.getErr != nil {
		return nil, todo.getErr
	}
	values := make([]homeassistant.TodoItem, len(todo.items))
	copy(values, todo.items)
	return values, nil
}

func (todo *memoryTodo) CompleteTodoItem(_ context.Context, _ string, uid string) error {
	for index := range todo.items {
		if todo.items[index].UID == uid {
			todo.items[index].Status = "completed"
			todo.completedCalls++
		}
	}
	return nil
}

func (todo *memoryTodo) UpdateTodoItem(_ context.Context, _ string, uid, summary, due, description string) error {
	for index := range todo.items {
		if todo.items[index].UID == uid {
			todo.items[index].Summary = summary
			todo.items[index].Due = due
			todo.items[index].Description = description
		}
	}
	return nil
}

func (todo *memoryTodo) AddTodoItem(_ context.Context, _ string, summary, due, description string) error {
	todo.nextUID++
	todo.items = append(todo.items, homeassistant.TodoItem{
		Summary:     summary,
		UID:         "new-" + time.Date(2000, 1, todo.nextUID, 0, 0, 0, 0, time.UTC).Format("02"),
		Status:      "needs_action",
		Due:         due,
		Description: description,
	})
	return nil
}

func testCatalog(t *testing.T) *catalog.Catalog {
	t.Helper()
	value, err := catalog.Decode(strings.NewReader(`{
      "title": "Example maintenance",
      "timezone": "America/New_York",
      "todo_entity": "todo.example_maintenance",
      "items": [{
        "id": "example-filter",
        "name": "Example air filter",
        "ha_summary": "Service example air filter",
        "ha_description": "DEMO-SIZE",
        "schedule": {"days": 37},
        "initial_due": "2031-02-17"
      }]
    }`))
	if err != nil {
		t.Fatal(err)
	}
	return value
}

func testCatalogWithSecondItem(t *testing.T) *catalog.Catalog {
	t.Helper()
	value, err := catalog.Decode(strings.NewReader(`{
      "title": "Example maintenance",
      "timezone": "America/New_York",
      "todo_entity": "todo.example_maintenance",
      "items": [
        {
          "id": "example-filter",
          "name": "Example air filter",
          "ha_summary": "Service example air filter",
          "schedule": {"days": 37},
          "initial_due": "2031-02-17"
        },
        {
          "id": "second-example",
          "name": "Second example item",
          "ha_summary": "Service second example item",
          "schedule": {"days": 23},
          "initial_due": "2026-09-03"
        }
      ]
    }`))
	if err != nil {
		t.Fatal(err)
	}
	return value
}

func TestRefreshFailureKeepsCatalogReadable(t *testing.T) {
	todo := &memoryTodo{getErr: errors.New("Home Assistant offline")}
	service, err := New(testCatalog(t), &memoryHistory{}, todo)
	if err != nil {
		t.Fatal(err)
	}

	before := service.Snapshot()
	if len(before.Items) != 1 || before.Items[0].Item.ID != "example-filter" {
		t.Fatalf("initial snapshot = %#v", before)
	}
	if err := service.Refresh(context.Background()); err == nil {
		t.Fatal("Refresh() error = nil, want Home Assistant failure")
	}
	after := service.Snapshot()
	if len(after.Items) != 1 || after.Items[0].Active != nil || after.SyncError == "" {
		t.Fatalf("degraded snapshot = %#v", after)
	}
}

func TestCompleteHonorsCancellationWhileAnotherOperationRuns(t *testing.T) {
	service, err := New(testCatalog(t), &memoryHistory{}, &memoryTodo{})
	if err != nil {
		t.Fatal(err)
	}
	service.operation <- struct{}{}
	defer service.endOperation()

	ctx, cancel := context.WithCancel(context.Background())
	cancel()
	_, err = service.Complete(ctx, "example-filter", "todo-1", Actor{ID: "user-1", Name: "Demo User"})
	if !errors.Is(err, context.Canceled) {
		t.Fatalf("Complete() error = %v, want context cancellation", err)
	}
}

func TestCompleteRejectsCanceledContextWhenOperationIsAvailable(t *testing.T) {
	service, err := New(testCatalog(t), &memoryHistory{}, &memoryTodo{})
	if err != nil {
		t.Fatal(err)
	}
	ctx, cancel := context.WithCancel(context.Background())
	cancel()

	_, err = service.Complete(ctx, "example-filter", "todo-1", Actor{ID: "user-1", Name: "Demo User"})
	if !errors.Is(err, context.Canceled) {
		t.Fatalf("Complete() error = %v, want context cancellation", err)
	}
	if len(service.operation) != 0 {
		t.Fatal("Complete() retained the operation semaphore after cancellation")
	}
}

func TestServiceOwnsCatalogAndSnapshotData(t *testing.T) {
	catalogValue := testCatalog(t)
	catalogValue.Items[0].Instructions = []string{"Example step"}
	todo := &memoryTodo{items: []homeassistant.TodoItem{{
		Summary: "Service example air filter",
		UID:     "todo-1",
		Status:  "needs_action",
		Due:     "2031-02-17",
	}}}
	service, err := New(catalogValue, &memoryHistory{}, todo)
	if err != nil {
		t.Fatal(err)
	}

	catalogValue.Title = "mutated outside service"
	catalogValue.Items[0].Name = "mutated outside service"
	catalogValue.Items[0].Instructions[0] = "mutated outside service"
	if err := service.Refresh(context.Background()); err != nil {
		t.Fatal(err)
	}

	snapshot := service.Snapshot()
	if snapshot.Items[0].Active == nil {
		t.Fatalf("Snapshot() = %#v", snapshot)
	}
	snapshot.Items[0].Item.Name = "mutated snapshot"
	snapshot.Items[0].Item.Instructions[0] = "mutated snapshot"
	snapshot.Items[0].Active.Summary = "mutated snapshot"
	catalogCopy := service.Catalog()
	catalogCopy.Title = "mutated catalog copy"
	catalogCopy.Items[0].Name = "mutated catalog copy"
	catalogCopy.Items[0].Instructions[0] = "mutated catalog copy"

	unchangedSnapshot := service.Snapshot()
	unchangedCatalog := service.Catalog()
	if unchangedSnapshot.Items[0].Item.Name != "Example air filter" ||
		unchangedSnapshot.Items[0].Item.Instructions[0] != "Example step" ||
		unchangedSnapshot.Items[0].Active.Summary != "Service example air filter" {
		t.Fatalf("internal snapshot was mutated: %#v", unchangedSnapshot.Items[0])
	}
	if unchangedCatalog.Title != "Example maintenance" ||
		unchangedCatalog.Items[0].Name != "Example air filter" ||
		unchangedCatalog.Items[0].Instructions[0] != "Example step" {
		t.Fatalf("internal catalog was mutated: %#v", unchangedCatalog)
	}
}

func TestRefreshDoesNotInferCompletionWhenPendingSourceIsMissing(t *testing.T) {
	store := &memoryHistory{}
	event, _, err := store.Begin(context.Background(), history.Completion{
		ItemID:      "example-filter",
		SourceUID:   "disappeared",
		PreviousDue: "2031-02-17",
		CompletedAt: time.Date(2026, 9, 3, 12, 0, 0, 0, time.UTC),
		UserID:      "user-1",
		UserName:    "Demo User",
		NextDue:     "2026-10-10",
	})
	if err != nil {
		t.Fatal(err)
	}
	service, err := New(testCatalog(t), store, &memoryTodo{})
	if err != nil {
		t.Fatal(err)
	}

	err = service.Refresh(context.Background())
	if err == nil || !strings.Contains(err.Error(), "refusing to infer completion") {
		t.Fatalf("Refresh() error = %v", err)
	}
	if got := store.events[event.ID-1].Phase; got != history.PhasePending {
		t.Fatalf("pending event phase = %q", got)
	}
	if got := service.Snapshot(); len(got.Items) != 1 || got.Items[0].Active != nil {
		t.Fatalf("snapshot = %#v", got)
	}
}

func TestRefreshIsolatesUnknownUnfinishedEvent(t *testing.T) {
	store := &memoryHistory{events: []history.Completion{{
		ID:          1,
		ItemID:      "removed-item",
		SourceUID:   "orphan",
		CompletedAt: time.Date(2026, 9, 3, 12, 0, 0, 0, time.UTC),
		NextDue:     "2026-10-10",
		Phase:       history.PhasePending,
	}}}
	todo := &memoryTodo{items: []homeassistant.TodoItem{{
		Summary: "Service second example item",
		UID:     "second-active",
		Status:  "needs_action",
		Due:     "2026-09-03",
	}}}
	service, err := New(testCatalogWithSecondItem(t), store, todo)
	if err != nil {
		t.Fatal(err)
	}

	err = service.Refresh(context.Background())
	if err == nil || !strings.Contains(err.Error(), "references unknown item") {
		t.Fatalf("Refresh() error = %v", err)
	}
	if !strings.Contains(todo.items[0].Description, "item-id=second-example") {
		t.Fatalf("unblocked todo description = %q", todo.items[0].Description)
	}
	snapshot := service.Snapshot()
	if len(snapshot.Items) != 2 {
		t.Fatalf("snapshot items = %d", len(snapshot.Items))
	}
	var secondActive bool
	for _, state := range snapshot.Items {
		if state.Item.ID == "second-example" && state.Active != nil && state.Active.UID == "second-active" {
			secondActive = true
		}
	}
	if !secondActive {
		t.Fatalf("second example did not remain usable: %#v", snapshot)
	}
}

func TestRefreshAdoptsAndCanonicalizesExistingTask(t *testing.T) {
	todo := &memoryTodo{items: []homeassistant.TodoItem{{
		Summary: "Service example air filter",
		UID:     "existing",
		Status:  "needs_action",
		Due:     "2027-01-01",
	}}}
	service, err := New(testCatalog(t), &memoryHistory{}, todo)
	if err != nil {
		t.Fatal(err)
	}
	service.now = func() time.Time { return time.Date(2026, 9, 3, 12, 0, 0, 0, time.UTC) }

	if err := service.Refresh(context.Background()); err != nil {
		t.Fatal(err)
	}
	if todo.items[0].Due != "2031-02-17" {
		t.Fatalf("due = %q", todo.items[0].Due)
	}
	if !strings.Contains(todo.items[0].Description, "item-id=example-filter") {
		t.Fatalf("description = %q", todo.items[0].Description)
	}
	if got := service.Snapshot(); len(got.Items) != 1 || got.Items[0].Active.UID != "existing" {
		t.Fatalf("snapshot = %#v", got)
	}
}

func TestCompleteIsOneActionAndIdempotent(t *testing.T) {
	todo := &memoryTodo{items: []homeassistant.TodoItem{{
		Summary:     "Service example air filter",
		UID:         "existing",
		Status:      "needs_action",
		Due:         "2031-02-17",
		Description: "DEMO-SIZE\n\n<!-- home-maintenance:item-id=example-filter -->",
	}}}
	store := &memoryHistory{}
	service, err := New(testCatalog(t), store, todo)
	if err != nil {
		t.Fatal(err)
	}
	service.now = func() time.Time {
		return time.Date(2026, 11, 1, 15, 30, 0, 0, time.FixedZone("PST", -8*60*60))
	}
	actor := Actor{ID: "user-1", Name: "Demo User"}

	first, err := service.Complete(context.Background(), "example-filter", "existing", actor)
	if err != nil {
		t.Fatal(err)
	}
	if !first.Created || first.Event.NextDue != "2026-12-08" || first.Event.UserName != "Demo User" {
		t.Fatalf("first completion = %#v", first)
	}
	if todo.completedCalls != 1 {
		t.Fatalf("completed calls = %d", todo.completedCalls)
	}

	second, err := service.Complete(context.Background(), "example-filter", "existing", actor)
	if err == nil || second.Created {
		t.Fatalf("second completion = %#v, err = %v", second, err)
	}
	if todo.completedCalls != 1 || len(store.events) != 1 {
		t.Fatalf("completed calls = %d, events = %d", todo.completedCalls, len(store.events))
	}
}

func TestRefreshRejectsDuplicateManagedTasks(t *testing.T) {
	description := "<!-- home-maintenance:item-id=example-filter -->"
	todo := &memoryTodo{items: []homeassistant.TodoItem{
		{UID: "one", Status: "needs_action", Description: description},
		{UID: "two", Status: "needs_action", Description: description},
	}}
	service, err := New(testCatalog(t), &memoryHistory{}, todo)
	if err != nil {
		t.Fatal(err)
	}
	if err := service.Refresh(context.Background()); err == nil || !strings.Contains(err.Error(), "multiple active") {
		t.Fatalf("error = %v", err)
	}
}

func TestRefreshRejectsMarkedAndLegacyDuplicate(t *testing.T) {
	todo := &memoryTodo{items: []homeassistant.TodoItem{
		{
			Summary:     "Service example air filter",
			UID:         "marked",
			Status:      "needs_action",
			Description: "<!-- home-maintenance:item-id=example-filter -->",
		},
		{
			Summary: "Service example air filter",
			UID:     "legacy",
			Status:  "needs_action",
		},
	}}
	service, err := New(testCatalog(t), &memoryHistory{}, todo)
	if err != nil {
		t.Fatal(err)
	}
	if err := service.Refresh(context.Background()); err == nil || !strings.Contains(err.Error(), "multiple active") {
		t.Fatalf("error = %v", err)
	}
}

func TestRefreshSchedulesRetainedHomeAssistantCompletion(t *testing.T) {
	completedAt := time.Date(2026, 10, 1, 17, 0, 0, 0, time.UTC)
	todo := &memoryTodo{items: []homeassistant.TodoItem{{
		Summary:     "Service example air filter",
		UID:         "completed-in-ha",
		Status:      "completed",
		Due:         "2031-02-17",
		Description: "<!-- home-maintenance:item-id=example-filter -->",
		Completed:   &completedAt,
	}}}
	store := &memoryHistory{}
	service, err := New(testCatalog(t), store, todo)
	if err != nil {
		t.Fatal(err)
	}

	if err := service.Refresh(context.Background()); err != nil {
		t.Fatal(err)
	}
	if len(store.events) != 1 {
		t.Fatalf("events = %d", len(store.events))
	}
	event := store.events[0]
	if event.UserName != "Home Assistant" || event.Phase != history.PhaseScheduled || event.NextDue != "2026-11-07" {
		t.Fatalf("event = %#v", event)
	}
	snapshot := service.Snapshot()
	if len(snapshot.Items) != 1 || snapshot.Items[0].Active == nil || snapshot.Items[0].Active.Due != "2026-11-07" {
		t.Fatalf("snapshot = %#v", snapshot)
	}
}

func TestRefreshRecalculatesDueDateWhenCatalogCadenceChanges(t *testing.T) {
	completedAt := time.Date(2026, 10, 1, 17, 0, 0, 0, time.UTC)
	store := &memoryHistory{events: []history.Completion{{
		ID:          1,
		ItemID:      "example-filter",
		SourceUID:   "previous",
		CompletedAt: completedAt,
		UserID:      "user-1",
		UserName:    "Demo User",
		NextDue:     "2030-01-01",
		NextUID:     "current",
		Phase:       history.PhaseScheduled,
	}}}
	todo := &memoryTodo{items: []homeassistant.TodoItem{{
		Summary:     "Service example air filter",
		UID:         "current",
		Status:      "needs_action",
		Due:         "2030-01-01",
		Description: "<!-- home-maintenance:item-id=example-filter -->",
	}}}
	service, err := New(testCatalog(t), store, todo)
	if err != nil {
		t.Fatal(err)
	}

	if err := service.Refresh(context.Background()); err != nil {
		t.Fatal(err)
	}
	if todo.items[0].Due != "2026-11-07" {
		t.Fatalf("due = %q", todo.items[0].Due)
	}
}

func TestRefreshIgnoresOlderHomeAssistantCompletion(t *testing.T) {
	latestAt := time.Date(2026, 9, 3, 17, 0, 0, 0, time.UTC)
	olderAt := time.Date(2026, 9, 1, 17, 0, 0, 0, time.UTC)
	store := &memoryHistory{events: []history.Completion{{
		ID:          1,
		ItemID:      "example-filter",
		SourceUID:   "newer-completed",
		CompletedAt: latestAt,
		UserID:      "user-1",
		UserName:    "Demo User",
		NextDue:     "2026-10-10",
		NextUID:     "current",
		Phase:       history.PhaseScheduled,
	}}}
	description := "<!-- home-maintenance:item-id=example-filter -->"
	todo := &memoryTodo{items: []homeassistant.TodoItem{
		{
			Summary:     "Service example air filter",
			UID:         "older-completed",
			Status:      "completed",
			Due:         "2031-02-17",
			Description: description,
			Completed:   &olderAt,
		},
		{
			Summary:     "Service example air filter",
			UID:         "current",
			Status:      "needs_action",
			Due:         "2026-10-10",
			Description: description,
		},
	}}
	service, err := New(testCatalog(t), store, todo)
	if err != nil {
		t.Fatal(err)
	}

	if err := service.Refresh(context.Background()); err != nil {
		t.Fatal(err)
	}
	if len(store.events) != 1 {
		t.Fatalf("history events = %d, want 1", len(store.events))
	}
	latest, found, err := store.Latest(context.Background(), "example-filter")
	if err != nil {
		t.Fatal(err)
	}
	if !found || !latest.CompletedAt.Equal(latestAt) || latest.NextUID != "current" {
		t.Fatalf("latest = %#v, found = %v", latest, found)
	}
	if todo.items[1].Due != "2026-10-10" {
		t.Fatalf("current due = %q", todo.items[1].Due)
	}
}
