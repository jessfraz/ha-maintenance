package web

import (
	"context"
	"crypto/sha256"
	"embed"
	"encoding/hex"
	"errors"
	"fmt"
	"html/template"
	"io/fs"
	"log/slog"
	"net"
	"net/http"
	"net/url"
	"strconv"
	"strings"
	"time"

	"github.com/jessfraz/home-maintenance/internal/catalog"
	"github.com/jessfraz/home-maintenance/internal/history"
	"github.com/jessfraz/home-maintenance/internal/maintenance"
)

//go:embed templates/*.html assets/*
var content embed.FS

type Config struct {
	Service        *maintenance.Service
	CSRFSecretPath string
	Logger         *slog.Logger
	Development    bool
	IngressProxyIP string
}

type Server struct {
	service        *maintenance.Service
	templates      *template.Template
	assets         http.Handler
	csrf           *csrfSigner
	logger         *slog.Logger
	development    bool
	ingressProxyIP net.IP
	assetVersion   string
}

type page struct {
	Title        string
	CatalogName  string
	IngressPath  string
	AssetVersion string
	Actor        maintenance.Actor
	SyncError    string
	Refreshed    string
	Home         *homePage
	Detail       *detailPage
}

type homePage struct {
	Next      *itemView
	Remaining []itemView
	Count     int
}

type detailPage struct {
	Item      itemView
	CSRFToken string
	Completed *completionView
	Following *itemView
}

type itemView struct {
	ID            string
	Name          string
	IngressPath   string
	Location      string
	Group         string
	Route         string
	DueExact      string
	DueRelative   string
	DueClass      string
	Schedule      string
	LastExact     string
	LastRelative  string
	LastBy        string
	Progress      int
	HasProgress   bool
	HasActiveTask bool
	ActiveUID     string
	ReorderURL    string
	Instructions  []string
	Specs         []catalog.Spec
	Resources     []catalog.Link
}

type completionView struct {
	By      string
	At      string
	NextDue string
}

func New(config Config) (*Server, error) {
	if config.Service == nil {
		return nil, errors.New("web service is required")
	}
	if config.Logger == nil {
		config.Logger = slog.Default()
	}
	if config.CSRFSecretPath == "" {
		return nil, errors.New("CSRF secret path is required")
	}
	signer, err := loadCSRFSigner(config.CSRFSecretPath)
	if err != nil {
		return nil, err
	}
	templates, err := template.ParseFS(content, "templates/*.html")
	if err != nil {
		return nil, fmt.Errorf("parse templates: %w", err)
	}
	assetsFS, err := fs.Sub(content, "assets")
	if err != nil {
		return nil, fmt.Errorf("open embedded assets: %w", err)
	}
	proxyIP := net.ParseIP(config.IngressProxyIP)
	if !config.Development && proxyIP == nil {
		return nil, fmt.Errorf("invalid ingress proxy IP %q", config.IngressProxyIP)
	}
	assetVersion, err := embeddedAssetVersion()
	if err != nil {
		return nil, err
	}
	return &Server{
		service:        config.Service,
		templates:      templates,
		assets:         http.FileServer(http.FS(assetsFS)),
		csrf:           signer,
		logger:         config.Logger,
		development:    config.Development,
		ingressProxyIP: proxyIP,
		assetVersion:   assetVersion,
	}, nil
}

func (server *Server) Handler() http.Handler {
	app := http.NewServeMux()
	app.HandleFunc("GET /assets/", server.asset)
	app.HandleFunc("GET /{$}", server.home)
	app.HandleFunc("GET /items/{id}", server.item)
	app.HandleFunc("POST /items/{id}/complete", server.complete)

	handler := http.NewServeMux()
	handler.HandleFunc("GET /health", server.health)
	handler.Handle("/", server.security(app))
	return handler
}

func (server *Server) health(response http.ResponseWriter, request *http.Request) {
	response.Header().Set("Content-Type", "text/plain; charset=utf-8")
	response.WriteHeader(http.StatusOK)
	_, _ = response.Write([]byte("ok\n"))
}

func (server *Server) asset(response http.ResponseWriter, request *http.Request) {
	request.URL.Path = strings.TrimPrefix(request.URL.Path, "/assets")
	response.Header().Set("Cache-Control", "public, max-age=31536000, immutable")
	server.assets.ServeHTTP(response, request)
}

