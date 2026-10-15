use std::{path::Path, time::Duration};

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
        Actor, CompleteItem, Completion, CreateItem, ImportCatalog, Item, ItemId, Occurrence, Page,
        Summary, UpdateItem,
    },
};

#[derive(Clone)]
pub struct Store {
    pool: SqlitePool,
}

// Definitions are one typed, versioned document. Indexed lifecycle/order fields
// live in columns; history is append-only and never hidden inside that document.
const EVENT_LIST: &str = "SELECT sequence,id,item_id,occurrence_id,completed_at,actor,previous_due,next_due FROM completions WHERE item_id=? ORDER BY sequence DESC LIMIT ?";
const EVENT_LIST_NEXT: &str = "SELECT sequence,id,item_id,occurrence_id,completed_at,actor,previous_due,next_due FROM completions WHERE item_id=? AND sequence<? ORDER BY sequence DESC LIMIT ?";
const IDEMPOTENCY_GET: &str =
    "SELECT fingerprint,response FROM idempotency WHERE actor=? AND key=?";
const IDEMPOTENCY_INSERT: &str =
    "INSERT INTO idempotency(actor,key,fingerprint,response) VALUES(?,?,?,?)";
const ITEM_INSERT: &str =
    "INSERT INTO items(id,definition,revision,occurrence_id,due) VALUES(?,?,1,?,?)";
const ITEM_UPDATE: &str = "UPDATE items SET definition=?,due=?,revision=? WHERE id=?";
const ITEM_ARCHIVE: &str = "UPDATE items SET archived=?,revision=? WHERE id=?";
const DATABASE_NOW: &str = "SELECT strftime('%Y-%m-%dT%H:%M:%fZ','now')";
const COMPLETION_INSERT: &str = "INSERT INTO completions(id,item_id,occurrence_id,completed_at,actor,previous_due,next_due) VALUES (?,?,?,?,?,?,?)";
const OCCURRENCE_ADVANCE: &str =
    "UPDATE items SET due=?,occurrence_id=?,revision=?,last_completion_id=? WHERE id=?";
