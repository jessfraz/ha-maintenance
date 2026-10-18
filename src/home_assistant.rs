use std::collections::BTreeSet;

use chrono::Utc;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use tokio::sync::RwLock;

use crate::error::Error;

/// Registry identities are opaque and never normalized or inferred from names.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, JsonSchema)]
#[serde(try_from = "String", into = "String")]
pub struct RegistryId(String);
impl TryFrom<String> for RegistryId {
    type Error = &'static str;
    fn try_from(value: String) -> Result<Self, Self::Error> {
        if value.is_empty()
            || value.len() > 128
            || !value
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
        {
            return Err(
                "registry ids must contain 1..128 ASCII letters, digits, underscores or hyphens",
            );
        }
        Ok(Self(value))
    }
}
impl From<RegistryId> for String {
    fn from(value: RegistryId) -> Self {
        value.0
    }
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, JsonSchema)]
#[serde(try_from = "String", into = "String")]
pub struct InstanceId(RegistryId);
impl TryFrom<String> for InstanceId {
    type Error = &'static str;
    fn try_from(value: String) -> Result<Self, Self::Error> {
        RegistryId::try_from(value).map(Self)
    }
}
impl From<InstanceId> for String {
    fn from(value: InstanceId) -> Self {
        value.0.into()
    }
}

