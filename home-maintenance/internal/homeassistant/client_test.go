package homeassistant

import (
	"context"
	"encoding/json"
	"net/http"
	"net/http/httptest"
	"reflect"
	"testing"
)

func TestGetTodoItemsUsesServiceResponseAPI(t *testing.T) {
	t.Parallel()

	server := httptest.NewServer(http.HandlerFunc(func(writer http.ResponseWriter, request *http.Request) {
		if request.URL.RequestURI() != "/services/todo/get_items?return_response" {
			t.Errorf("request URI = %q", request.URL.RequestURI())
		}
		if authorization := request.Header.Get("Authorization"); authorization != "Bearer test-token" {
			t.Errorf("Authorization = %q", authorization)
		}
		var body map[string]any
		if err := json.NewDecoder(request.Body).Decode(&body); err != nil {
			t.Errorf("decode body: %v", err)
		}
		wantStatus := []any{"needs_action", "completed"}
		if !reflect.DeepEqual(body["status"], wantStatus) {
			t.Errorf("status = %#v, want %#v", body["status"], wantStatus)
		}
		writer.Header().Set("Content-Type", "application/json")
		_, _ = writer.Write([]byte(`{
  "changed_states": [],
  "service_response": {
    "todo.example_maintenance": {
      "items": [{
        "summary": "Run example action",
        "uid": "abc",
        "status": "needs_action",
        "due": "2032-05-06"
      }]
    }
  }
}`))
	}))
	defer server.Close()

	client, err := NewClient(server.URL, "test-token", server.Client())
	if err != nil {
		t.Fatalf("NewClient() error = %v", err)
	}
	items, err := client.GetTodoItems(context.Background(), "todo.example_maintenance")
	if err != nil {
		t.Fatalf("GetTodoItems() error = %v", err)
	}
	if len(items) != 1 || items[0].UID != "abc" {
		t.Fatalf("items = %#v", items)
	}
}

func TestCompleteTodoItemUsesUID(t *testing.T) {
	t.Parallel()

	server := httptest.NewServer(http.HandlerFunc(func(writer http.ResponseWriter, request *http.Request) {
		if request.URL.Path != "/services/todo/update_item" {
			t.Errorf("request path = %q", request.URL.Path)
		}
		var body map[string]any
		if err := json.NewDecoder(request.Body).Decode(&body); err != nil {
			t.Errorf("decode body: %v", err)
		}
		if body["item"] != "item-uid" || body["status"] != "completed" {
			t.Errorf("body = %#v", body)
		}
		writer.Header().Set("Content-Type", "application/json")
		_, _ = writer.Write([]byte(`[]`))
	}))
	defer server.Close()

	client, err := NewClient(server.URL, "test-token", server.Client())
	if err != nil {
		t.Fatalf("NewClient() error = %v", err)
	}
	if err := client.CompleteTodoItem(context.Background(), "todo.example_maintenance", "item-uid"); err != nil {
		t.Fatalf("CompleteTodoItem() error = %v", err)
	}
}

func TestUpdateTodoItemOmitsEmptyDueDate(t *testing.T) {
	t.Parallel()

	server := httptest.NewServer(http.HandlerFunc(func(writer http.ResponseWriter, request *http.Request) {
		var body map[string]any
		if err := json.NewDecoder(request.Body).Decode(&body); err != nil {
			t.Errorf("decode body: %v", err)
		}
		if _, exists := body["due_date"]; exists {
			t.Errorf("body contains empty due_date: %#v", body)
		}
		if body["item"] != "item-uid" || body["description"] != "managed" {
			t.Errorf("body = %#v", body)
		}
		writer.WriteHeader(http.StatusOK)
	}))
	defer server.Close()

	client, err := NewClient(server.URL, "test-token", server.Client())
	if err != nil {
		t.Fatalf("NewClient() error = %v", err)
	}
	if err := client.UpdateTodoItem(
		context.Background(),
		"todo.example_maintenance",
		"item-uid",
		"Service example device",
		"",
		"managed",
	); err != nil {
		t.Fatalf("UpdateTodoItem() error = %v", err)
	}
}
