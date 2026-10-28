use std::{
    io::{BufRead, BufReader, Read, Write},
    net::{SocketAddr, TcpListener},
    process::{Command, Output, Stdio},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread,
    time::Duration,
};

use home_maintenance::{
    api::{self, Context},
    models::{
        Completion, CreateItem, ImportCatalog, Item, Migration, MigrationReport, Page, Reorder,
        ReorderReceipt, Summary,
    },
    store::Store,
};
use serde::{Deserialize, Serialize, de::DeserializeOwned};

const TOKEN: &str = "example-cli-owner-credential-at-least-32-characters";

fn binary(url: &str, args: &[&str], input: &str, token: Option<&str>) -> Output {
    configured_binary(Some(url), args, input, token, &[])
}

fn configured_binary(
    url: Option<&str>,
    args: &[&str],
    input: &str,
    token: Option<&str>,
    variables: &[(&str, &str)],
) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_ha-maintenance"));
    if let Some(url) = url {
        command.args(["--url", url]);
    }
    command
        .args(args)
        .env_remove("MAINTENANCE_API_URL")
        .env_remove("MAINTENANCE_API_TOKEN")
        .env_remove("HOME_ASSISTANT_URL")
        .env_remove("HOME_ASSISTANT_TOKEN")
        .env_remove("HTTP_PROXY")
        .env_remove("HTTPS_PROXY")
        .env_remove("ALL_PROXY")
        .env_remove("http_proxy")
        .env_remove("https_proxy")
        .env_remove("all_proxy")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(token) = token {
        command.env("MAINTENANCE_API_TOKEN", token);
    }
    command.envs(variables.iter().copied());
    let mut child = command.spawn().unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(input.as_bytes())
        .unwrap();
    child.wait_with_output().unwrap()
}

#[derive(Debug, Deserialize)]
struct Envelope<T> {
    status: u16,
    etag: Option<String>,
    body: T,
}
#[derive(Debug, Deserialize)]
struct Diagnostic {
    status: Option<u16>,
    error_code: String,
}
fn success<T: DeserializeOwned>(output: Output, status: u16) -> Envelope<T> {
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output.stderr.is_empty());
    let result: Envelope<T> = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(result.status, status);
    result
}
fn failure(output: Output, status: Option<u16>, code: &str) {
    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    let result: Diagnostic = serde_json::from_slice(&output.stderr).unwrap();
    assert_eq!(result.status, status);
    assert_eq!(result.error_code, code);
    // There is no private error message, URL, body or credential field.
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Exact {
        status: Option<u16>,
        error_code: String,
    }
    let exact: Exact = serde_json::from_slice(&output.stderr).unwrap();
    assert_eq!(exact.status, result.status);
    assert_eq!(exact.error_code, result.error_code);
}
fn json(value: &impl Serialize) -> String {
    serde_json::to_string(value).unwrap()
}

struct Service {
    _directory: tempfile::TempDir,
    store: Store,
    server: dropshot::HttpServer<Arc<Context>>,
    url: String,
}
impl Service {
    async fn new() -> Self {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::open(&directory.path().join("maintenance.sqlite3"))
            .await
            .unwrap();
        let server = api::start(
            SocketAddr::from(([127, 0, 0, 1], 0)),
            Arc::new(Context::new(store.clone(), TOKEN)),
        )
        .unwrap();
        let url = format!("http://{}", server.local_addr());
        Self {
            _directory: directory,
            store,
            server,
            url,
        }
    }
    fn call(&self, args: &[&str], input: &str) -> Output {
        binary(&self.url, args, input, Some(TOKEN))
    }
    async fn finish(self) {
        self.server.close().await.unwrap();
        self.store.close().await.unwrap();
    }
}

