use std::{
    collections::{BTreeMap, BTreeSet},
    path::Path,
    time::Duration,
};

use chrono::{DateTime, Days, NaiveDate, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use sqlx::{
    FromRow, SqliteConnection, SqlitePool,
    sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions},
};
use uuid::Uuid;

use crate::{
    error::Error,
    models::{
        Actor, CompleteItem, Completion, CreateItem, ImportCatalog, ImportedCompletion, Item,
        ItemId, ItemStatus, Migration, MigrationAction, MigrationItem, MigrationMapping,
        MigrationReport, Occurrence, Page, Reorder, ReorderContent, ReorderItem, ReorderReceipt,
        ReorderStatus, ResolveReorder, Schedule, SetChecklistStep, StepCompletion, Summary,
        UpdateItem, validate_due,
    },
};

/// Management authority is supplied by the authenticated API principal.
#[derive(Clone, Copy)]
pub enum Management {
    Administrator,
    Owner,
}
impl Management {
    fn check_instruction(self, schedule: Schedule) -> Result<(), Error> {
        if schedule == Schedule::Instruction && !matches!(self, Self::Owner) {
            Err(Error::Forbidden)
        } else {
            Ok(())
        }
    }
}

#[derive(Clone)]
pub struct Store {
    pool: SqlitePool,
}

// Definitions are one typed, versioned document. Indexed lifecycle/order fields
// live in columns; history is append-only and never hidden inside that document.
const EVENT_LIST: &str = "SELECT sequence,id,item_id,occurrence_id,completed_at,actor,previous_due,next_due,checklist,source FROM completions WHERE item_id=? ORDER BY sequence DESC LIMIT ?";
const EVENT_LIST_NEXT: &str = "SELECT sequence,id,item_id,occurrence_id,completed_at,actor,previous_due,next_due,checklist,source FROM completions WHERE item_id=? AND sequence<? ORDER BY sequence DESC LIMIT ?";
const IDEMPOTENCY_GET: &str =
    "SELECT fingerprint,response FROM idempotency WHERE actor=? AND key=?";
const IDEMPOTENCY_INSERT: &str =
    "INSERT INTO idempotency(actor,key,fingerprint,response) VALUES(?,?,?,?)";
const ITEM_INSERT: &str =
    "INSERT INTO items(id,definition,revision,occurrence_id,due,checklist) VALUES(?,?,1,?,?,?)";
const ITEM_UPDATE: &str = "UPDATE items SET definition=?,due=?,checklist=?,revision=? WHERE id=?";
const CHECKLIST_UPDATE: &str = "UPDATE items SET checklist=?,revision=? WHERE id=?";
const ITEM_ARCHIVE: &str = "UPDATE items SET archived=?,revision=? WHERE id=?";
const DATABASE_NOW: &str = "SELECT strftime('%Y-%m-%dT%H:%M:%fZ','now')";
const COMPLETION_INSERT: &str = "INSERT INTO completions(id,item_id,occurrence_id,completed_at,actor,previous_due,next_due,checklist) VALUES (?,?,?,?,?,?,?,?)";
const OCCURRENCE_ADVANCE: &str = "UPDATE items SET due=?,occurrence_id=?,revision=?,last_completion_id=?,checklist=?,status=? WHERE id=?";
const SUMMARY_FIRST: &str = "SELECT id,json_extract(definition,'$.timezone') AS timezone,due FROM items WHERE archived=0 AND status='needs_action' AND json_extract(definition,'$.schedule.kind')!='instruction' ORDER BY sort_due,id LIMIT 200";
const SUMMARY_NEXT: &str = "SELECT id,json_extract(definition,'$.timezone') AS timezone,due FROM items WHERE archived=0 AND status='needs_action' AND json_extract(definition,'$.schedule.kind')!='instruction' AND (sort_due,id)>(?,?) ORDER BY sort_due,id LIMIT 200";
const COMPLETION_COUNT: &str = "SELECT count(*) FROM completions";
const REORDER_INSERT: &str = "INSERT INTO reorders(id,receipt) VALUES(?,?)";
const REORDER_GET: &str =
    "SELECT sequence,receipt,status,revision,resolved_at,resolved_by FROM reorders WHERE id=?";
const REORDER_LIST: &str = "SELECT sequence,receipt,status,revision,resolved_at,resolved_by FROM reorders WHERE status=? ORDER BY sequence LIMIT ?";
const REORDER_LIST_NEXT: &str = "SELECT sequence,receipt,status,revision,resolved_at,resolved_by FROM reorders WHERE status=? AND sequence>? ORDER BY sequence LIMIT ?";
const REORDER_RESOLVE: &str =
    "UPDATE reorders SET status=?,revision=?,resolved_at=?,resolved_by=? WHERE id=?";
const MIGRATION_SOURCES: &str = "SELECT source_key,target_id,fingerprint FROM migration_sources WHERE source_key IN (SELECT value FROM json_each(?))";
const MIGRATION_TARGETS: &str = "SELECT id FROM items WHERE id IN (SELECT value FROM json_each(?))";
const MIGRATION_EVENT_IDS: &str =
    "SELECT id FROM completions WHERE id IN (SELECT value FROM json_each(?))";
const MIGRATION_OCCURRENCES: &str = "SELECT occurrence_id FROM completions WHERE occurrence_id IN (SELECT value FROM json_each(?1)) UNION ALL SELECT occurrence_id FROM items WHERE occurrence_id IN (SELECT value FROM json_each(?1))";
const MIGRATION_EVENT_SOURCES: &str =
    "SELECT source_key FROM completions WHERE source_key IN (SELECT value FROM json_each(?))";
const MIGRATION_SOURCE_INSERT: &str =
    "INSERT INTO migration_sources(source_key,target_id,fingerprint) VALUES(?,?,?)";
const MIGRATION_ITEM_UPDATE: &str =
    "UPDATE items SET status=?,archived=?,source=?,source_key=?,last_completion_id=? WHERE id=?";
const MIGRATION_EVENT_INSERT: &str = "INSERT INTO completions(id,item_id,occurrence_id,completed_at,actor,previous_due,next_due,checklist,source,source_key) VALUES(?,?,?,?,?,?,?,?,?,?)";
#[derive(FromRow)]
struct ReorderRow {
    sequence: i64,
    receipt: String,
    status: String,
    revision: i64,
    resolved_at: Option<String>,
    resolved_by: Option<String>,
}
impl TryFrom<ReorderRow> for Reorder {
    type Error = Error;
    fn try_from(row: ReorderRow) -> Result<Self, Error> {
        let status = match row.status.as_str() {
            "pending" => ReorderStatus::Pending,
            "fulfilled" => ReorderStatus::Fulfilled,
            "dismissed" => ReorderStatus::Dismissed,
            _ => return Err(corrupt()),
        };
        Ok(Self {
            request: serde_json::from_str(&row.receipt)?,
            status,
            revision: row.revision,
            resolved_at: row.resolved_at.as_deref().map(parse_time).transpose()?,
            resolved_by: row
                .resolved_by
                .as_deref()
                .map(serde_json::from_str)
                .transpose()?,
        })
    }
}
#[derive(FromRow)]
struct ItemRow {
    id: String,
    definition: String,
    revision: i64,
    archived: bool,
    occurrence_id: String,
    status: String,
    source: Option<String>,
    due: Option<String>,
    checklist: String,
    completion_id: Option<String>,
    completed_occurrence: Option<String>,
    completed_at: Option<String>,
    actor: Option<String>,
    previous_due: Option<String>,
    next_due: Option<String>,
    completion_checklist: Option<String>,
    completion_source: Option<String>,
}
macro_rules! item_select {
    () => { "SELECT i.id,i.definition,i.revision,i.archived,i.status,i.source,i.occurrence_id,i.due,i.checklist,c.id AS completion_id,c.occurrence_id AS completed_occurrence,c.completed_at,c.actor,c.previous_due,c.next_due,c.checklist AS completion_checklist,c.source AS completion_source FROM items i LEFT JOIN completions c ON c.sequence=i.last_completion_id" };
}
const ITEM_GET: &str = concat!(item_select!(), " WHERE i.id=?");
const ITEM_LIST: &str = concat!(
    item_select!(),
    " WHERE i.archived=? ORDER BY i.sort_due,i.id LIMIT ?"
);
const ITEM_LIST_NEXT: &str = concat!(
    item_select!(),
    " WHERE i.archived=? AND (i.sort_due,i.id)>(?,?) ORDER BY i.sort_due,i.id LIMIT ?"
);
#[derive(FromRow)]
struct CompletionRow {
    sequence: i64,
    id: String,
    item_id: String,
    occurrence_id: Option<String>,
    completed_at: Option<String>,
    actor: Option<String>,
    previous_due: Option<String>,
    next_due: Option<String>,
    checklist: String,
    source: Option<String>,
}

