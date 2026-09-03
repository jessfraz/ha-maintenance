package main

import (
	"context"
	"errors"
	"flag"
	"fmt"
	"log/slog"
	"net/http"
	"os"
	"os/signal"
	"strconv"
	"sync"
	"syscall"
	"time"

	"github.com/jessfraz/home-maintenance/internal/catalog"
	"github.com/jessfraz/home-maintenance/internal/history"
	"github.com/jessfraz/home-maintenance/internal/homeassistant"
	"github.com/jessfraz/home-maintenance/internal/maintenance"
	appweb "github.com/jessfraz/home-maintenance/internal/web"
)

const defaultIngressProxyIP = "172.30.32.2"

func main() {
	if err := run(); err != nil {
		slog.Error("home-maintenance stopped", "error", err)
		os.Exit(1)
	}
}

func run() error {
	var (
		configPath    = flag.String("config", "/config/items.json", "path to the private maintenance catalog")
		databasePath  = flag.String("database", "/data/history.db", "path to the SQLite history database")
		csrfPath      = flag.String("csrf-secret", "/data/csrf.key", "path to the persistent CSRF secret")
		listenAddress = flag.String("listen", ":8099", "HTTP listen address")
		development   = flag.Bool("development", false, "allow direct local requests with a development identity")
		demo          = flag.Bool("demo", false, "use an in-memory Home Assistant projection")
	)
	flag.Parse()

	logger := slog.New(slog.NewTextHandler(os.Stdout, &slog.HandlerOptions{}))
	slog.SetDefault(logger)

	catalogValue, err := catalog.Load(*configPath)
	if err != nil {
		return err
	}
	store, err := history.Open(*databasePath)
	if err != nil {
		return err
	}
	defer store.Close()

	var todo maintenance.TodoClient
	if *demo {
		todo = newDemoTodo(catalogValue)
		*development = true
	} else {
		token := os.Getenv("SUPERVISOR_TOKEN")
		baseURL := os.Getenv("HOME_ASSISTANT_API_URL")
		if baseURL == "" {
			baseURL = "http://supervisor/core/api"
		}
		client, clientErr := homeassistant.NewClient(baseURL, token, nil)
		if clientErr != nil {
			return clientErr
		}
		todo = client
	}

	service, err := maintenance.New(catalogValue, store, todo)
	if err != nil {
		return err
	}
	startupContext, cancelStartup := context.WithTimeout(context.Background(), 30*time.Second)
	if err := service.Refresh(startupContext); err != nil {
		logger.Warn("initial Home Assistant reconciliation is degraded; serving cached catalog and retrying", "error", err)
	}
	cancelStartup()

	server, err := appweb.New(appweb.Config{
		Service:        service,
		CSRFSecretPath: *csrfPath,
		Logger:         logger,
		Development:    *development,
		IngressProxyIP: envOrDefault("INGRESS_PROXY_IP", defaultIngressProxyIP),
	})
	if err != nil {
		return err
	}

	rootContext, stop := signal.NotifyContext(context.Background(), os.Interrupt, syscall.SIGTERM)
	defer stop()
	go service.Run(rootContext, 15*time.Second)

	httpServer := &http.Server{
		Addr:              *listenAddress,
		Handler:           server.Handler(),
		ReadHeaderTimeout: 5 * time.Second,
		ReadTimeout:       10 * time.Second,
		WriteTimeout:      50 * time.Second,
		IdleTimeout:       60 * time.Second,
	}
	serverError := make(chan error, 1)
	go func() {
		logger.Info("serving maintenance UI", "address", *listenAddress, "items", len(catalogValue.Items), "demo", *demo)
		serverError <- httpServer.ListenAndServe()
	}()

	select {
	case err := <-serverError:
		if !errors.Is(err, http.ErrServerClosed) {
			return err
		}
	case <-rootContext.Done():
	}

	shutdownContext, cancelShutdown := context.WithTimeout(context.Background(), 10*time.Second)
	defer cancelShutdown()
	return httpServer.Shutdown(shutdownContext)
}

func envOrDefault(name, fallback string) string {
	if value := os.Getenv(name); value != "" {
		return value
	}
	return fallback
}

type demoTodo struct {
	mu      sync.Mutex
	items   []homeassistant.TodoItem
	counter int
}

func newDemoTodo(catalogValue *catalog.Catalog) *demoTodo {
	todo := &demoTodo{}
	for _, item := range catalogValue.Items {
		if item.InitialDue == "" {
			continue
		}
		todo.counter++
		todo.items = append(todo.items, homeassistant.TodoItem{
			Summary: item.HASummary,
			UID:     "demo-" + strconv.Itoa(todo.counter),
			Status:  "needs_action",
			Due:     item.InitialDue,
		})
	}
	return todo
}

func (todo *demoTodo) GetTodoItems(context.Context, string) ([]homeassistant.TodoItem, error) {
	todo.mu.Lock()
	defer todo.mu.Unlock()
	items := make([]homeassistant.TodoItem, len(todo.items))
	copy(items, todo.items)
	return items, nil
}

func (todo *demoTodo) CompleteTodoItem(_ context.Context, _ string, uid string) error {
	todo.mu.Lock()
	defer todo.mu.Unlock()
	for index := range todo.items {
		if todo.items[index].UID == uid {
			todo.items[index].Status = "completed"
			return nil
		}
	}
	return fmt.Errorf("demo todo UID %q not found", uid)
}

func (todo *demoTodo) UpdateTodoItem(_ context.Context, _ string, uid, summary, due, description string) error {
	todo.mu.Lock()
	defer todo.mu.Unlock()
	for index := range todo.items {
		if todo.items[index].UID == uid {
			todo.items[index].Summary = summary
			todo.items[index].Due = due
			todo.items[index].Description = description
			return nil
		}
	}
	return fmt.Errorf("demo todo UID %q not found", uid)
}

func (todo *demoTodo) AddTodoItem(_ context.Context, _ string, summary, due, description string) error {
	todo.mu.Lock()
	defer todo.mu.Unlock()
	todo.counter++
	todo.items = append(todo.items, homeassistant.TodoItem{
		Summary:     summary,
		UID:         "demo-" + strconv.Itoa(todo.counter),
		Status:      "needs_action",
		Due:         due,
		Description: description,
	})
	return nil
}
