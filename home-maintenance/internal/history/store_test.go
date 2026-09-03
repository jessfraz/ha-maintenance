package history

import (
	"context"
	"io"
	"os"
	"path/filepath"
	"strings"
	"testing"
	"time"
)

func TestHistorySurvivesColdBackupRestore(t *testing.T) {
	t.Parallel()

	sourceDirectory := t.TempDir()
	sourcePath := filepath.Join(sourceDirectory, "history.db")
	store, err := Open(sourcePath)
	if err != nil {
		t.Fatalf("Open() error = %v", err)
	}
	ctx := context.Background()
	event, _, err := store.Begin(ctx, Completion{
		ItemID:      "example-filter",
		SourceUID:   "source-backup",
		PreviousDue: "2031-02-17",
		CompletedAt: time.Date(2026, time.September, 3, 18, 0, 0, 0, time.UTC),
		UserID:      "user-1",
		UserName:    "Demo User",
		NextDue:     "2026-10-10",
	})
	if err != nil {
		t.Fatalf("Begin() error = %v", err)
	}
	if err := store.MarkCompleted(ctx, event.ID); err != nil {
		t.Fatalf("MarkCompleted() error = %v", err)
	}
	if err := store.MarkScheduled(ctx, event.ID, "next-backup"); err != nil {
		t.Fatalf("MarkScheduled() error = %v", err)
	}
	if err := store.Close(); err != nil {
		t.Fatalf("Close() before backup error = %v", err)
	}

	restoreDirectory := t.TempDir()
	entries, err := os.ReadDir(sourceDirectory)
	if err != nil {
		t.Fatalf("ReadDir() error = %v", err)
	}
	for _, entry := range entries {
		if entry.IsDir() {
			continue
		}
		if err := copyFile(
			filepath.Join(sourceDirectory, entry.Name()),
			filepath.Join(restoreDirectory, entry.Name()),
		); err != nil {
			t.Fatalf("copy backup file %s: %v", entry.Name(), err)
		}
	}

	restored, err := Open(filepath.Join(restoreDirectory, "history.db"))
	if err != nil {
		t.Fatalf("Open() restored database error = %v", err)
	}
	t.Cleanup(func() {
		if err := restored.Close(); err != nil {
			t.Errorf("Close() restored database error = %v", err)
		}
	})
	latest, found, err := restored.Latest(ctx, "example-filter")
	if err != nil {
		t.Fatalf("Latest() restored history error = %v", err)
	}
	if !found || latest.UserName != "Demo User" || latest.NextUID != "next-backup" {
		t.Fatalf("Latest() restored history = %#v, found = %v", latest, found)
	}
}

func copyFile(source, destination string) error {
	input, err := os.Open(source)
	if err != nil {
		return err
	}
	defer input.Close()
	output, err := os.OpenFile(destination, os.O_CREATE|os.O_WRONLY|os.O_EXCL, 0o600)
	if err != nil {
		return err
	}
	if _, err := io.Copy(output, input); err != nil {
		output.Close()
		return err
	}
	return output.Close()
}