fn item() -> CreateItem {
    let catalog: ImportCatalog =
        serde_json::from_str(include_str!("../config/items.example.json")).unwrap();
    let mut item = catalog.items.into_iter().next().unwrap();
    item.id = "example-cli-item".to_owned().try_into().unwrap();
    item.definition.reorder_url = Some("https://example.com/replacement".into());
    item
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn real_binary_preserves_revisions_occurrences_keys_and_durable_state() {
    let service = Service::new().await;
    success::<String>(binary(&service.url, &["health"], "", None), 200);
    failure(
        binary(
            &service.url,
            &["summary"],
            "",
            Some("example-invalid-credential"),
        ),
        Some(401),
        "http_error",
    );
    success::<Summary>(service.call(&["summary"], ""), 200);
    success::<home_maintenance::api::System>(service.call(&["system"], ""), 200);
    success::<home_maintenance::installation::InstallationReport>(
        service.call(&["installation"], ""),
        200,
    );
    success::<home_maintenance::installation::InstallationReport>(
        service.call(&["installation-check"], ""),
        200,
    );
    success::<home_maintenance::home_assistant::RegistryCatalog>(
        service.call(&["registry"], ""),
        200,
    );

    let input = item();
    let mut file = tempfile::NamedTempFile::new().unwrap();
    file.write_all(json(&input).as_bytes()).unwrap();
    let created = success::<Item>(
        service.call(
            &[
                "items",
                "create",
                "--input",
                file.path().to_str().unwrap(),
                "--idempotency-key",
                "example-create",
            ],
            "",
        ),
        201,
    );
    let id = input.id.as_ref();
    let etag = created.etag.unwrap();
    assert_eq!(etag, format!("\"{}\"", created.body.revision));
    let replay = success::<Item>(
        service.call(
            &["items", "create", "--idempotency-key", "example-create"],
            &json(&input),
        ),
        201,
    );
    assert_eq!(replay.body.occurrence.id, created.body.occurrence.id);
    let mut different = input.clone();
    different.definition.summary = "Different deliberate work".into();
    failure(
        service.call(
            &["items", "create", "--idempotency-key", "example-create"],
            &json(&different),
        ),
        Some(409),
        "conflict",
    );

    let fetched = success::<Item>(service.call(&["items", "get", id], ""), 200);
    assert_eq!(fetched.etag.as_ref(), Some(&etag));
    // Missing fields must not become null. An explicit null must clear description.
    let updated = success::<Item>(
        service.call(
            &["items", "update", id, "--if-match", &etag],
            r#"{"description":null}"#,
        ),
        200,
    );
    assert_eq!(updated.body.definition.summary, input.definition.summary);
    assert!(updated.body.definition.description.is_none());
    failure(
        service.call(
            &["items", "update", id, "--if-match", &etag],
            r#"{"summary":"Stale edit"}"#,
        ),
        Some(412),
        "revision_conflict",
    );
    let etag = updated.etag.unwrap();

    let reorder = success::<ReorderReceipt>(
        service.call(
            &[
                "items",
                "reorder",
                id,
                "--if-match",
                &etag,
                "--idempotency-key",
                "example-reorder",
            ],
            "{}",
        ),
        201,
    )
    .body;
    let replay = success::<ReorderReceipt>(
        service.call(
            &[
                "items",
                "reorder",
                id,
                "--if-match",
                &etag,
                "--idempotency-key",
                "example-reorder",
            ],
            "{}",
        ),
        201,
    )
    .body;
    assert_eq!(reorder.id, replay.id);
    let request_id = reorder.id.to_string();
    let reorders = success::<Page<Reorder>>(
        service.call(
            &["reorders", "list", "--status", "pending", "--limit", "1"],
            "",
        ),
        200,
    )
    .body;
    assert_eq!(reorders.items.len(), 1);
    let request = success::<Reorder>(service.call(&["reorders", "get", &request_id], ""), 200);
    let resolved = success::<Reorder>(
        service.call(
            &[
                "reorders",
                "resolve",
                &request_id,
                "--if-match",
                request.etag.as_ref().unwrap(),
            ],
            r#"{"status":"fulfilled"}"#,
        ),
        200,
    );
    assert_eq!(
        resolved.body.status,
        home_maintenance::models::ReorderStatus::Fulfilled
    );

    let current = updated.body;
    let mut etag = etag;
    if let Some(step) = current.occurrence.checklist.first() {
        let body = home_maintenance::models::SetChecklistStep {
            occurrence_id: current.occurrence.id,
            completed: true,
        };
        let checked = success::<Item>(
            service.call(
                &[
                    "items",
                    "checklist",
                    id,
                    &step.id.to_string(),
                    "--if-match",
                    &etag,
                    "--idempotency-key",
                    "example-check",
                ],
                &json(&body),
            ),
            200,
        );
        assert!(checked.body.occurrence.checklist[0].completion.is_some());
        etag = checked.etag.unwrap();
    }
    let complete = home_maintenance::models::CompleteItem {
        occurrence_id: current.occurrence.id,
    };
    let args = [
        "items",
        "complete",
        id,
        "--if-match",
        &etag,
        "--idempotency-key",
        "example-complete",
    ];
    let completed = success::<Item>(service.call(&args, &json(&complete)), 200);
    let replay = success::<Item>(service.call(&args, &json(&complete)), 200);
    assert_eq!(completed.body.revision, replay.body.revision);
    let history = success::<Page<Completion>>(
        service.call(&["items", "events", id, "--limit", "1"], ""),
        200,
    )
    .body;
    assert_eq!(history.items.len(), 1);
    assert_eq!(history.items[0].occurrence_id, Some(current.occurrence.id));

    let archived = success::<Item>(
        service.call(
            &[
                "items",
                "archive",
                id,
                "--if-match",
                completed.etag.as_ref().unwrap(),
            ],
            "",
        ),
        200,
    );
    assert!(archived.body.archived);
    let restored = success::<Item>(
        service.call(
            &[
                "items",
                "restore",
                id,
                "--if-match",
                archived.etag.as_ref().unwrap(),
            ],
            "",
        ),
        200,
    );
    assert!(!restored.body.archived);
    let archived = success::<Item>(
        service.call(
            &[
                "items",
                "archive",
                id,
                "--if-match",
                restored.etag.as_ref().unwrap(),
            ],
            "",
        ),
        200,
    );
    let listed = success::<Page<Item>>(
        service.call(&["items", "list", "--archived", "--limit", "1"], ""),
        200,
    )
    .body;
    assert_eq!(listed.items.len(), 1);
    let deleted = success::<Option<Item>>(
        service.call(
            &[
                "items",
                "delete",
                id,
                "--if-match",
                archived.etag.as_ref().unwrap(),
            ],
            "",
        ),
        204,
    );
    assert!(deleted.body.is_none());
    failure(
        service.call(&["items", "get", id], ""),
        Some(404),
        "not_found",
    );
    // Deletion is durable and did not remove the independent reorder receipt.
    assert!(service.store.get(&input.id).await.is_err());
    success::<Reorder>(service.call(&["reorders", "get", &request_id], ""), 200);
    service.finish().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn real_binary_import_and_migration_keep_source_identity_and_history() {
    let service = Service::new().await;
    let catalog = ImportCatalog {
        items: vec![item()],
    };
    let imported = success::<Page<Item>>(service.call(&["import"], &json(&catalog)), 200).body;
    assert_eq!(imported.items.len(), 1);
    let migration: Migration =
        serde_json::from_str(include_str!("../config/migration.example.json")).unwrap();
    let preview = success::<MigrationReport>(
        service.call(&["migrations", "preview"], &json(&migration)),
        200,
    )
    .body;
    assert!(preview.applicable);
    let applied = success::<MigrationReport>(
        service.call(
            &[
                "migrations",
                "apply",
                "--idempotency-key",
                "example-migrate",
            ],
            &json(&migration),
        ),
        201,
    )
    .body;
    assert_eq!(preview, applied);
    let replay = success::<MigrationReport>(
        service.call(
            &[
                "migrations",
                "apply",
                "--idempotency-key",
                "example-migrate",
            ],
            &json(&migration),
        ),
        201,
    )
    .body;
    assert_eq!(replay, applied);
    for entry in migration.items {
        let item = success::<Item>(
            service.call(&["items", "get", entry.item.id.as_ref()], ""),
            200,
        )
        .body;
        assert_eq!(item.source, Some(entry.source));
        assert_eq!(item.status, entry.status);
        let history = success::<Page<Completion>>(
            service.call(&["items", "events", entry.item.id.as_ref()], ""),
            200,
        )
        .body;
        assert_eq!(history.items.len(), entry.history.len());
    }
    service.finish().await;
}

#[derive(Debug)]
struct Request {
    method: String,
    target: String,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}
impl Request {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, v)| v.as_str())
    }
}
// A real TCP HTTP fixture captures every request, including accidental retries
// or pagination. It deliberately permits malformed responses and redirects.
struct HttpFixture {
    url: String,
    requests: mpsc::Receiver<Request>,
    stop: Arc<AtomicBool>,
    worker: Option<thread::JoinHandle<()>>,
}
impl HttpFixture {
    fn new(status: u16, headers: &str, body: &str) -> Self {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        listener.set_nonblocking(true).unwrap();
        let response = format!(
            "HTTP/1.1 {status} Fixture\r\nConnection: close\r\nContent-Type: application/json\r\nContent-Length: {}\r\n{headers}\r\n{body}",
            body.len()
        );
        let (sender, requests) = mpsc::channel();
        let stop = Arc::new(AtomicBool::new(false));
        let stopped = stop.clone();
        let worker = thread::spawn(move || {
            while !stopped.load(Ordering::Relaxed) {
                let (stream, _) = match listener.accept() {
                    Ok(pair) => pair,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(2));
                        continue;
                    }
                    Err(error) => panic!("fixture listener failed: {error}"),
                };
                // Darwin inherits nonblocking mode from the listening socket.
                stream.set_nonblocking(false).unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(3)))
                    .unwrap();
                let mut reader = BufReader::new(stream);
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
                let parts: Vec<_> = line.split_whitespace().collect();
                let (method, target) = (parts[0].to_owned(), parts[1].to_owned());
                let mut headers = Vec::new();
                loop {
                    line.clear();
                    reader.read_line(&mut line).unwrap();
                    if line == "\r\n" {
                        break;
                    }
                    let (name, value) = line.split_once(':').unwrap();
                    headers.push((name.to_ascii_lowercase(), value.trim().to_owned()));
                }
                let length = headers
                    .iter()
                    .find(|(n, _)| n == "content-length")
                    .map(|(_, v)| v.parse().unwrap())
                    .unwrap_or(0);
                let mut body = vec![0; length];
                reader.read_exact(&mut body).unwrap();
                sender
                    .send(Request {
                        method,
                        target,
                        headers,
                        body,
                    })
                    .unwrap();
                reader.get_mut().write_all(response.as_bytes()).unwrap();
            }
        });
        Self {
            url,
            requests,
            stop,
            worker: Some(worker),
        }
    }
    fn only_request(&self) -> Request {
        let request = self.requests.recv_timeout(Duration::from_secs(3)).unwrap();
        assert!(
            self.requests.try_recv().is_err(),
            "unexpected extra request"
        );
        request
    }
    fn no_requests(&self) {
        assert!(self.requests.try_recv().is_err());
    }
}
impl Drop for HttpFixture {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        let result = self.worker.take().unwrap().join();
        if !thread::panicking() {
            result.unwrap();
        }
    }
}

