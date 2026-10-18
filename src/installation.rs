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
#[derive(Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PresenceReport {
    pub version: String,
    pub backup: BackupReport,
    #[serde(default)]
    pub owner_id: Option<String>,
}
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct InstallationReport {
    pub version: String,
    pub integration_connected: bool,
    pub backup: BackupReport,
}
struct Presence {
    at: i64,
    backup: BackupReport,
    owner_id: Option<String>,
}
#[derive(Default)]
pub struct Installation {
    presence: RwLock<Option<Presence>>,
}
impl Installation {
    pub async fn report(&self) -> InstallationReport {
        let presence = self.presence.read().await;
        let connected = presence
            .as_ref()
            .is_some_and(|presence| Utc::now().timestamp().saturating_sub(presence.at) < 180);
        InstallationReport {
            version: env!("CARGO_PKG_VERSION").into(),
            integration_connected: connected,
            backup: if connected {
                presence
                    .as_ref()
                    .map(|presence| presence.backup.clone())
                    .unwrap_or_default()
            } else {
                BackupReport::default()
            },
        }
    }
    pub async fn is_owner(&self, id: &str) -> bool {
        self.presence.read().await.as_ref().is_some_and(|presence| {
            Utc::now().timestamp().saturating_sub(presence.at) < 180
                && presence.owner_id.as_deref() == Some(id)
        })
    }
    pub async fn record(&self, report: PresenceReport) -> Result<(), crate::error::Error> {
        if report
            .owner_id
            .as_ref()
            .is_some_and(|id| id.is_empty() || id.len() > 197 || id.chars().any(char::is_control))
            || report.version.len() > 40
            || semver::Version::parse(&report.version).is_err()
            || report
                .backup
                .last_backup
                .is_some_and(|at| at < 0 || at > Utc::now().timestamp() + 60)
        {
            return Err(crate::error::Error::Invalid("invalid integration report"));
        }
        *self.presence.write().await = Some(Presence {
            at: Utc::now().timestamp(),
            backup: report.backup,
            owner_id: report.owner_id,
        });
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use crate::installation::{BackupReport, Installation, Presence, PresenceReport};
    use chrono::Utc;

    #[tokio::test]
    async fn instruction_owner_expires_with_the_integration_presence_lease() {
        let installation = Installation::default();
        assert!(!installation.is_owner("example-owner").await);
        installation
            .record(PresenceReport {
                version: env!("CARGO_PKG_VERSION").into(),
                backup: BackupReport::default(),
                owner_id: Some("example-owner".into()),
            })
            .await
            .unwrap();
        assert!(installation.is_owner("example-owner").await);
        assert!(!installation.is_owner("example-admin").await);
        *installation.presence.write().await = Some(Presence {
            at: Utc::now().timestamp() - 180,
            backup: BackupReport::default(),
            owner_id: Some("example-owner".into()),
        });
        assert!(!installation.is_owner("example-owner").await);
        assert!(!installation.report().await.integration_connected);
        let legacy: PresenceReport = serde_json::from_str(
            r#"{"version":"0.3.0","backup":{"status":"unavailable","last_backup":null}}"#,
        )
        .unwrap();
        installation.record(legacy).await.unwrap();
        assert!(!installation.is_owner("example-owner").await);
    }
}
