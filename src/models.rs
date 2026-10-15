use std::collections::BTreeSet;

use chrono::{DateTime, Datelike, Days, Months, NaiveDate, Utc};
use chrono_tz::Tz;
use schemars::{JsonSchema, schema::Schema};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use uuid::Uuid;

use crate::error::Error;

/// Stable public item identity; occurrence identities change after completion.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, JsonSchema)]
#[serde(try_from = "String", into = "String")]
pub struct ItemId(String);

impl TryFrom<String> for ItemId {
    type Error = &'static str;
    fn try_from(value: String) -> Result<Self, Self::Error> {
        if value.is_empty()
            || value.len() > 80
            || value.split('-').any(|part| {
                part.is_empty()
                    || !part
                        .bytes()
                        .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
            })
        {
            return Err("item id must contain 1..80 lowercase letters, digits and single hyphens");
        }
        Ok(Self(value))
    }
}
impl From<ItemId> for String {
    fn from(id: ItemId) -> Self {
        id.0
    }
}
impl AsRef<str> for ItemId {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(try_from = "String", into = "String")]
pub struct TimeZone(#[schemars(with = "String")] pub Tz);
impl TryFrom<String> for TimeZone {
    type Error = &'static str;
    fn try_from(value: String) -> Result<Self, Self::Error> {
        value
            .parse()
            .map(Self)
            .map_err(|_| "timezone must be an IANA timezone")
    }
}
impl From<TimeZone> for String {
    fn from(value: TimeZone) -> Self {
        value.0.name().into()
    }
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(try_from = "ScheduleFields", into = "ScheduleFields")]
pub struct Schedule {
    days: u32,
    months: u32,
    years: u32,
}
#[derive(Clone, Copy, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct ScheduleFields {
    #[serde(default)]
    days: u32,
    #[serde(default)]
    months: u32,
    #[serde(default)]
    years: u32,
}
impl TryFrom<ScheduleFields> for Schedule {
    type Error = Error;
    fn try_from(value: ScheduleFields) -> Result<Self, Error> {
        Self::new(value.days, value.months, value.years)
    }
}
impl From<Schedule> for ScheduleFields {
    fn from(value: Schedule) -> Self {
        Self {
            days: value.days,
            months: value.months,
            years: value.years,
        }
    }
}
impl Schedule {
    pub fn new(days: u32, months: u32, years: u32) -> Result<Self, Error> {
        if days > 36_600 || months > 1_200 || years > 100 || days + months + years == 0 {
            return Err(Error::Invalid(
                "schedule must have a positive bounded calendar interval",
            ));
        }
        Ok(Self {
            days,
            months,
            years,
        })
    }
    /// Calendar arithmetic precedes day arithmetic; clamp month ends, never roll into the next month.
    pub fn next(self, from: NaiveDate) -> Result<NaiveDate, Error> {
        from.checked_add_months(Months::new(self.years * 12 + self.months))
            .and_then(|date| date.checked_add_days(Days::new(u64::from(self.days))))
            .filter(|date| validate_date(*date).is_ok())
            .ok_or(Error::Invalid(
                "next due date is outside the supported calendar",
            ))
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Spec {
    pub label: String,
    pub value: String,
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Resource {
    pub label: String,
    pub url: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ChecklistStep {
    pub id: Uuid,
    pub summary: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct StepCompletion {
    pub completed_at: DateTime<Utc>,
    pub actor: Actor,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ChecklistStepState {
    pub id: Uuid,
    pub summary: String,
    pub completion: Option<StepCompletion>,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct Definition {
    pub name: String,
    pub summary: String,
    #[serde(default)]
    pub description: Option<String>,
    pub timezone: TimeZone,
    #[serde(default)]
    pub location: Option<String>,
    #[serde(default)]
    pub group: Option<String>,
    pub schedule: Schedule,
    #[serde(default)]
    pub instructions: Vec<String>,
    #[serde(default)]
    pub checklist: Vec<ChecklistStep>,
    #[serde(default)]
    pub specs: Vec<Spec>,
    #[serde(default)]
    pub resources: Vec<Resource>,
    #[serde(default)]
    pub reorder_url: Option<String>,
}
impl Definition {
    pub fn validate(&self) -> Result<(), Error> {
        text(&self.name, 200)?;
        text(&self.summary, 200)?;
        if let Some(description) = &self.description {
            text(description, 16_000)?;
        }
        for value in [&self.location, &self.group].into_iter().flatten() {
            text(value, 200)?;
        }
        if self.instructions.len() > 100
            || self.checklist.len() > 100
            || self.specs.len() > 100
            || self.resources.len() > 100
        {
            return Err(Error::Invalid(
                "at most 100 instructions, checklist steps, specs or links per item",
            ));
        }
        let mut step_ids = BTreeSet::new();
        for step in &self.checklist {
            text(&step.summary, 200)?;
            if !step_ids.insert(step.id) {
                return Err(Error::Invalid("checklist step ids must be unique"));
            }
        }
        for instruction in &self.instructions {
            text(instruction, 2_000)?;
        }
        for spec in &self.specs {
            text(&spec.label, 100)?;
            text(&spec.value, 500)?;
        }
        for resource in &self.resources {
            text(&resource.label, 100)?;
            url(&resource.url)?;
        }
        if let Some(value) = &self.reorder_url {
            url(value)?;
        }
        Ok(())
    }

    /// Edits preserve checks only for the same step and wording. New work must
    /// not inherit a check recorded for a different instruction.
    pub fn checklist_state(&self, previous: &[ChecklistStepState]) -> Vec<ChecklistStepState> {
        self.checklist
            .iter()
            .map(|step| ChecklistStepState {
                id: step.id,
                summary: step.summary.clone(),
                completion: previous
                    .iter()
                    .find(|state| state.id == step.id && state.summary == step.summary)
                    .and_then(|state| state.completion.clone()),
            })
            .collect()
    }
}
fn text(value: &str, maximum: usize) -> Result<(), Error> {
    if value.trim().is_empty()
        || value.len() > maximum
        || value
            .chars()
            .any(|c| c.is_control() && c != '\n' && c != '\t')
    {
        return Err(Error::Invalid(
            "text is empty, too long or contains control characters",
        ));
    }
    Ok(())
}
fn url(value: &str) -> Result<(), Error> {
    let parsed = reqwest::Url::parse(value)
        .map_err(|_| Error::Invalid("links must be absolute HTTP or HTTPS URLs"))?;
    if value.len() > 2048
        || !matches!(parsed.scheme(), "http" | "https")
        || parsed.host_str().is_none()
        || !parsed.username().is_empty()
        || parsed.password().is_some()
    {
        return Err(Error::Invalid(
            "links must be absolute HTTP or HTTPS URLs without credentials",
        ));
    }
    Ok(())
}
pub fn validate_date(date: NaiveDate) -> Result<(), Error> {
    // An all-day calendar event also needs a representable exclusive end date.
    if date.year() < 1 || date.succ_opt().is_none_or(|end| end.year() > 9999) {
        return Err(Error::Invalid(
            "due date must be between 0001-01-01 and 9999-12-30",
        ));
    }
    Ok(())
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CreateItem {
    pub id: ItemId,
    pub due: NaiveDate,
    #[serde(flatten)]
    pub definition: Definition,
}
impl CreateItem {
    pub fn validate(&self) -> Result<(), Error> {
        self.definition.validate()?;
        validate_date(self.due)
    }
}

/// Missing keeps a field; explicit null clears optional fields only.
#[derive(Clone, Debug)]
pub struct Change<T>(pub Option<T>);
impl<T> Default for Change<T> {
    fn default() -> Self {
        Self(None)
    }
}
impl<T> Change<T> {
    pub fn is_missing(&self) -> bool {
        self.0.is_none()
    }
    fn apply(self, target: &mut T) {
        if let Some(value) = self.0 {
            *target = value;
        }
    }
}
impl<'de, T: Deserialize<'de>> Deserialize<'de> for Change<T> {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        T::deserialize(deserializer).map(|value| Self(Some(value)))
    }
}
impl<T: Serialize> Serialize for Change<T> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.0.serialize(serializer)
    }
}
impl<T: JsonSchema> JsonSchema for Change<T> {
    fn schema_name() -> String {
        T::schema_name()
    }
    fn json_schema(generator: &mut schemars::r#gen::SchemaGenerator) -> Schema {
        T::json_schema(generator)
    }
    fn is_referenceable() -> bool {
        T::is_referenceable()
    }
}
#[derive(Clone, Debug, Default, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct UpdateItem {
    #[serde(default, skip_serializing_if = "Change::is_missing")]
    pub name: Change<String>,
    #[serde(default, skip_serializing_if = "Change::is_missing")]
    pub summary: Change<String>,
    #[serde(default, skip_serializing_if = "Change::is_missing")]
    pub description: Change<Option<String>>,
    #[serde(default, skip_serializing_if = "Change::is_missing")]
    pub timezone: Change<TimeZone>,
    #[serde(default, skip_serializing_if = "Change::is_missing")]
    pub location: Change<Option<String>>,
    #[serde(default, skip_serializing_if = "Change::is_missing")]
    pub group: Change<Option<String>>,
    #[serde(default, skip_serializing_if = "Change::is_missing")]
    pub schedule: Change<Schedule>,
    #[serde(default, skip_serializing_if = "Change::is_missing")]
    pub due: Change<NaiveDate>,
    #[serde(default, skip_serializing_if = "Change::is_missing")]
    pub instructions: Change<Vec<String>>,
    #[serde(default, skip_serializing_if = "Change::is_missing")]
    pub checklist: Change<Vec<ChecklistStep>>,
    #[serde(default, skip_serializing_if = "Change::is_missing")]
    pub specs: Change<Vec<Spec>>,
    #[serde(default, skip_serializing_if = "Change::is_missing")]
    pub resources: Change<Vec<Resource>>,
    #[serde(default, skip_serializing_if = "Change::is_missing")]
    pub reorder_url: Change<Option<String>>,
}
impl UpdateItem {
    pub fn apply(self, definition: &mut Definition, due: &mut NaiveDate) -> Result<(), Error> {
        self.name.apply(&mut definition.name);
        self.summary.apply(&mut definition.summary);
        self.description.apply(&mut definition.description);
        self.timezone.apply(&mut definition.timezone);
        self.location.apply(&mut definition.location);
        self.group.apply(&mut definition.group);
        self.schedule.apply(&mut definition.schedule);
        self.due.apply(due);
        self.instructions.apply(&mut definition.instructions);
        self.checklist.apply(&mut definition.checklist);
        self.specs.apply(&mut definition.specs);
        self.resources.apply(&mut definition.resources);
        self.reorder_url.apply(&mut definition.reorder_url);
        definition.validate()?;
        validate_date(*due)
    }
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Actor {
    User { id: String, name: String },
    System { id: SystemActor, name: String },
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub enum SystemActor {
    #[serde(rename = "owner")]
    Owner,
    #[serde(rename = "ha:automation")]
    Automation,
    #[serde(rename = "ha:integration")]
    Integration,
}
impl Actor {
    pub fn key(&self) -> String {
        match self {
            Self::User { id, .. } => format!("user:{id}"),
            Self::System { id, .. } => match id {
                SystemActor::Owner => "system:owner",
                SystemActor::Automation => "system:ha:automation",
                SystemActor::Integration => "system:ha:integration",
            }
            .into(),
        }
    }
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct Occurrence {
    pub id: Uuid,
    pub due: NaiveDate,
    pub checklist: Vec<ChecklistStepState>,
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct Completion {
    pub id: Uuid,
    pub item_id: ItemId,
    pub occurrence_id: Uuid,
    pub completed_at: DateTime<Utc>,
    pub actor: Actor,
    pub previous_due: NaiveDate,
    pub next_due: NaiveDate,
    pub checklist: Vec<ChecklistStepState>,
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct Item {
    pub id: ItemId,
    #[serde(flatten)]
    pub definition: Definition,
    pub revision: i64,
    pub archived: bool,
    pub occurrence: Occurrence,
    pub last_completion: Option<Completion>,
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CompleteItem {
    pub occurrence_id: Uuid,
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SetChecklistStep {
    pub occurrence_id: Uuid,
    pub completed: bool,
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ImportCatalog {
    pub items: Vec<CreateItem>,
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct Page<T> {
    pub items: Vec<T>,
    pub next_page: Option<String>,
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct Summary {
    pub active: i64,
    pub overdue: i64,
    pub due_soon: i64,
    pub completed: i64,
}

#[cfg(test)]
mod tests {
    use crate::models::{CreateItem, ItemId, Schedule, TimeZone};
    use chrono::{Datelike, NaiveDate, TimeZone as _, Utc};
    #[test]
    fn calendar_intervals_clamp_month_ends_and_leap_years() {
        let monthly = Schedule::new(0, 1, 0).unwrap();
        assert_eq!(
            monthly
                .next(NaiveDate::from_ymd_opt(2031, 1, 31).unwrap())
                .unwrap(),
            NaiveDate::from_ymd_opt(2031, 2, 28).unwrap()
        );
        let yearly = Schedule::new(0, 0, 1).unwrap();
        assert_eq!(
            yearly
                .next(NaiveDate::from_ymd_opt(2032, 2, 29).unwrap())
                .unwrap(),
            NaiveDate::from_ymd_opt(2033, 2, 28).unwrap()
        );
        assert!(Schedule::new(0, 0, 0).is_err());
        assert!(
            yearly
                .next(NaiveDate::from_ymd_opt(9999, 1, 1).unwrap())
                .is_err()
        );
        let daily = Schedule::new(1, 0, 0).unwrap();
        assert_eq!(
            daily
                .next(NaiveDate::from_ymd_opt(9999, 12, 29).unwrap())
                .unwrap(),
            NaiveDate::from_ymd_opt(9999, 12, 30).unwrap()
        );
        assert!(
            daily
                .next(NaiveDate::from_ymd_opt(9999, 12, 30).unwrap())
                .is_err()
        );
    }
    #[test]
    fn recurrence_uses_local_calendar_dates_across_dst() {
        let timezone = TimeZone::try_from("America/New_York".to_owned()).unwrap();
        for date in [(2031, 3, 9), (2031, 11, 2)] {
            let utc = Utc
                .with_ymd_and_hms(date.0, date.1, date.2, 0, 30, 0)
                .unwrap();
            let local = utc.with_timezone(&timezone.0);
            let next = Schedule::new(1, 0, 0)
                .unwrap()
                .next(local.date_naive())
                .unwrap();
            assert_eq!(next.signed_duration_since(local.date_naive()).num_days(), 1);
            assert_eq!(next.day(), date.2);
        }
    }
    #[test]
    fn owned_contracts_reject_invalid_ids_timezones_and_unknown_fields() {
        for id in ["", "Example", "a--b", "../example", "example/"] {
            assert!(ItemId::try_from(id.to_owned()).is_err());
        }
        assert!(TimeZone::try_from("Not/AZone".to_owned()).is_err());
        let catalog: crate::models::ImportCatalog =
            serde_json::from_str(include_str!("../config/items.example.json")).unwrap();
        for item in catalog.items {
            item.validate().unwrap();
        }
        assert!(serde_json::from_str::<CreateItem>(r#"{"id":"example","name":"Example","summary":"Inspect the example","due":"2031-01-01","timezone":"Etc/UTC","schedule":{"days":1},"actor":"forged"}"#).is_err());
    }

    #[test]
    fn summaries_and_descriptions_have_explicit_text_bounds() {
        let catalog: crate::models::ImportCatalog =
            serde_json::from_str(include_str!("../config/items.example.json")).unwrap();
        let mut item = catalog.items[0].clone();
        item.definition.summary = "x".repeat(200);
        item.definition.description = Some("x".repeat(16_000));
        item.validate().unwrap();
        item.definition.summary.push('x');
        assert!(item.validate().is_err());
        item.definition.summary = "Inspect the example".into();
        item.definition.description.as_mut().unwrap().push('x');
        assert!(item.validate().is_err());
        for description in [" ", "unsafe\u{0000}text"] {
            item.definition.description = Some(description.into());
            assert!(item.validate().is_err());
        }
        item.definition.description = None;
        item.validate().unwrap();
    }
}
