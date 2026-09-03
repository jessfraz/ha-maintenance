package maintenance

import (
	"context"
	"errors"
	"fmt"
	"regexp"
	"slices"
	"sort"
	"strings"
	"sync"
	"time"

	"github.com/jessfraz/home-maintenance/internal/catalog"
	"github.com/jessfraz/home-maintenance/internal/history"
	"github.com/jessfraz/home-maintenance/internal/homeassistant"
)

const markerFormat = "<!-- home-maintenance:item-id=%s -->"

var markerPattern = regexp.MustCompile(`<!-- home-maintenance:item-id=([a-z0-9]+(?:-[a-z0-9]+)*) -->`)

type TodoClient interface {
	GetTodoItems(context.Context, string) ([]homeassistant.TodoItem, error)
	CompleteTodoItem(context.Context, string, string) error
	UpdateTodoItem(context.Context, string, string, string, string, string) error
	AddTodoItem(context.Context, string, string, string, string) error
}

type HistoryStore interface {
	Begin(context.Context, history.Completion) (history.Completion, bool, error)
	MarkCompleted(context.Context, int64) error
	MarkScheduled(context.Context, int64, string) error
	Latest(context.Context, string) (history.Completion, bool, error)
	ByID(context.Context, int64) (history.Completion, error)
	Unfinished(context.Context) ([]history.Completion, error)
}

type Actor struct {
	ID   string
	Name string
}

type ItemState struct {
	Item   catalog.Item
	Active *homeassistant.TodoItem
	Latest *history.Completion
}

type Snapshot struct {
	Items       []ItemState
	RefreshedAt time.Time
	SyncError   string
}

type CompletionResult struct {
	Event   history.Completion
	Created bool
}

type Service struct {
	catalog *catalog.Catalog
	history HistoryStore
	todo    TodoClient
	now     func() time.Time

	operation chan struct{}
	stateMu   sync.RWMutex
	state     Snapshot
}

func New(catalogValue *catalog.Catalog, store HistoryStore, todo TodoClient) (*Service, error) {
	if catalogValue == nil {
		return nil, errors.New("maintenance catalog is required")
	}
	if store == nil {
		return nil, errors.New("maintenance history store is required")
	}
	if todo == nil {
		return nil, errors.New("Home Assistant todo client is required")
	}
	ownedCatalog := cloneCatalog(catalogValue)
	states := make([]ItemState, 0, len(ownedCatalog.Items))
	for _, item := range ownedCatalog.Items {
		states = append(states, ItemState{Item: item})
	}
	sort.SliceStable(states, func(left, right int) bool {
		return stateLess(states[left], states[right], ownedCatalog.Location)
	})
	return &Service{
		catalog:   &ownedCatalog,
		history:   store,
		todo:      todo,
		now:       time.Now,
		operation: make(chan struct{}, 1),
		state:     Snapshot{Items: states},
	}, nil
}

func (service *Service) Catalog() catalog.Catalog {
	return cloneCatalog(service.catalog)
}

func (service *Service) Snapshot() Snapshot {
	service.stateMu.RLock()
	defer service.stateMu.RUnlock()

	items := make([]ItemState, len(service.state.Items))
	for index, state := range service.state.Items {
		items[index].Item = cloneItem(state.Item)
		if state.Active != nil {
			active := *state.Active
			if active.Completed != nil {
				completed := *active.Completed
				active.Completed = &completed
			}
			items[index].Active = &active
		}
		if state.Latest != nil {
			latest := *state.Latest
			items[index].Latest = &latest
		}
	}
	return Snapshot{
		Items:       items,
		RefreshedAt: service.state.RefreshedAt,
		SyncError:   service.state.SyncError,
	}
}

func (service *Service) Completion(ctx context.Context, id int64, itemID string) (history.Completion, bool, error) {
	completion, err := service.history.ByID(ctx, id)
	if err != nil {
		return history.Completion{}, false, err
	}
	if completion.ItemID != itemID || completion.Phase != history.PhaseScheduled {
		return history.Completion{}, false, nil
	}
	return completion, true, nil
}

func (service *Service) Refresh(ctx context.Context) error {
	if err := service.beginOperation(ctx); err != nil {
		return fmt.Errorf("refresh maintenance: %w", err)
	}
	defer service.endOperation()

	blockedItems, syncErrors := service.resumeUnfinished(ctx)
	items, err := service.todo.GetTodoItems(ctx, service.catalog.TodoEntity)
	if err != nil {
		syncErrors = append(syncErrors, err)
	} else {
		if err := service.reconcile(ctx, items, blockedItems); err != nil {
			syncErrors = append(syncErrors, err)
		}
		items, err = service.todo.GetTodoItems(ctx, service.catalog.TodoEntity)
		if err != nil {
			syncErrors = append(syncErrors, err)
		} else if err := service.publish(ctx, items); err != nil {
			syncErrors = append(syncErrors, err)
		}
	}
	if err := errors.Join(syncErrors...); err != nil {
		service.recordError(err)
		return err
	}
	return nil
}

