package maintenance

import (
	"context"
	"io"
	"os"
	"path/filepath"
	"testing"
	"time"

	"github.com/jessfraz/home-maintenance/internal/history"
	"github.com/jessfraz/home-maintenance/internal/homeassistant"
)

func TestRestoredHistoryReconstructsHomeAssistantProjection(t *testing.T) {
	t.Parallel()
	ctx := context.Background()
	sourceDirectory := t.TempDir()
	sourceStore, err := history.Open(filepath.Join(sourceDirectory, "history.db"))
	if err != nil {
		t.Fatal(err)
	}
	todo := &memoryTodo{items: []homeassistant.TodoItem{{
		Summary:     "Service example air filter",
		UID:         "before-backup",
		Status:      "needs_action",
		Due:         "2031-02-17",
		Description: "<!-- home-maintenance:item-id=example-filter -->",
	}}}
	service, err := New(testCatalog(t), sourceStore, todo)
	if err != nil {
		t.Fatal(err)
	}
	service.now = func() time.Time {
		return time.Date(2026, 9, 3, 12, 0, 0, 0, time.UTC)
	}
	if _, err := service.Complete(
		ctx,
		"example-filter",
		"before-backup",
		Actor{ID: "user-1", Name: "Demo User"},
	); err != nil {
		t.Fatal(err)
	}
	if err := sourceStore.Close(); err != nil {
		t.Fatal(err)
	}

	restoreDirectory := t.TempDir()
	if err := copyDirectoryFiles(sourceDirectory, restoreDirectory); err != nil {
		t.Fatal(err)
	}
	restoredStore, err := history.Open(filepath.Join(restoreDirectory, "history.db"))
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() {
		if err := restoredStore.Close(); err != nil {
			t.Errorf("Close() restored history error = %v", err)
		}
	})
	restoredTodo := &memoryTodo{}
	restoredService, err := New(testCatalog(t), restoredStore, restoredTodo)
	if err != nil {
		t.Fatal(err)
	}
	if err := restoredService.Refresh(ctx); err != nil {
		t.Fatal(err)
	}

	snapshot := restoredService.Snapshot()
	if len(snapshot.Items) != 1 || snapshot.Items[0].Active == nil {
		t.Fatalf("restored snapshot = %#v", snapshot)
	}
	state := snapshot.Items[0]
	if state.Active.Due != "2026-10-10" || state.Latest == nil || state.Latest.UserName != "Demo User" {
		t.Fatalf("restored item state = %#v", state)
	}
}

func copyDirectoryFiles(sourceDirectory, destinationDirectory string) error {
	entries, err := os.ReadDir(sourceDirectory)
	if err != nil {
		return err
	}
	for _, entry := range entries {
		if entry.IsDir() {
			continue
		}
		source, err := os.Open(filepath.Join(sourceDirectory, entry.Name()))
		if err != nil {
			return err
		}
		destination, err := os.OpenFile(
			filepath.Join(destinationDirectory, entry.Name()),
			os.O_CREATE|os.O_WRONLY|os.O_EXCL,
			0o600,
		)
		if err != nil {
			source.Close()
			return err
		}
		_, copyErr := io.Copy(destination, source)
		closeDestinationErr := destination.Close()
		closeSourceErr := source.Close()
		if copyErr != nil {
			return copyErr
		}
		if closeDestinationErr != nil {
			return closeDestinationErr
		}
		if closeSourceErr != nil {
			return closeSourceErr
		}
	}
	return nil
}
