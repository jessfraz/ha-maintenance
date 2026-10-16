use std::{
    net::{IpAddr, SocketAddr},
    sync::Arc,
};

use anyhow::{Context as _, Result};
use dropshot::{
    ApiDescription, Body, ClientErrorStatusCode as Status, ConfigDropshot, HttpError,
    HttpResponseCreated, HttpResponseHeaders, HttpResponseOk, HttpResponseUpdatedNoContent,
    HttpServer, HttpServerStarter, Path, Query, RequestContext, TypedBody, endpoint,
};
use http::Response;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;
use uuid::Uuid;

use crate::{
    installation::{Installation, InstallationReport, PresenceReport},
    models::{
        Actor, CompleteItem, Completion, CreateItem, ImportCatalog, Item, ItemId, Page, Reorder,
        ReorderItem, ReorderReceipt, ReorderStatus, ResolveReorder, SetChecklistStep, Summary,
        SystemActor, UpdateItem,
    },
    store::Store,
};

#[derive(Clone)]
pub struct Context {
    pub store: Store,
    owner: [u8; 32],
    integration: Option<[u8; 32]>,
    ingress_peer: Option<IpAddr>,
    installation: Arc<Installation>,
}
impl Context {
    pub fn new(store: Store, owner: &str) -> Self {
        Self {
            store,
            owner: Sha256::digest(owner).into(),
            integration: None,
            ingress_peer: None,
            installation: Arc::new(Installation::default()),
        }
    }
    pub fn with_integration(mut self, token: &str) -> Self {
        self.integration = Some(Sha256::digest(token).into());
        self
    }
    pub fn for_ingress(mut self, peer: IpAddr) -> Self {
        self.ingress_peer = Some(peer);
        self
    }
}
#[derive(Clone, Copy, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    Owner,
    Integration,
}
struct Principal {
    actor: Actor,
    admin: bool,
    role: Role,
}
fn fail(status: Status, message: &str) -> HttpError {
    HttpError::for_client_error(None, status, message.into())
}
fn header<'a>(request: &'a RequestContext<Arc<Context>>, name: &str) -> Option<&'a str> {
    request
        .request
        .headers()
        .get(name)
        .and_then(|value| std::str::from_utf8(value.as_bytes()).ok())
}
fn ingress_access(request: &RequestContext<Arc<Context>>) -> Result<(), HttpError> {
    if request
        .context()
        .ingress_peer
        .is_some_and(|peer| request.request.remote_addr().ip() != peer)
    {
        return Err(fail(
            Status::FORBIDDEN,
            "only the Supervisor ingress peer is accepted",
        ));
    }
    Ok(())
}
fn valid_identity(value: &str) -> bool {
    !value.is_empty() && value.len() <= 200 && !value.chars().any(char::is_control)
}
fn user_actor(id: &str, name: &str) -> Result<Actor, HttpError> {
    if !valid_identity(id) || !valid_identity(name) {
        return Err(fail(Status::BAD_REQUEST, "invalid actor identity"));
    }
    Ok(Actor::User {
        id: id.into(),
        name: name.into(),
    })
}
fn authenticate(request: &RequestContext<Arc<Context>>) -> Result<Principal, HttpError> {
    let context = request.context();
    if context.ingress_peer.is_some() {
        ingress_access(request)?;
        if request.request.method() != http::Method::GET
            && header(request, "x-maintenance-request") != Some("1")
        {
            return Err(fail(Status::FORBIDDEN, "missing browser request header"));
        }
        let id = header(request, "x-remote-user-id")
            .ok_or_else(|| fail(Status::UNAUTHORIZED, "Home Assistant user required"))?;
        let name = header(request, "x-remote-user-display-name")
            .or_else(|| header(request, "x-remote-user-name"))
            .unwrap_or(id);
        // The packaged dashboard is panel_admin:true, enforced by Supervisor.
        // Non-admin completions enter through the separately scoped HA adapter.
        return Ok(Principal {
            actor: user_actor(&format!("ha:{id}"), name)?,
            admin: true,
            role: Role::Owner,
        });
    }
    let token = header(request, "authorization")
        .and_then(|value| value.strip_prefix("Bearer "))
        .filter(|value| value.len() <= 256)
        .ok_or_else(|| fail(Status::UNAUTHORIZED, "Bearer authentication required"))?;
    let digest: [u8; 32] = Sha256::digest(token).into();
    if bool::from(digest.ct_eq(&context.owner)) {
        // An owner credential cannot impersonate an HA user with request headers.
        return Ok(Principal {
            actor: Actor::System {
                id: SystemActor::Owner,
                name: "Owner credential".into(),
            },
            admin: true,
            role: Role::Owner,
        });
    }
    if !context
        .integration
        .as_ref()
        .is_some_and(|expected| bool::from(digest.ct_eq(expected)))
    {
        return Err(fail(Status::UNAUTHORIZED, "invalid credential"));
    }
    let actor = if let Some(system_actor) = header(request, "x-maintenance-system-actor") {
        if header(request, "x-maintenance-actor-id").is_some()
            || !matches!(system_actor, "ha:automation" | "ha:integration")
        {
            return Err(fail(Status::BAD_REQUEST, "invalid system actor"));
        }
        Actor::System {
            id: if system_actor == "ha:automation" {
                SystemActor::Automation
            } else {
                SystemActor::Integration
            },
            name: if system_actor == "ha:automation" {
                "Home Assistant automation"
            } else {
                "Home Assistant integration"
            }
            .into(),
        }
    } else if let Some(id) = header(request, "x-maintenance-actor-id") {
        if !id.starts_with("ha:") || id.len() <= 3 {
            return Err(fail(Status::BAD_REQUEST, "invalid Home Assistant actor"));
        }
        user_actor(
            id,
            header(request, "x-maintenance-actor-name").unwrap_or(id),
        )?
    } else if request.request.method() == http::Method::GET {
        Actor::System {
            id: SystemActor::Integration,
            name: "Home Assistant integration".into(),
        }
    } else {
        return Err(fail(
            Status::FORBIDDEN,
            "integration writes require a trusted actor",
        ));
    };
    let admin = matches!(actor, Actor::User { .. })
        && header(request, "x-maintenance-admin") == Some("true");
    Ok(Principal {
        actor,
        admin,
        role: Role::Integration,
    })
}
fn administrator(request: &RequestContext<Arc<Context>>) -> Result<Principal, HttpError> {
    let principal = authenticate(request)?;
    if !principal.admin {
        return Err(fail(Status::FORBIDDEN, "administrator access required"));
    }
    Ok(principal)
}
fn revision(request: &RequestContext<Arc<Context>>) -> Result<i64, HttpError> {
    let value = header(request, "if-match").ok_or_else(|| {
        fail(
            Status::PRECONDITION_REQUIRED,
            "If-Match is required; use the current ETag",
        )
    })?;
    value
        .strip_prefix('"')
        .and_then(|value| value.strip_suffix('"'))
        .and_then(|value| value.parse().ok())
        .filter(|value| *value > 0)
        .ok_or_else(|| {
            fail(
                Status::BAD_REQUEST,
                "If-Match must be a quoted positive revision",
            )
        })
}
fn idempotency(request: &RequestContext<Arc<Context>>) -> Result<&str, HttpError> {
    header(request, "idempotency-key")
        .filter(|value| {
            !value.is_empty()
                && value.len() <= 200
                && value.bytes().all(|byte| byte.is_ascii_graphic())
        })
        .ok_or_else(|| {
            fail(
                Status::BAD_REQUEST,
                "Idempotency-Key must contain 1..200 printable ASCII characters",
            )
        })
}
#[derive(Serialize, JsonSchema)]
struct PrivateHeaders {
    #[serde(rename = "cache-control")]
    cache_control: String,
}
fn private<T: Serialize + JsonSchema + Send + Sync + 'static>(
    value: T,
) -> HttpResponseHeaders<HttpResponseOk<T>, PrivateHeaders> {
    HttpResponseHeaders::new(
        HttpResponseOk(value),
        PrivateHeaders {
            cache_control: "no-store".into(),
        },
    )
}
#[derive(Serialize, JsonSchema)]
struct ItemHeaders {
    etag: String,
    #[serde(rename = "cache-control")]
    cache_control: String,
}
fn item_headers(item: &Item) -> ItemHeaders {
    ItemHeaders {
        etag: format!("\"{}\"", item.revision),
        cache_control: "no-store".into(),
    }
}
type ItemResponse = HttpResponseHeaders<HttpResponseOk<Item>, ItemHeaders>;
fn item_response(item: Item) -> ItemResponse {
    let headers = item_headers(&item);
    HttpResponseHeaders::new(HttpResponseOk(item), headers)
}
type ReorderResponse = HttpResponseHeaders<HttpResponseOk<Reorder>, ItemHeaders>;
fn reorder_response(reorder: Reorder) -> ReorderResponse {
    let headers = ItemHeaders {
        etag: format!("\"{}\"", reorder.revision),
        cache_control: "no-store".into(),
    };
    HttpResponseHeaders::new(HttpResponseOk(reorder), headers)
}
#[derive(Deserialize, JsonSchema)]
struct ItemPath {
    id: ItemId,
}
#[derive(Deserialize, JsonSchema)]
struct ChecklistPath {
    id: ItemId,
    step_id: Uuid,
}
#[derive(Deserialize, JsonSchema)]
struct ReorderPath {
    id: Uuid,
}
#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct ReorderQuery {
    #[serde(default)]
    status: ReorderStatus,
    limit: Option<u32>,
    page_token: Option<String>,
}
#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct ListQuery {
    #[serde(default)]
    archived: bool,
    limit: Option<u32>,
    page_token: Option<String>,
}
#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct EventQuery {
    limit: Option<u32>,
    page_token: Option<String>,
}
fn page_limit(value: Option<u32>) -> Result<u32, HttpError> {
    match value {
        None => Ok(100),
        Some(value @ 1..=200) => Ok(value),
        _ => Err(fail(Status::BAD_REQUEST, "limit must be between 1 and 200")),
    }
}