#[test]
fn binary_sends_exact_auth_revisions_keys_body_and_one_opaque_cursor_page() {
    let fixture = HttpFixture::new(200, "", r#"{"items":[],"next_page":"example-next-page"}"#);
    let result = success::<Page<Item>>(
        binary(
            &fixture.url,
            &[
                "items",
                "list",
                "--archived",
                "--limit",
                "2",
                "--page-token",
                "opaque+/=&?",
            ],
            "",
            Some(TOKEN),
        ),
        200,
    );
    assert_eq!(result.body.next_page.as_deref(), Some("example-next-page"));
    let request = fixture.only_request();
    assert_eq!(request.method, "GET");
    let url = reqwest::Url::parse(&format!("{}{}", fixture.url, request.target)).unwrap();
    let query: std::collections::BTreeMap<_, _> = url.query_pairs().collect();
    assert_eq!(query.get("page_token").unwrap(), "opaque+/=&?");
    assert_eq!(query.get("limit").unwrap(), "2");
    assert_eq!(query.get("archived").unwrap(), "true");
    assert_eq!(
        request.header("authorization"),
        Some(format!("Bearer {TOKEN}").as_str())
    );
    assert!(request.header("idempotency-key").is_none());

    let fixture = HttpFixture::new(
        409,
        "",
        r#"{"error_code":"conflict","message":"example-private-body"}"#,
    );
    failure(
        binary(
            &fixture.url,
            &[
                "items",
                "complete",
                "example-item",
                "--if-match",
                "\"17\"",
                "--idempotency-key",
                "example-exact-key",
            ],
            r#"{"occurrence_id":"00000000-0000-0000-0000-000000000001"}"#,
            Some(TOKEN),
        ),
        Some(409),
        "conflict",
    );
    let request = fixture.only_request();
    assert_eq!(request.method, "POST");
    assert_eq!(request.target, "/v1/items/example-item/completions");
    assert_eq!(request.header("if-match"), Some("\"17\""));
    assert_eq!(request.header("idempotency-key"), Some("example-exact-key"));
    let body: home_maintenance::models::CompleteItem =
        serde_json::from_slice(&request.body).unwrap();
    assert_eq!(body.occurrence_id, uuid::Uuid::from_u128(1));
}

#[test]
fn binary_refuses_redirects_and_never_leaks_untrusted_errors_or_retries() {
    let target = HttpFixture::new(
        200,
        "",
        r#"{"active":0,"overdue":0,"due_soon":0,"completed":0}"#,
    );
    for status in [301, 302, 307, 308, 400, 401, 403, 409, 412, 428, 500, 503] {
        let fixture = HttpFixture::new(
            status,
            &format!("Location: {}/example-private-location\r\n", target.url),
            &format!(
                r#"{{"error_code":"example-private-code","message":"{TOKEN} {} example-private-body"}}"#,
                target.url
            ),
        );
        failure(
            binary(&fixture.url, &["summary"], "", Some(TOKEN)),
            Some(status),
            if status < 400 {
                "redirect_refused"
            } else {
                "http_error"
            },
        );
        fixture.only_request();
    }
    target.no_requests();
    let fixture = HttpFixture::new(500, "", "example-private-non-json-body");
    failure(
        binary(&fixture.url, &["summary"], "", Some(TOKEN)),
        Some(500),
        "http_error",
    );
    fixture.only_request();
    let fixture = HttpFixture::new(200, "", "example-private-malformed-json");
    failure(
        binary(&fixture.url, &["summary"], "", Some(TOKEN)),
        Some(200),
        "invalid_json_response",
    );
    fixture.only_request();
}

#[test]
fn binary_rejects_invalid_inputs_and_arguments_before_network_without_echoing_them() {
    let fixture = HttpFixture::new(200, "", "null");
    for input in [
        "example-private-json",
        r#"{"occurrence_id":"example-private-invalid-uuid"}"#,
        r#"{"occurrence_id":"00000000-0000-0000-0000-000000000001","example-private-extra":true}"#,
    ] {
        failure(
            binary(
                &fixture.url,
                &[
                    "items",
                    "complete",
                    "example-item",
                    "--if-match",
                    "\"1\"",
                    "--idempotency-key",
                    "example-key",
                ],
                input,
                Some(TOKEN),
            ),
            None,
            "invalid_json_input",
        );
    }
    for args in [
        vec![
            "items",
            "update",
            "example-item",
            "--if-match",
            "example-private-invalid-revision",
        ],
        vec!["items", "list", "--limit", "201"],
        vec!["items", "get", "../example-private-path"],
        vec!["items", "create"],
        vec!["example-private-command"],
    ] {
        failure(
            binary(&fixture.url, &args, "{}", Some(TOKEN)),
            None,
            "invalid_arguments",
        );
    }
    failure(
        binary(
            &fixture.url,
            &[
                "items",
                "update",
                "example-item",
                "--if-match",
                "\"1\"",
                "--input",
                "/example-missing-private-path",
            ],
            "",
            Some(TOKEN),
        ),
        None,
        "input_read_failed",
    );
    failure(
        binary(&fixture.url, &["summary"], "", None),
        None,
        "credential_required",
    );
    failure(
        binary(
            &fixture.url,
            &["summary"],
            "",
            Some("example-private-token\n"),
        ),
        None,
        "invalid_credential",
    );
    for url in [
        "not-an-example-url",
        "https://example.com/private-path",
        "https://example.com/?private-query",
        "https://example.com/#private-fragment",
        "https://example:private-password@example.com",
    ] {
        failure(
            binary(url, &["summary"], "", Some(TOKEN)),
            None,
            "invalid_url",
        );
    }
    fixture.no_requests();
}

#[test]
fn binary_uses_token_file_and_explicit_integration_actor_without_exposing_env_in_help() {
    let fixture = HttpFixture::new(204, "", "");
    let mut token = tempfile::NamedTempFile::new().unwrap();
    writeln!(token, "{TOKEN}").unwrap();
    success::<Option<()>>(
        binary(
            &fixture.url,
            &[
                "--token-file",
                token.path().to_str().unwrap(),
                "--system-actor",
                "ha:integration",
                "integration-presence",
            ],
            r#"{"version":"0.10.3","backup":{"status":"unavailable","last_backup":null}}"#,
            Some("example-ignored-env-token"),
        ),
        204,
    );
    let request = fixture.only_request();
    assert_eq!(request.method, "POST");
    assert_eq!(request.target, "/v1/integration/presence");
    assert_eq!(
        request.header("authorization"),
        Some(format!("Bearer {TOKEN}").as_str())
    );
    assert_eq!(
        request.header("x-maintenance-system-actor"),
        Some("ha:integration")
    );
    let output = binary(&fixture.url, &["--help"], "", Some(TOKEN));
    assert!(output.status.success());
    assert!(!String::from_utf8(output.stdout).unwrap().contains(TOKEN));
    assert!(output.stderr.is_empty());
    fixture.no_requests();
}

#[test]
fn binary_deadline_and_connection_failures_are_bounded_and_do_not_retry() {
    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let worker = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        // The request is accepted but no response arrives before the deadline.
        stream
            .set_read_timeout(Some(Duration::from_secs(3)))
            .unwrap();
        let mut bytes = [0; 4096];
        assert!(stream.read(&mut bytes).unwrap() > 0);
        thread::sleep(Duration::from_millis(1500));
        listener.set_nonblocking(true).unwrap();
        assert!(listener.accept().is_err(), "unexpected retry after timeout");
    });
    failure(
        binary(&url, &["--timeout", "1", "summary"], "", Some(TOKEN)),
        None,
        "request_failed",
    );
    worker.join().unwrap();
    // This port has been closed, so no fixture or live service receives a call.
    failure(
        binary(&url, &["summary"], "", Some(TOKEN)),
        None,
        "request_failed",
    );
}