func (service *Service) Run(ctx context.Context, interval time.Duration) {
	if interval <= 0 {
		interval = 15 * time.Second
	}
	ticker := time.NewTicker(interval)
	defer ticker.Stop()
	for {
		select {
		case <-ctx.Done():
			return
		case <-ticker.C:
			_ = service.Refresh(ctx)
		}
	}
}

func (service *Service) Complete(ctx context.Context, itemID, expectedUID string, actor Actor) (CompletionResult, error) {
	if err := service.beginOperation(ctx); err != nil {
		return CompletionResult{}, fmt.Errorf("complete maintenance: %w", err)
	}
	defer service.endOperation()

	item, exists := service.catalog.Item(itemID)
	if !exists {
		return CompletionResult{}, fmt.Errorf("unknown maintenance item %q", itemID)
	}
	expectedUID = strings.TrimSpace(expectedUID)
	if expectedUID == "" {
		return CompletionResult{}, errors.New("the maintenance task changed; refresh and try again")
	}
	actor.ID = strings.TrimSpace(actor.ID)
	actor.Name = strings.TrimSpace(actor.Name)
	if actor.ID == "" || actor.Name == "" {
		return CompletionResult{}, errors.New("authenticated Home Assistant user is required")
	}

	items, err := service.todo.GetTodoItems(ctx, service.catalog.TodoEntity)
	if err != nil {
		return CompletionResult{}, err
	}
	active, err := findActive(item, items)
	if err != nil {
		return CompletionResult{}, err
	}
	if active == nil || active.UID != expectedUID {
		return CompletionResult{}, errors.New("the maintenance task changed; refresh and try again")
	}

	now := service.now().In(service.catalog.Location)
	nextDue := item.Schedule.Next(now).Format(time.DateOnly)
	event, created, err := service.history.Begin(ctx, history.Completion{
		ItemID:      item.ID,
		SourceUID:   active.UID,
		PreviousDue: active.Due,
		CompletedAt: now,
		UserID:      actor.ID,
		UserName:    actor.Name,
		NextDue:     nextDue,
	})
	if err != nil {
		return CompletionResult{}, err
	}
	if err := service.finishCompletion(ctx, item, event); err != nil {
		return CompletionResult{}, err
	}
	completedEvent, exists, err := service.history.Latest(ctx, item.ID)
	if err != nil {
		return CompletionResult{}, err
	}
	if !exists || completedEvent.ID == 0 {
		return CompletionResult{}, errors.New("completed maintenance event was not persisted")
	}

	items, err = service.todo.GetTodoItems(ctx, service.catalog.TodoEntity)
	if err == nil {
		_ = service.publish(ctx, items)
	}
	return CompletionResult{Event: completedEvent, Created: created}, nil
}

func (service *Service) resumeUnfinished(ctx context.Context) (map[string]struct{}, []error) {
	events, err := service.history.Unfinished(ctx)
	if err != nil {
		return nil, []error{err}
	}
	blockedItems := make(map[string]struct{})
	var resumeErrors []error
	for _, event := range events {
		item, exists := service.catalog.Item(event.ItemID)
		if !exists {
			blockedItems[event.ItemID] = struct{}{}
			resumeErrors = append(resumeErrors, fmt.Errorf("unfinished event %d references unknown item %q", event.ID, event.ItemID))
			continue
		}
		if err := service.finishCompletion(ctx, item, event); err != nil {
			blockedItems[event.ItemID] = struct{}{}
			resumeErrors = append(resumeErrors, fmt.Errorf("resume completion %d: %w", event.ID, err))
		}
	}
	return blockedItems, resumeErrors
}

