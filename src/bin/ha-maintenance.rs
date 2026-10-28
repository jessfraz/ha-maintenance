use std::{
    io::{Read, Write},
    path::PathBuf,
    process::ExitCode,
    time::Duration,
};

use clap::{Args, Parser, Subcommand, ValueEnum};
use home_maintenance::{
    api::System,
    home_assistant::{RegistryCatalog, RegistryReport},
    installation::{InstallationReport, PresenceReport},
    models::{
        CompleteItem, Completion, CreateItem, ImportCatalog, Item, ItemId, Migration,
        MigrationReport, Page, Reorder, ReorderItem, ReorderReceipt, ResolveReorder,
        SetChecklistStep, Summary, UpdateItem,
    },
};
use reqwest::{
    Client, Method, RequestBuilder, Url,
    header::{AUTHORIZATION, HeaderValue},
};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use uuid::Uuid;

#[derive(Parser)]
#[command(
    name = "ha-maintenance",
    version,
    about = "One-request JSON client for the Maintenance API"
)]
struct Cli {
    /// Native uses MAINTENANCE_API_URL/TOKEN; HA uses HOME_ASSISTANT_URL/TOKEN.
    #[arg(long, value_enum, default_value = "native", global = true)]
    transport: Transport,
    /// Override the selected transport's origin. No path, query, or credentials.
    #[arg(long, global = true)]
    url: Option<String>,
    /// Read this credential file instead of the selected transport's token env.
    #[arg(long, global = true)]
    token_file: Option<PathBuf>,
    /// Optional trusted integration identity. Does not grant administrator access.
    #[arg(long, value_enum, global = true)]
    system_actor: Option<SystemActor>,
    /// Total request deadline, including the response body (seconds).
    #[arg(long, default_value = "30", value_parser = clap::value_parser!(u64).range(1..=300), global = true)]
    timeout: u64,
    #[command(subcommand)]
    command: Command,
}

#[derive(Clone, Copy, PartialEq, Eq, ValueEnum)]
enum Transport {
    Native,
    HomeAssistant,
}
impl Transport {
    fn url_env(self) -> &'static str {
        match self {
            Self::Native => "MAINTENANCE_API_URL",
            Self::HomeAssistant => "HOME_ASSISTANT_URL",
        }
    }
    fn token_env(self) -> &'static str {
        match self {
            Self::Native => "MAINTENANCE_API_TOKEN",
            Self::HomeAssistant => "HOME_ASSISTANT_TOKEN",
        }
    }
}

#[derive(Clone, Copy, ValueEnum)]
enum SystemActor {
    #[value(name = "ha:integration")]
    Integration,
    #[value(name = "ha:automation")]
    Automation,
}
impl SystemActor {
    fn as_str(self) -> &'static str {
        match self {
            Self::Integration => "ha:integration",
            Self::Automation => "ha:automation",
        }
    }
}

#[derive(Subcommand)]
enum Command {
    /// Unauthenticated readiness check.
    Health,
    Summary,
    System,
    Registry,
    Installation,
    InstallationCheck,
    /// Dedicated integration credential required.
    IntegrationPresence(Input),
    /// Dedicated integration system credential required.
    IntegrationRegistry(Input),
    #[command(subcommand)]
    Items(Items),
    Import(Input),
    #[command(subcommand)]
    Migrations(Migrations),
    #[command(subcommand)]
    Reorders(Reorders),
}

impl Command {
    fn supports_home_assistant(&self) -> bool {
        // custom_components/home_maintenance/http.py exposes only this surface.
        // Health, imports, migrations and adapter reports stay native-only.
        matches!(
            self,
            Self::Summary | Self::System | Self::Registry | Self::Items(_) | Self::Reorders(_)
        )
    }
}

#[derive(Args)]
struct Input {
    /// JSON file, or - for stdin. Decoded using the server's request type.
    #[arg(long, default_value = "-")]
    input: PathBuf,
}
impl Input {
    fn read<T: DeserializeOwned>(&self) -> Result<T, Failure> {
        let bytes = if self.input.as_os_str() == "-" {
            let mut bytes = Vec::new();
            std::io::stdin()
                .read_to_end(&mut bytes)
                .map_err(|_| Failure::local("input_read_failed"))?;
            bytes
        } else {
            std::fs::read(&self.input).map_err(|_| Failure::local("input_read_failed"))?
        };
        // Parse errors can quote private input. Never forward them to stderr.
        serde_json::from_slice(&bytes).map_err(|_| Failure::local("invalid_json_input"))
    }
}

