package homeassistant

import (
	"bytes"
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"net/http"
	"strings"
	"time"
)

const defaultTimeout = 10 * time.Second

type TodoItem struct {
	Summary     string     `json:"summary"`
	UID         string     `json:"uid"`
	Status      string     `json:"status"`
	Due         string     `json:"due,omitempty"`
	Description string     `json:"description,omitempty"`
	Completed   *time.Time `json:"completed,omitempty"`
}

type Client struct {
	baseURL    string
	token      string
	httpClient *http.Client
}

type APIError struct {
	StatusCode int
	Body       string
}

func (err *APIError) Error() string {
	return fmt.Sprintf("Home Assistant API returned %d: %s", err.StatusCode, err.Body)
}

func NewClient(baseURL, token string, httpClient *http.Client) (*Client, error) {
	baseURL = strings.TrimRight(strings.TrimSpace(baseURL), "/")
	if baseURL == "" {
		return nil, errors.New("Home Assistant API URL is required")
	}
	token = strings.TrimSpace(token)
	if token == "" {
		return nil, errors.New("Home Assistant API token is required")
	}
	if httpClient == nil {
		httpClient = &http.Client{Timeout: defaultTimeout}
	}
	return &Client{baseURL: baseURL, token: token, httpClient: httpClient}, nil
}

func (client *Client) GetTodoItems(ctx context.Context, entityID string) ([]TodoItem, error) {
	payload := map[string]any{
		"entity_id": entityID,
		"status":    []string{"needs_action", "completed"},
	}
	var response struct {
		ServiceResponse map[string]struct {
			Items []TodoItem `json:"items"`
		} `json:"service_response"`
	}
	if err := client.call(ctx, "todo/get_items?return_response", payload, &response); err != nil {
		return nil, err
	}
	entity, exists := response.ServiceResponse[entityID]
	if !exists {
		return nil, fmt.Errorf("Home Assistant response omitted %s", entityID)
	}
	return entity.Items, nil
}

func (client *Client) CompleteTodoItem(ctx context.Context, entityID, uid string) error {
	return client.call(ctx, "todo/update_item", map[string]any{
		"entity_id": entityID,
		"item":      uid,
		"status":    "completed",
	}, nil)
}

func (client *Client) UpdateTodoItem(ctx context.Context, entityID, uid, summary, dueDate, description string) error {
	payload := map[string]any{
		"entity_id":   entityID,
		"item":        uid,
		"rename":      summary,
		"description": description,
	}
	if dueDate != "" {
		payload["due_date"] = dueDate
	}
	return client.call(ctx, "todo/update_item", payload, nil)
}

func (client *Client) AddTodoItem(ctx context.Context, entityID, summary, dueDate, description string) error {
	payload := map[string]any{
		"entity_id": entityID,
		"item":      summary,
		"due_date":  dueDate,
	}
	if description != "" {
		payload["description"] = description
	}
	return client.call(ctx, "todo/add_item", payload, nil)
}

func (client *Client) call(ctx context.Context, action string, payload any, destination any) error {
	body, err := json.Marshal(payload)
	if err != nil {
		return fmt.Errorf("encode Home Assistant request: %w", err)
	}
	request, err := http.NewRequestWithContext(
		ctx,
		http.MethodPost,
		client.baseURL+"/services/"+action,
		bytes.NewReader(body),
	)
	if err != nil {
		return fmt.Errorf("create Home Assistant request: %w", err)
	}
	request.Header.Set("Authorization", "Bearer "+client.token)
	request.Header.Set("Content-Type", "application/json")

	response, err := client.httpClient.Do(request)
	if err != nil {
		return fmt.Errorf("call Home Assistant: %w", err)
	}
	defer response.Body.Close()

	responseBody, err := io.ReadAll(io.LimitReader(response.Body, 2<<20))
	if err != nil {
		return fmt.Errorf("read Home Assistant response: %w", err)
	}
	if response.StatusCode < http.StatusOK || response.StatusCode >= http.StatusMultipleChoices {
		message := strings.TrimSpace(string(responseBody))
		if message == "" {
			message = http.StatusText(response.StatusCode)
		}
		return &APIError{StatusCode: response.StatusCode, Body: message}
	}
	if destination == nil || len(responseBody) == 0 {
		return nil
	}
	if err := json.Unmarshal(responseBody, destination); err != nil {
		return fmt.Errorf("decode Home Assistant response: %w", err)
	}
	return nil
}