func (server *Server) home(response http.ResponseWriter, request *http.Request) {
	actor, ok := server.actor(response, request)
	if !ok {
		return
	}
	snapshot := server.service.Snapshot()
	catalogValue := server.service.Catalog()
	prefix := ingressPath(request)
	views := make([]itemView, 0, len(snapshot.Items))
	for _, state := range snapshot.Items {
		views = append(views, server.itemView(state, prefix, catalogValue.Location, catalogValue.DueSoonDays))
	}
	model := page{
		Title:        catalogValue.Title,
		CatalogName:  catalogValue.Title,
		IngressPath:  prefix,
		AssetVersion: server.assetVersion,
		Actor:        actor,
		SyncError:    snapshot.SyncError,
		Refreshed:    formatRefresh(snapshot.RefreshedAt),
		Home:         &homePage{Count: len(views), Remaining: views},
	}
	if len(views) > 0 {
		next := views[0]
		model.Home.Next = &next
		model.Home.Remaining = views[1:]
	}
	server.render(response, "home.html", model)
}

func (server *Server) item(response http.ResponseWriter, request *http.Request) {
	actor, ok := server.actor(response, request)
	if !ok {
		return
	}
	itemID := request.PathValue("id")
	snapshot := server.service.Snapshot()
	state, following, exists := findState(snapshot, itemID)
	if !exists {
		http.NotFound(response, request)
		return
	}
	catalogValue := server.service.Catalog()
	prefix := ingressPath(request)
	view := server.itemView(state, prefix, catalogValue.Location, catalogValue.DueSoonDays)
	model := page{
		Title:        state.Item.Name,
		CatalogName:  catalogValue.Title,
		IngressPath:  prefix,
		AssetVersion: server.assetVersion,
		Actor:        actor,
		SyncError:    snapshot.SyncError,
		Detail:       &detailPage{Item: view},
	}
	if view.HasActiveTask {
		model.Detail.CSRFToken = server.csrf.token(actor.ID, itemID, view.ActiveUID)
	}
	if following != nil {
		followingView := server.itemView(*following, prefix, catalogValue.Location, catalogValue.DueSoonDays)
		model.Detail.Following = &followingView
	}
	if completionID, err := strconv.ParseInt(request.URL.Query().Get("completed"), 10, 64); err == nil && completionID > 0 {
		completion, found, readErr := server.service.Completion(request.Context(), completionID, itemID)
		if readErr != nil {
			server.logger.Error("read completion", "error", readErr, "completion_id", completionID)
		} else if found {
			model.Detail.Completed = completionViewFor(completion, catalogValue.Location)
		}
	}
	server.render(response, "item.html", model)
}

func (server *Server) complete(response http.ResponseWriter, request *http.Request) {
	actor, ok := server.actor(response, request)
	if !ok {
		return
	}
	request.Body = http.MaxBytesReader(response, request.Body, 16<<10)
	if err := request.ParseForm(); err != nil {
		http.Error(response, "Invalid form", http.StatusBadRequest)
		return
	}
	itemID := request.PathValue("id")
	expectedUID := request.PostFormValue("uid")
	if !server.csrf.valid(request.PostFormValue("csrf"), actor.ID, itemID, expectedUID) {
		http.Error(response, "This page is stale. Go back and try again.", http.StatusForbidden)
		return
	}
	operationContext, cancel := context.WithTimeout(request.Context(), 45*time.Second)
	defer cancel()
	result, err := server.service.Complete(operationContext, itemID, expectedUID, actor)
	if err != nil {
		server.logger.Error("complete maintenance", "error", err, "item_id", itemID, "user_id", actor.ID)
		http.Error(response, "Could not complete maintenance. Refresh and try again.", http.StatusConflict)
		return
	}
	location := ingressPath(request) + "/items/" + url.PathEscape(itemID) + "?completed=" + strconv.FormatInt(result.Event.ID, 10)
	http.Redirect(response, request, location, http.StatusSeeOther)
}

