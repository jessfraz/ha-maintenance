package history

import (
	"context"
	"database/sql"
	"errors"
	"fmt"
	"time"

	_ "modernc.org/sqlite"
)

type Phase string

const (
	PhasePending   Phase = "pending"
	PhaseCompleted Phase = "completed"
	PhaseScheduled Phase = "scheduled"
)

type Completion struct {
	ID          int64
	ItemID      string
	SourceUID   string
	PreviousDue string
	CompletedAt time.Time
	UserID      string
	UserName    string
	NextDue     string
	NextUID     string
	Phase       Phase
}

type Store struct {
	db *sql.DB
}

func Open(path string) (*Store, error) {
	db, err := sql.Open("sqlite", path)
	if err != nil {
		return nil, fmt.Errorf("open history database: %w", err)
	}
	db.SetMaxOpenConns(1)
	db.SetMaxIdleConns(1)

	store := &Store{db: db}
	if err := store.initialize(context.Background()); err != nil {
		db.Close()
		return nil, err
	}
	return store, nil
}

func (store *Store) Close() error {
	return store.db.Close()
}

func (store *Store) initialize(ctx context.Context) error {
	statements := []string{
		`PRAGMA journal_mode = WAL`,
		`PRAGMA foreign_keys = ON`,
		`PRAGMA busy_timeout = 5000`,
		`CREATE TABLE IF NOT EXISTS completion_events (
            id INTEGER PRIMARY KEY,
            item_id TEXT NOT NULL,
            source_uid TEXT NOT NULL,
            previous_due TEXT NOT NULL,
            completed_at TEXT NOT NULL,
            user_id TEXT NOT NULL,
            user_name TEXT NOT NULL,
            next_due TEXT NOT NULL,
            next_uid TEXT NOT NULL DEFAULT '',
            phase TEXT NOT NULL CHECK (phase IN ('pending', 'completed', 'scheduled')),
            UNIQUE (item_id, source_uid)
        )`,
		`CREATE INDEX IF NOT EXISTS idx_completion_events_item_completed
            ON completion_events (item_id, completed_at DESC)
            WHERE phase IN ('completed', 'scheduled')`,
		`PRAGMA optimize`,
	}
	for _, statement := range statements {
		if _, err := store.db.ExecContext(ctx, statement); err != nil {
			return fmt.Errorf("initialize history database: %w", err)
		}
	}
	return nil
}

func (store *Store) Begin(ctx context.Context, completion Completion) (Completion, bool, error) {
	result, err := store.db.ExecContext(ctx, `
        INSERT INTO completion_events (
            item_id, source_uid, previous_due, completed_at,
            user_id, user_name, next_due, phase
        ) VALUES (?, ?, ?, ?, ?, ?, ?, ?)
        ON CONFLICT (item_id, source_uid) DO NOTHING
    `,
		completion.ItemID,
		completion.SourceUID,
		completion.PreviousDue,
		completion.CompletedAt.UTC().Format(time.RFC3339Nano),
		completion.UserID,
		completion.UserName,
		completion.NextDue,
		PhasePending,
	)
	if err != nil {
		return Completion{}, false, fmt.Errorf("begin completion: %w", err)
	}
	rows, err := result.RowsAffected()
	if err != nil {
		return Completion{}, false, fmt.Errorf("read inserted completion count: %w", err)
	}
	stored, err := store.BySourceUID(ctx, completion.ItemID, completion.SourceUID)
	if err != nil {
		return Completion{}, false, err
	}
	return stored, rows == 1, nil
}

func (store *Store) MarkCompleted(ctx context.Context, id int64) error {
	return store.updatePhase(ctx, id, PhaseCompleted)
}

func (store *Store) MarkScheduled(ctx context.Context, id int64, nextUID string) error {
	result, err := store.db.ExecContext(ctx, `
        UPDATE completion_events
        SET phase = ?, next_uid = ?
        WHERE id = ? AND phase = ?
    `, PhaseScheduled, nextUID, id, PhaseCompleted)
	if err != nil {
		return fmt.Errorf("mark completion scheduled: %w", err)
	}
	return requireChanged(result, "mark completion scheduled")
}