func (service *Service) finishCompletion(ctx context.Context, item catalog.Item, event history.Completion) error {
	if event.Phase == history.PhasePending {
		items, err := service.todo.GetTodoItems(ctx, service.catalog.TodoEntity)
		if err != nil {
			return err
		}
		sourceActive := todoByUID(items, event.SourceUID, "needs_action")
		sourceCompleted := todoByUID(items, event.SourceUID, "completed")
		switch {
		case sourceActive != nil:
			if err := service.todo.CompleteTodoItem(ctx, service.catalog.TodoEntity, event.SourceUID); err != nil {
				return err
			}
		case sourceCompleted != nil:
			// The prior completion call succeeded before the process stopped.
		default:
			return fmt.Errorf("source todo %q is missing; refusing to infer completion", event.SourceUID)
		}
		if err := service.history.MarkCompleted(ctx, event.ID); err != nil {
			return err
		}
		event.Phase = history.PhaseCompleted
	}
	if event.Phase != history.PhaseCompleted {
		return nil
	}

	items, err := service.todo.GetTodoItems(ctx, service.catalog.TodoEntity)
	if err != nil {
		return err
	}
	active, err := findActive(item, items)
	if err != nil {
		return err
	}
	if active == nil {
		if err := service.todo.AddTodoItem(
			ctx,
			service.catalog.TodoEntity,
			item.HASummary,
			event.NextDue,
			description(item),
		); err != nil {
			return err
		}
		items, err = service.todo.GetTodoItems(ctx, service.catalog.TodoEntity)
		if err != nil {
			return err
		}
		active, err = findActive(item, items)
		if err != nil {
			return err
		}
	}
	if active == nil {
		return fmt.Errorf("Home Assistant did not return the next task for %q after creating it", item.Name)
	}
	if err := service.ensureCanonical(ctx, item, active, event.NextDue); err != nil {
		return err
	}
	return service.history.MarkScheduled(ctx, event.ID, active.UID)
}

func (service *Service) reconcile(ctx context.Context, todoItems []homeassistant.TodoItem, blockedItems map[string]struct{}) error {
	var reconcileErrors []error
	for _, item := range service.catalog.Items {
		if _, blocked := blockedItems[item.ID]; blocked {
			continue
		}
		active, err := findActive(item, todoItems)
		if err != nil {
			reconcileErrors = append(reconcileErrors, err)
			continue
		}
		latest, exists, err := service.history.Latest(ctx, item.ID)
		if err != nil {
			reconcileErrors = append(reconcileErrors, err)
			continue
		}
		due := item.InitialDue
		if exists {
			due = item.Schedule.Next(latest.CompletedAt.In(service.catalog.Location)).Format(time.DateOnly)
		}
		completed := findLatestCompleted(item, todoItems)
		if completed != nil && completed.Completed != nil && (!exists || completed.Completed.After(latest.CompletedAt)) {
			completedAt := completed.Completed.In(service.catalog.Location)
			event, _, beginErr := service.history.Begin(ctx, history.Completion{
				ItemID:      item.ID,
				SourceUID:   completed.UID,
				PreviousDue: completed.Due,
				CompletedAt: completedAt,
				UserID:      "home-assistant",
				UserName:    "Home Assistant",
				NextDue:     item.Schedule.Next(completedAt).Format(time.DateOnly),
			})
			if beginErr != nil {
				reconcileErrors = append(reconcileErrors, beginErr)
				continue
			}
			if err := service.finishCompletion(ctx, item, event); err != nil {
				reconcileErrors = append(reconcileErrors, err)
				continue
			}
			continue
		}
		if active == nil {
			if due == "" {
				continue
			}
			if err := service.todo.AddTodoItem(
				ctx,
				service.catalog.TodoEntity,
				item.HASummary,
				due,
				description(item),
			); err != nil {
				reconcileErrors = append(reconcileErrors, fmt.Errorf("create task for %q: %w", item.Name, err))
			}
			continue
		}
		if due == "" {
			due = active.Due
		}
		if err := service.ensureCanonical(ctx, item, active, due); err != nil {
			reconcileErrors = append(reconcileErrors, err)
		}
	}
	return errors.Join(reconcileErrors...)
}

func (service *Service) ensureCanonical(ctx context.Context, item catalog.Item, active *homeassistant.TodoItem, due string) error {
	desiredDescription := description(item)
	if active.Summary == item.HASummary && active.Due == due && active.Description == desiredDescription {
		return nil
	}
	if err := service.todo.UpdateTodoItem(
		ctx,
		service.catalog.TodoEntity,
		active.UID,
		item.HASummary,
		due,
		desiredDescription,
	); err != nil {
		return fmt.Errorf("update task for %q: %w", item.Name, err)
	}
	return nil
}

func (service *Service) publish(ctx context.Context, todoItems []homeassistant.TodoItem) error {
	now := service.now()
	states := make([]ItemState, 0, len(service.catalog.Items))
	var publishErrors []error
	for _, item := range service.catalog.Items {
		active, err := findActive(item, todoItems)
		if err != nil {
			publishErrors = append(publishErrors, err)
			active = nil
		}
		latest, exists, err := service.history.Latest(ctx, item.ID)
		if err != nil {
			publishErrors = append(publishErrors, err)
			exists = false
		}
		state := ItemState{Item: item, Active: cloneTodo(active)}
		if exists {
			latestCopy := latest
			state.Latest = &latestCopy
		}
		states = append(states, state)
	}
	sort.SliceStable(states, func(left, right int) bool {
		return stateLess(states[left], states[right], service.catalog.Location)
	})
	service.stateMu.Lock()
	service.state = Snapshot{Items: states, RefreshedAt: now}
	service.stateMu.Unlock()
	return errors.Join(publishErrors...)
}

