package catalog

import (
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"net/url"
	"os"
	"regexp"
	"strings"
	"time"
)

const defaultDueSoonDays = 14

var itemIDPattern = regexp.MustCompile(`^[a-z0-9]+(?:-[a-z0-9]+)*$`)

type Catalog struct {
	Schema                string `json:"$schema,omitempty"`
	Title                 string `json:"title"`
	TimeZone              string `json:"timezone"`
	TodoEntity            string `json:"todo_entity"`
	ConfiguredDueSoonDays *int   `json:"due_soon_days,omitempty"`
	Items                 []Item `json:"items"`

	DueSoonDays int            `json:"-"`
	Location    *time.Location `json:"-"`
}

type Item struct {
	ID            string   `json:"id"`
	Name          string   `json:"name"`
	Location      string   `json:"location,omitempty"`
	Group         string   `json:"group,omitempty"`
	HASummary     string   `json:"ha_summary"`
	HADescription string   `json:"ha_description,omitempty"`
	Schedule      Schedule `json:"schedule"`
	InitialDue    string   `json:"initial_due,omitempty"`
	ReorderURL    string   `json:"reorder_url,omitempty"`
	Instructions  []string `json:"instructions,omitempty"`
	Specs         []Spec   `json:"specs,omitempty"`
	Resources     []Link   `json:"resources,omitempty"`
}

type Schedule struct {
	Days   int `json:"days,omitempty"`
	Months int `json:"months,omitempty"`
	Years  int `json:"years,omitempty"`
}

type Spec struct {
	Label string `json:"label"`
	Value string `json:"value"`
}

type Link struct {
	Label string `json:"label"`
	URL   string `json:"url"`
}

func Load(path string) (*Catalog, error) {
	file, err := os.Open(path)
	if err != nil {
		return nil, fmt.Errorf("open catalog: %w", err)
	}
	defer file.Close()

	return Decode(file)
}

func Decode(reader io.Reader) (*Catalog, error) {
	decoder := json.NewDecoder(reader)
	decoder.DisallowUnknownFields()

	var value Catalog
	if err := decoder.Decode(&value); err != nil {
		return nil, fmt.Errorf("decode catalog: %w", err)
	}
	if err := ensureEOF(decoder); err != nil {
		return nil, err
	}
	if err := value.validate(); err != nil {
		return nil, err
	}

	return &value, nil
}

func ensureEOF(decoder *json.Decoder) error {
	var extra any
	if err := decoder.Decode(&extra); !errors.Is(err, io.EOF) {
		if err == nil {
			return errors.New("decode catalog: multiple JSON values")
		}
		return fmt.Errorf("decode catalog: %w", err)
	}
	return nil
}

func (catalog *Catalog) validate() error {
	catalog.Title = strings.TrimSpace(catalog.Title)
	if catalog.Title == "" {
		return errors.New("catalog title is required")
	}

	catalog.TimeZone = strings.TrimSpace(catalog.TimeZone)
	location, err := time.LoadLocation(catalog.TimeZone)
	if err != nil {
		return fmt.Errorf("catalog timezone %q: %w", catalog.TimeZone, err)
	}
	catalog.Location = location

	catalog.TodoEntity = strings.TrimSpace(catalog.TodoEntity)
	if !strings.HasPrefix(catalog.TodoEntity, "todo.") {
		return fmt.Errorf("catalog todo_entity %q must start with todo.", catalog.TodoEntity)
	}
	if catalog.ConfiguredDueSoonDays != nil && *catalog.ConfiguredDueSoonDays < 0 {
		return errors.New("catalog due_soon_days cannot be negative")
	}
	if catalog.ConfiguredDueSoonDays == nil {
		catalog.DueSoonDays = defaultDueSoonDays
	} else {
		catalog.DueSoonDays = *catalog.ConfiguredDueSoonDays
	}
	if len(catalog.Items) == 0 {
		return errors.New("catalog must contain at least one item")
	}

	ids := make(map[string]struct{}, len(catalog.Items))
	summaries := make(map[string]struct{}, len(catalog.Items))
	for index := range catalog.Items {
		item := &catalog.Items[index]
		if err := item.validate(); err != nil {
			return fmt.Errorf("item %d: %w", index+1, err)
		}
		if _, exists := ids[item.ID]; exists {
			return fmt.Errorf("item %d: duplicate id %q", index+1, item.ID)
		}
		ids[item.ID] = struct{}{}

		if _, exists := summaries[item.HASummary]; exists {
			return fmt.Errorf("item %d: duplicate ha_summary %q", index+1, item.HASummary)
		}
		summaries[item.HASummary] = struct{}{}
	}

	return nil
}