const SUMMARY_FIRST: &str = "SELECT id,json_extract(definition,'$.timezone') AS timezone,due FROM items WHERE archived=0 ORDER BY due,id LIMIT 200";
const SUMMARY_NEXT: &str = "SELECT id,json_extract(definition,'$.timezone') AS timezone,due FROM items WHERE archived=0 AND (due,id)>(?,?) ORDER BY due,id LIMIT 200";
const COMPLETION_COUNT: &str = "SELECT count(*) FROM completions";
#[derive(FromRow)]
struct ItemRow {
    id: String,
    definition: String,
    revision: i64,
    archived: bool,
    occurrence_id: String,
    due: String,
    completion_id: Option<String>,
    completed_occurrence: Option<String>,
    completed_at: Option<String>,
    actor: Option<String>,
    previous_due: Option<String>,
    next_due: Option<String>,
}
macro_rules! item_select {
    () => { "SELECT i.id,i.definition,i.revision,i.archived,i.occurrence_id,i.due,c.id AS completion_id,c.occurrence_id AS completed_occurrence,c.completed_at,c.actor,c.previous_due,c.next_due FROM items i LEFT JOIN completions c ON c.sequence=i.last_completion_id" };
}
const ITEM_GET: &str = concat!(item_select!(), " WHERE i.id=?");
const ITEM_LIST: &str = concat!(
    item_select!(),
    " WHERE i.archived=? ORDER BY i.due,i.id LIMIT ?"
);
const ITEM_LIST_NEXT: &str = concat!(
    item_select!(),
    " WHERE i.archived=? AND (i.due,i.id)>(?,?) ORDER BY i.due,i.id LIMIT ?"
);
#[derive(FromRow)]
struct CompletionRow {
    sequence: i64,
    id: String,
    item_id: String,
    occurrence_id: String,
    completed_at: String,
    actor: String,
    previous_due: String,
    next_due: String,
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
        let last_completion = match (
            row.completion_id,
            row.completed_occurrence,
            row.completed_at,
            row.actor,
            row.previous_due,
            row.next_due,
        ) {
            (None, None, None, None, None, None) => None,
            (
                Some(event_id),
                Some(occurrence),
                Some(at),
                Some(actor),
                Some(previous),
                Some(next),
            ) => Some(Completion {
                id: parse_uuid(&event_id)?,
                item_id: id.clone(),
                occurrence_id: parse_uuid(&occurrence)?,
                completed_at: parse_time(&at)?,
                actor: serde_json::from_str(&actor)?,
                previous_due: parse_date(&previous)?,
                next_due: parse_date(&next)?,
            }),
            _ => return Err(corrupt()),
        };
        Ok(Self {
            id,
            definition: serde_json::from_str(&row.definition)?,
            revision: row.revision,
            archived: row.archived,
            occurrence: Occurrence {
                id: parse_uuid(&row.occurrence_id)?,
                due: parse_date(&row.due)?,
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
            occurrence_id: parse_uuid(&row.occurrence_id)?,
            completed_at: parse_time(&row.completed_at)?,
            actor: serde_json::from_str(&row.actor)?,
            previous_due: parse_date(&row.previous_due)?,
            next_due: parse_date(&row.next_due)?,
        })
    }
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ItemCursor {
    archived: bool,
    due: NaiveDate,
    id: ItemId,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct EventCursor {
    item_id: ItemId,
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
                .bind(after.due.to_string())
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
    pub async fn create(&self, input: CreateItem, actor: &Actor, key: &str) -> Result<Item, Error> {
        input.validate()?;
        let fingerprint = fingerprint("create", &input)?;
        let mut transaction = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        if let Some(item) = replay(&mut transaction, actor, key, &fingerprint).await? {
            return Ok(item);
        }
        insert_item(&mut transaction, &input).await?;
        let item = get_item(&mut transaction, &input.id).await?;
        save_replay(&mut transaction, actor, key, &fingerprint, &item).await?;
        transaction.commit().await?;
        Ok(item)
    }
    pub async fn import(&self, input: ImportCatalog) -> Result<Page<Item>, Error> {
        if input.items.is_empty() || input.items.len() > 200 {
            return Err(Error::Invalid("import requires 1..200 items"));
        }
        for item in &input.items {
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
    pub async fn update(
        &self,
        id: &ItemId,
        revision: i64,
        input: UpdateItem,
    ) -> Result<Item, Error> {
        let mut transaction = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let mut item = get_item(&mut transaction, id).await?;
        check_revision(&item, revision)?;
        input.apply(&mut item.definition, &mut item.occurrence.due)?;
        let revision = next_revision(item.revision)?;
        sqlx::query(ITEM_UPDATE)
            .bind(serde_json::to_string(&item.definition)?)
            .bind(item.occurrence.due.to_string())
            .bind(revision)
            .bind(id.as_ref())
            .execute(&mut *transaction)
            .await?;
        let result = get_item(&mut transaction, id).await?;
        transaction.commit().await?;
        Ok(result)
    }
    pub async fn archive(&self, id: &ItemId, revision: i64, archived: bool) -> Result<Item, Error> {
        let mut transaction = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let item = get_item(&mut transaction, id).await?;
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
        check_revision(&item, revision)?;
        if item.archived {
            return Err(Error::Conflict("archived items cannot be completed"));
        }
        if item.occurrence.id != input.occurrence_id {
            return Err(Error::Conflict("this occurrence is no longer current"));
        }
        let now: String = sqlx::query_scalar(DATABASE_NOW)
            .fetch_one(&mut *transaction)
            .await?;
        let completed_at = parse_time(&now)?;
        let next_due = item.definition.schedule.next(
            completed_at
                .with_timezone(&item.definition.timezone.0)
                .date_naive(),
        )?;
        let event_id = Uuid::now_v7();
        let sequence = sqlx::query(COMPLETION_INSERT)
            .bind(event_id.to_string())
            .bind(id.as_ref())
            .bind(input.occurrence_id.to_string())
            .bind(now)
            .bind(serde_json::to_string(actor)?)
            .bind(item.occurrence.due.to_string())
            .bind(next_due.to_string())
            .execute(&mut *transaction)
            .await?
            .last_insert_rowid();
        sqlx::query(OCCURRENCE_ADVANCE)
            .bind(next_due.to_string())
            .bind(Uuid::now_v7().to_string())
            .bind(next_revision(item.revision)?)
            .bind(sequence)
            .bind(id.as_ref())
            .execute(&mut *transaction)
            .await?;
        let result = get_item(&mut transaction, id).await?;
        save_replay(&mut transaction, actor, key, &fingerprint, &result).await?;
        transaction.commit().await?;
        Ok(result)
    }
    pub async fn summary(&self) -> Result<Summary, Error> {
        #[derive(FromRow)]
        struct DueRow {
            id: String,
            timezone: String,
            due: String,
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
                let due = parse_date(&row.due)?;
                summary.active += 1;
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
            after = rows.last().map(|row| (row.due.clone(), row.id.clone()));
        }
        summary.completed = sqlx::query_scalar(COMPLETION_COUNT)
            .fetch_one(&mut *transaction)
            .await?;
        transaction.commit().await?;
        Ok(summary)
    }
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
async fn insert_item(connection: &mut SqliteConnection, input: &CreateItem) -> Result<(), Error> {
    let result = sqlx::query(ITEM_INSERT)
        .bind(input.id.as_ref())
        .bind(serde_json::to_string(&input.definition)?)
        .bind(Uuid::now_v7().to_string())
        .bind(input.due.to_string())
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
fn fingerprint(operation: &str, input: &impl Serialize) -> Result<Vec<u8>, Error> {
    let mut hash = Sha256::new();
    hash.update(operation);
    hash.update([0]);
    hash.update(serde_json::to_vec(input)?);
    Ok(hash.finalize().to_vec())
}
async fn replay(
    connection: &mut SqliteConnection,
    actor: &Actor,
    key: &str,
    fingerprint: &[u8],
) -> Result<Option<Item>, Error> {
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
    response: &Item,
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
                    NaiveDate::from_ymd_opt(2031, 1 + (index % 12), 1 + index % 27).unwrap();
                items.push(input);
            }
            store.import(ImportCatalog { items }).await.unwrap();
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
        assert!(details.iter().any(|line| line.contains("items_due")));
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
                ],
            ),
            (
                "update item",
                ITEM_UPDATE,
                vec![
                    Text(definition),
                    Text("2031-02-17".into()),
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
            store.import(ImportCatalog { items }).await.unwrap();
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
                            due: crate::models::Change(Some(
                                NaiveDate::from_ymd_opt(
                                    if index % 2 == 0 { 2030 } else { 2032 },
                                    1,
                                    1,
                                )
                                .unwrap(),
                            )),
                            ..Default::default()
                        },
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
}