/// List items in due-date order, using a cursor scoped to the archive filter.
#[endpoint {method=GET,path="/v1/items",tags=["items"]}]
async fn items_list(
    request: RequestContext<Arc<Context>>,
    query: Query<ListQuery>,
) -> Result<HttpResponseHeaders<HttpResponseOk<Page<Item>>, PrivateHeaders>, HttpError> {
    authenticate(&request)?;
    let query = query.into_inner();
    Ok(private(
        request
            .context()
            .store
            .list(
                query.archived,
                query.page_token.as_deref(),
                page_limit(query.limit)?,
            )
            .await?,
    ))
}
/// Create a new maintenance definition and its first occurrence.
#[endpoint {method=POST,path="/v1/items",tags=["items"]}]
async fn items_create(
    request: RequestContext<Arc<Context>>,
    body: TypedBody<CreateItem>,
) -> Result<HttpResponseHeaders<HttpResponseCreated<Item>, ItemHeaders>, HttpError> {
    let principal = administrator(&request)?;
    let item = request
        .context()
        .store
        .create(body.into_inner(), &principal.actor, idempotency(&request)?)
        .await?;
    let headers = item_headers(&item);
    Ok(HttpResponseHeaders::new(HttpResponseCreated(item), headers))
}
/// Import only new definitions, atomically. Missing items never delete history.
#[endpoint {method=POST,path="/v1/imports",tags=["items"]}]
async fn items_import(
    request: RequestContext<Arc<Context>>,
    body: TypedBody<ImportCatalog>,
) -> Result<HttpResponseHeaders<HttpResponseOk<Page<Item>>, PrivateHeaders>, HttpError> {
    administrator(&request)?;
    Ok(private(
        request.context().store.import(body.into_inner()).await?,
    ))
}
#[endpoint {method=GET,path="/v1/items/{id}",tags=["items"]}]
async fn item_get(
    request: RequestContext<Arc<Context>>,
    path: Path<ItemPath>,
) -> Result<ItemResponse, HttpError> {
    authenticate(&request)?;
    Ok(item_response(
        request.context().store.get(&path.into_inner().id).await?,
    ))
}
/// Omitted fields stay unchanged; null clears only optional fields.
#[endpoint {method=PATCH,path="/v1/items/{id}",tags=["items"]}]
async fn item_update(
    request: RequestContext<Arc<Context>>,
    path: Path<ItemPath>,
    body: TypedBody<UpdateItem>,
) -> Result<ItemResponse, HttpError> {
    administrator(&request)?;
    Ok(item_response(
        request
            .context()
            .store
            .update(
                &path.into_inner().id,
                revision(&request)?,
                body.into_inner(),
            )
            .await?,
    ))
}
/// Commit one completion and one next occurrence in the same transaction.
#[endpoint {method=POST,path="/v1/items/{id}/completions",tags=["items"]}]
async fn item_complete(
    request: RequestContext<Arc<Context>>,
    path: Path<ItemPath>,
    body: TypedBody<CompleteItem>,
) -> Result<ItemResponse, HttpError> {
    let principal = authenticate(&request)?;
    Ok(item_response(
        request
            .context()
            .store
            .complete(
                &path.into_inner().id,
                revision(&request)?,
                body.into_inner(),
                &principal.actor,
                idempotency(&request)?,
            )
            .await?,
    ))
}
/// Persist a reorder request and immutable acknowledgement without purchasing anything.
/// Exact retries return the original receipt, even after source or request changes.
#[endpoint {method=POST,path="/v1/items/{id}/reorders",tags=["reorders"]}]
async fn item_reorder(
    request: RequestContext<Arc<Context>>,
    path: Path<ItemPath>,
    body: TypedBody<ReorderItem>,
) -> Result<HttpResponseHeaders<HttpResponseCreated<ReorderReceipt>, PrivateHeaders>, HttpError> {
    let principal = authenticate(&request)?;
    let receipt = request
        .context()
        .store
        .reorder(
            &path.into_inner().id,
            revision(&request)?,
            body.into_inner(),
            &principal.actor,
            idempotency(&request)?,
        )
        .await?;
    Ok(HttpResponseHeaders::new(
        HttpResponseCreated(receipt),
        PrivateHeaders {
            cache_control: "no-store".into(),
        },
    ))
}
/// List requests in stable creation order with a status-scoped cursor.
#[endpoint {method=GET,path="/v1/reorders",tags=["reorders"]}]
async fn reorders_list(
    request: RequestContext<Arc<Context>>,
    query: Query<ReorderQuery>,
) -> Result<HttpResponseHeaders<HttpResponseOk<Page<Reorder>>, PrivateHeaders>, HttpError> {
    authenticate(&request)?;
    let query = query.into_inner();
    Ok(private(
        request
            .context()
            .store
            .reorders(
                query.status,
                query.page_token.as_deref(),
                page_limit(query.limit)?,
            )
            .await?,
    ))
}
#[endpoint {method=GET,path="/v1/reorders/{id}",tags=["reorders"]}]
async fn reorder_get(
    request: RequestContext<Arc<Context>>,
    path: Path<ReorderPath>,
) -> Result<ReorderResponse, HttpError> {
    authenticate(&request)?;
    Ok(reorder_response(
        request
            .context()
            .store
            .get_reorder(path.into_inner().id)
            .await?,
    ))
}
/// Resolve a pending request. Terminal records and their receipts are retained.
/// Repeating the same status with the current ETag is a no-op; a stale ETag is 412.
#[endpoint {method=PUT,path="/v1/reorders/{id}/resolution",tags=["reorders"]}]
async fn reorder_resolve(
    request: RequestContext<Arc<Context>>,
    path: Path<ReorderPath>,
    body: TypedBody<ResolveReorder>,
) -> Result<ReorderResponse, HttpError> {
    let principal = administrator(&request)?;
    Ok(reorder_response(
        request
            .context()
            .store
            .resolve_reorder(
                path.into_inner().id,
                revision(&request)?,
                body.into_inner(),
                &principal.actor,
            )
            .await?,
    ))
}
/// Check one step of the current occurrence without advancing its schedule.
#[endpoint {method=PUT,path="/v1/items/{id}/checklist/{step_id}",tags=["items"]}]
async fn checklist_step_set(
    request: RequestContext<Arc<Context>>,
    path: Path<ChecklistPath>,
    body: TypedBody<SetChecklistStep>,
) -> Result<ItemResponse, HttpError> {
    let principal = authenticate(&request)?;
    let path = path.into_inner();
    Ok(item_response(
        request
            .context()
            .store
            .set_checklist_step(
                &path.id,
                path.step_id,
                revision(&request)?,
                body.into_inner(),
                &principal.actor,
                idempotency(&request)?,
            )
            .await?,
    ))
}
#[endpoint {method=PUT,path="/v1/items/{id}/archive",tags=["items"]}]
async fn item_archive(
    request: RequestContext<Arc<Context>>,
    path: Path<ItemPath>,
) -> Result<ItemResponse, HttpError> {
    administrator(&request)?;
    Ok(item_response(
        request
            .context()
            .store
            .archive(&path.into_inner().id, revision(&request)?, true)
            .await?,
    ))
}
#[endpoint {method=DELETE,path="/v1/items/{id}/archive",tags=["items"]}]
async fn item_restore(
    request: RequestContext<Arc<Context>>,
    path: Path<ItemPath>,
) -> Result<ItemResponse, HttpError> {
    administrator(&request)?;
    Ok(item_response(
        request
            .context()
            .store
            .archive(&path.into_inner().id, revision(&request)?, false)
            .await?,
    ))
}
#[endpoint {method=GET,path="/v1/items/{id}/events",tags=["items"]}]
async fn item_events(
    request: RequestContext<Arc<Context>>,
    path: Path<ItemPath>,
    query: Query<EventQuery>,
) -> Result<HttpResponseHeaders<HttpResponseOk<Page<Completion>>, PrivateHeaders>, HttpError> {
    authenticate(&request)?;
    let query = query.into_inner();
    Ok(private(
        request
            .context()
            .store
            .events(
                &path.into_inner().id,
                query.page_token.as_deref(),
                page_limit(query.limit)?,
            )
            .await?,
    ))
}
#[endpoint {method=GET,path="/v1/summary",tags=["system"]}]
async fn summary(
    request: RequestContext<Arc<Context>>,
) -> Result<HttpResponseHeaders<HttpResponseOk<Summary>, PrivateHeaders>, HttpError> {
    authenticate(&request)?;
    Ok(private(request.context().store.summary().await?))
}
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct System {
    pub version: String,
    pub credential_role: Role,
    pub integration_connected: bool,
}
#[endpoint {method=GET,path="/v1/system",tags=["system"]}]
async fn system(
    request: RequestContext<Arc<Context>>,
) -> Result<HttpResponseHeaders<HttpResponseOk<System>, PrivateHeaders>, HttpError> {
    let principal = authenticate(&request)?;
    let report = request.context().installation.report().await;
    Ok(private(System {
        version: env!("CARGO_PKG_VERSION").into(),
        credential_role: principal.role,
        integration_connected: report.integration_connected,
    }))
}
#[endpoint {method=GET,path="/v1/installation",tags=["system"]}]
async fn installation(
    request: RequestContext<Arc<Context>>,
) -> Result<HttpResponseHeaders<HttpResponseOk<InstallationReport>, PrivateHeaders>, HttpError> {
    administrator(&request)?;
    Ok(private(request.context().installation.report().await))
}
#[endpoint {method=POST,path="/v1/installation/check",tags=["system"]}]
async fn installation_check(
    request: RequestContext<Arc<Context>>,
) -> Result<HttpResponseHeaders<HttpResponseOk<InstallationReport>, PrivateHeaders>, HttpError> {
    administrator(&request)?;
    sqlx::query_scalar::<_, i64>("SELECT 1")
        .fetch_one(request.context().store.pool())
        .await
        .map_err(crate::error::Error::from)?;
    Ok(private(request.context().installation.report().await))
}
#[endpoint {method=POST,path="/v1/integration/presence",tags=["system"]}]
async fn integration_presence(
    request: RequestContext<Arc<Context>>,
    body: TypedBody<PresenceReport>,
) -> Result<HttpResponseUpdatedNoContent, HttpError> {
    let principal = authenticate(&request)?;
    if !matches!(principal.role, Role::Integration) || request.context().ingress_peer.is_some() {
        return Err(fail(
            Status::FORBIDDEN,
            "dedicated integration credential required",
        ));
    }
    request
        .context()
        .installation
        .record(body.into_inner())
        .await?;
    Ok(HttpResponseUpdatedNoContent())
}
/// Readiness only. Never returns household state or credentials.
#[endpoint {method=GET,path="/health",tags=["system"]}]
async fn health(
    request: RequestContext<Arc<Context>>,
) -> Result<HttpResponseOk<String>, HttpError> {
    ingress_access(&request)?;
    sqlx::query_scalar::<_, i64>("SELECT 1")
        .fetch_one(request.context().store.pool())
        .await
        .map_err(crate::error::Error::from)?;
    Ok(HttpResponseOk("ok".into()))
}
#[endpoint {method=GET,path="/openapi.json",tags=["system"]}]
async fn schema(request: RequestContext<Arc<Context>>) -> Result<Response<Body>, HttpError> {
    ingress_access(&request)?;
    static_response(
        serde_json::to_string(
            &openapi().map_err(|_| HttpError::for_internal_error("schema unavailable".into()))?,
        )
        .map_err(|_| HttpError::for_internal_error("schema unavailable".into()))?,
        "application/json",
    )
}
fn static_response(
    content: String,
    content_type: &'static str,
) -> Result<Response<Body>, HttpError> {
    Response::builder().header("content-type",content_type).header("cache-control","no-store")
        .header("content-security-policy","default-src 'self'; script-src 'self'; style-src 'self'; object-src 'none'; base-uri 'self'; form-action 'self'; frame-ancestors 'self'")
        .header("x-content-type-options","nosniff").header("referrer-policy","no-referrer").body(Body::from(content)).map_err(|_|HttpError::for_internal_error("response unavailable".into()))
}
fn dashboard(request: &RequestContext<Arc<Context>>) -> Result<Response<Body>, HttpError> {
    ingress_access(request)?;
    let base = if request.context().ingress_peer.is_some() {
        header(request, "x-ingress-path").unwrap_or("")
    } else {
        ""
    };
    if !base.is_empty()
        && (!base.starts_with('/')
            || base.starts_with("//")
            || base.len() > 2048
            || !base
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'/' | b'_' | b'-')))
    {
        return Err(fail(Status::BAD_REQUEST, "invalid ingress prefix"));
    }
    static_response(
        include_str!("../web/index.html").replace(
            "__MAINTENANCE_BASE__",
            &format!("{}/", base.trim_end_matches('/')),
        ),
        "text/html; charset=utf-8",
    )
}
#[endpoint {method=GET,path="/",tags=["web"]}]
async fn index(request: RequestContext<Arc<Context>>) -> Result<Response<Body>, HttpError> {
    dashboard(&request)
}
#[endpoint {method=GET,path="/items/{id}",tags=["web"]}]
async fn detail(
    request: RequestContext<Arc<Context>>,
    _path: Path<ItemPath>,
) -> Result<Response<Body>, HttpError> {
    dashboard(&request)
}
#[endpoint {method=GET,path="/app.js",tags=["web"]}]
async fn script(request: RequestContext<Arc<Context>>) -> Result<Response<Body>, HttpError> {
    ingress_access(&request)?;
    static_response(
        include_str!("../web/app.js").into(),
        "text/javascript; charset=utf-8",
    )
}
#[endpoint {method=GET,path="/style.css",tags=["web"]}]
async fn style(request: RequestContext<Arc<Context>>) -> Result<Response<Body>, HttpError> {
    ingress_access(&request)?;
    static_response(
        include_str!("../web/style.css").into(),
        "text/css; charset=utf-8",
    )
}