#[derive(Args)]
struct Revision {
    /// Exact quoted ETag captured by the caller, e.g. '"7"'. Never refreshed automatically.
    #[arg(long, value_parser = parse_revision)]
    if_match: String,
}
fn parse_revision(value: &str) -> Result<String, &'static str> {
    value
        .strip_prefix('"')
        .and_then(|v| v.strip_suffix('"'))
        .and_then(|v| v.parse::<i64>().ok())
        .filter(|v| *v > 0)
        .map(|_| value.to_owned())
        .ok_or("expected a quoted positive revision")
}

#[derive(Args)]
struct Key {
    /// Caller-owned stable key. Preserve it with the body and revision for uncertain outcomes.
    #[arg(long, value_parser = parse_key)]
    idempotency_key: String,
}
fn parse_key(value: &str) -> Result<String, &'static str> {
    if value.is_empty() || value.len() > 200 || !value.bytes().all(|b| b.is_ascii_graphic()) {
        Err("expected 1..200 printable ASCII characters without whitespace")
    } else {
        Ok(value.to_owned())
    }
}

#[derive(Args)]
struct ItemPath {
    #[arg(value_parser = parse_item_id)]
    id: ItemId,
}
fn parse_item_id(value: &str) -> Result<ItemId, &'static str> {
    ItemId::try_from(value.to_owned())
}

#[derive(Args)]
struct PageArgs {
    #[arg(long, value_parser = clap::value_parser!(u32).range(1..=200))]
    limit: Option<u32>,
    /// Opaque cursor from the previous response. Only this page is fetched.
    #[arg(long)]
    page_token: Option<String>,
}
impl PageArgs {
    fn apply(&self, mut request: RequestBuilder) -> RequestBuilder {
        if let Some(limit) = self.limit {
            request = request.query(&[("limit", limit)]);
        }
        if let Some(token) = &self.page_token {
            request = request.query(&[("page_token", token)]);
        }
        request
    }
}

#[derive(Subcommand)]
enum Items {
    List {
        #[arg(long)]
        archived: bool,
        #[command(flatten)]
        page: PageArgs,
    },
    Get(ItemPath),
    Create {
        #[command(flatten)]
        input: Input,
        #[command(flatten)]
        key: Key,
    },
    Update {
        #[command(flatten)]
        item: ItemPath,
        #[command(flatten)]
        revision: Revision,
        #[command(flatten)]
        input: Input,
    },
    Complete {
        #[command(flatten)]
        item: ItemPath,
        #[command(flatten)]
        revision: Revision,
        #[command(flatten)]
        key: Key,
        #[command(flatten)]
        input: Input,
    },
    Checklist {
        #[command(flatten)]
        item: ItemPath,
        step_id: Uuid,
        #[command(flatten)]
        revision: Revision,
        #[command(flatten)]
        key: Key,
        #[command(flatten)]
        input: Input,
    },
    Reorder {
        #[command(flatten)]
        item: ItemPath,
        #[command(flatten)]
        revision: Revision,
        #[command(flatten)]
        key: Key,
        #[command(flatten)]
        input: Input,
    },
    Events {
        #[command(flatten)]
        item: ItemPath,
        #[command(flatten)]
        page: PageArgs,
    },
    Archive {
        #[command(flatten)]
        item: ItemPath,
        #[command(flatten)]
        revision: Revision,
    },
    Restore {
        #[command(flatten)]
        item: ItemPath,
        #[command(flatten)]
        revision: Revision,
    },
    /// Permanently delete an archived item and its history.
    Delete {
        #[command(flatten)]
        item: ItemPath,
        #[command(flatten)]
        revision: Revision,
    },
}
#[derive(Subcommand)]
enum Migrations {
    Preview(Input),
    Apply {
        #[command(flatten)]
        input: Input,
        #[command(flatten)]
        key: Key,
    },
}
#[derive(Subcommand)]
enum Reorders {
    List {
        #[arg(long, value_enum)]
        status: Option<Status>,
        #[command(flatten)]
        page: PageArgs,
    },
    Get {
        id: Uuid,
    },
    Resolve {
        id: Uuid,
        #[command(flatten)]
        revision: Revision,
        #[command(flatten)]
        input: Input,
    },
}
#[derive(Clone, Copy, ValueEnum)]
enum Status {
    Pending,
    Fulfilled,
    Dismissed,
}
impl Status {
    fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Fulfilled => "fulfilled",
            Self::Dismissed => "dismissed",
        }
    }
}