#[derive(
    Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum ReferenceKind {
    Device,
    Entity,
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct HomeAssistantReference {
    pub kind: ReferenceKind,
    pub instance_id: InstanceId,
    pub id: RegistryId,
}

/// Only a configured HTTP(S) origin may be used for Home Assistant links.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(try_from = "String", into = "String")]
pub struct BaseUrl(String);
impl TryFrom<String> for BaseUrl {
    type Error = &'static str;
    fn try_from(value: String) -> Result<Self, Self::Error> {
        let invalid = "Home Assistant base URL must be an HTTP(S) origin without credentials, path, query or fragment";
        let url = reqwest::Url::parse(&value).map_err(|_| invalid)?;
        if value.len() > 2048
            || !matches!(url.scheme(), "http" | "https")
            || url.host_str().is_none()
            || !url.username().is_empty()
            || url.password().is_some()
            || url.path() != "/"
            || url.query().is_some()
            || url.fragment().is_some()
        {
            return Err(invalid);
        }
        Ok(Self(url.origin().ascii_serialization()))
    }
}
impl From<BaseUrl> for String {
    fn from(value: BaseUrl) -> Self {
        value.0
    }
}

/// The mutable entity_id is a display/link hint, never a reference identity.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(try_from = "String", into = "String")]
pub struct EntityId(String);
impl TryFrom<String> for EntityId {
    type Error = &'static str;
    fn try_from(value: String) -> Result<Self, Self::Error> {
        let invalid = "entity_id must be a Home Assistant domain.object_id";
        let Some((domain, object)) = value.split_once('.') else {
            return Err(invalid);
        };
        let safe_part = |part: &str| {
            !part.is_empty()
                && part
                    .chars()
                    .all(|character| character.is_alphanumeric() || character == '_')
        };
        // HA owns the entity grammar. Validate only a bounded, safe link hint;
        // stored references use the separate, immutable registry id.
        if value.chars().count() > 255 || !safe_part(domain) || !safe_part(object) {
            return Err(invalid);
        }
        Ok(Self(value))
    }
}
impl From<EntityId> for String {
    fn from(value: EntityId) -> Self {
        value.0
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum RegistryTarget {
    Device {
        id: RegistryId,
        name: String,
        disabled: bool,
    },
    Entity {
        id: RegistryId,
        name: String,
        disabled: bool,
        entity_id: EntityId,
        device_id: Option<RegistryId>,
    },
}
impl RegistryTarget {
    fn identity(&self) -> (ReferenceKind, &RegistryId) {
        match self {
            Self::Device { id, .. } => (ReferenceKind::Device, id),
            Self::Entity { id, .. } => (ReferenceKind::Entity, id),
        }
    }
    fn name(&self) -> &str {
        match self {
            Self::Device { name, .. } | Self::Entity { name, .. } => name,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RegistrySnapshot {
    pub instance_id: InstanceId,
    pub base_url: Option<BaseUrl>,
    pub targets: Vec<RegistryTarget>,
}
impl RegistrySnapshot {
    fn validate(&self) -> Result<(), Error> {
        if self.targets.len() > 20_000 {
            return Err(Error::Invalid("registry supports at most 20000 targets"));
        }
        let mut identities = BTreeSet::new();
        for target in &self.targets {
            if target.name().trim().is_empty()
                || target.name().chars().count() > 200
                || target.name().chars().any(char::is_control)
            {
                return Err(Error::Invalid(
                    "registry names must contain 1..200 characters without controls",
                ));
            }
            if !identities.insert(target.identity()) {
                return Err(Error::Invalid(
                    "registry targets must have unique kind and id",
                ));
            }
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum RegistryUnavailableReason {
    Unavailable,
    LimitExceeded,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "status", rename_all = "snake_case", deny_unknown_fields)]
pub enum RegistryReport {
    Ready(RegistrySnapshot),
    Unavailable {
        instance_id: InstanceId,
        reason: RegistryUnavailableReason,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum RegistryCatalog {
    NotConnected,
    Ready(RegistrySnapshot),
    Stale(RegistrySnapshot),
    Unavailable {
        instance_id: InstanceId,
        reason: RegistryUnavailableReason,
    },
}
struct ReceivedRegistry {
    at: i64,
    report: RegistryReport,
}
#[derive(Default)]
pub struct Registry {
    received: RwLock<Option<ReceivedRegistry>>,
}
impl Registry {
    pub async fn record(&self, report: RegistryReport) -> Result<(), Error> {
        if let RegistryReport::Ready(snapshot) = &report {
            snapshot.validate()?;
        }
        // Validate the complete replacement before taking the write lock. An
        // invalid report must never discard a previously usable snapshot.
        *self.received.write().await = Some(ReceivedRegistry {
            at: Utc::now().timestamp(),
            report,
        });
        Ok(())
    }
    pub async fn catalog(&self) -> RegistryCatalog {
        let received = self.received.read().await;
        match received.as_ref() {
            None => RegistryCatalog::NotConnected,
            Some(ReceivedRegistry {
                at,
                report: RegistryReport::Ready(snapshot),
            }) => {
                if Utc::now().timestamp().saturating_sub(*at) < 180 {
                    RegistryCatalog::Ready(snapshot.clone())
                } else {
                    RegistryCatalog::Stale(snapshot.clone())
                }
            }
            Some(ReceivedRegistry {
                report:
                    RegistryReport::Unavailable {
                        instance_id,
                        reason,
                    },
                ..
            }) => RegistryCatalog::Unavailable {
                instance_id: instance_id.clone(),
                reason: *reason,
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::home_assistant::{
        BaseUrl, EntityId, InstanceId, ReceivedRegistry, Registry, RegistryCatalog, RegistryId,
        RegistryReport, RegistrySnapshot, RegistryTarget, RegistryUnavailableReason,
    };
    use chrono::Utc;

    fn snapshot() -> RegistrySnapshot {
        RegistrySnapshot {
            instance_id: InstanceId::try_from("example-instance".to_owned()).unwrap(),
            base_url: Some(BaseUrl::try_from("https://example.invalid".to_owned()).unwrap()),
            targets: vec![RegistryTarget::Device {
                id: RegistryId::try_from("example-device".to_owned()).unwrap(),
                name: "Example device".into(),
                disabled: false,
            }],
        }
    }
    #[test]
    fn identifiers_and_origins_validate_without_normalizing_registry_identity() {
        for value in [
            "0123456789abcdef0123456789abcdef",
            "01JB1E6BQDMQWVVP5T4PRZQF8A",
            "550e8400-e29b-41d4-a716-446655440000",
            "Example_Device-2",
        ] {
            let id = RegistryId::try_from(value.to_owned()).unwrap();
            assert_eq!(String::from(id), value);
        }
        for value in [
            "",
            "sensor.example",
            "example/id",
            "example id",
            "é",
            "example\n",
        ] {
            assert!(RegistryId::try_from(value.to_owned()).is_err(), "{value:?}");
        }
        assert!(RegistryId::try_from("x".repeat(128)).is_ok());
        assert!(RegistryId::try_from("x".repeat(129)).is_err());
        assert_eq!(
            String::from(BaseUrl::try_from("https://EXAMPLE.invalid:443/".to_owned()).unwrap()),
            "https://example.invalid"
        );
        for value in [
            "http://example.local:8123",
            "https://example.invalid",
            "http://[::1]:8123/",
        ] {
            assert!(BaseUrl::try_from(value.to_owned()).is_ok(), "{value}");
        }
        for value in [
            "javascript:alert(1)",
            "ftp://example.invalid",
            "https://user:password@example.invalid",
            "https://example.invalid/config",
            "https://example.invalid/?token=example",
            "https://example.invalid/#example",
        ] {
            assert!(BaseUrl::try_from(value.to_owned()).is_err(), "{value}");
        }
        for value in [
            "sensor.example",
            "binary_sensor.example__2",
            "sensor._example",
            "sensor.Example",
            "sensor.١٢٣",
        ] {
            assert!(EntityId::try_from(value.to_owned()).is_ok());
        }
        for value in [
            "sensor",
            "sensor.example.other",
            "sensor.example/path",
            "sensor.example?query",
            "sensor.example name",
            "sensor.example\n",
        ] {
            assert!(EntityId::try_from(value.to_owned()).is_err(), "{value}");
        }
    }
    #[tokio::test]
    async fn registry_replacements_are_validated_expire_and_remain_ephemeral() {
        let registry = Registry::default();
        assert_eq!(registry.catalog().await, RegistryCatalog::NotConnected);
        let snapshot = snapshot();
        registry
            .record(RegistryReport::Ready(snapshot.clone()))
            .await
            .unwrap();
        let mut invalid = snapshot.clone();
        invalid.targets.push(invalid.targets[0].clone());
        assert!(
            registry
                .record(RegistryReport::Ready(invalid))
                .await
                .is_err()
        );
        assert_eq!(
            registry.catalog().await,
            RegistryCatalog::Ready(snapshot.clone())
        );
        *registry.received.write().await = Some(ReceivedRegistry {
            at: Utc::now().timestamp() - 180,
            report: RegistryReport::Ready(snapshot.clone()),
        });
        assert_eq!(
            registry.catalog().await,
            RegistryCatalog::Stale(snapshot.clone())
        );
        let unavailable = RegistryReport::Unavailable {
            instance_id: snapshot.instance_id.clone(),
            reason: RegistryUnavailableReason::LimitExceeded,
        };
        registry.record(unavailable).await.unwrap();
        assert_eq!(
            registry.catalog().await,
            RegistryCatalog::Unavailable {
                instance_id: snapshot.instance_id.clone(),
                reason: RegistryUnavailableReason::LimitExceeded
            }
        );
        let another = RegistrySnapshot {
            instance_id: InstanceId::try_from("another-instance".to_owned()).unwrap(),
            targets: vec![],
            base_url: None,
        };
        let (first, second) = tokio::join!(
            registry.record(RegistryReport::Ready(snapshot.clone())),
            registry.record(RegistryReport::Ready(another.clone()))
        );
        first.unwrap();
        second.unwrap();
        let current = registry.catalog().await;
        assert!(
            current == RegistryCatalog::Ready(snapshot)
                || current == RegistryCatalog::Ready(another)
        );
        assert_eq!(
            Registry::default().catalog().await,
            RegistryCatalog::NotConnected
        );
    }
}