pub fn description() -> Result<ApiDescription<Arc<Context>>> {
    let mut api = ApiDescription::new();
    let endpoints: [dropshot::ApiEndpoint<Arc<Context>>; 25] = [
        items_list.into(),
        items_create.into(),
        items_import.into(),
        item_get.into(),
        item_update.into(),
        item_complete.into(),
        item_reorder.into(),
        reorders_list.into(),
        reorder_get.into(),
        reorder_resolve.into(),
        checklist_step_set.into(),
        item_archive.into(),
        item_restore.into(),
        item_events.into(),
        summary.into(),
        system.into(),
        installation.into(),
        installation_check.into(),
        integration_presence.into(),
        health.into(),
        schema.into(),
        index.into(),
        detail.into(),
        script.into(),
        style.into(),
    ];
    for endpoint in endpoints {
        api.register(endpoint).context("register API endpoint")?;
    }
    Ok(api)
}
pub fn openapi() -> Result<serde_json::Value> {
    description()?.openapi("Maintenance API",semver::Version::parse(env!("CARGO_PKG_VERSION"))?).description("Local maintenance definitions, recurring occurrences, completion history and durable reorder requests. Bearer authentication. Creates, completions, reorders and checklist changes require Idempotency-Key; item mutations require the quoted If-Match revision.").json().map_err(Into::into)
}
pub fn start(bind: SocketAddr, context: Arc<Context>) -> Result<HttpServer<Arc<Context>>> {
    start_with_logger(bind, context, crate::logging::logger())
}
pub fn start_with_logger(
    bind: SocketAddr,
    context: Arc<Context>,
    logger: slog::Logger,
) -> Result<HttpServer<Arc<Context>>> {
    HttpServerStarter::new(
        &ConfigDropshot {
            bind_address: bind,
            default_request_body_max_bytes: 512 * 1024,
            default_handler_task_mode: dropshot::HandlerTaskMode::Detached,
            ..Default::default()
        },
        description()?,
        context,
        &logger,
    )
    .map_err(|_| anyhow::anyhow!("start HTTP listener"))
    .map(HttpServerStarter::start)
}