func (service *Service) recordError(err error) {
	service.stateMu.Lock()
	service.state.SyncError = err.Error()
	service.stateMu.Unlock()
}

// beginOperation serializes Home Assistant mutations while allowing callers to
// abandon the wait when their context is canceled.
func (service *Service) beginOperation(ctx context.Context) error {
	if err := ctx.Err(); err != nil {
		return err
	}
	select {
	case service.operation <- struct{}{}:
		if err := ctx.Err(); err != nil {
			service.endOperation()
			return err
		}
		return nil
	case <-ctx.Done():
		return ctx.Err()
	}
}

func (service *Service) endOperation() {
	<-service.operation
}

func cloneCatalog(value *catalog.Catalog) catalog.Catalog {
	cloned := *value
	if value.ConfiguredDueSoonDays != nil {
		dueSoonDays := *value.ConfiguredDueSoonDays
		cloned.ConfiguredDueSoonDays = &dueSoonDays
	}
	cloned.Items = make([]catalog.Item, len(value.Items))
	for index, item := range value.Items {
		cloned.Items[index] = cloneItem(item)
	}
	return cloned
}

func cloneItem(value catalog.Item) catalog.Item {
	value.Instructions = slices.Clone(value.Instructions)
	value.Specs = slices.Clone(value.Specs)
	value.Resources = slices.Clone(value.Resources)
	return value
}

func stateLess(left, right ItemState, location *time.Location) bool {
	leftDue, leftOK := parseDate(left.Active, location)
	rightDue, rightOK := parseDate(right.Active, location)
	if leftOK != rightOK {
		return leftOK
	}
	if leftOK && !leftDue.Equal(rightDue) {
		return leftDue.Before(rightDue)
	}
	if left.Item.Location != right.Item.Location {
		return left.Item.Location < right.Item.Location
	}
	if left.Item.Group != right.Item.Group {
		return left.Item.Group < right.Item.Group
	}
	return left.Item.Name < right.Item.Name
}

func parseDate(item *homeassistant.TodoItem, location *time.Location) (time.Time, bool) {
	if item == nil || item.Due == "" {
		return time.Time{}, false
	}
	due, err := time.ParseInLocation(time.DateOnly, item.Due, location)
	return due, err == nil
}

func findActive(item catalog.Item, todoItems []homeassistant.TodoItem) (*homeassistant.TodoItem, error) {
	var marked []homeassistant.TodoItem
	var legacy []homeassistant.TodoItem
	for _, todoItem := range todoItems {
		if todoItem.Status != "needs_action" {
			continue
		}
		managedID, hasMarker := markerID(todoItem.Description)
		if hasMarker && managedID == item.ID {
			marked = append(marked, todoItem)
			continue
		}
		if !hasMarker && todoItem.Summary == item.HASummary {
			legacy = append(legacy, todoItem)
		}
	}
	if len(marked)+len(legacy) > 1 {
		return nil, fmt.Errorf("multiple active Home Assistant tasks match %q", item.ID)
	}
	if len(marked) == 1 {
		return &marked[0], nil
	}
	if len(legacy) == 1 {
		return &legacy[0], nil
	}
	return nil, nil
}

func todoByUID(items []homeassistant.TodoItem, uid, status string) *homeassistant.TodoItem {
	for index := range items {
		if items[index].UID == uid && items[index].Status == status {
			copy := items[index]
			return &copy
		}
	}
	return nil
}

func findLatestCompleted(item catalog.Item, items []homeassistant.TodoItem) *homeassistant.TodoItem {
	var latest *homeassistant.TodoItem
	for index := range items {
		candidate := items[index]
		managedID, marked := markerID(candidate.Description)
		if candidate.Status != "completed" || !marked || managedID != item.ID || candidate.Completed == nil {
			continue
		}
		if latest == nil || latest.Completed.Before(*candidate.Completed) {
			copy := candidate
			latest = &copy
		}
	}
	return latest
}

func markerID(description string) (string, bool) {
	match := markerPattern.FindStringSubmatch(description)
	if len(match) != 2 {
		return "", false
	}
	return match[1], true
}

func description(item catalog.Item) string {
	marker := fmt.Sprintf(markerFormat, item.ID)
	if item.HADescription == "" {
		return marker
	}
	return strings.TrimSpace(item.HADescription) + "\n\n" + marker
}

func cloneTodo(item *homeassistant.TodoItem) *homeassistant.TodoItem {
	if item == nil {
		return nil
	}
	copy := *item
	return &copy
}