func (store *Store) updatePhase(ctx context.Context, id int64, phase Phase) error {
	result, err := store.db.ExecContext(ctx, `
        UPDATE completion_events
        SET phase = ?
        WHERE id = ? AND phase = ?
    `, phase, id, PhasePending)
	if err != nil {
		return fmt.Errorf("update completion phase: %w", err)
	}
	return requireChanged(result, "update completion phase")
}

func requireChanged(result sql.Result, operation string) error {
	rows, err := result.RowsAffected()
	if err != nil {
		return fmt.Errorf("%s: %w", operation, err)
	}
	if rows != 1 {
		return fmt.Errorf("%s: expected one changed row, got %d", operation, rows)
	}
	return nil
}

func (store *Store) Latest(ctx context.Context, itemID string) (Completion, bool, error) {
	row := store.db.QueryRowContext(ctx, `
        SELECT id, item_id, source_uid, previous_due, completed_at,
               user_id, user_name, next_due, next_uid, phase
        FROM completion_events
        WHERE item_id = ?
          AND phase IN ('completed', 'scheduled')
        ORDER BY completed_at DESC
        LIMIT 1
    `, itemID)
	completion, err := scanCompletion(row)
	if errors.Is(err, sql.ErrNoRows) {
		return Completion{}, false, nil
	}
	if err != nil {
		return Completion{}, false, fmt.Errorf("read latest completion: %w", err)
	}
	return completion, true, nil
}

func (store *Store) ByID(ctx context.Context, id int64) (Completion, error) {
	row := store.db.QueryRowContext(ctx, `
        SELECT id, item_id, source_uid, previous_due, completed_at,
               user_id, user_name, next_due, next_uid, phase
        FROM completion_events
        WHERE id = ?
    `, id)
	completion, err := scanCompletion(row)
	if err != nil {
		return Completion{}, fmt.Errorf("read completion %d: %w", id, err)
	}
	return completion, nil
}

func (store *Store) BySourceUID(ctx context.Context, itemID, sourceUID string) (Completion, error) {
	row := store.db.QueryRowContext(ctx, `
        SELECT id, item_id, source_uid, previous_due, completed_at,
               user_id, user_name, next_due, next_uid, phase
        FROM completion_events
        WHERE item_id = ? AND source_uid = ?
    `, itemID, sourceUID)
	completion, err := scanCompletion(row)
	if err != nil {
		return Completion{}, fmt.Errorf("read completion for %s: %w", itemID, err)
	}
	return completion, nil
}

func (store *Store) Unfinished(ctx context.Context) ([]Completion, error) {
	rows, err := store.db.QueryContext(ctx, `
        SELECT id, item_id, source_uid, previous_due, completed_at,
               user_id, user_name, next_due, next_uid, phase
        FROM completion_events
        WHERE phase IN ('pending', 'completed')
        ORDER BY id
    `)
	if err != nil {
		return nil, fmt.Errorf("read unfinished completions: %w", err)
	}
	defer rows.Close()

	var completions []Completion
	for rows.Next() {
		completion, err := scanCompletion(rows)
		if err != nil {
			return nil, fmt.Errorf("scan unfinished completion: %w", err)
		}
		completions = append(completions, completion)
	}
	if err := rows.Err(); err != nil {
		return nil, fmt.Errorf("iterate unfinished completions: %w", err)
	}
	return completions, nil
}

type scanner interface {
	Scan(dest ...any) error
}

func scanCompletion(row scanner) (Completion, error) {
	var value Completion
	var completedAt string
	if err := row.Scan(
		&value.ID,
		&value.ItemID,
		&value.SourceUID,
		&value.PreviousDue,
		&completedAt,
		&value.UserID,
		&value.UserName,
		&value.NextDue,
		&value.NextUID,
		&value.Phase,
	); err != nil {
		return Completion{}, err
	}
	parsedCompletedAt, err := time.Parse(time.RFC3339Nano, completedAt)
	if err != nil {
		return Completion{}, fmt.Errorf("parse completed_at: %w", err)
	}
	value.CompletedAt = parsedCompletedAt
	return value, nil
}