fn sort_due(due: Option<NaiveDate>) -> String {
    due.map(|date| date.to_string())
        .unwrap_or_else(|| "9999-12-31".into())
}
fn parse_status(value: &str) -> Result<ItemStatus, Error> {
    match value {
        "needs_action" => Ok(ItemStatus::NeedsAction),
        "completed" => Ok(ItemStatus::Completed),
        _ => Err(corrupt()),
    }
}
fn corrupt() -> Error {
    Error::Corrupt
}
fn parse_date(value: &str) -> Result<NaiveDate, Error> {
    NaiveDate::parse_from_str(value, "%Y-%m-%d").map_err(|_| corrupt())
}
fn parse_uuid(value: &str) -> Result<Uuid, Error> {
    Uuid::parse_str(value).map_err(|_| corrupt())
}
fn parse_time(value: &str) -> Result<DateTime<Utc>, Error> {
    DateTime::parse_from_rfc3339(value)
        .map(|time| time.with_timezone(&Utc))
        .map_err(|_| corrupt())
}
impl TryFrom<ItemRow> for Item {
    type Error = Error;
    fn try_from(row: ItemRow) -> Result<Self, Error> {
        let id = ItemId::try_from(row.id).map_err(|_| corrupt())?;
        let last_completion = if let Some(event_id) = row.completion_id {
            Some(Completion {
                id: parse_uuid(&event_id)?,
                item_id: id.clone(),
                occurrence_id: row
                    .completed_occurrence
                    .as_deref()
                    .map(parse_uuid)
                    .transpose()?,
                completed_at: row.completed_at.as_deref().map(parse_time).transpose()?,
                actor: row.actor.as_deref().map(serde_json::from_str).transpose()?,
                previous_due: row.previous_due.as_deref().map(parse_date).transpose()?,
                next_due: row.next_due.as_deref().map(parse_date).transpose()?,
                checklist: serde_json::from_str(
                    row.completion_checklist.as_deref().ok_or_else(corrupt)?,
                )?,
                source: row
                    .completion_source
                    .as_deref()
                    .map(serde_json::from_str)
                    .transpose()?,
            })
        } else {
            None
        };
        Ok(Self {
            id,
            definition: serde_json::from_str(&row.definition)?,
            revision: row.revision,
            archived: row.archived,
            status: parse_status(&row.status)?,
            source: row
                .source
                .as_deref()
                .map(serde_json::from_str)
                .transpose()?,
            occurrence: Occurrence {
                id: parse_uuid(&row.occurrence_id)?,
                due: row.due.as_deref().map(parse_date).transpose()?,
                checklist: serde_json::from_str(&row.checklist)?,
            },
            last_completion,
        })
    }
}
impl TryFrom<CompletionRow> for Completion {
    type Error = Error;
    fn try_from(row: CompletionRow) -> Result<Self, Error> {
        Ok(Self {
            id: parse_uuid(&row.id)?,
            item_id: ItemId::try_from(row.item_id).map_err(|_| corrupt())?,
            occurrence_id: row.occurrence_id.as_deref().map(parse_uuid).transpose()?,
            completed_at: row.completed_at.as_deref().map(parse_time).transpose()?,
            actor: row.actor.as_deref().map(serde_json::from_str).transpose()?,
            previous_due: row.previous_due.as_deref().map(parse_date).transpose()?,
            next_due: row.next_due.as_deref().map(parse_date).transpose()?,
            source: row
                .source
                .as_deref()
                .map(serde_json::from_str)
                .transpose()?,
            checklist: serde_json::from_str(&row.checklist)?,
        })
    }
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ItemCursor {
    archived: bool,
    due: Option<NaiveDate>,
    id: ItemId,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct EventCursor {
    item_id: ItemId,
    sequence: i64,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ReorderCursor {
    status: ReorderStatus,
    sequence: i64,
}

impl Store {
    pub async fn open(path: &Path) -> Result<Self, Error> {
        let options = SqliteConnectOptions::new()
            .filename(path)
            .create_if_missing(true)
            .foreign_keys(true)
            .journal_mode(SqliteJournalMode::Wal)
            .busy_timeout(Duration::from_secs(30));
        let pool = SqlitePoolOptions::new()
            .max_connections(4)
            .connect_with(options)
            .await?;
        if let Err(error) = sqlx::migrate!().run(&pool).await {
            pool.close().await;
            return Err(error.into());
        }
        Ok(Self { pool })
    }
    pub fn pool(&self) -> &SqlitePool {
        &self.pool
    }
    pub async fn close(&self) -> Result<(), Error> {
        sqlx::query("PRAGMA wal_checkpoint(TRUNCATE)")
            .execute(&self.pool)
            .await?;
        self.pool.close().await;
        Ok(())
    }
    pub async fn get(&self, id: &ItemId) -> Result<Item, Error> {
        let mut connection = self.pool.acquire().await?;
        get_item(&mut connection, id).await
    }
    pub async fn list(
        &self,
        archived: bool,
        cursor: Option<&str>,
        limit: u32,
    ) -> Result<Page<Item>, Error> {
        check_limit(limit)?;
        let after: Option<ItemCursor> = cursor.map(decode).transpose()?;
        if after
            .as_ref()
            .is_some_and(|value| value.archived != archived)
        {
            return Err(Error::Invalid("page token belongs to a different filter"));
        }
        let rows: Vec<ItemRow> = if let Some(after) = after {
            sqlx::query_as(ITEM_LIST_NEXT)
                .bind(archived)
                .bind(sort_due(after.due))
                .bind(after.id.as_ref())
                .bind(i64::from(limit) + 1)
                .fetch_all(&self.pool)
                .await?
        } else {
            sqlx::query_as(ITEM_LIST)
                .bind(archived)
                .bind(i64::from(limit) + 1)
                .fetch_all(&self.pool)
                .await?
        };
        let more = rows.len() > limit as usize;
        let items: Vec<Item> = rows
            .into_iter()
            .take(limit as usize)
            .map(Item::try_from)
            .collect::<Result<_, _>>()?;
        let next_page = if more {
            items
                .last()
                .map(|item| {
                    encode(&ItemCursor {
                        archived,
                        due: item.occurrence.due,
                        id: item.id.clone(),
                    })
                })
                .transpose()?
        } else {
            None
        };
        Ok(Page { items, next_page })
    }
    pub async fn events(
        &self,
        id: &ItemId,
        cursor: Option<&str>,
        limit: u32,
    ) -> Result<Page<Completion>, Error> {
        check_limit(limit)?;
        self.get(id).await?;
        let after: Option<EventCursor> = cursor.map(decode).transpose()?;
        if after
            .as_ref()
            .is_some_and(|value| &value.item_id != id || value.sequence <= 0)
        {
            return Err(Error::Invalid("page token belongs to a different item"));
        }
        let rows: Vec<CompletionRow> = if let Some(after) = after {
            sqlx::query_as(EVENT_LIST_NEXT)
                .bind(id.as_ref())
                .bind(after.sequence)
                .bind(i64::from(limit) + 1)
                .fetch_all(&self.pool)
                .await?
        } else {
            sqlx::query_as(EVENT_LIST)
                .bind(id.as_ref())
                .bind(i64::from(limit) + 1)
                .fetch_all(&self.pool)
                .await?
        };
        let more = rows.len() > limit as usize;
        let next_page = if more {
            rows.get(limit as usize - 1)
                .map(|row| {
                    encode(&EventCursor {
                        item_id: id.clone(),
                        sequence: row.sequence,
                    })
                })
                .transpose()?
        } else {
            None
        };
        let items = rows
            .into_iter()
            .take(limit as usize)
            .map(Completion::try_from)
            .collect::<Result<_, _>>()?;
        Ok(Page { items, next_page })
    }
    pub async fn create(
        &self,
        input: CreateItem,
        actor: &Actor,
        key: &str,
        management: Management,
    ) -> Result<Item, Error> {
        management.check_instruction(input.definition.schedule)?;
        let fingerprint = create_fingerprint(&input)?;
        let mut transaction = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        // An acknowledgement from an older version remains replayable even if
        // its original definition would now need an explicit first due date.
        if let Some(item) = replay(&mut transaction, actor, key, &fingerprint).await? {
            return Ok(item);
        }
        input.validate()?;
        insert_item(&mut transaction, &input).await?;
        let item = get_item(&mut transaction, &input.id).await?;
        save_replay(&mut transaction, actor, key, &fingerprint, &item).await?;
        transaction.commit().await?;
        Ok(item)
    }
    pub async fn import(
        &self,
        input: ImportCatalog,
        management: Management,
    ) -> Result<Page<Item>, Error> {
        if input.items.is_empty() || input.items.len() > 200 {
            return Err(Error::Invalid("import requires 1..200 items"));
        }
        for item in &input.items {
            management.check_instruction(item.definition.schedule)?;
            item.validate()?;
        }
        let mut transaction = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let mut items = Vec::with_capacity(input.items.len());
        for item in input.items {
            insert_item(&mut transaction, &item).await?;
            items.push(get_item(&mut transaction, &item.id).await?);
        }
        transaction.commit().await?;
        Ok(Page {
            items,
            next_page: None,
        })
    }
    /// Preview is one read snapshot and never creates an item, event, or receipt.
    pub async fn preview_migration(&self, input: Migration) -> Result<MigrationReport, Error> {
        input.validate()?;
        let mut transaction = self.pool.begin().await?;
        let report = migration_report(&mut transaction, &input).await?;
        transaction.commit().await?;
        Ok(report)
    }
    /// Mapping, history and the immutable request receipt commit together.
    pub async fn migrate(
        &self,
        input: Migration,
        actor: &Actor,
        key: &str,
        management: Management,
    ) -> Result<MigrationReport, Error> {
        for item in &input.items {
            management.check_instruction(item.item.definition.schedule)?;
        }
        let fingerprint = fingerprint("migrate", &input)?;
        let mut transaction = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        if let Some(report) = replay(&mut transaction, actor, key, &fingerprint).await? {
            return Ok(report);
        }
        input.validate()?;
        for item in &input.items {
            item.item.validate()?;
            if item.item.definition.schedule == Schedule::Instruction
                && item.status != ItemStatus::NeedsAction
            {
                return Err(Error::Invalid("instructions must remain active"));
            }
        }
        let report = migration_report(&mut transaction, &input).await?;
        if !report.applicable {
            return Err(Error::Conflict(
                "migration conflicts; run the preview for the mapping report",
            ));
        }
        for (input, mapping) in input.items.iter().zip(&report.items) {
            if mapping.action == MigrationAction::Unchanged {
                continue;
            }
            insert_item(&mut transaction, &input.item).await?;
            let source = serde_json::to_string(&input.source)?;
            let mut last = None;
            for event in &input.history {
                last = Some(
                    insert_imported_completion(&mut transaction, &input.item.id, event).await?,
                );
            }
            sqlx::query(MIGRATION_ITEM_UPDATE)
                .bind(input.status.as_str())
                .bind(input.archived)
                .bind(&source)
                .bind(&source)
                .bind(last)
                .bind(input.item.id.as_ref())
                .execute(&mut *transaction)
                .await?;
            sqlx::query(MIGRATION_SOURCE_INSERT)
                .bind(source)
                .bind(input.item.id.as_ref())
                .bind(fingerprint_item(input)?)
                .execute(&mut *transaction)
                .await?;
        }
        save_replay(&mut transaction, actor, key, &fingerprint, &report).await?;
        transaction.commit().await?;
        Ok(report)
    }
    pub async fn update(
        &self,
        id: &ItemId,
        revision: i64,
        input: UpdateItem,
        management: Management,
    ) -> Result<Item, Error> {
        let mut transaction = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let mut item = get_item(&mut transaction, id).await?;
        management.check_instruction(item.definition.schedule)?;
        check_revision(&item, revision)?;
        input.apply(&mut item.definition, &mut item.occurrence.due)?;
        management.check_instruction(item.definition.schedule)?;
        if item.definition.schedule == Schedule::Instruction
            && item.status != ItemStatus::NeedsAction
        {
            return Err(Error::Invalid("completed tasks cannot become instructions"));
        }
        item.occurrence.checklist = item.definition.checklist_state(&item.occurrence.checklist);
        let revision = next_revision(item.revision)?;
        sqlx::query(ITEM_UPDATE)
            .bind(serde_json::to_string(&item.definition)?)
            .bind(item.occurrence.due.map(|due| due.to_string()))
            .bind(serde_json::to_string(&item.occurrence.checklist)?)
            .bind(revision)
            .bind(id.as_ref())
            .execute(&mut *transaction)
            .await?;
        let result = get_item(&mut transaction, id).await?;
        transaction.commit().await?;
        Ok(result)
    }
    pub async fn archive(
        &self,
        id: &ItemId,
        revision: i64,
        archived: bool,
        management: Management,
    ) -> Result<Item, Error> {
        let mut transaction = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let item = get_item(&mut transaction, id).await?;
        management.check_instruction(item.definition.schedule)?;
        check_revision(&item, revision)?;
        sqlx::query(ITEM_ARCHIVE)
            .bind(archived)
            .bind(next_revision(item.revision)?)
            .bind(id.as_ref())
            .execute(&mut *transaction)
            .await?;
        let result = get_item(&mut transaction, id).await?;
        transaction.commit().await?;
        Ok(result)
    }
    pub async fn complete(
        &self,
        id: &ItemId,
        revision: i64,
        input: CompleteItem,
        actor: &Actor,
        key: &str,
    ) -> Result<Item, Error> {
        let fingerprint = fingerprint("complete", &(id, revision, &input))?;
        let mut transaction = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        // Replay precedes optimistic checks: a lost response can be retried even
        // after the item advanced, without completing its new occurrence.
        if let Some(item) = replay(&mut transaction, actor, key, &fingerprint).await? {
            return Ok(item);
        }
        let item = get_item(&mut transaction, id).await?;
        reject_instruction_action(item.definition.schedule)?;
        check_revision(&item, revision)?;
        if item.archived || item.status == ItemStatus::Completed {
            return Err(Error::Conflict(
                "archived or completed items cannot be completed",
            ));
        }
        if item.occurrence.id != input.occurrence_id {
            return Err(Error::Conflict("this occurrence is no longer current"));
        }
        let now: String = sqlx::query_scalar(DATABASE_NOW)
            .fetch_one(&mut *transaction)
            .await?;
        let completed_at = parse_time(&now)?;
        let next_due = match item.definition.schedule {
            Schedule::Recurring { interval } => Some(
                interval.next(
                    completed_at
                        .with_timezone(&item.definition.timezone.0)
                        .date_naive(),
                )?,
            ),
            Schedule::Once | Schedule::Standing => None,
            Schedule::Instruction => {
                return Err(Error::Conflict("instructions cannot be completed"));
            }
        };
        let once = item.definition.schedule == Schedule::Once;
        let event_id = Uuid::now_v7();
        let sequence = sqlx::query(COMPLETION_INSERT)
            .bind(event_id.to_string())
            .bind(id.as_ref())
            .bind(input.occurrence_id.to_string())
            .bind(now)
            .bind(serde_json::to_string(actor)?)
            .bind(item.occurrence.due.map(|due| due.to_string()))
            .bind(next_due.map(|due| due.to_string()))
            .bind(serde_json::to_string(&item.occurrence.checklist)?)
            .execute(&mut *transaction)
            .await?
            .last_insert_rowid();
        sqlx::query(OCCURRENCE_ADVANCE)
            .bind(if once { item.occurrence.due } else { next_due }.map(|due| due.to_string()))
            .bind(
                if once {
                    item.occurrence.id
                } else {
                    Uuid::now_v7()
                }
                .to_string(),
            )
            .bind(next_revision(item.revision)?)
            .bind(sequence)
            .bind(serde_json::to_string(&if once {
                item.occurrence.checklist.clone()
            } else {
                item.definition.checklist_state(&[])
            })?)
            .bind(if once { "completed" } else { "needs_action" })
            .bind(id.as_ref())
            .execute(&mut *transaction)
            .await?;
        let result = get_item(&mut transaction, id).await?;
        save_replay(&mut transaction, actor, key, &fingerprint, &result).await?;
        transaction.commit().await?;
        Ok(result)
    }

    pub async fn set_checklist_step(
        &self,
        id: &ItemId,
        step_id: Uuid,
        revision: i64,
        input: SetChecklistStep,
        actor: &Actor,
        key: &str,
    ) -> Result<Item, Error> {
        let fingerprint = fingerprint("checklist", &(id, step_id, revision, &input))?;
        let mut transaction = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        if let Some(item) = replay(&mut transaction, actor, key, &fingerprint).await? {
            return Ok(item);
        }
        let mut item = get_item(&mut transaction, id).await?;
        reject_instruction_action(item.definition.schedule)?;
        check_revision(&item, revision)?;
        if item.archived || item.status == ItemStatus::Completed {
            return Err(Error::Conflict(
                "archived or completed items cannot be checked",
            ));
        }
        if item.occurrence.id != input.occurrence_id {
            return Err(Error::Conflict("this occurrence is no longer current"));
        }
        let step = item
            .occurrence
            .checklist
            .iter_mut()
            .find(|step| step.id == step_id)
            .ok_or(Error::NotFound)?;
        if step.completion.is_some() != input.completed {
            step.completion = if input.completed {
                let now: String = sqlx::query_scalar(DATABASE_NOW)
                    .fetch_one(&mut *transaction)
                    .await?;
                Some(StepCompletion {
                    completed_at: parse_time(&now)?,
                    actor: actor.clone(),
                })
            } else {
                None
            };
            sqlx::query(CHECKLIST_UPDATE)
                .bind(serde_json::to_string(&item.occurrence.checklist)?)
                .bind(next_revision(item.revision)?)
                .bind(id.as_ref())
                .execute(&mut *transaction)
                .await?;
        }
        let result = get_item(&mut transaction, id).await?;
        save_replay(&mut transaction, actor, key, &fingerprint, &result).await?;
        transaction.commit().await?;
        Ok(result)
    }
    pub async fn reorder(
        &self,
        id: &ItemId,
        revision: i64,
        input: ReorderItem,
        actor: &Actor,
        key: &str,
    ) -> Result<ReorderReceipt, Error> {
        let fingerprint = fingerprint("reorder", &(id, revision, input))?;
        let mut transaction = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        // A durable acknowledgement outlives every mutable source check. In
        // particular a delayed retry must never become another physical press.
        if let Some(receipt) = replay(&mut transaction, actor, key, &fingerprint).await? {
            return Ok(receipt);
        }
        let item = get_item(&mut transaction, id).await?;
        reject_instruction_action(item.definition.schedule)?;
        check_revision(&item, revision)?;
        if item.archived {
            return Err(Error::Conflict("archived items cannot be reordered"));
        }
        let reorder_url = item
            .definition
            .reorder_url
            .ok_or(Error::Conflict("the item has no reorder URL"))?;
        let now: String = sqlx::query_scalar(DATABASE_NOW)
            .fetch_one(&mut *transaction)
            .await?;
        let receipt = ReorderReceipt {
            id: Uuid::now_v7(),
            item_id: item.id,
            item_revision: revision,
            requested_at: parse_time(&now)?,
            actor: actor.clone(),
            item: ReorderContent {
                name: item.definition.name,
                summary: item.definition.summary,
                description: item.definition.description,
                specs: item.definition.specs,
                resources: item.definition.resources,
                reorder_url,
            },
        };
        sqlx::query(REORDER_INSERT)
            .bind(receipt.id.to_string())
            .bind(serde_json::to_string(&receipt)?)
            .execute(&mut *transaction)
            .await?;
        save_replay(&mut transaction, actor, key, &fingerprint, &receipt).await?;
        transaction.commit().await?;
        Ok(receipt)
    }

    pub async fn get_reorder(&self, id: Uuid) -> Result<Reorder, Error> {
        let mut connection = self.pool.acquire().await?;
        get_reorder(&mut connection, id).await
    }

    pub async fn reorders(
        &self,
        status: ReorderStatus,
        cursor: Option<&str>,
        limit: u32,
    ) -> Result<Page<Reorder>, Error> {
        check_limit(limit)?;
        let after: Option<ReorderCursor> = cursor.map(decode).transpose()?;
        if after
            .as_ref()
            .is_some_and(|value| value.status != status || value.sequence <= 0)
        {
            return Err(Error::Invalid(
                "page token belongs to a different reorder filter",
            ));
        }
        let rows: Vec<ReorderRow> = if let Some(after) = after {
            sqlx::query_as(REORDER_LIST_NEXT)
                .bind(status.as_str())
                .bind(after.sequence)
                .bind(i64::from(limit) + 1)
                .fetch_all(&self.pool)
                .await?
        } else {
            sqlx::query_as(REORDER_LIST)
                .bind(status.as_str())
                .bind(i64::from(limit) + 1)
                .fetch_all(&self.pool)
                .await?
        };
        let next_page = if rows.len() > limit as usize {
            rows.get(limit as usize - 1)
                .map(|row| {
                    encode(&ReorderCursor {
                        status,
                        sequence: row.sequence,
                    })
                })
                .transpose()?
        } else {
            None
        };
        let items = rows
            .into_iter()
            .take(limit as usize)
            .map(Reorder::try_from)
            .collect::<Result<_, _>>()?;
        Ok(Page { items, next_page })
    }

    pub async fn resolve_reorder(
        &self,
        id: Uuid,
        revision: i64,
        input: ResolveReorder,
        actor: &Actor,
    ) -> Result<Reorder, Error> {
        let mut transaction = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let mut reorder = get_reorder(&mut transaction, id).await?;
        if reorder.revision != revision {
            return Err(Error::Precondition);
        }
        let status = ReorderStatus::from(input.status);
        if reorder.status == status {
            return Ok(reorder);
        }
        if reorder.status != ReorderStatus::Pending {
            return Err(Error::Conflict(
                "resolved reorder requests cannot change status",
            ));
        }
        let now: String = sqlx::query_scalar(DATABASE_NOW)
            .fetch_one(&mut *transaction)
            .await?;
        reorder.status = status;
        reorder.revision = next_revision(reorder.revision)?;
        reorder.resolved_at = Some(parse_time(&now)?);
        reorder.resolved_by = Some(actor.clone());
        sqlx::query(REORDER_RESOLVE)
            .bind(status.as_str())
            .bind(reorder.revision)
            .bind(now)
            .bind(serde_json::to_string(actor)?)
            .bind(id.to_string())
            .execute(&mut *transaction)
            .await?;
        transaction.commit().await?;
        Ok(reorder)
    }

    pub async fn summary(&self) -> Result<Summary, Error> {
        #[derive(FromRow)]
        struct DueRow {
            id: String,
            timezone: String,
            due: Option<String>,
        }
        // SQLite has no IANA timezone arithmetic. Read only dates/timezones in
        // bounded keyset pages; instructions, household metadata and history are
        // not needed to compute due counts.
        let mut transaction = self.pool.begin().await?;
        let now = Utc::now();
        let mut summary = Summary {
            active: 0,
            overdue: 0,
            due_soon: 0,
            completed: 0,
        };
        let mut after: Option<(String, String)> = None;
        loop {
            let rows: Vec<DueRow> = if let Some((due, id)) = &after {
                sqlx::query_as(SUMMARY_NEXT)
                    .bind(due)
                    .bind(id)
                    .fetch_all(&mut *transaction)
                    .await?
            } else {
                sqlx::query_as(SUMMARY_FIRST)
                    .fetch_all(&mut *transaction)
                    .await?
            };
            for row in &rows {
                let zone = crate::models::TimeZone::try_from(row.timezone.clone())
                    .map_err(|_| corrupt())?;
                let today = now.with_timezone(&zone.0).date_naive();
                summary.active += 1;
                let Some(due) = row.due.as_deref().map(parse_date).transpose()? else {
                    continue;
                };
                if due < today {
                    summary.overdue += 1;
                }
                if due >= today
                    && today
                        .checked_add_days(Days::new(14))
                        .is_some_and(|end| due <= end)
                {
                    summary.due_soon += 1;
                }
            }
            if rows.len() < 200 {
                break;
            }
            after = rows.last().map(|row| {
                (
                    row.due.clone().unwrap_or_else(|| "9999-12-31".into()),
                    row.id.clone(),
                )
            });
        }
        summary.completed = sqlx::query_scalar(COMPLETION_COUNT)
            .fetch_one(&mut *transaction)
            .await?;
        transaction.commit().await?;
        Ok(summary)
    }
}
fn reject_instruction_action(schedule: Schedule) -> Result<(), Error> {
    if schedule == Schedule::Instruction {
        Err(Error::Conflict(
            "standing instructions cannot be completed, checked or reordered",
        ))
    } else {
        Ok(())
    }
}
fn fingerprint_item(item: &MigrationItem) -> Result<Vec<u8>, Error> {
    fingerprint("migration-item", item)
}
fn count_values<T: Ord>(values: impl Iterator<Item = T>) -> BTreeMap<T, usize> {
    let mut counts = BTreeMap::new();
    for value in values {
        *counts.entry(value).or_insert(0) += 1;
    }
    counts
}
async fn migration_report(
    connection: &mut SqliteConnection,
    input: &Migration,
) -> Result<MigrationReport, Error> {
    let source_keys = input
        .items
        .iter()
        .map(|item| serde_json::to_string(&item.source))
        .collect::<Result<Vec<_>, _>>()?;
    let targets = input
        .items
        .iter()
        .map(|item| item.item.id.as_ref())
        .collect::<Vec<_>>();
    let ids = input
        .items
        .iter()
        .flat_map(|item| item.history.iter().filter_map(|event| event.id))
        .map(|id| id.to_string())
        .collect::<Vec<_>>();
    let occurrences = input
        .items
        .iter()
        .flat_map(|item| item.history.iter().filter_map(|event| event.occurrence_id))
        .map(|id| id.to_string())
        .collect::<Vec<_>>();
    let event_sources = input
        .items
        .iter()
        .flat_map(|item| {
            item.history
                .iter()
                .filter_map(|event| event.source.as_ref())
        })
        .map(serde_json::to_string)
        .collect::<Result<Vec<_>, _>>()?;
    let sources: Vec<(String, String, Vec<u8>)> = sqlx::query_as(MIGRATION_SOURCES)
        .bind(serde_json::to_string(&source_keys)?)
        .fetch_all(&mut *connection)
        .await?;
    let sources = sources
        .into_iter()
        .map(|(source, target, fingerprint)| (source, (target, fingerprint)))
        .collect::<BTreeMap<_, _>>();
    let existing_targets: BTreeSet<String> = sqlx::query_scalar::<_, String>(MIGRATION_TARGETS)
        .bind(serde_json::to_string(&targets)?)
        .fetch_all(&mut *connection)
        .await?
        .into_iter()
        .collect();
    let existing_ids: BTreeSet<String> = sqlx::query_scalar::<_, String>(MIGRATION_EVENT_IDS)
        .bind(serde_json::to_string(&ids)?)
        .fetch_all(&mut *connection)
        .await?
        .into_iter()
        .collect();
    let existing_occurrences: BTreeSet<String> =
        sqlx::query_scalar::<_, String>(MIGRATION_OCCURRENCES)
            .bind(serde_json::to_string(&occurrences)?)
            .fetch_all(&mut *connection)
            .await?
            .into_iter()
            .collect();
    let existing_event_sources: BTreeSet<String> =
        sqlx::query_scalar::<_, String>(MIGRATION_EVENT_SOURCES)
            .bind(serde_json::to_string(&event_sources)?)
            .fetch_all(&mut *connection)
            .await?
            .into_iter()
            .collect();
    let source_counts = count_values(source_keys.iter());
    let target_counts = count_values(targets.iter());
    let id_counts = count_values(ids.iter());
    let occurrence_counts = count_values(occurrences.iter());
    let event_source_counts = count_values(event_sources.iter());
    let mut mappings = Vec::with_capacity(input.items.len());
    for (item, source) in input.items.iter().zip(&source_keys) {
        let mut conflicts = Vec::new();
        if let Err(error) = validate_due(item.item.definition.schedule, item.item.due) {
            conflicts.push(error.to_string());
        }
        if item.item.definition.schedule == Schedule::Instruction
            && item.status != ItemStatus::NeedsAction
        {
            conflicts.push("instructions must remain active".into());
        }
        if source_counts.get(source).is_some_and(|count| *count > 1) {
            conflicts.push("duplicate source identity in migration".into());
        }
        if target_counts
            .get(&item.item.id.as_ref())
            .is_some_and(|count| *count > 1)
        {
            conflicts.push("duplicate target id in migration".into());
        }
        let unchanged = if let Some((target, stored)) = sources.get(source) {
            if target == item.item.id.as_ref() && *stored == fingerprint_item(item)? {
                true
            } else {
                conflicts.push(
                    "source identity was already imported with different content or target".into(),
                );
                false
            }
        } else {
            if existing_targets.contains(item.item.id.as_ref()) {
                conflicts.push("target item id already exists".into());
            }
            false
        };
        for event in &item.history {
            for (value, counts, existing, label) in [
                (
                    event.id.map(|id| id.to_string()),
                    &id_counts,
                    &existing_ids,
                    "event id",
                ),
                (
                    event.occurrence_id.map(|id| id.to_string()),
                    &occurrence_counts,
                    &existing_occurrences,
                    "event occurrence id",
                ),
                (
                    event
                        .source
                        .as_ref()
                        .map(serde_json::to_string)
                        .transpose()?,
                    &event_source_counts,
                    &existing_event_sources,
                    "event source identity",
                ),
            ] {
                if let Some(value) = value {
                    if counts.get(&value).is_some_and(|count| *count > 1) {
                        conflicts.push(format!("duplicate {label} in migration"));
                    }
                    if !unchanged && existing.contains(&value) {
                        conflicts.push(format!("{label} already exists"));
                    }
                }
            }
        }
        conflicts.sort();
        conflicts.dedup();
        let action = if !conflicts.is_empty() {
            MigrationAction::Conflict
        } else if unchanged {
            MigrationAction::Unchanged
        } else {
            MigrationAction::Created
        };
        mappings.push(MigrationMapping {
            source: item.source.clone(),
            target_id: item.item.id.clone(),
            action,
            conflicts,
            history_events: item.history.len(),
        });
    }
    Ok(MigrationReport {
        applicable: mappings.iter().all(|mapping| mapping.conflicts.is_empty()),
        items: mappings,
    })
}
async fn insert_imported_completion(
    connection: &mut SqliteConnection,
    item: &ItemId,
    event: &ImportedCompletion,
) -> Result<i64, Error> {
    let source = event
        .source
        .as_ref()
        .map(serde_json::to_string)
        .transpose()?;
    Ok(sqlx::query(MIGRATION_EVENT_INSERT)
        .bind(event.id.unwrap_or_else(Uuid::now_v7).to_string())
        .bind(item.as_ref())
        .bind(event.occurrence_id.map(|id| id.to_string()))
        .bind(event.completed_at.map(|at| at.to_rfc3339()))
        .bind(
            event
                .actor
                .as_ref()
                .map(serde_json::to_string)
                .transpose()?,
        )
        .bind(event.previous_due.map(|due| due.to_string()))
        .bind(event.next_due.map(|due| due.to_string()))
        .bind(serde_json::to_string(&event.checklist)?)
        .bind(&source)
        .bind(&source)
        .execute(connection)
        .await?
        .last_insert_rowid())
}
fn check_revision(item: &Item, revision: i64) -> Result<(), Error> {
    if item.revision != revision {
        Err(Error::Precondition)
    } else {
        Ok(())
    }
}
fn next_revision(revision: i64) -> Result<i64, Error> {
    revision
        .checked_add(1)
        .ok_or(Error::Conflict("item revision limit reached"))
}
async fn get_item(connection: &mut SqliteConnection, id: &ItemId) -> Result<Item, Error> {
    let row: ItemRow = sqlx::query_as(ITEM_GET)
        .bind(id.as_ref())
        .fetch_optional(connection)
        .await?
        .ok_or(Error::NotFound)?;
    row.try_into()
}
async fn get_reorder(connection: &mut SqliteConnection, id: Uuid) -> Result<Reorder, Error> {
    let row: ReorderRow = sqlx::query_as(REORDER_GET)
        .bind(id.to_string())
        .fetch_optional(connection)
        .await?
        .ok_or(Error::NotFound)?;
    row.try_into()
}
async fn insert_item(connection: &mut SqliteConnection, input: &CreateItem) -> Result<(), Error> {
    let result = sqlx::query(ITEM_INSERT)
        .bind(input.id.as_ref())
        .bind(serde_json::to_string(&input.definition)?)
        .bind(Uuid::now_v7().to_string())
        .bind(input.due.map(|due| due.to_string()))
        .bind(serde_json::to_string(
            &input.definition.checklist_state(&[]),
        )?)
        .execute(connection)
        .await;
    match result {
        Ok(_) => Ok(()),
        Err(sqlx::Error::Database(error)) if error.is_unique_violation() => {
            Err(Error::Conflict("item id already exists"))
        }
        Err(error) => Err(error.into()),
    }
}
fn create_fingerprint(input: &CreateItem) -> Result<Vec<u8>, Error> {
    // Keep the original canonical fingerprint for existing recurring creates.
    // Receipt JSON is upgraded by the schema migration; operation identity is not.
    #[derive(Serialize)]
    struct LegacyCreate<'a> {
        id: &'a ItemId,
        due: NaiveDate,
        name: &'a str,
        summary: &'a str,
        description: &'a Option<String>,
        timezone: crate::models::TimeZone,
        location: &'a Option<String>,
        group: &'a Option<String>,
        schedule: crate::models::Interval,
        instructions: &'a [String],
        checklist: &'a [crate::models::ChecklistStep],
        specs: &'a [crate::models::Spec],
        resources: &'a [crate::models::Resource],
        reorder_url: &'a Option<String>,
    }
    if let (Some(due), Schedule::Recurring { interval }) = (input.due, input.definition.schedule) {
        let definition = &input.definition;
        fingerprint(
            "create",
            &LegacyCreate {
                id: &input.id,
                due,
                name: &definition.name,
                summary: &definition.summary,
                description: &definition.description,
                timezone: definition.timezone,
                location: &definition.location,
                group: &definition.group,
                schedule: interval,
                instructions: &definition.instructions,
                checklist: &definition.checklist,
                specs: &definition.specs,
                resources: &definition.resources,
                reorder_url: &definition.reorder_url,
            },
        )
    } else {
        fingerprint("create", input)
    }
}
fn fingerprint(operation: &str, input: &impl Serialize) -> Result<Vec<u8>, Error> {
    let mut hash = Sha256::new();
    hash.update(operation);
    hash.update([0]);
    hash.update(serde_json::to_vec(input)?);
    Ok(hash.finalize().to_vec())
}
async fn replay<T: serde::de::DeserializeOwned>(
    connection: &mut SqliteConnection,
    actor: &Actor,
    key: &str,
    fingerprint: &[u8],
) -> Result<Option<T>, Error> {
    let row: Option<(Vec<u8>, String)> = sqlx::query_as(IDEMPOTENCY_GET)
        .bind(actor.key())
        .bind(key)
        .fetch_optional(connection)
        .await?;
    match row {
        None => Ok(None),
        Some((stored, response)) if stored == fingerprint => {
            Ok(Some(serde_json::from_str(&response)?))
        }
        Some(_) => Err(Error::Conflict(
            "idempotency key was already used for a different request",
        )),
    }
}
async fn save_replay(
    connection: &mut SqliteConnection,
    actor: &Actor,
    key: &str,
    fingerprint: &[u8],
    response: &impl Serialize,
) -> Result<(), Error> {
    sqlx::query(IDEMPOTENCY_INSERT)
        .bind(actor.key())
        .bind(key)
        .bind(fingerprint)
        .bind(serde_json::to_string(response)?)
        .execute(connection)
        .await?;
    Ok(())
}
fn encode(value: &impl Serialize) -> Result<String, Error> {
    Ok(serde_json::to_vec(value)?
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect())
}
fn decode<T: serde::de::DeserializeOwned>(value: &str) -> Result<T, Error> {
    if value.len() > 2048 || !value.len().is_multiple_of(2) || !value.is_ascii() {
        return Err(Error::Invalid("invalid page token"));
    }
    let bytes = value
        .as_bytes()
        .as_chunks::<2>()
        .0
        .iter()
        .map(|pair| {
            std::str::from_utf8(pair)
                .ok()
                .and_then(|hex| u8::from_str_radix(hex, 16).ok())
                .ok_or(Error::Invalid("invalid page token"))
        })
        .collect::<Result<Vec<_>, _>>()?;
    serde_json::from_slice(&bytes).map_err(|_| Error::Invalid("invalid page token"))
}

fn check_limit(limit: u32) -> Result<(), Error> {
    if (1..=200).contains(&limit) {
        Ok(())
    } else {
        Err(Error::Invalid("limit must be between 1 and 200"))
    }
}

#[cfg(test)]
mod tests {
    use crate::{
        models::{Actor, CompleteItem, ImportCatalog, SystemActor},
        store::*,
    };
    use sqlx::{QueryBuilder, Row, Sqlite};

    #[tokio::test]
    async fn real_queries_use_bounded_indexes_with_representative_state() {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::open(&directory.path().join("plans.sqlite3"))
            .await
            .unwrap();
        let seed: ImportCatalog =
            serde_json::from_str(include_str!("../config/items.example.json")).unwrap();
        for batch in 0..10 {
            let mut items = Vec::new();
            for index in 0..200 {
                let mut input = seed.items[0].clone();
                input.id = format!("example-{batch:02}-{index:03}").try_into().unwrap();
                input.due =
                    Some(NaiveDate::from_ymd_opt(2031, 1 + (index % 12), 1 + index % 27).unwrap());
                items.push(input);
            }
            store
                .import(ImportCatalog { items }, Management::Owner)
                .await
                .unwrap();
        }
        let actor = Actor::System {
            id: SystemActor::Owner,
            name: "Owner credential".into(),
        };
        let id: ItemId = "example-00-000".to_owned().try_into().unwrap();
        let mut item = store.get(&id).await.unwrap();
        for index in 0..20 {
            item = store
                .complete(
                    &id,
                    item.revision,
                    CompleteItem {
                        occurrence_id: item.occurrence.id,
                    },
                    &actor,
                    &format!("example-event-{index}"),
                )
                .await
                .unwrap();
        }
        for index in 1..101 {
            let other_id: ItemId = format!("example-01-{index:03}").try_into().unwrap();
            let other = store.get(&other_id).await.unwrap();
            store
                .complete(
                    &other_id,
                    other.revision,
                    CompleteItem {
                        occurrence_id: other.occurrence.id,
                    },
                    &actor,
                    &format!("example-other-{index}"),
                )
                .await
                .unwrap();
        }
        let latest = store.get(&id).await.unwrap();
        store
            .complete(
                &id,
                latest.revision,
                CompleteItem {
                    occurrence_id: latest.occurrence.id,
                },
                &actor,
                "example-latest",
            )
            .await
            .unwrap();
        sqlx::query("ANALYZE").execute(store.pool()).await.unwrap();
        let mut evidence = Vec::new();
        // Each builder uses the exact literal used by the production query,
        // with representative bound parameters, against the migrated database.
        let mut first = QueryBuilder::<Sqlite>::new("EXPLAIN QUERY PLAN ");
        first.push(ITEM_LIST).push(" ");
        let plan = first
            .build()
            .bind(false)
            .bind(101_i64)
            .fetch_all(store.pool())
            .await
            .unwrap();
        let details = plan
            .iter()
            .map(|row| row.get::<String, _>("detail"))
            .collect::<Vec<_>>();
        assert!(details.iter().any(|line| line.contains("items_due")));
        assert!(!details.iter().any(|line| line.contains("TEMP B-TREE")));
        evidence.push(("item first page", details));
        let mut next = QueryBuilder::<Sqlite>::new("EXPLAIN QUERY PLAN ");
        next.push(ITEM_LIST_NEXT);
        let plan = next
            .build()
            .bind(false)
            .bind("2031-06-15")
            .bind("example-05-100")
            .bind(101_i64)
            .fetch_all(store.pool())
            .await
            .unwrap();
        let details = plan
            .iter()
            .map(|row| row.get::<String, _>("detail"))
            .collect::<Vec<_>>();
        assert!(details.iter().any(|line| line.contains("items_due")));
        assert!(!details.iter().any(|line| line.contains("TEMP B-TREE")));
        evidence.push(("item continuation", details));
        let mut get = QueryBuilder::<Sqlite>::new("EXPLAIN QUERY PLAN ");
        get.push(ITEM_GET);
        let plan = get
            .build()
            .bind(id.as_ref())
            .fetch_all(store.pool())
            .await
            .unwrap();
        let details = plan
            .iter()
            .map(|row| row.get::<String, _>("detail"))
            .collect::<Vec<_>>();
        assert!(details.iter().all(|line| line.starts_with("SEARCH")));
        evidence.push(("item and latest completion", details));
        let mut checklist_update = QueryBuilder::<Sqlite>::new("EXPLAIN QUERY PLAN ");
        checklist_update.push(CHECKLIST_UPDATE);
        let rows = checklist_update
            .build()
            .bind(serde_json::to_string(&latest.occurrence.checklist).unwrap())
            .bind(42_i64)
            .bind(id.as_ref())
            .fetch_all(store.pool())
            .await
            .unwrap();
        let details = rows
            .iter()
            .map(|row| row.get::<String, _>("detail"))
            .collect::<Vec<_>>();
        assert!(details.iter().all(|line| line.starts_with("SEARCH")));
        evidence.push(("checklist update", details));
        let mut events = QueryBuilder::<Sqlite>::new("EXPLAIN QUERY PLAN ");
        events.push(EVENT_LIST_NEXT);
        let plan = events
            .build()
            .bind(id.as_ref())
            .bind(121_i64)
            .bind(11_i64)
            .fetch_all(store.pool())
            .await
            .unwrap();
        let details = plan
            .iter()
            .map(|row| row.get::<String, _>("detail"))
            .collect::<Vec<_>>();
        assert!(
            details
                .iter()
                .any(|line| line.contains("completions_item_sequence"))
        );
        assert!(!details.iter().any(|line| line.contains("TEMP B-TREE")));
        evidence.push(("history continuation", details));
        let mut replay = QueryBuilder::<Sqlite>::new("EXPLAIN QUERY PLAN ");
        replay.push(IDEMPOTENCY_GET);
        let plan = replay
            .build()
            .bind(actor.key())
            .bind("example-event-10")
            .fetch_all(store.pool())
            .await
            .unwrap();
        let details = plan
            .iter()
            .map(|row| row.get::<String, _>("detail"))
            .collect::<Vec<_>>();
        assert!(details.iter().all(|line| line.starts_with("SEARCH")));
        evidence.push(("idempotency lookup", details));
        let mut summary = QueryBuilder::<Sqlite>::new("EXPLAIN QUERY PLAN ");
        summary.push(SUMMARY_NEXT);
        let plan = summary
            .build()
            .bind("2031-06-15")
            .bind("example-05-100")
            .fetch_all(store.pool())
            .await
            .unwrap();
        let details = plan
            .iter()
            .map(|row| row.get::<String, _>("detail"))
            .collect::<Vec<_>>();
        assert!(details.iter().any(|line| line.contains("items_active_due")));
        assert!(!details.iter().any(|line| line.contains("TEMP B-TREE")));
        evidence.push(("summary continuation", details));
        for (name, query) in [
            ("summary first page", SUMMARY_FIRST),
            ("completion count", COMPLETION_COUNT),
            ("database clock", DATABASE_NOW),
        ] {
            let mut explain = QueryBuilder::<Sqlite>::new("EXPLAIN QUERY PLAN ");
            explain.push(query);
            let rows = explain.build().fetch_all(store.pool()).await.unwrap();
            evidence.push((
                name,
                rows.iter()
                    .map(|row| row.get::<String, _>("detail"))
                    .collect(),
            ));
        }
        let mut history_first = QueryBuilder::<Sqlite>::new("EXPLAIN QUERY PLAN ");
        history_first.push(EVENT_LIST);
        let rows = history_first
            .build()
            .bind(id.as_ref())
            .bind(11_i64)
            .fetch_all(store.pool())
            .await
            .unwrap();
        evidence.push((
            "history first page",
            rows.iter()
                .map(|row| row.get::<String, _>("detail"))
                .collect(),
        ));
        // Compile actual write programs with representative values. EXPLAIN
        // validates their SQLite programs without executing the mutations.
        enum Parameter {
            Text(String),
            Integer(i64),
            Blob(Vec<u8>),
        }
        let definition = serde_json::to_string(&seed.items[0].definition).unwrap();
        let checklist =
            serde_json::to_string(&seed.items[0].definition.checklist_state(&[])).unwrap();
        let actor_json = serde_json::to_string(&actor).unwrap();
        let response = serde_json::to_string(&item).unwrap();
        let event_id = Uuid::now_v7().to_string();
        let occurrence = Uuid::now_v7().to_string();
        use Parameter::{Blob, Integer, Text};
        for (name, query, parameters) in [
            (
                "insert item",
                ITEM_INSERT,
                vec![
                    Text("example-plan".into()),
                    Text(definition.clone()),
                    Text(occurrence.clone()),
                    Text("2031-02-17".into()),
                    Text(checklist.clone()),
                ],
            ),
            (
                "update item",
                ITEM_UPDATE,
                vec![
                    Text(definition),
                    Text("2031-02-17".into()),
                    Text(checklist.clone()),
                    Integer(42),
                    Text(id.as_ref().into()),
                ],
            ),
            (
                "update checklist",
                CHECKLIST_UPDATE,
                vec![
                    Text(checklist.clone()),
                    Integer(42),
                    Text(id.as_ref().into()),
                ],
            ),
            (
                "archive",
                ITEM_ARCHIVE,
                vec![Integer(1), Integer(42), Text(id.as_ref().into())],
            ),
            (
                "insert completion",
                COMPLETION_INSERT,
                vec![
                    Text(event_id),
                    Text(id.as_ref().into()),
                    Text(occurrence.clone()),
                    Text("2031-02-17T12:00:00Z".into()),
                    Text(actor_json),
                    Text("2031-02-17".into()),
                    Text("2031-04-05".into()),
                    Text(checklist.clone()),
                ],
            ),
            (
                "advance occurrence",
                OCCURRENCE_ADVANCE,
                vec![
                    Text("2031-04-05".into()),
                    Text(occurrence),
                    Integer(42),
                    Integer(20),
                    Text(checklist),
                    Text("needs_action".into()),
                    Text(id.as_ref().into()),
                ],
            ),
            (
                "save idempotency",
                IDEMPOTENCY_INSERT,
                vec![
                    Text(actor.key()),
                    Text("example-plan-key".into()),
                    Blob(vec![0; 32]),
                    Text(response),
                ],
            ),
        ] {
            let mut explain = QueryBuilder::<Sqlite>::new("EXPLAIN ");
            explain.push(query);
            let mut program = explain.build();
            for parameter in parameters {
                program = match parameter {
                    Text(value) => program.bind(value),
                    Integer(value) => program.bind(value),
                    Blob(value) => program.bind(value),
                };
            }
            let rows = program.fetch_all(store.pool()).await.unwrap();
            assert!(!rows.is_empty());
            evidence.push((
                name,
                vec![format!(
                    "{} SQLite VM instructions, representative bindings",
                    rows.len()
                )],
            ));
        }
        assert_eq!(store.summary().await.unwrap().active, 2_000);
        let report = serde_json::to_string_pretty(&evidence).unwrap();
        println!("{report}");
        if let Ok(path) = std::env::var("MAINTENANCE_QUERY_PLAN_OUTPUT") {
            std::fs::write(path, report).unwrap();
        }
        store.close().await.unwrap();
    }
    #[tokio::test]
    async fn migration_fidelity_upgrade_preserves_existing_state_and_all_receipt_kinds() {
        use crate::models::{ChecklistStepState, ReorderContent, ReorderItem, ReorderReceipt};
        let directory = tempfile::tempdir().unwrap();
        let migration_directory = directory.path().join("old-migrations");
        std::fs::create_dir(&migration_directory).unwrap();
        for (name, sql) in [
            (
                "0001_maintenance.sql",
                include_str!("../migrations/0001_maintenance.sql"),
            ),
            (
                "0002_reorders.sql",
                include_str!("../migrations/0002_reorders.sql"),
            ),
        ] {
            std::fs::write(migration_directory.join(name), sql).unwrap();
        }
        let database = directory.path().join("upgrade.sqlite3");
        let pool = SqlitePoolOptions::new()
            .connect_with(
                SqliteConnectOptions::new()
                    .filename(&database)
                    .create_if_missing(true)
                    .foreign_keys(true),
            )
            .await
            .unwrap();
        sqlx::migrate::Migrator::new(migration_directory.as_path())
            .await
            .unwrap()
            .run(&pool)
            .await
            .unwrap();
        let seed: ImportCatalog =
            serde_json::from_str(include_str!("../config/items.example.json")).unwrap();
        let input = seed.items[0].clone();
        let actor = Actor::System {
            id: SystemActor::Owner,
            name: "Owner credential".into(),
        };
        let mut original = Item {
            id: input.id.clone(),
            definition: input.definition.clone(),
            revision: 1,
            archived: false,
            status: ItemStatus::NeedsAction,
            source: None,
            occurrence: Occurrence {
                id: Uuid::now_v7(),
                due: input.due,
                checklist: input.definition.checklist_state(&[]),
            },
            last_completion: None,
        };
        // These transformations construct the actual 0.2 persisted JSON shapes, not a new-schema Store against old tables.
        let old_definition: String = sqlx::query_scalar(
            "SELECT json_set(?1,'$.schedule',json_extract(?1,'$.schedule.interval'))",
        )
        .bind(serde_json::to_string(&input.definition).unwrap())
        .fetch_one(&pool)
        .await
        .unwrap();
        sqlx::query("INSERT INTO items(id,definition,revision,occurrence_id,due,checklist) VALUES(?,?,1,?,?,?)").bind(input.id.as_ref()).bind(old_definition).bind(original.occurrence.id.to_string()).bind(input.due.unwrap().to_string()).bind(serde_json::to_string(&original.occurrence.checklist).unwrap()).execute(&pool).await.unwrap();
        let at = parse_time("2025-02-03T04:05:06Z").unwrap();
        if original.occurrence.checklist.is_empty() {
            original
                .definition
                .checklist
                .push(crate::models::ChecklistStep {
                    id: Uuid::from_u128(1),
                    summary: "Inspect example".into(),
                });
            original.occurrence.checklist = original.definition.checklist_state(&[]);
        }
        let step_id = original.occurrence.checklist[0].id;
        let mut checked = original.clone();
        checked.revision = 2;
        checked.occurrence.checklist[0].completion = Some(StepCompletion {
            completed_at: at,
            actor: actor.clone(),
        });
        let event = Completion {
            id: Uuid::now_v7(),
            item_id: input.id.clone(),
            occurrence_id: Some(original.occurrence.id),
            completed_at: Some(at),
            actor: Some(actor.clone()),
            previous_due: input.due,
            next_due: Some(NaiveDate::from_ymd_opt(2031, 4, 5).unwrap()),
            checklist: checked.occurrence.checklist.clone(),
            source: None,
        };
        let sequence = sqlx::query(COMPLETION_INSERT)
            .bind(event.id.to_string())
            .bind(input.id.as_ref())
            .bind(original.occurrence.id.to_string())
            .bind(at.to_rfc3339())
            .bind(serde_json::to_string(&actor).unwrap())
            .bind(event.previous_due.unwrap().to_string())
            .bind(event.next_due.unwrap().to_string())
            .bind(serde_json::to_string(&event.checklist).unwrap())
            .execute(&pool)
            .await
            .unwrap()
            .last_insert_rowid();
        let mut completed = checked.clone();
        completed.revision = 3;
        completed.occurrence = Occurrence {
            id: Uuid::now_v7(),
            due: event.next_due,
            checklist: Vec::<ChecklistStepState>::new(),
        };
        completed.last_completion = Some(event);
        sqlx::query("UPDATE items SET revision=3,occurrence_id=?,due=?,checklist=?,last_completion_id=? WHERE id=?").bind(completed.occurrence.id.to_string()).bind(completed.occurrence.due.unwrap().to_string()).bind("[]").bind(sequence).bind(input.id.as_ref()).execute(&pool).await.unwrap();
        let completion = CompleteItem {
            occurrence_id: original.occurrence.id,
        };
        let check = SetChecklistStep {
            occurrence_id: original.occurrence.id,
            completed: true,
        };
        for (key, hash, item) in [
            ("old-create", create_fingerprint(&input).unwrap(), &original),
            (
                "old-check",
                fingerprint("checklist", &(&original.id, step_id, 1_i64, &check)).unwrap(),
                &checked,
            ),
            (
                "old-complete",
                fingerprint("complete", &(&original.id, 2_i64, &completion)).unwrap(),
                &completed,
            ),
        ] {
            let response:String=sqlx::query_scalar("SELECT json_remove(json_set(?1,'$.schedule',json_extract(?1,'$.schedule.interval')),'$.status','$.source','$.last_completion.source')").bind(serde_json::to_string(item).unwrap()).fetch_one(&pool).await.unwrap();
            sqlx::query(IDEMPOTENCY_INSERT)
                .bind(actor.key())
                .bind(key)
                .bind(hash)
                .bind(response)
                .execute(&pool)
                .await
                .unwrap();
        }
        let reorder = ReorderReceipt {
            id: Uuid::now_v7(),
            item_id: original.id.clone(),
            item_revision: 3,
            requested_at: at,
            actor: actor.clone(),
            item: ReorderContent {
                name: input.definition.name.clone(),
                summary: input.definition.summary.clone(),
                description: input.definition.description.clone(),
                specs: vec![],
                resources: vec![],
                reorder_url: "https://example.invalid/part".into(),
            },
        };
        let reorder_json = serde_json::to_string(&reorder).unwrap();
        sqlx::query(REORDER_INSERT)
            .bind(reorder.id.to_string())
            .bind(&reorder_json)
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query(IDEMPOTENCY_INSERT)
            .bind(actor.key())
            .bind("old-reorder")
            .bind(fingerprint("reorder", &(&original.id, 3_i64, &ReorderItem::default())).unwrap())
            .bind(&reorder_json)
            .execute(&pool)
            .await
            .unwrap();
        pool.close().await;
        let upgraded = Store::open(&database).await.unwrap();
        let current = upgraded.get(&input.id).await.unwrap();
        assert_eq!(current.occurrence.id, completed.occurrence.id);
        assert_eq!(current.occurrence.due, completed.occurrence.due);
        assert_eq!(current.definition.schedule, input.definition.schedule);
        assert_eq!(
            upgraded
                .events(&input.id, None, 100)
                .await
                .unwrap()
                .items
                .len(),
            1
        );
        assert_eq!(
            serde_json::to_vec(
                &upgraded
                    .create(input.clone(), &actor, "old-create", Management::Owner)
                    .await
                    .unwrap()
            )
            .unwrap(),
            serde_json::to_vec(&original).unwrap()
        );
        assert_eq!(
            serde_json::to_vec(
                &upgraded
                    .set_checklist_step(&original.id, step_id, 1, check, &actor, "old-check")
                    .await
                    .unwrap()
            )
            .unwrap(),
            serde_json::to_vec(&checked).unwrap()
        );
        assert_eq!(
            serde_json::to_vec(
                &upgraded
                    .complete(&original.id, 2, completion, &actor, "old-complete")
                    .await
                    .unwrap()
            )
            .unwrap(),
            serde_json::to_vec(&completed).unwrap()
        );
        assert_eq!(
            serde_json::to_string(
                &upgraded
                    .reorder(
                        &original.id,
                        3,
                        ReorderItem::default(),
                        &actor,
                        "old-reorder"
                    )
                    .await
                    .unwrap()
            )
            .unwrap(),
            reorder_json
        );
        let foreign_keys: Vec<(String, i64, String, i64)> =
            sqlx::query_as("PRAGMA foreign_key_check")
                .fetch_all(upgraded.pool())
                .await
                .unwrap();
        assert!(foreign_keys.is_empty());
        upgraded.close().await.unwrap();
    }
    #[tokio::test]
    async fn reorder_queries_use_status_sequence_and_identity_indexes() {
        use crate::models::{ReorderItem, ReorderResolution, ReorderStatus, ResolveReorder};
        let directory = tempfile::tempdir().unwrap();
        let store = Store::open(&directory.path().join("reorder-plans.sqlite3"))
            .await
            .unwrap();
        let seed: ImportCatalog =
            serde_json::from_str(include_str!("../config/items.example.json")).unwrap();
        let mut input = seed.items[0].clone();
        input.definition.reorder_url = Some("https://example.invalid/replacement".into());
        let actor = Actor::System {
            id: SystemActor::Integration,
            name: "Home Assistant integration".into(),
        };
        let item = store
            .create(input, &actor, "seed-reorder-plans", Management::Owner)
            .await
            .unwrap();
        let mut sample = None;
        for index in 0..600 {
            let receipt = store
                .reorder(
                    &item.id,
                    item.revision,
                    ReorderItem::default(),
                    &actor,
                    &format!("plan-press-{index}"),
                )
                .await
                .unwrap();
            if index % 3 != 0 {
                store
                    .resolve_reorder(
                        receipt.id,
                        1,
                        ResolveReorder {
                            status: if index % 3 == 1 {
                                ReorderResolution::Fulfilled
                            } else {
                                ReorderResolution::Dismissed
                            },
                        },
                        &actor,
                    )
                    .await
                    .unwrap();
            }
            sample = Some(receipt);
        }
        sqlx::query("ANALYZE").execute(store.pool()).await.unwrap();
        let receipt = sample.unwrap();
        enum Parameter {
            Text(String),
            Integer(i64),
        }
        use Parameter::{Integer, Text};
        let queries = [
            (
                "reorder first page",
                REORDER_LIST,
                vec![Text("pending".into()), Integer(101)],
            ),
            (
                "reorder continuation",
                REORDER_LIST_NEXT,
                vec![Text("pending".into()), Integer(300), Integer(101)],
            ),
            (
                "reorder receipt lookup",
                REORDER_GET,
                vec![Text(receipt.id.to_string())],
            ),
            (
                "reorder insertion",
                REORDER_INSERT,
                vec![
                    Text(Uuid::now_v7().to_string()),
                    Text(serde_json::to_string(&receipt).unwrap()),
                ],
            ),
            (
                "reorder resolution",
                REORDER_RESOLVE,
                vec![
                    Text("fulfilled".into()),
                    Integer(2),
                    Text("2031-02-17T12:00:00Z".into()),
                    Text(serde_json::to_string(&actor).unwrap()),
                    Text(receipt.id.to_string()),
                ],
            ),
        ];
        let mut evidence = Vec::new();
        for (name, query, parameters) in queries {
            let mut details = Vec::new();
            for prefix in ["EXPLAIN QUERY PLAN ", "EXPLAIN "] {
                let mut builder = QueryBuilder::<Sqlite>::new(prefix);
                builder.push(query);
                let mut statement = builder.build();
                for parameter in &parameters {
                    statement = match parameter {
                        Text(value) => statement.bind(value),
                        Integer(value) => statement.bind(value),
                    };
                }
                let rows = statement.fetch_all(store.pool()).await.unwrap();
                if prefix == "EXPLAIN QUERY PLAN " {
                    details = rows
                        .iter()
                        .map(|row| row.get::<String, _>("detail"))
                        .collect::<Vec<_>>();
                    assert!(
                        !details
                            .iter()
                            .any(|line| line.contains("TEMP B-TREE") || line.starts_with("SCAN")),
                        "{name}: {details:?}"
                    );
                    if name == "reorder first page" || name == "reorder continuation" {
                        // sequence aliases rowid. For this continuation's remaining
                        // 300 rows, SQLite may prefer that ordered range seek to
                        // secondary-index lookups for the 100 pending receipts.
                        let sequence_seek = name == "reorder continuation"
                            && details == ["SEARCH reorders USING INTEGER PRIMARY KEY (rowid>?)"];
                        assert!(
                            sequence_seek
                                || details
                                    .iter()
                                    .any(|line| line.contains("reorders_status_sequence")),
                            "{details:?}"
                        );
                    } else if name != "reorder insertion" {
                        assert!(
                            details.iter().all(|line| line.starts_with("SEARCH")),
                            "{details:?}"
                        );
                    }
                } else {
                    assert!(!rows.is_empty());
                    details.push(format!(
                        "{} SQLite VM instructions, representative bindings",
                        rows.len()
                    ));
                }
            }
            evidence.push((name, details));
        }
        let page = store
            .reorders(ReorderStatus::Pending, None, 100)
            .await
            .unwrap();
        assert_eq!(page.items.len(), 100);
        assert!(page.next_page.is_some());
        let continuation: Vec<ReorderRow> = sqlx::query_as(REORDER_LIST_NEXT)
            .bind("pending")
            .bind(300)
            .bind(101)
            .fetch_all(store.pool())
            .await
            .unwrap();
        assert_eq!(continuation.len(), 100);
        assert_eq!(continuation.first().unwrap().sequence, 301);
        assert_eq!(continuation.last().unwrap().sequence, 598);
        assert!(continuation.iter().all(|row| row.status == "pending"));
        assert!(
            continuation
                .windows(2)
                .all(|rows| rows[1].sequence - rows[0].sequence == 3)
        );
        let report = serde_json::to_string_pretty(&evidence).unwrap();
        println!("{report}");
        if let Ok(path) = std::env::var("MAINTENANCE_REORDER_QUERY_PLAN_OUTPUT") {
            std::fs::write(path, report).unwrap();
        }
        store.close().await.unwrap();
    }
    #[tokio::test]
    async fn summary_counts_one_snapshot_while_items_move_between_pages() {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::open(&directory.path().join("summary.sqlite3"))
            .await
            .unwrap();
        let seed: ImportCatalog =
            serde_json::from_str(include_str!("../config/items.example.json")).unwrap();
        for batch in 0..3 {
            let items = (0..200)
                .map(|index| {
                    let mut input = seed.items[0].clone();
                    input.id = format!("example-{batch:02}-{index:03}").try_into().unwrap();
                    input
                })
                .collect();
            store
                .import(ImportCatalog { items }, Management::Owner)
                .await
                .unwrap();
        }
        let writer = store.clone();
        let mutations = tokio::spawn(async move {
            let id: ItemId = "example-00-000".to_owned().try_into().unwrap();
            for index in 0..80 {
                let item = writer.get(&id).await.unwrap();
                writer
                    .update(
                        &id,
                        item.revision,
                        crate::models::UpdateItem {
                            due: crate::models::Change(Some(Some(
                                NaiveDate::from_ymd_opt(
                                    if index % 2 == 0 { 2030 } else { 2032 },
                                    1,
                                    1,
                                )
                                .unwrap(),
                            ))),
                            ..Default::default()
                        },
                        Management::Owner,
                    )
                    .await
                    .unwrap();
            }
        });
        for _ in 0..20 {
            assert_eq!(store.summary().await.unwrap().active, 600);
        }
        mutations.await.unwrap();
        store.close().await.unwrap();
    }
    #[tokio::test]
    async fn migration_queries_use_indexed_batch_lookups_and_upgrade_null_safe_ordering() {
        use crate::models::{ImportedCompletion, MigrationItem, SourceRef};
        let directory = tempfile::tempdir().unwrap();
        let store = Store::open(&directory.path().join("migration-plans.sqlite3"))
            .await
            .unwrap();
        let seed: ImportCatalog =
            serde_json::from_str(include_str!("../config/items.example.json")).unwrap();
        let actor = Actor::System {
            id: SystemActor::Owner,
            name: "Owner credential".into(),
        };
        let legacy = SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .unwrap();
        sqlx::raw_sql(include_str!("../migrations/0001_maintenance.sql"))
            .execute(&legacy)
            .await
            .unwrap();
        for batch in 0..4 {
            let mut items = Vec::new();
            for index in 0..200 {
                let mut input = seed.items[0].clone();
                input.id = format!("example-plan-{batch}-{index:03}")
                    .try_into()
                    .unwrap();
                input.due = if index % 4 == 0 {
                    None
                } else {
                    Some(NaiveDate::from_ymd_opt(2031, 1 + (index % 12), 1 + index % 27).unwrap())
                };
                if input.due.is_none() {
                    input.definition.schedule = Schedule::Once;
                }
                let source = SourceRef {
                    system: "example-source".into(),
                    collection: "example-list".into(),
                    id: format!("item-{batch}-{index}"),
                };
                let event_source = SourceRef {
                    system: "example-source".into(),
                    collection: "example-history".into(),
                    id: format!("event-{batch}-{index}"),
                };
                items.push(MigrationItem {
                    source,
                    item: input.clone(),
                    status: if index % 3 == 0 {
                        ItemStatus::Completed
                    } else {
                        ItemStatus::NeedsAction
                    },
                    archived: index % 5 == 0,
                    history: vec![ImportedCompletion {
                        source: Some(event_source),
                        ..Default::default()
                    }],
                });
                sqlx::query("INSERT INTO items(id,definition,revision,archived,occurrence_id,due,checklist) VALUES(?,?,1,?,?,?,?)")
                    .bind(input.id.as_ref()).bind(serde_json::to_string(&input.definition).unwrap()).bind(index%5==0).bind(Uuid::now_v7().to_string())
                    .bind(input.due.unwrap_or_else(|| NaiveDate::from_ymd_opt(2031,1,1).unwrap()).to_string()).bind("[]").execute(&legacy).await.unwrap();
            }
            store
                .migrate(
                    Migration { items },
                    &actor,
                    &format!("seed-{batch}"),
                    Management::Owner,
                )
                .await
                .unwrap();
        }
        sqlx::query("ANALYZE").execute(store.pool()).await.unwrap();
        sqlx::query("ANALYZE").execute(&legacy).await.unwrap();
        enum Parameter {
            Text(String),
            Integer(i64),
            Blob(Vec<u8>),
        }
        use Parameter::{Blob, Integer, Text};
        let source = serde_json::to_string(&SourceRef {
            system: "example-source".into(),
            collection: "example-list".into(),
            id: "item-0-001".into(),
        })
        .unwrap();
        let event_source = serde_json::to_string(&SourceRef {
            system: "example-source".into(),
            collection: "example-history".into(),
            id: "event-0-001".into(),
        })
        .unwrap();
        let id: ItemId = "example-plan-0-001".to_owned().try_into().unwrap();
        let item = store.get(&id).await.unwrap();
        let event = item.last_completion.as_ref().unwrap();
        let receipt = MigrationReport {
            items: vec![],
            applicable: true,
        };
        let queries = [
            (
                "migration source mappings",
                MIGRATION_SOURCES,
                vec![Text(serde_json::to_string(&[&source]).unwrap())],
            ),
            (
                "migration target ids",
                MIGRATION_TARGETS,
                vec![Text(serde_json::to_string(&[id.as_ref()]).unwrap())],
            ),
            (
                "migration event ids",
                MIGRATION_EVENT_IDS,
                vec![Text(
                    serde_json::to_string(&[event.id.to_string()]).unwrap(),
                )],
            ),
            (
                "migration live and historical occurrences",
                MIGRATION_OCCURRENCES,
                vec![Text(
                    serde_json::to_string(&[item.occurrence.id.to_string()]).unwrap(),
                )],
            ),
            (
                "migration event sources",
                MIGRATION_EVENT_SOURCES,
                vec![Text(serde_json::to_string(&[&event_source]).unwrap())],
            ),
            (
                "migration source insertion",
                MIGRATION_SOURCE_INSERT,
                vec![
                    Text(source.clone()),
                    Text("example-new-plan".into()),
                    Blob(vec![0; 32]),
                ],
            ),
            (
                "migration item state",
                MIGRATION_ITEM_UPDATE,
                vec![
                    Text("completed".into()),
                    Integer(0),
                    Text(source.clone()),
                    Text(source),
                    Integer(1),
                    Text(id.as_ref().into()),
                ],
            ),
            (
                "migration history insertion",
                MIGRATION_EVENT_INSERT,
                vec![
                    Text(Uuid::now_v7().to_string()),
                    Text(id.as_ref().into()),
                    Text(Uuid::now_v7().to_string()),
                    Text("2025-02-03T04:05:06Z".into()),
                    Text(serde_json::to_string(&actor).unwrap()),
                    Text("2031-01-01".into()),
                    Text("2031-02-01".into()),
                    Text("[]".into()),
                    Text(event_source.clone()),
                    Text(event_source),
                ],
            ),
            (
                "migration receipt insertion",
                IDEMPOTENCY_INSERT,
                vec![
                    Text(actor.key()),
                    Text("plan-migration-receipt".into()),
                    Blob(vec![0; 32]),
                    Text(serde_json::to_string(&receipt).unwrap()),
                ],
            ),
        ];
        let mut evidence = Vec::new();
        for (name, query, parameters) in queries {
            let mut details = Vec::new();
            for prefix in ["EXPLAIN QUERY PLAN ", "EXPLAIN "] {
                let mut builder = QueryBuilder::<Sqlite>::new(prefix);
                builder.push(query);
                let mut statement = builder.build();
                for parameter in &parameters {
                    statement = match parameter {
                        Text(value) => statement.bind(value),
                        Integer(value) => statement.bind(value),
                        Blob(value) => statement.bind(value),
                    };
                }
                let rows = statement.fetch_all(store.pool()).await.unwrap();
                if prefix == "EXPLAIN QUERY PLAN " {
                    details = rows
                        .iter()
                        .map(|row| row.get::<String, _>("detail"))
                        .collect::<Vec<_>>();
                    assert!(
                        !details.iter().any(|line| line.contains("SCAN items")
                            || line.contains("SCAN completions")
                            || line.contains("SCAN migration_sources")
                            || line.contains("TEMP B-TREE")),
                        "{name}: {details:?}"
                    );
                    if !name.ends_with("insertion") {
                        assert!(
                            details.iter().any(|line| line.starts_with("SEARCH")),
                            "{name}: {details:?}"
                        );
                    }
                } else {
                    assert!(!rows.is_empty());
                    details.push(format!(
                        "{} SQLite VM instructions, representative bindings",
                        rows.len()
                    ));
                }
            }
            evidence.push((name, details));
        }
        // These are the exact 0.2 ordering queries; compare their real plans on
        // the old schema with the production null-safe queries on the new one.
        const OLD_SELECT: &str = "SELECT i.id,i.definition,i.revision,i.archived,i.occurrence_id,i.due,i.checklist,c.id AS completion_id,c.occurrence_id AS completed_occurrence,c.completed_at,c.actor,c.previous_due,c.next_due,c.checklist AS completion_checklist FROM items i LEFT JOIN completions c ON c.sequence=i.last_completion_id";
        const OLD_SUMMARY_FIRST: &str = "SELECT id,json_extract(definition,'$.timezone') AS timezone,due FROM items WHERE archived=0 ORDER BY due,id LIMIT 200";
        const OLD_SUMMARY_NEXT: &str = "SELECT id,json_extract(definition,'$.timezone') AS timezone,due FROM items WHERE archived=0 AND (due,id)>(?,?) ORDER BY due,id LIMIT 200";
        for (name, new, old_suffix, summary, parameters) in [
            (
                "item first page",
                ITEM_LIST,
                " WHERE i.archived=? ORDER BY i.due,i.id LIMIT ?",
                false,
                vec![Integer(0), Integer(101)],
            ),
            (
                "item continuation",
                ITEM_LIST_NEXT,
                " WHERE i.archived=? AND (i.due,i.id)>(?,?) ORDER BY i.due,i.id LIMIT ?",
                false,
                vec![
                    Integer(0),
                    Text("2031-06-15".into()),
                    Text("example-plan-0-100".into()),
                    Integer(101),
                ],
            ),
            (
                "summary first page",
                SUMMARY_FIRST,
                OLD_SUMMARY_FIRST,
                true,
                vec![],
            ),
            (
                "summary continuation",
                SUMMARY_NEXT,
                OLD_SUMMARY_NEXT,
                true,
                vec![Text("2031-06-15".into()), Text("example-plan-0-100".into())],
            ),
        ] {
            for (phase, pool) in [("before", &legacy), ("after", store.pool())] {
                let mut builder = QueryBuilder::<Sqlite>::new("EXPLAIN QUERY PLAN ");
                if phase == "after" {
                    builder.push(new);
                } else if summary {
                    builder.push(old_suffix);
                } else {
                    builder.push(OLD_SELECT).push(old_suffix);
                }
                let mut statement = builder.build();
                for parameter in &parameters {
                    statement = match parameter {
                        Text(value) => statement.bind(value),
                        Integer(value) => statement.bind(value),
                        Blob(value) => statement.bind(value),
                    };
                }
                let rows = statement.fetch_all(pool).await.unwrap();
                let details = rows
                    .iter()
                    .map(|row| row.get::<String, _>("detail"))
                    .collect::<Vec<_>>();
                assert!(
                    !details.iter().any(|line| line.contains("TEMP B-TREE")),
                    "{name} {phase}: {details:?}"
                );
                assert!(
                    details
                        .iter()
                        .any(|line| line.contains(if summary && phase == "after" {
                            "items_active_due"
                        } else {
                            "items_due"
                        })),
                    "{name} {phase}: {details:?}"
                );
                evidence.push((
                    if phase == "before" {
                        match name {
                            "item first page" => "before item first page",
                            "item continuation" => "before item continuation",
                            "summary first page" => "before summary first page",
                            _ => "before summary continuation",
                        }
                    } else {
                        match name {
                            "item first page" => "after item first page",
                            "item continuation" => "after item continuation",
                            "summary first page" => "after summary first page",
                            _ => "after summary continuation",
                        }
                    },
                    details,
                ));
            }
        }
        let report = serde_json::to_string_pretty(&evidence).unwrap();
        println!("{report}");
        if let Ok(path) = std::env::var("MAINTENANCE_MIGRATION_QUERY_PLAN_OUTPUT") {
            std::fs::write(path, report).unwrap();
        }
        store.close().await.unwrap();
        legacy.close().await;
    }
    #[tokio::test]
    async fn instruction_summary_filters_across_pages_without_changing_the_index_plan() {
        // These are the exact 0.3 summary statements, retained only to compare the
        // changed query against the same representative SQLite dataset.
        const PREVIOUS_FIRST: &str = "SELECT id,json_extract(definition,'$.timezone') AS timezone,due FROM items WHERE archived=0 AND status='needs_action' ORDER BY sort_due,id LIMIT 200";
        const PREVIOUS_NEXT: &str = "SELECT id,json_extract(definition,'$.timezone') AS timezone,due FROM items WHERE archived=0 AND status='needs_action' AND (sort_due,id)>(?,?) ORDER BY sort_due,id LIMIT 200";
        let directory = tempfile::tempdir().unwrap();
        let store = Store::open(&directory.path().join("instruction-plans.sqlite3"))
            .await
            .unwrap();
        let seed: ImportCatalog =
            serde_json::from_str(include_str!("../config/items.example.json")).unwrap();
        let today = Utc::now().date_naive();
        let mut inputs = Vec::new();
        for index in 0..700 {
            let mut input = seed.items[0].clone();
            input.definition.timezone =
                crate::models::TimeZone::try_from("Etc/UTC".to_owned()).unwrap();
            if index < 300 {
                input.id = format!("example-instruction-{index:03}")
                    .try_into()
                    .unwrap();
                input.definition.schedule = Schedule::Instruction;
                input.definition.checklist.clear();
                input.definition.reorder_url = None;
                input.due = None;
            } else {
                input.id = format!("example-task-{index:03}").try_into().unwrap();
                input.definition.schedule = Schedule::Standing;
                input.due = (index < 550).then_some(today);
            }
            inputs.push(input);
        }
        for items in inputs.chunks(200) {
            store
                .import(
                    ImportCatalog {
                        items: items.to_vec(),
                    },
                    Management::Owner,
                )
                .await
                .unwrap();
        }
        sqlx::query("ANALYZE").execute(store.pool()).await.unwrap();
        let mut evidence = Vec::new();
        for (name, statement, continuation) in [
            ("0.3 first page", PREVIOUS_FIRST, false),
            ("0.4 first page", SUMMARY_FIRST, false),
            ("0.3 continuation", PREVIOUS_NEXT, true),
            ("0.4 continuation", SUMMARY_NEXT, true),
        ] {
            let mut explain = QueryBuilder::<Sqlite>::new("EXPLAIN QUERY PLAN ");
            explain.push(statement);
            let mut query = explain.build();
            if continuation {
                query = query.bind(today.to_string()).bind("example-task-499");
            }
            let rows = query.fetch_all(store.pool()).await.unwrap();
            let details: Vec<String> = rows.iter().map(|row| row.get("detail")).collect();
            assert!(
                details.iter().any(|line| line.contains("items_active_due")),
                "{name}: {details:?}"
            );
            assert!(
                !details
                    .iter()
                    .any(|line| line.starts_with("SCAN") || line.contains("TEMP B-TREE")),
                "{name}: {details:?}"
            );
            evidence.push((name, details));
        }
        let mut measured = Vec::new();
        for (version, first, next, expected) in [
            ("0.3", PREVIOUS_FIRST, PREVIOUS_NEXT, 700),
            ("0.4", SUMMARY_FIRST, SUMMARY_NEXT, 400),
        ] {
            let started = std::time::Instant::now();
            let mut count = 0;
            let mut after: Option<(String, String)> = None;
            let mut pages = 0;
            loop {
                let mut query = sqlx::query(if after.is_some() { next } else { first });
                if let Some((due, id)) = &after {
                    query = query.bind(due).bind(id);
                }
                let rows = query.fetch_all(store.pool()).await.unwrap();
                count += rows.len();
                pages += 1;
                if rows.len() < 200 {
                    break;
                }
                after = rows.last().map(|row| {
                    (
                        row.get::<Option<String>, _>("due")
                            .unwrap_or_else(|| "9999-12-31".into()),
                        row.get("id"),
                    )
                });
            }
            assert_eq!(count, expected);
            measured.push((version, count, pages, started.elapsed().as_micros()));
        }
        let summary = store.summary().await.unwrap();
        assert_eq!(summary.active, 400);
        assert_eq!(summary.due_soon, 250);
        assert_eq!(summary.overdue, 0);
        assert_eq!(summary.completed, 0);
        let evidence = serde_json::to_string_pretty(&(evidence, measured)).unwrap();
        println!("{evidence}");
        if let Ok(path) = std::env::var("MAINTENANCE_INSTRUCTION_QUERY_PLAN_OUTPUT") {
            std::fs::write(path, evidence).unwrap();
        }
        store.close().await.unwrap();
    }
    #[tokio::test]
    async fn legacy_undated_recurrence_preserves_facts_and_acknowledgements_until_deliberate_correction()
     {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("legacy-undated.sqlite3");
        let original = Store::open(&path).await.unwrap();
        let catalog: ImportCatalog =
            serde_json::from_str(include_str!("../config/items.example.json")).unwrap();
        let mut input = catalog.items[0].clone();
        input.due = None;
        let actor = Actor::System {
            id: SystemActor::Owner,
            name: "Owner credential".into(),
        };
        // The 0.3 schema and storage producer allowed this definition. Seed its
        // original item and receipt using those unchanged persistence functions.
        let mut transaction = original.pool.begin_with("BEGIN IMMEDIATE").await.unwrap();
        insert_item(&mut transaction, &input).await.unwrap();
        let receipt = get_item(&mut transaction, &input.id).await.unwrap();
        save_replay(
            &mut transaction,
            &actor,
            "legacy-create",
            &create_fingerprint(&input).unwrap(),
            &receipt,
        )
        .await
        .unwrap();
        transaction.commit().await.unwrap();
        original.close().await.unwrap();
        let upgraded = Store::open(&path).await.unwrap();
        let current = upgraded.get(&input.id).await.unwrap();
        assert_eq!(
            serde_json::to_vec(&current).unwrap(),
            serde_json::to_vec(&receipt).unwrap()
        );
        assert!(matches!(
            upgraded
                .create(
                    input.clone(),
                    &actor,
                    "fresh-invalid-create",
                    Management::Owner
                )
                .await,
            Err(Error::Invalid(_))
        ));
        assert!(matches!(
            upgraded
                .update(
                    &input.id,
                    current.revision,
                    UpdateItem {
                        summary: crate::models::Change(Some("Date still unknown".into())),
                        ..Default::default()
                    },
                    Management::Owner
                )
                .await,
            Err(Error::Invalid(_))
        ));
        let completed = upgraded
            .complete(
                &input.id,
                current.revision,
                CompleteItem {
                    occurrence_id: current.occurrence.id,
                },
                &actor,
                "known-completion",
            )
            .await
            .unwrap();
        assert!(completed.occurrence.due.is_some());
        let event = completed.last_completion.as_ref().unwrap();
        assert!(event.completed_at.is_some());
        assert!(event.previous_due.is_none());
        let replayed = upgraded
            .create(input, &actor, "legacy-create", Management::Owner)
            .await
            .unwrap();
        assert_eq!(
            serde_json::to_vec(&replayed).unwrap(),
            serde_json::to_vec(&receipt).unwrap()
        );
        let current = upgraded.get(&completed.id).await.unwrap();
        assert_eq!(
            serde_json::to_vec(&current).unwrap(),
            serde_json::to_vec(&completed).unwrap()
        );
        assert_eq!(
            upgraded
                .events(&completed.id, None, 100)
                .await
                .unwrap()
                .items
                .len(),
            1
        );
        upgraded.close().await.unwrap();
    }
}
