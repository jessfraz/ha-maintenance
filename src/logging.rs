//! Dropshot attaches raw request URIs and errors to log records. Only the fixed
//! operational vocabulary below may leave the process, including on failures.
use serde::Serialize;
use slog::{Drain, KV};
use std::{fmt, io::Write, sync::Mutex};
use uuid::Uuid;

pub fn logger() -> slog::Logger {
    with_writer(std::io::stderr())
}
pub fn with_writer(writer: impl Write + Send + 'static) -> slog::Logger {
    slog::Logger::root(SafeLog(Mutex::new(writer)), slog::o!())
}
struct SafeLog<W>(Mutex<W>);
#[derive(Default, Serialize)]
struct Fields {
    #[serde(skip_serializing_if = "Option::is_none")]
    request_id: Option<Uuid>,
    #[serde(skip_serializing_if = "Option::is_none")]
    method: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    status: Option<u16>,
}
#[derive(Serialize)]
struct Entry {
    timestamp: chrono::DateTime<chrono::Utc>,
    event: &'static str,
    #[serde(flatten)]
    fields: Fields,
}
impl<W: Write + Send> Drain for SafeLog<W> {
    type Ok = ();
    type Err = slog::Never;
    fn log(&self, record: &slog::Record<'_>, values: &slog::OwnedKVList) -> Result<(), Self::Err> {
        let event = match record.msg().as_str() {
            Some("request completed") => "request_completed",
            Some("listening") => "server_started",
            Some("beginning graceful shutdown") => "server_stopping",
            Some("integration_discovery_failed") => "integration_discovery_failed",
            Some("accept error") => "accept_failed",
            _ => return Ok(()),
        };
        let mut fields = Fields::default();
        let _ = values.serialize(record, &mut fields);
        let _ = record.kv().serialize(record, &mut fields);
        if fields.method == Some("GET") && fields.status.is_some_and(|status| status < 400) {
            return Ok(());
        }
        if let Ok(mut line) = serde_json::to_vec(&Entry {
            timestamp: chrono::Utc::now(),
            event,
            fields,
        }) {
            line.push(b'\n');
            if let Ok(mut writer) = self.0.lock() {
                let _ = writer.write_all(&line);
            }
        }
        Ok(())
    }
}
impl slog::Serializer for Fields {
    fn emit_arguments(&mut self, key: slog::Key, value: &fmt::Arguments<'_>) -> slog::Result {
        match key.as_str() {
            "req_id" | "request_id" => self.request_id = Uuid::parse_str(&value.to_string()).ok(),
            "method" => {
                self.method = match value.to_string().as_str() {
                    "GET" => Some("GET"),
                    "POST" => Some("POST"),
                    "PATCH" => Some("PATCH"),
                    "PUT" => Some("PUT"),
                    "DELETE" => Some("DELETE"),
                    _ => None,
                }
            }
            "response_code" => {
                self.status = value
                    .to_string()
                    .parse()
                    .ok()
                    .filter(|status| (100..=599).contains(status))
            }
            _ => {}
        }
        Ok(())
    }
}