func TestCompletionLifecycle(t *testing.T) {
	t.Parallel()

	store, err := Open(filepath.Join(t.TempDir(), "history.db"))
	if err != nil {
		t.Fatalf("Open() error = %v", err)
	}
	t.Cleanup(func() {
		if err := store.Close(); err != nil {
			t.Errorf("Close() error = %v", err)
		}
	})

	ctx := context.Background()
	completedAt := time.Date(2026, time.September, 3, 18, 0, 0, 0, time.UTC)
	input := Completion{
		ItemID:      "example-filter",
		SourceUID:   "source-1",
		PreviousDue: "2031-02-17",
		CompletedAt: completedAt,
		UserID:      "user-1",
		UserName:    "Demo User",
		NextDue:     "2026-10-10",
	}
	created, inserted, err := store.Begin(ctx, input)
	if err != nil {
		t.Fatalf("Begin() error = %v", err)
	}
	if !inserted || created.Phase != PhasePending {
		t.Fatalf("Begin() = %#v, inserted = %v", created, inserted)
	}
	if err := store.MarkCompleted(ctx, created.ID); err != nil {
		t.Fatalf("MarkCompleted() error = %v", err)
	}
	if err := store.MarkScheduled(ctx, created.ID, "next-1"); err != nil {
		t.Fatalf("MarkScheduled() error = %v", err)
	}

	latest, found, err := store.Latest(ctx, input.ItemID)
	if err != nil {
		t.Fatalf("Latest() error = %v", err)
	}
	if !found || latest.NextUID != "next-1" || latest.UserName != "Demo User" {
		t.Fatalf("Latest() = %#v, found = %v", latest, found)
	}

	duplicate, inserted, err := store.Begin(ctx, input)
	if err != nil {
		t.Fatalf("duplicate Begin() error = %v", err)
	}
	if inserted || duplicate.ID != created.ID {
		t.Fatalf("duplicate Begin() = %#v, inserted = %v", duplicate, inserted)
	}

}

func TestMarkScheduledRequiresCompletedPhase(t *testing.T) {
	t.Parallel()

	store, err := Open(filepath.Join(t.TempDir(), "history.db"))
	if err != nil {
		t.Fatalf("Open() error = %v", err)
	}
	t.Cleanup(func() {
		if err := store.Close(); err != nil {
			t.Errorf("Close() error = %v", err)
		}
	})

	event, _, err := store.Begin(context.Background(), Completion{
		ItemID:      "example-filter",
		SourceUID:   "source-1",
		PreviousDue: "2031-02-17",
		CompletedAt: time.Date(2031, time.January, 1, 12, 0, 0, 0, time.UTC),
		UserID:      "user-1",
		UserName:    "Demo User",
		NextDue:     "2031-04-01",
	})
	if err != nil {
		t.Fatalf("Begin() error = %v", err)
	}
	if err := store.MarkScheduled(context.Background(), event.ID, "next-1"); err == nil {
		t.Fatal("MarkScheduled() error = nil, want invalid transition")
	}
	stored, err := store.ByID(context.Background(), event.ID)
	if err != nil {
		t.Fatalf("ByID() error = %v", err)
	}
	if stored.Phase != PhasePending || stored.NextUID != "" {
		t.Fatalf("stored completion = %#v", stored)
	}
}

func TestLatestQueryUsesPurposeBuiltIndex(t *testing.T) {
	t.Parallel()

	store, err := Open(filepath.Join(t.TempDir(), "history.db"))
	if err != nil {
		t.Fatalf("Open() error = %v", err)
	}
	t.Cleanup(func() {
		if err := store.Close(); err != nil {
			t.Errorf("Close() error = %v", err)
		}
	})

	rows, err := store.db.Query(`
        EXPLAIN QUERY PLAN
        SELECT id, item_id, source_uid, previous_due, completed_at,
               user_id, user_name, next_due, next_uid, phase
        FROM completion_events
        WHERE item_id = ?
          AND phase IN ('completed', 'scheduled')
        ORDER BY completed_at DESC
        LIMIT 1
    `, "example-filter")
	if err != nil {
		t.Fatalf("EXPLAIN QUERY PLAN error = %v", err)
	}
	defer rows.Close()

	var detail strings.Builder
	for rows.Next() {
		var id, parent, unused int
		var rowDetail string
		if err := rows.Scan(&id, &parent, &unused, &rowDetail); err != nil {
			t.Fatalf("scan query plan: %v", err)
		}
		detail.WriteString(rowDetail)
	}
	if err := rows.Err(); err != nil {
		t.Fatalf("iterate query plan: %v", err)
	}
	if !strings.Contains(detail.String(), "idx_completion_events_item_completed") {
		t.Fatalf("query plan = %q, want completion index", detail.String())
	}
}