func (item *Item) validate() error {
	item.ID = strings.TrimSpace(item.ID)
	if !itemIDPattern.MatchString(item.ID) {
		return fmt.Errorf("id %q must be a lowercase kebab-case identifier", item.ID)
	}
	item.Name = strings.TrimSpace(item.Name)
	if item.Name == "" {
		return errors.New("name is required")
	}
	item.Location = strings.TrimSpace(item.Location)
	item.Group = strings.TrimSpace(item.Group)
	item.HASummary = strings.TrimSpace(item.HASummary)
	if item.HASummary == "" {
		return errors.New("ha_summary is required")
	}
	item.HADescription = strings.TrimSpace(item.HADescription)
	if err := item.Schedule.validate(); err != nil {
		return err
	}
	if item.InitialDue != "" {
		if _, err := time.Parse(time.DateOnly, item.InitialDue); err != nil {
			return fmt.Errorf("initial_due %q must use YYYY-MM-DD", item.InitialDue)
		}
	}
	if err := validateHTTPURL("reorder_url", item.ReorderURL); err != nil {
		return err
	}
	for index := range item.Specs {
		item.Specs[index].Label = strings.TrimSpace(item.Specs[index].Label)
		item.Specs[index].Value = strings.TrimSpace(item.Specs[index].Value)
		if item.Specs[index].Label == "" || item.Specs[index].Value == "" {
			return fmt.Errorf("spec %d requires both label and value", index+1)
		}
	}
	for index := range item.Resources {
		item.Resources[index].Label = strings.TrimSpace(item.Resources[index].Label)
		if item.Resources[index].Label == "" {
			return fmt.Errorf("resource %d label is required", index+1)
		}
		if err := validateHTTPURL(fmt.Sprintf("resource %d url", index+1), item.Resources[index].URL); err != nil {
			return err
		}
	}
	return nil
}

func (schedule Schedule) validate() error {
	if schedule.Days < 0 || schedule.Months < 0 || schedule.Years < 0 {
		return errors.New("schedule values cannot be negative")
	}
	if schedule.Days == 0 && schedule.Months == 0 && schedule.Years == 0 {
		return errors.New("schedule requires at least one positive value")
	}
	return nil
}

func validateHTTPURL(field, value string) error {
	if value == "" {
		return nil
	}
	parsed, err := url.Parse(value)
	if err != nil || parsed.Host == "" || (parsed.Scheme != "http" && parsed.Scheme != "https") {
		return fmt.Errorf("%s %q must be an absolute HTTP or HTTPS URL", field, value)
	}
	return nil
}

func (catalog *Catalog) Item(id string) (Item, bool) {
	for _, item := range catalog.Items {
		if item.ID == id {
			return item, true
		}
	}
	return Item{}, false
}

func (schedule Schedule) Next(from time.Time) time.Time {
	next := addCalendarClamped(from, schedule.Years, schedule.Months)
	return next.AddDate(0, 0, schedule.Days)
}

func addCalendarClamped(value time.Time, years, months int) time.Time {
	targetMonth := time.Date(value.Year()+years, value.Month()+time.Month(months), 1, 0, 0, 0, 0, value.Location())
	lastDay := targetMonth.AddDate(0, 1, -1).Day()
	day := min(value.Day(), lastDay)
	return time.Date(targetMonth.Year(), targetMonth.Month(), day, value.Hour(), value.Minute(), value.Second(), value.Nanosecond(), value.Location())
}