#[derive(Serialize)]
struct Output<T> {
    status: u16,
    etag: Option<String>,
    body: T,
}
#[derive(Serialize)]
struct Failure {
    status: Option<u16>,
    error_code: &'static str,
}
impl Failure {
    fn local(error_code: &'static str) -> Self {
        Self {
            status: None,
            error_code,
        }
    }
    fn http(status: u16, error_code: &'static str) -> Self {
        Self {
            status: Some(status),
            error_code,
        }
    }
}

struct Api {
    client: Client,
    origin: Url,
    authorization: Option<HeaderValue>,
    transport: Transport,
    system_actor: Option<SystemActor>,
}
impl Api {
    fn new(cli: &Cli) -> Result<Self, Failure> {
        if cli.transport == Transport::HomeAssistant {
            if !cli.command.supports_home_assistant() {
                return Err(Failure::local("unsupported_transport_operation"));
            }
            if cli.system_actor.is_some() {
                return Err(Failure::local("unsupported_transport_actor"));
            }
        }
        let configured_url = cli
            .url
            .clone()
            .or_else(|| std::env::var(cli.transport.url_env()).ok())
            .ok_or_else(|| Failure::local("url_required"))?;
        let origin = Url::parse(&configured_url).map_err(|_| Failure::local("invalid_url"))?;
        if !matches!(origin.scheme(), "http" | "https")
            || origin.host_str().is_none()
            || !origin.username().is_empty()
            || origin.password().is_some()
            || origin.path() != "/"
            || origin.query().is_some()
            || origin.fragment().is_some()
        {
            return Err(Failure::local("invalid_url"));
        }
        let authorization = if matches!(cli.command, Command::Health) {
            None
        } else {
            let token = if let Some(path) = &cli.token_file {
                std::fs::read_to_string(path)
                    .map_err(|_| Failure::local("credential_read_failed"))?
                    .trim_end_matches(['\r', '\n'])
                    .to_owned()
            } else {
                std::env::var(cli.transport.token_env())
                    .map_err(|_| Failure::local("credential_required"))?
            };
            if token.is_empty()
                || (cli.transport == Transport::Native && token.len() > 256)
                || !token.bytes().all(|b| b.is_ascii_graphic())
            {
                return Err(Failure::local("invalid_credential"));
            }
            let mut header = HeaderValue::from_str(&format!("Bearer {token}"))
                .map_err(|_| Failure::local("invalid_credential"))?;
            header.set_sensitive(true);
            Some(header)
        };
        let client = Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .retry(reqwest::retry::never())
            .timeout(Duration::from_secs(cli.timeout))
            .build()
            .map_err(|_| Failure::local("client_setup_failed"))?;
        Ok(Self {
            client,
            origin,
            authorization,
            transport: cli.transport,
            system_actor: cli.system_actor,
        })
    }
    fn request(&self, method: Method, path: &str) -> Result<RequestBuilder, Failure> {
        let path = match self.transport {
            Transport::Native => path.to_owned(),
            Transport::HomeAssistant => format!("/api/home_maintenance{path}"),
        };
        let url = self
            .origin
            .join(&path)
            .map_err(|_| Failure::local("invalid_path"))?;
        let mut request = self.client.request(method, url);
        if let Some(auth) = &self.authorization {
            request = request.header(AUTHORIZATION, auth.clone());
        }
        if let Some(actor) = self.system_actor {
            request = request.header("X-Maintenance-System-Actor", actor.as_str());
        }
        Ok(request)
    }
}
fn revision(request: RequestBuilder, revision: Revision) -> RequestBuilder {
    request.header("If-Match", revision.if_match)
}
fn keyed(request: RequestBuilder, key: Key) -> RequestBuilder {
    request.header("Idempotency-Key", key.idempotency_key)
}
fn body<T: DeserializeOwned + Serialize>(
    request: RequestBuilder,
    input: Input,
) -> Result<RequestBuilder, Failure> {
    Ok(request.json(&input.read::<T>()?))
}

