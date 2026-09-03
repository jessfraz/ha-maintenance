package catalog

import (
	"strings"
	"testing"
	"time"
)

func TestDecodeValidCatalog(t *testing.T) {
	t.Parallel()

	value, err := Decode(strings.NewReader(`{
  "title": "Example maintenance",
  "timezone": "America/New_York",
  "todo_entity": "todo.example_maintenance",
  "items": [{
    "id": "example-filter",
    "name": "Example air filter",
    "ha_summary": "Replace example air filter",
    "schedule": {"days": 37},
    "reorder_url": "https://example.com/filter"
  }]
}`))
	if err != nil {
		t.Fatalf("Decode() error = %v", err)
	}
	if value.DueSoonDays != 14 {
		t.Fatalf("DueSoonDays = %d, want 14", value.DueSoonDays)
	}
	if value.Location.String() != "America/New_York" {
		t.Fatalf("Location = %q", value.Location)
	}
}

func TestDecodeRejectsDuplicateMapping(t *testing.T) {
	t.Parallel()

	_, err := Decode(strings.NewReader(`{
  "title": "Example maintenance",
  "timezone": "UTC",
  "todo_entity": "todo.example_maintenance",
  "items": [
    {"id":"one","name":"One","ha_summary":"Same","schedule":{"days":1}},
    {"id":"two","name":"Two","ha_summary":"Same","schedule":{"days":1}}
  ]
}`))
	if err == nil || !strings.Contains(err.Error(), "duplicate ha_summary") {
		t.Fatalf("Decode() error = %v, want duplicate ha_summary", err)
	}
}

func TestDecodeRejectsUnknownFields(t *testing.T) {
	t.Parallel()

	_, err := Decode(strings.NewReader(`{
  "title": "Example maintenance",
  "timezone": "UTC",
  "todo_entity": "todo.example_maintenance",
  "surprise": true,
  "items": [{"id":"one","name":"One","ha_summary":"One","schedule":{"days":1}}]
}`))
	if err == nil || !strings.Contains(err.Error(), "unknown field") {
		t.Fatalf("Decode() error = %v, want unknown field", err)
	}
}

func TestDecodePreservesExplicitZeroDueSoonDays(t *testing.T) {
	t.Parallel()

	value, err := Decode(strings.NewReader(`{
  "title": "Example maintenance",
  "timezone": "UTC",
  "todo_entity": "todo.example_maintenance",
  "due_soon_days": 0,
  "items": [{"id":"one","name":"One","ha_summary":"One","schedule":{"days":1}}]
}`))
	if err != nil {
		t.Fatalf("Decode() error = %v", err)
	}
	if value.DueSoonDays != 0 {
		t.Fatalf("DueSoonDays = %d, want 0", value.DueSoonDays)
	}
}

func TestScheduleNextClampsCalendarMonth(t *testing.T) {
	t.Parallel()

	location := time.FixedZone("test", -8*60*60)
	from := time.Date(2027, time.January, 31, 9, 30, 0, 0, location)
	next := (Schedule{Months: 1}).Next(from)
	want := time.Date(2027, time.February, 28, 9, 30, 0, 0, location)
	if !next.Equal(want) {
		t.Fatalf("Next() = %v, want %v", next, want)
	}
}