func (server *Server) security(next http.Handler) http.Handler {
	return http.HandlerFunc(func(response http.ResponseWriter, request *http.Request) {
		if !server.development && !server.fromIngress(request) {
			http.Error(response, "Ingress only", http.StatusForbidden)
			return
		}
		response.Header().Set("Content-Security-Policy", "default-src 'self'; style-src 'self'; script-src 'self'; img-src 'self' data:; connect-src 'self'; frame-ancestors 'self'")
		response.Header().Set("Referrer-Policy", "no-referrer")
		response.Header().Set("X-Content-Type-Options", "nosniff")
		response.Header().Set("X-Frame-Options", "SAMEORIGIN")
		if !strings.HasPrefix(request.URL.Path, "/assets/") {
			response.Header().Set("Cache-Control", "no-store")
		}
		next.ServeHTTP(response, request)
	})
}

func (server *Server) fromIngress(request *http.Request) bool {
	host, _, err := net.SplitHostPort(request.RemoteAddr)
	if err != nil {
		host = request.RemoteAddr
	}
	return server.ingressProxyIP.Equal(net.ParseIP(host))
}

func (server *Server) actor(response http.ResponseWriter, request *http.Request) (maintenance.Actor, bool) {
	if server.development {
		return maintenance.Actor{ID: "development", Name: "Demo User"}, true
	}
	actor := maintenance.Actor{
		ID:   strings.TrimSpace(request.Header.Get("X-Remote-User-Id")),
		Name: strings.TrimSpace(request.Header.Get("X-Remote-User-Display-Name")),
	}
	if actor.Name == "" {
		actor.Name = strings.TrimSpace(request.Header.Get("X-Remote-User-Name"))
	}
	if actor.ID == "" || actor.Name == "" {
		http.Error(response, "Authenticated Home Assistant user required", http.StatusUnauthorized)
		return maintenance.Actor{}, false
	}
	return actor, true
}

func (server *Server) render(response http.ResponseWriter, name string, model page) {
	response.Header().Set("Content-Type", "text/html; charset=utf-8")
	if err := server.templates.ExecuteTemplate(response, name, model); err != nil {
		server.logger.Error("render page", "error", err, "template", name)
	}
}

func (server *Server) itemView(state maintenance.ItemState, prefix string, location *time.Location, dueSoonDays int) itemView {
	now := time.Now().In(location)
	view := itemView{
		ID:           state.Item.ID,
		Name:         state.Item.Name,
		IngressPath:  prefix,
		Location:     state.Item.Location,
		Group:        state.Item.Group,
		Route:        "/items/" + url.PathEscape(state.Item.ID),
		Schedule:     formatSchedule(state.Item.Schedule),
		ReorderURL:   state.Item.ReorderURL,
		Instructions: state.Item.Instructions,
		Specs:        state.Item.Specs,
		Resources:    state.Item.Resources,
	}
	if state.Active != nil {
		view.HasActiveTask = true
		view.ActiveUID = state.Active.UID
		if due, err := time.ParseInLocation(time.DateOnly, state.Active.Due, location); err == nil {
			view.DueExact = due.Format("January 2, 2006")
			view.DueRelative = relativeDate(due, now)
			view.DueClass = dueClass(due, now, dueSoonDays)
			if state.Latest != nil {
				view.Progress, view.HasProgress = progress(
					dateOnly(state.Latest.CompletedAt.In(location)),
					due,
					dateOnly(now),
				)
			}
		}
	}
	if state.Latest != nil {
		completed := state.Latest.CompletedAt.In(location)
		view.LastExact = completed.Format("January 2, 2006 at 3:04 PM")
		view.LastRelative = relativeDate(completed, now)
		view.LastBy = state.Latest.UserName
	}
	return view
}

func findState(snapshot maintenance.Snapshot, id string) (maintenance.ItemState, *maintenance.ItemState, bool) {
	for index := range snapshot.Items {
		if snapshot.Items[index].Item.ID != id {
			continue
		}
		var following *maintenance.ItemState
		for next := range snapshot.Items {
			if snapshot.Items[next].Item.ID != id {
				copy := snapshot.Items[next]
				following = &copy
				break
			}
		}
		return snapshot.Items[index], following, true
	}
	return maintenance.ItemState{}, nil, false
}

func completionViewFor(completion history.Completion, location *time.Location) *completionView {
	nextDue, _ := time.ParseInLocation(time.DateOnly, completion.NextDue, location)
	return &completionView{
		By:      completion.UserName,
		At:      completion.CompletedAt.In(location).Format("January 2, 2006 at 3:04 PM"),
		NextDue: nextDue.Format("January 2, 2006"),
	}
}