async fn send<T: DeserializeOwned + Serialize>(request: RequestBuilder) -> Result<(), Failure> {
    let response = request
        .send()
        .await
        .map_err(|_| Failure::local("request_failed"))?;
    let status = response.status().as_u16();
    if !response.status().is_success() {
        // Only known server codes may cross this boundary. Messages, request IDs,
        // redirects and arbitrary error_code strings can contain private data.
        #[derive(Deserialize)]
        struct ApiError {
            error_code: Option<String>,
        }
        let code = response
            .json::<ApiError>()
            .await
            .ok()
            .and_then(|e| e.error_code);
        let error_code = match code.as_deref() {
            Some("invalid_input") => "invalid_input",
            Some("owner_required") => "owner_required",
            Some("not_found") => "not_found",
            Some("item_deleted") => "item_deleted",
            Some("conflict") => "conflict",
            Some("revision_conflict") => "revision_conflict",
            _ if (300..400).contains(&status) => "redirect_refused",
            _ => "http_error",
        };
        return Err(Failure::http(status, error_code));
    }
    let etag = response
        .headers()
        .get("etag")
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);
    let result = if status == 204 {
        None
    } else {
        Some(
            response
                .json::<T>()
                .await
                .map_err(|_| Failure::http(status, "invalid_json_response"))?,
        )
    };
    write_json(
        std::io::stdout().lock(),
        &Output {
            status,
            etag,
            body: result,
        },
    )
}
fn write_json(writer: impl Write, value: &impl Serialize) -> Result<(), Failure> {
    let mut writer = writer;
    serde_json::to_writer(&mut writer, value).map_err(|_| Failure::local("output_failed"))?;
    writer
        .write_all(b"\n")
        .map_err(|_| Failure::local("output_failed"))
}

