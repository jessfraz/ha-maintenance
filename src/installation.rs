use chrono::Utc;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use tokio::sync::RwLock;

#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum BackupStatus {
    Complete,
    AppOnly,
    Missing,
    Outdated,
    #[default]
    Unavailable,
    Standalone,
}
#[derive(Clone, Debug, Default, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct BackupReport {
    pub status: BackupStatus,
    pub last_backup: Option<i64>,
}
#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PresenceReport {
    pub version: String,
    pub backup: BackupReport,
}
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct InstallationReport {
    pub version: String,
    pub integration_connected: bool,
    pub backup: BackupReport,
}
#[derive(Default)]
pub struct Installation {
    presence: RwLock<Option<(i64, BackupReport)>>,
}
impl Installation {
    pub async fn report(&self) -> InstallationReport {
        let presence = self.presence.read().await;
        let connected = presence
            .as_ref()
            .is_some_and(|(at, _)| Utc::now().timestamp().saturating_sub(*at) < 180);
        InstallationReport {
            version: env!("CARGO_PKG_VERSION").into(),
            integration_connected: connected,
            backup: if connected {
                presence
                    .as_ref()
                    .map(|(_, backup)| backup.clone())
                    .unwrap_or_default()
            } else {
                BackupReport::default()
            },
        }
    }
    pub async fn record(&self, report: PresenceReport) -> Result<(), crate::error::Error> {
        if report.version.len() > 40
            || semver::Version::parse(&report.version).is_err()
            || report
                .backup
                .last_backup
                .is_some_and(|at| at < 0 || at > Utc::now().timestamp() + 60)
        {
            return Err(crate::error::Error::Invalid("invalid integration report"));
        }
        *self.presence.write().await = Some((Utc::now().timestamp(), report.backup));
        Ok(())
    }
}