func ingressPath(request *http.Request) string {
	return strings.TrimRight(request.Header.Get("X-Ingress-Path"), "/")
}

func embeddedAssetVersion() (string, error) {
	digest := sha256.New()
	for _, path := range []string{"assets/favicon.svg", "assets/ingress.js", "assets/style.css"} {
		contents, err := content.ReadFile(path)
		if err != nil {
			return "", fmt.Errorf("read embedded asset %s: %w", path, err)
		}
		digest.Write(contents)
	}
	return hex.EncodeToString(digest.Sum(nil))[:12], nil
}

func formatRefresh(value time.Time) string {
	if value.IsZero() {
		return "waiting for Home Assistant"
	}
	return "synced " + value.Format("3:04:05 PM")
}

func formatSchedule(schedule catalog.Schedule) string {
	parts := make([]string, 0, 3)
	if schedule.Years > 0 {
		parts = append(parts, plural(schedule.Years, "year"))
	}
	if schedule.Months > 0 {
		parts = append(parts, plural(schedule.Months, "month"))
	}
	if schedule.Days > 0 {
		parts = append(parts, plural(schedule.Days, "day"))
	}
	return "every " + strings.Join(parts, ", ")
}

func relativeDate(target, now time.Time) string {
	targetDate := dateOnly(target)
	nowDate := dateOnly(now)
	if targetDate.Equal(nowDate) {
		return "today"
	}
	if targetDate.After(nowDate) {
		return "in " + calendarDistance(nowDate, targetDate)
	}
	return calendarDistance(targetDate, nowDate) + " ago"
}

func calendarDistance(start, end time.Time) string {
	years := 0
	for next := addCalendarClamped(start, 1, 0); !next.After(end); next = addCalendarClamped(start, years+1, 0) {
		years++
	}
	cursor := addCalendarClamped(start, years, 0)
	months := 0
	for next := addCalendarClamped(cursor, 0, months+1); !next.After(end); next = addCalendarClamped(cursor, 0, months+1) {
		months++
	}
	cursor = addCalendarClamped(cursor, 0, months)
	days := civilDaysBetween(cursor, end)
	parts := make([]string, 0, 3)
	if years > 0 {
		parts = append(parts, plural(years, "year"))
	}
	if months > 0 {
		parts = append(parts, plural(months, "month"))
	}
	if days > 0 || len(parts) == 0 {
		parts = append(parts, plural(days, "day"))
	}
	return strings.Join(parts, ", ")
}

func addCalendarClamped(value time.Time, years, months int) time.Time {
	targetMonth := time.Date(value.Year()+years, value.Month()+time.Month(months), 1, 0, 0, 0, 0, value.Location())
	lastDay := targetMonth.AddDate(0, 1, -1).Day()
	return time.Date(targetMonth.Year(), targetMonth.Month(), min(value.Day(), lastDay), 0, 0, 0, 0, value.Location())
}

func dateOnly(value time.Time) time.Time {
	return time.Date(value.Year(), value.Month(), value.Day(), 0, 0, 0, 0, value.Location())
}

func plural(value int, unit string) string {
	if value == 1 {
		return "1 " + unit
	}
	return strconv.Itoa(value) + " " + unit + "s"
}

func dueClass(due, now time.Time, dueSoonDays int) string {
	days := civilDaysBetween(dateOnly(now), dateOnly(due))
	switch {
	case days < 0:
		return "overdue"
	case days == 0:
		return "today"
	case days <= dueSoonDays:
		return "soon"
	default:
		return "upcoming"
	}
}

func progress(completed, due, now time.Time) (int, bool) {
	total := civilDaysBetween(completed, due)
	if total <= 0 {
		return 0, false
	}
	percent := civilDaysBetween(completed, now) * 100 / total
	if percent < 0 {
		percent = 0
	}
	if percent > 100 {
		percent = 100
	}
	return percent, true
}

func civilDaysBetween(start, end time.Time) int {
	startUTC := time.Date(start.Year(), start.Month(), start.Day(), 0, 0, 0, 0, time.UTC)
	endUTC := time.Date(end.Year(), end.Month(), end.Day(), 0, 0, 0, 0, time.UTC)
	return int(endUTC.Sub(startUTC) / (24 * time.Hour))
}