async fn run(cli: Cli) -> Result<(), Failure> {
    let api = Api::new(&cli)?;
    match cli.command {
        Command::Health => send::<String>(api.request(Method::GET, "/health")?).await,
        Command::Summary => send::<Summary>(api.request(Method::GET, "/v1/summary")?).await,
        Command::System => send::<System>(api.request(Method::GET, "/v1/system")?).await,
        Command::Registry => {
            send::<RegistryCatalog>(api.request(Method::GET, "/v1/home-assistant/registry")?).await
        }
        Command::Installation => {
            send::<InstallationReport>(api.request(Method::GET, "/v1/installation")?).await
        }
        Command::InstallationCheck => {
            send::<InstallationReport>(api.request(Method::POST, "/v1/installation/check")?).await
        }
        Command::IntegrationPresence(input) => {
            send::<()>(body::<PresenceReport>(
                api.request(Method::POST, "/v1/integration/presence")?,
                input,
            )?)
            .await
        }
        Command::IntegrationRegistry(input) => {
            send::<()>(body::<RegistryReport>(
                api.request(Method::PUT, "/v1/integration/registry")?,
                input,
            )?)
            .await
        }
        Command::Import(input) => {
            send::<Page<Item>>(body::<ImportCatalog>(
                api.request(Method::POST, "/v1/imports")?,
                input,
            )?)
            .await
        }
        Command::Migrations(Migrations::Preview(input)) => {
            send::<MigrationReport>(body::<Migration>(
                api.request(Method::POST, "/v1/migrations/preview")?,
                input,
            )?)
            .await
        }
        Command::Migrations(Migrations::Apply { input, key }) => {
            send::<MigrationReport>(body::<Migration>(
                keyed(api.request(Method::POST, "/v1/migrations")?, key),
                input,
            )?)
            .await
        }
        Command::Reorders(command) => match command {
            Reorders::List { status, page } => {
                let mut request = api.request(Method::GET, "/v1/reorders")?;
                if let Some(status) = status {
                    request = request.query(&[("status", status.as_str())]);
                }
                send::<Page<Reorder>>(page.apply(request)).await
            }
            Reorders::Get { id } => {
                send::<Reorder>(api.request(Method::GET, &format!("/v1/reorders/{id}"))?).await
            }
            Reorders::Resolve {
                id,
                revision: rev,
                input,
            } => {
                send::<Reorder>(body::<ResolveReorder>(
                    revision(
                        api.request(Method::PUT, &format!("/v1/reorders/{id}/resolution"))?,
                        rev,
                    ),
                    input,
                )?)
                .await
            }
        },
        Command::Items(command) => match command {
            Items::List { archived, page } => {
                send::<Page<Item>>(
                    page.apply(
                        api.request(Method::GET, "/v1/items")?
                            .query(&[("archived", archived)]),
                    ),
                )
                .await
            }
            Items::Create { input, key } => {
                send::<Item>(body::<CreateItem>(
                    keyed(api.request(Method::POST, "/v1/items")?, key),
                    input,
                )?)
                .await
            }
            Items::Get(item) => {
                send::<Item>(api.request(Method::GET, &format!("/v1/items/{}", item.id.as_ref()))?)
                    .await
            }
            Items::Update {
                item,
                revision: rev,
                input,
            } => {
                send::<Item>(body::<UpdateItem>(
                    revision(
                        api.request(Method::PATCH, &format!("/v1/items/{}", item.id.as_ref()))?,
                        rev,
                    ),
                    input,
                )?)
                .await
            }
            Items::Complete {
                item,
                revision: rev,
                key,
                input,
            } => {
                send::<Item>(body::<CompleteItem>(
                    keyed(
                        revision(
                            api.request(
                                Method::POST,
                                &format!("/v1/items/{}/completions", item.id.as_ref()),
                            )?,
                            rev,
                        ),
                        key,
                    ),
                    input,
                )?)
                .await
            }
            Items::Checklist {
                item,
                step_id,
                revision: rev,
                key,
                input,
            } => {
                send::<Item>(body::<SetChecklistStep>(
                    keyed(
                        revision(
                            api.request(
                                Method::PUT,
                                &format!("/v1/items/{}/checklist/{step_id}", item.id.as_ref()),
                            )?,
                            rev,
                        ),
                        key,
                    ),
                    input,
                )?)
                .await
            }
            Items::Reorder {
                item,
                revision: rev,
                key,
                input,
            } => {
                send::<ReorderReceipt>(body::<ReorderItem>(
                    keyed(
                        revision(
                            api.request(
                                Method::POST,
                                &format!("/v1/items/{}/reorders", item.id.as_ref()),
                            )?,
                            rev,
                        ),
                        key,
                    ),
                    input,
                )?)
                .await
            }
            Items::Events { item, page } => {
                send::<Page<Completion>>(page.apply(api.request(
                    Method::GET,
                    &format!("/v1/items/{}/events", item.id.as_ref()),
                )?))
                .await
            }
            Items::Archive {
                item,
                revision: rev,
            } => {
                send::<Item>(revision(
                    api.request(
                        Method::PUT,
                        &format!("/v1/items/{}/archive", item.id.as_ref()),
                    )?,
                    rev,
                ))
                .await
            }
            Items::Restore {
                item,
                revision: rev,
            } => {
                send::<Item>(revision(
                    api.request(
                        Method::DELETE,
                        &format!("/v1/items/{}/archive", item.id.as_ref()),
                    )?,
                    rev,
                ))
                .await
            }
            Items::Delete {
                item,
                revision: rev,
            } => {
                send::<()>(revision(
                    api.request(Method::DELETE, &format!("/v1/items/{}", item.id.as_ref()))?,
                    rev,
                ))
                .await
            }
        },
    }
}

#[tokio::main]
async fn main() -> ExitCode {
    // Clap's ordinary errors quote argument values. Only help/version are safe
    // to print verbatim; invalid args get the same bounded JSON diagnostics.
    let cli = match Cli::try_parse() {
        Ok(cli) => cli,
        Err(error)
            if matches!(
                error.kind(),
                clap::error::ErrorKind::DisplayHelp | clap::error::ErrorKind::DisplayVersion
            ) =>
        {
            return if error.print().is_ok() {
                ExitCode::SUCCESS
            } else {
                ExitCode::FAILURE
            };
        }
        Err(_) => {
            let _ = write_json(
                std::io::stderr().lock(),
                &Failure::local("invalid_arguments"),
            );
            return ExitCode::from(2);
        }
    };
    match run(cli).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            let _ = write_json(std::io::stderr().lock(), &error);
            ExitCode::FAILURE
        }
    }
}