#[test]
fn home_assistant_transport_selects_its_own_origin_and_credential_without_fallback() {
    let ignored = HttpFixture::new(500, "", "null");
    let ha = HttpFixture::new(200, "", r#"{"items":[],"next_page":"example-ha-cursor"}"#);
    let page = success::<Page<Item>>(
        configured_binary(
            None,
            &[
                "--transport",
                "home-assistant",
                "items",
                "list",
                "--limit",
                "2",
                "--page-token",
                "opaque+/=&?",
            ],
            "",
            Some(TOKEN),
            &[
                ("MAINTENANCE_API_URL", &ignored.url),
                ("HOME_ASSISTANT_URL", &ha.url),
                ("HOME_ASSISTANT_TOKEN", "example-home-assistant-token"),
            ],
        ),
        200,
    );
    assert_eq!(page.body.next_page.as_deref(), Some("example-ha-cursor"));
    let request = ha.only_request();
    assert_eq!(request.method, "GET");
    let url = reqwest::Url::parse(&format!("{}{}", ha.url, request.target)).unwrap();
    assert_eq!(url.path(), "/api/home_maintenance/v1/items");
    let query: std::collections::BTreeMap<_, _> = url.query_pairs().collect();
    assert_eq!(query.get("page_token").unwrap(), "opaque+/=&?");
    assert_eq!(query.get("limit").unwrap(), "2");
    assert_eq!(
        request.header("authorization"),
        Some("Bearer example-home-assistant-token")
    );
    assert!(request.header("x-maintenance-system-actor").is_none());
    assert!(request.header("x-remote-user-id").is_none());
    ignored.no_requests();

    let native = HttpFixture::new(
        200,
        "",
        r#"{"active":1,"overdue":0,"due_soon":0,"completed":0}"#,
    );
    let summary = success::<Summary>(
        configured_binary(
            None,
            &["summary"],
            "",
            Some(TOKEN),
            &[
                ("MAINTENANCE_API_URL", &native.url),
                ("HOME_ASSISTANT_URL", &ignored.url),
                ("HOME_ASSISTANT_TOKEN", "example-home-assistant-token"),
            ],
        ),
        200,
    );
    assert_eq!(summary.body.active, 1);
    let request = native.only_request();
    assert_eq!(request.target, "/v1/summary");
    assert_eq!(
        request.header("authorization"),
        Some(format!("Bearer {TOKEN}").as_str())
    );
    ignored.no_requests();

    // The selected transport never borrows the other transport's credential.
    failure(
        configured_binary(
            Some(&ignored.url),
            &["--transport", "home-assistant", "summary"],
            "",
            Some(TOKEN),
            &[],
        ),
        None,
        "credential_required",
    );
    failure(
        configured_binary(
            Some(&ignored.url),
            &["summary"],
            "",
            None,
            &[("HOME_ASSISTANT_TOKEN", "example-home-assistant-token")],
        ),
        None,
        "credential_required",
    );
    failure(
        configured_binary(
            None,
            &["--transport", "home-assistant", "summary"],
            "",
            Some(TOKEN),
            &[("MAINTENANCE_API_URL", &ignored.url)],
        ),
        None,
        "url_required",
    );
    ignored.no_requests();
}

#[test]
fn home_assistant_mutations_preserve_headers_and_native_only_commands_make_zero_calls() {
    let fixture = HttpFixture::new(
        412,
        "",
        r#"{"error_code":"revision_conflict","message":"example-private-conflict"}"#,
    );
    failure(
        configured_binary(
            Some(&fixture.url),
            &[
                "--transport",
                "home-assistant",
                "items",
                "complete",
                "example-item",
                "--if-match",
                "\"27\"",
                "--idempotency-key",
                "example-ha-key",
            ],
            r#"{"occurrence_id":"00000000-0000-0000-0000-000000000001"}"#,
            Some(TOKEN),
            &[("HOME_ASSISTANT_TOKEN", "example-home-assistant-token")],
        ),
        Some(412),
        "revision_conflict",
    );
    let request = fixture.only_request();
    assert_eq!(request.method, "POST");
    assert_eq!(
        request.target,
        "/api/home_maintenance/v1/items/example-item/completions"
    );
    assert_eq!(request.header("if-match"), Some("\"27\""));
    assert_eq!(request.header("idempotency-key"), Some("example-ha-key"));
    assert_eq!(
        request.header("authorization"),
        Some("Bearer example-home-assistant-token")
    );
    let body: home_maintenance::models::CompleteItem =
        serde_json::from_slice(&request.body).unwrap();
    assert_eq!(body.occurrence_id, uuid::Uuid::from_u128(1));

    let unused = HttpFixture::new(500, "", "null");
    for command in [
        vec!["health"],
        vec!["import"],
        vec!["installation"],
        vec!["installation-check"],
        vec!["integration-presence"],
        vec!["integration-registry"],
        vec!["migrations", "preview"],
        vec![
            "migrations",
            "apply",
            "--idempotency-key",
            "example-migration-key",
        ],
    ] {
        let mut args = vec!["--transport", "home-assistant"];
        args.extend(command);
        // Native-only rejection precedes credential and input loading.
        failure(
            binary(&unused.url, &args, "example-private-invalid-input", None),
            None,
            "unsupported_transport_operation",
        );
    }
    failure(
        binary(
            &unused.url,
            &[
                "--transport",
                "home-assistant",
                "--system-actor",
                "ha:integration",
                "summary",
            ],
            "",
            None,
        ),
        None,
        "unsupported_transport_actor",
    );
    unused.no_requests();
}
