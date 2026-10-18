use chrono::NaiveDate;
use home_maintenance::{
    api::{self, Context, Role, System},
    models::{
        Actor, Change, ChecklistStep, CompleteItem, Completion, CreateItem, Definition,
        ImportCatalog, Item, ItemId, Page, Schedule, SetChecklistStep, Summary, TimeZone,
        UpdateItem,
    },
    store::Store,
};
use reqwest::{Client, Response, StatusCode};
use std::{
    net::{IpAddr, Ipv4Addr, SocketAddr},
    sync::{Arc, Mutex},
};
use uuid::Uuid;

const OWNER: &str = "example-owner-credential-at-least-32-characters";
const INTEGRATION: &str = "example-integration-credential-at-least-32-characters";
struct Fixture {
    directory: tempfile::TempDir,
    store: Store,
    server: dropshot::HttpServer<Arc<Context>>,
    client: Client,
    origin: String,
}
impl Fixture {
    async fn new() -> Self {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::open(&directory.path().join("maintenance.sqlite3"))
            .await
            .unwrap();
        let context = Context::new(store.clone(), OWNER).with_integration(INTEGRATION);
        let server = api::start(SocketAddr::from(([127, 0, 0, 1], 0)), Arc::new(context)).unwrap();
        let origin = format!("http://{}", server.local_addr());
        Self {
            directory,
            store,
            server,
            client: Client::new(),
            origin,
        }
    }
    async fn create(&self, id: &str) -> Item {
        let response = self
            .client
            .post(format!("{}/v1/items", self.origin))
            .bearer_auth(OWNER)
            .header("Idempotency-Key", id)
            .json(&input(id))
            .send()
            .await
            .unwrap();
        assert_eq!(
            response.status(),
            StatusCode::CREATED,
            "{}",
            response.text().await.unwrap()
        );
        response.json().await.unwrap()
    }
    async fn get(&self, id: &str) -> Item {
        self.client
            .get(format!("{}/v1/items/{id}", self.origin))
            .bearer_auth(OWNER)
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap()
            .json()
            .await
            .unwrap()
    }
    async fn finish(self) {
        self.server.close().await.unwrap();
        self.store.close().await.unwrap();
    }
}
fn input(id: &str) -> CreateItem {
    CreateItem {
        id: ItemId::try_from(id.to_owned()).unwrap(),
        due: Some(NaiveDate::from_ymd_opt(2031, 2, 17).unwrap()),
        definition: Definition {
            name: "Example filter".into(),
            summary: "Replace the example filter".into(),
            description: Some("Replace the part and verify normal operation.".into()),
            timezone: TimeZone::try_from("Etc/UTC".to_owned()).unwrap(),
            location: None,
            group: None,
            schedule: Schedule::new(47, 0, 0).unwrap(),
            instructions: vec!["Replace the example filter.".into()],
            checklist: vec![
                ChecklistStep {
                    id: Uuid::from_u128(1),
                    summary: "Inspect the example part".into(),
                },
                ChecklistStep {
                    id: Uuid::from_u128(2),
                    summary: "Install the demonstration replacement".into(),
                },
            ],
            specs: vec![],
            resources: vec![],
            reorder_url: None,
        },
    }
}
async fn complete(f: &Fixture, item: &Item, key: &str) -> Response {
    f.client
        .post(format!(
            "{}/v1/items/{}/completions",
            f.origin,
            item.id.as_ref()
        ))
        .bearer_auth(OWNER)
        .header("If-Match", format!("\"{}\"", item.revision))
        .header("Idempotency-Key", key)
        .json(&CompleteItem {
            occurrence_id: item.occurrence.id,
        })
        .send()
        .await
        .unwrap()
}

async fn set_step(f: &Fixture, item: &Item, step_id: Uuid, completed: bool, key: &str) -> Response {
    f.client
        .put(format!(
            "{}/v1/items/{}/checklist/{step_id}",
            f.origin,
            item.id.as_ref()
        ))
        .bearer_auth(OWNER)
        .header("If-Match", format!("\"{}\"", item.revision))
        .header("Idempotency-Key", key)
        .json(&SetChecklistStep {
            occurrence_id: item.occurrence.id,
            completed,
        })
        .send()
        .await
        .unwrap()
}

#[tokio::test]
async fn due_dates_leave_room_for_the_calendar_exclusive_end() {
    let f = Fixture::new().await;
    let last_day = NaiveDate::from_ymd_opt(9999, 12, 31).unwrap();
    let last_due = NaiveDate::from_ymd_opt(9999, 12, 30).unwrap();
    let mut invalid = input("example-invalid-date");
    invalid.due = Some(last_day);
    let response = f
        .client
        .post(format!("{}/v1/items", f.origin))
        .bearer_auth(OWNER)
        .header("Idempotency-Key", "invalid-calendar-date")
        .json(&invalid)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);

    let mut valid = input("example-last-due");
    valid.due = Some(last_due);
    let response = f
        .client
        .post(format!("{}/v1/items", f.origin))
        .bearer_auth(OWNER)
        .header("Idempotency-Key", "last-calendar-date")
        .json(&valid)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::CREATED);
    let item: Item = response.json().await.unwrap();
    let response = f
        .client
        .patch(format!("{}/v1/items/{}", f.origin, item.id.as_ref()))
        .bearer_auth(OWNER)
        .header("If-Match", format!("\"{}\"", item.revision))
        .json(&UpdateItem {
            due: Change(Some(Some(last_day))),
            ..UpdateItem::default()
        })
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert_eq!(f.get(item.id.as_ref()).await.occurrence.due, Some(last_due));

    let response = f
        .client
        .post(format!("{}/v1/imports", f.origin))
        .bearer_auth(OWNER)
        .json(&ImportCatalog {
            items: vec![input("example-import-date"), invalid],
        })
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let response = f
        .client
        .get(format!("{}/v1/items/example-import-date", f.origin))
        .bearer_auth(OWNER)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    f.finish().await;
}

#[tokio::test]
async fn checklist_checks_are_replayable_and_only_parent_completion_advances_the_schedule() {
    let f = Fixture::new().await;
    let before = f.create("example-checklist").await;
    let step = before.occurrence.checklist[0].id;
    let endpoint = format!("{}/v1/items/example-checklist/checklist/{step}", f.origin);
    let payload = SetChecklistStep {
        occurrence_id: before.occurrence.id,
        completed: true,
    };
    let unauthorized = f.client.put(&endpoint).json(&payload).send().await.unwrap();
    assert_eq!(unauthorized.status(), StatusCode::UNAUTHORIZED);
    let no_revision = f
        .client
        .put(&endpoint)
        .bearer_auth(OWNER)
        .json(&payload)
        .send()
        .await
        .unwrap();
    assert_eq!(no_revision.status(), StatusCode::PRECONDITION_REQUIRED);
    let no_retry_key = f
        .client
        .put(&endpoint)
        .bearer_auth(OWNER)
        .header("If-Match", format!("\"{}\"", before.revision))
        .json(&payload)
        .send()
        .await
        .unwrap();
    assert_eq!(no_retry_key.status(), StatusCode::BAD_REQUEST);
    let (a, b) = tokio::join!(
        set_step(&f, &before, step, true, "check"),
        set_step(&f, &before, step, true, "check")
    );
    assert_eq!(a.status(), StatusCode::OK);
    assert_eq!(b.status(), StatusCode::OK);
    let checked: Item = a.json().await.unwrap();
    let replayed: Item = b.json().await.unwrap();
    assert_eq!(checked.occurrence.checklist, replayed.occurrence.checklist);
    assert_eq!(checked.revision, before.revision + 1);
    assert_eq!(checked.occurrence.id, before.occurrence.id);
    assert_eq!(checked.occurrence.due, before.occurrence.due);
    assert!(checked.last_completion.is_none());
    assert!(checked.occurrence.checklist[1].completion.is_none());
    let actor = &checked.occurrence.checklist[0]
        .completion
        .as_ref()
        .unwrap()
        .actor;
    assert_eq!(actor.key(), "system:owner");
    let summary = f.store.summary().await.unwrap();
    assert_eq!(summary.completed, 0);

    let unchanged: Item = set_step(&f, &checked, step, true, "check-no-op")
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(unchanged.revision, checked.revision);
    assert_eq!(unchanged.occurrence.checklist, checked.occurrence.checklist);
    assert_eq!(
        set_step(&f, &checked, step, false, "check").await.status(),
        StatusCode::CONFLICT
    );
    let undone: Item = set_step(&f, &checked, step, false, "uncheck")
        .await
        .json()
        .await
        .unwrap();
    assert!(undone.occurrence.checklist[0].completion.is_none());
    let checked: Item = set_step(&f, &undone, step, true, "recheck")
        .await
        .json()
        .await
        .unwrap();
    let after: Item = complete(&f, &checked, "complete-parent")
        .await
        .json()
        .await
        .unwrap();
    assert_ne!(after.occurrence.id, before.occurrence.id);
    assert!(
        after
            .occurrence
            .checklist
            .iter()
            .all(|step| step.completion.is_none())
    );
    let event = after.last_completion.as_ref().unwrap();
    assert_eq!(event.checklist, checked.occurrence.checklist);
    assert_eq!(f.store.summary().await.unwrap().completed, 1);

    // A saved response for the old occurrence cannot apply its checks to the successor.
    let old_response: Item = set_step(&f, &before, step, true, "check")
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(old_response.occurrence.id, before.occurrence.id);
    let current = f.get("example-checklist").await;
    assert_eq!(current.occurrence.id, after.occurrence.id);
    assert!(
        current
            .occurrence
            .checklist
            .iter()
            .all(|step| step.completion.is_none())
    );
    let mut wrong_occurrence = current.clone();
    wrong_occurrence.occurrence.id = before.occurrence.id;
    assert_eq!(
        set_step(&f, &wrong_occurrence, step, true, "stale-occurrence")
            .await
            .status(),
        StatusCode::CONFLICT
    );
    assert_eq!(
        set_step(&f, &current, Uuid::from_u128(999), true, "missing-step")
            .await
            .status(),
        StatusCode::NOT_FOUND
    );
    let archived = f
        .store
        .archive(&current.id, current.revision, true)
        .await
        .unwrap();
    assert_eq!(
        set_step(&f, &archived, step, true, "archived-step")
            .await
            .status(),
        StatusCode::CONFLICT
    );
    f.finish().await;
}

#[tokio::test]
async fn checklist_template_edits_reset_changed_work_and_preserve_historical_wording() {
    let f = Fixture::new().await;
    let item = f.create("example-template").await;
    let item: Item = set_step(&f, &item, Uuid::from_u128(1), true, "first")
        .await
        .json()
        .await
        .unwrap();
    let checked: Item = set_step(&f, &item, Uuid::from_u128(2), true, "second")
        .await
        .json()
        .await
        .unwrap();
    let mut steps = checked.definition.checklist.clone();
    steps[0].summary = "Inspect the replacement connection".into();
    steps.push(ChecklistStep {
        id: Uuid::from_u128(3),
        summary: "Check normal operation".into(),
    });
    let patch = UpdateItem {
        summary: Change(Some("Replace and verify the example filter".into())),
        description: Change(Some(None)),
        checklist: Change(Some(steps.clone())),
        ..UpdateItem::default()
    };
    let response = f
        .client
        .patch(format!("{}/v1/items/example-template", f.origin))
        .bearer_auth(OWNER)
        .header("If-Match", format!("\"{}\"", checked.revision))
        .json(&patch)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let edited: Item = response.json().await.unwrap();
    assert_eq!(
        edited.definition.summary,
        "Replace and verify the example filter"
    );
    assert!(edited.definition.description.is_none());
    assert_eq!(edited.occurrence.due, checked.occurrence.due);
    assert!(edited.occurrence.checklist[0].completion.is_none());
    assert_eq!(
        edited.occurrence.checklist[1].completion,
        checked.occurrence.checklist[1].completion
    );
    assert!(edited.occurrence.checklist[2].completion.is_none());

    let mut duplicate = steps.clone();
    duplicate.push(steps[0].clone());
    let response = f
        .client
        .patch(format!("{}/v1/items/example-template", f.origin))
        .bearer_auth(OWNER)
        .header("If-Match", format!("\"{}\"", edited.revision))
        .json(&UpdateItem {
            checklist: Change(Some(duplicate)),
            ..UpdateItem::default()
        })
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert_eq!(f.get("example-template").await.revision, edited.revision);

    let completed: Item = complete(&f, &edited, "complete-template")
        .await
        .json()
        .await
        .unwrap();
    let snapshot = completed
        .last_completion
        .as_ref()
        .unwrap()
        .checklist
        .clone();
    let response = f
        .client
        .patch(format!("{}/v1/items/example-template", f.origin))
        .bearer_auth(OWNER)
        .header("If-Match", format!("\"{}\"", completed.revision))
        .json(&UpdateItem {
            checklist: Change(Some(Vec::new())),
            ..UpdateItem::default()
        })
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let cleared: Item = response.json().await.unwrap();
    assert!(cleared.occurrence.checklist.is_empty());
    assert_eq!(
        cleared.last_completion.as_ref().unwrap().checklist,
        snapshot
    );
    let history = f.store.events(&cleared.id, None, 100).await.unwrap();
    assert_eq!(history.items[0].checklist, snapshot);
    f.finish().await;
}

#[tokio::test]
async fn concurrent_steps_and_parent_completion_preserve_occurrence_ownership() {
    let f = Fixture::new().await;
    let before = f.create("example-race").await;
    let (a, b) = tokio::join!(
        set_step(&f, &before, Uuid::from_u128(1), true, "racing-step"),
        complete(&f, &before, "racing-parent")
    );
    assert!(matches!(
        (a.status(), b.status()),
        (StatusCode::OK, StatusCode::PRECONDITION_FAILED)
            | (StatusCode::PRECONDITION_FAILED, StatusCode::OK)
    ));
    let current = f.get("example-race").await;
    let after = if a.status() == StatusCode::OK {
        assert_eq!(current.occurrence.id, before.occurrence.id);
        assert!(current.occurrence.checklist[0].completion.is_some());
        complete(&f, &current, "finish-after-race")
            .await
            .json::<Item>()
            .await
            .unwrap()
    } else {
        current
    };
    assert_ne!(after.occurrence.id, before.occurrence.id);
    assert!(
        after
            .occurrence
            .checklist
            .iter()
            .all(|step| step.completion.is_none())
    );
    assert_eq!(f.store.summary().await.unwrap().completed, 1);
    f.finish().await;
}

#[tokio::test]
async fn concurrent_distinct_steps_require_refresh_instead_of_losing_a_check() {
    let f = Fixture::new().await;
    let before = f.create("example-step-race").await;
    let (a, b) = tokio::join!(
        set_step(&f, &before, Uuid::from_u128(1), true, "step-one"),
        set_step(&f, &before, Uuid::from_u128(2), true, "step-two")
    );
    assert!(matches!(
        (a.status(), b.status()),
        (StatusCode::OK, StatusCode::PRECONDITION_FAILED)
            | (StatusCode::PRECONDITION_FAILED, StatusCode::OK)
    ));
    let current = f.get("example-step-race").await;
    assert_eq!(
        current
            .occurrence
            .checklist
            .iter()
            .filter(|step| step.completion.is_some())
            .count(),
        1
    );
    let missing = current
        .occurrence
        .checklist
        .iter()
        .find(|step| step.completion.is_none())
        .unwrap();
    let after: Item = set_step(&f, &current, missing.id, true, "refreshed-step")
        .await
        .json()
        .await
        .unwrap();
    assert!(
        after
            .occurrence
            .checklist
            .iter()
            .all(|step| step.completion.is_some())
    );
    assert_eq!(after.occurrence.id, before.occurrence.id);
    assert_eq!(f.store.summary().await.unwrap().completed, 0);
    f.finish().await;
}

#[tokio::test]
async fn completion_is_atomic_replayable_and_scoped_to_one_occurrence() {
    let f = Fixture::new().await;
    let before = f.create("example-filter").await;
    let (first, second) = tokio::join!(
        complete(&f, &before, "same-request"),
        complete(&f, &before, "same-request")
    );
    assert_eq!(first.status(), StatusCode::OK);
    assert_eq!(second.status(), StatusCode::OK);
    let first: Item = first.json().await.unwrap();
    let second: Item = second.json().await.unwrap();
    assert_eq!(first.occurrence.id, second.occurrence.id);
    assert_ne!(first.occurrence.id, before.occurrence.id);
    assert_eq!(first.revision, before.revision + 1);
    let event = first.last_completion.as_ref().unwrap();
    assert_eq!(event.occurrence_id, Some(before.occurrence.id));
    assert_eq!(
        event.next_due,
        Some(
            first
                .definition
                .schedule
                .next(
                    event
                        .completed_at
                        .unwrap()
                        .with_timezone(&first.definition.timezone.0)
                        .date_naive()
                )
                .unwrap()
        )
    );
    assert_eq!(
        complete(&f, &before, "different-request").await.status(),
        StatusCode::PRECONDITION_FAILED
    );
    let events: Page<Completion> = f
        .client
        .get(format!("{}/v1/items/example-filter/events", f.origin))
        .bearer_auth(OWNER)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(events.items.len(), 1);
    assert_eq!(
        f.get("example-filter").await.occurrence.id,
        first.occurrence.id
    );
    let response = complete(&f, &first, "same-request").await;
    assert_eq!(response.status(), StatusCode::CONFLICT);
    f.finish().await;
}
#[tokio::test]
async fn concurrent_distinct_completions_commit_only_one_event() {
    let f = Fixture::new().await;
    let item = f.create("example-filter").await;
    let (a, b) = tokio::join!(
        complete(&f, &item, "request-a"),
        complete(&f, &item, "request-b")
    );
    assert!(matches!(
        (a.status(), b.status()),
        (StatusCode::OK, StatusCode::PRECONDITION_FAILED)
            | (StatusCode::PRECONDITION_FAILED, StatusCode::OK)
    ));
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM completions")
        .fetch_one(f.store.pool())
        .await
        .unwrap();
    assert_eq!(count, 1);
    f.finish().await;
}
#[tokio::test]
async fn restart_and_cold_restore_keep_history_occurrences_and_idempotency() {
    let f = Fixture::new().await;
    let first = f.create("example-filter").await;
    let first: Item = set_step(&f, &first, Uuid::from_u128(1), true, "durable-step")
        .await
        .json()
        .await
        .unwrap();
    let after: Item = complete(&f, &first, "completion")
        .await
        .json()
        .await
        .unwrap();
    let backup = tempfile::tempdir().unwrap();
    let source = f.directory.path().join("maintenance.sqlite3");
    f.server.close().await.unwrap();
    f.store.close().await.unwrap();
    std::fs::copy(source, backup.path().join("restored.sqlite3")).unwrap();
    let restored = Store::open(&backup.path().join("restored.sqlite3"))
        .await
        .unwrap();
    let item = restored.get(&first.id).await.unwrap();
    assert_eq!(item.occurrence.id, after.occurrence.id);
    assert_eq!(
        item.last_completion.as_ref().unwrap().checklist,
        first.occurrence.checklist
    );
    assert!(
        item.occurrence
            .checklist
            .iter()
            .all(|step| step.completion.is_none())
    );
    assert_eq!(
        item.last_completion.as_ref().unwrap().id,
        after.last_completion.as_ref().unwrap().id
    );
    let replay = restored
        .complete(
            &first.id,
            first.revision,
            CompleteItem {
                occurrence_id: first.occurrence.id,
            },
            &Actor::System {
                id: home_maintenance::models::SystemActor::Owner,
                name: "Owner credential".into(),
            },
            "completion",
        )
        .await
        .unwrap();
    assert_eq!(replay.occurrence.id, after.occurrence.id);
    restored.close().await.unwrap();
}
#[tokio::test]
async fn import_validates_and_rolls_back_the_whole_catalog() {
    let f = Fixture::new().await;
    let response = f
        .client
        .post(format!("{}/v1/imports", f.origin))
        .bearer_auth(OWNER)
        .json(&ImportCatalog {
            items: vec![input("example-one"), input("example-one")],
        })
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::CONFLICT);
    assert!(matches!(
        f.store.get(&input("example-one").id).await,
        Err(home_maintenance::error::Error::NotFound)
    ));
    let mut invalid = input("example-invalid");
    invalid.definition.name = String::new();
    assert!(
        f.store
            .import(ImportCatalog {
                items: vec![input("example-two"), invalid]
            })
            .await
            .is_err()
    );
    assert_eq!(f.store.summary().await.unwrap().active, 0);
    f.finish().await;
}
#[tokio::test]
async fn typed_patch_clears_optionals_and_rejects_invalid_changes_without_writes() {
    let f = Fixture::new().await;
    let item = f.create("example-filter").await;
    let patch: UpdateItem =
        serde_json::from_str(r#"{"location":"Example room","group":null}"#).unwrap();
    let updated = f
        .store
        .update(&item.id, item.revision, patch)
        .await
        .unwrap();
    assert_eq!(updated.definition.location.as_deref(), Some("Example room"));
    assert!(updated.definition.group.is_none());
    assert_eq!(updated.definition.name, item.definition.name);
    let patch: UpdateItem = serde_json::from_str(r#"{"location":null,"name":""}"#).unwrap();
    assert!(
        f.store
            .update(&item.id, updated.revision, patch)
            .await
            .is_err()
    );
    assert_eq!(
        f.get("example-filter").await.definition.location.as_deref(),
        Some("Example room")
    );
    assert!(serde_json::from_str::<UpdateItem>(r#"{"name":null}"#).is_err());
    assert!(serde_json::from_str::<UpdateItem>(r#"{"unknown":true}"#).is_err());
    f.finish().await;
}
#[tokio::test]
async fn pagination_is_bounded_ordered_and_filter_scoped() {
    let f = Fixture::new().await;
    for id in ["example-a", "example-b", "example-c"] {
        f.create(id).await;
    }
    let page: Page<Item> = f
        .client
        .get(format!("{}/v1/items?limit=2", f.origin))
        .bearer_auth(OWNER)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(page.items.len(), 2);
    assert_eq!(page.items[0].id.as_ref(), "example-a");
    let cursor = page.next_page.unwrap();
    let page: Page<Item> = f
        .client
        .get(format!("{}/v1/items", f.origin))
        .query(&[("limit", "2"), ("page_token", &cursor)])
        .bearer_auth(OWNER)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(page.items.len(), 1);
    assert_eq!(page.items[0].id.as_ref(), "example-c");
    assert!(page.next_page.is_none());
    assert_eq!(
        f.client
            .get(format!("{}/v1/items", f.origin))
            .query(&[("archived", "true"), ("page_token", &cursor)])
            .bearer_auth(OWNER)
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        f.client
            .get(format!("{}/v1/items?limit=0", f.origin))
            .bearer_auth(OWNER)
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        f.client
            .get(format!("{}/v1/items?limit=201", f.origin))
            .bearer_auth(OWNER)
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::BAD_REQUEST
    );
    f.finish().await;
}
#[tokio::test]
async fn event_cursor_cannot_cross_items_and_history_survives_archive() {
    let f = Fixture::new().await;
    let mut item = f.create("example-a").await;
    let other = f.create("example-b").await;
    for _ in 0..3 {
        item = complete(&f, &item, &Uuid::now_v7().to_string())
            .await
            .json()
            .await
            .unwrap();
    }
    let page = f.store.events(&item.id, None, 2).await.unwrap();
    assert_eq!(page.items.len(), 2);
    let cursor = page.next_page.unwrap();
    assert!(f.store.events(&other.id, Some(&cursor), 2).await.is_err());
    assert_eq!(
        f.store
            .events(&item.id, Some(&cursor), 2)
            .await
            .unwrap()
            .items
            .len(),
        1
    );
    let archived = f
        .store
        .archive(&item.id, item.revision, true)
        .await
        .unwrap();
    assert!(archived.archived);
    assert!(
        f.store
            .list(false, None, 100)
            .await
            .unwrap()
            .items
            .iter()
            .all(|value| value.id != item.id)
    );
    assert_eq!(
        complete(&f, &archived, "archived").await.status(),
        StatusCode::CONFLICT
    );
    assert_eq!(
        f.store
            .events(&item.id, None, 100)
            .await
            .unwrap()
            .items
            .len(),
        3
    );
    let restored = f
        .store
        .archive(&item.id, archived.revision, false)
        .await
        .unwrap();
    assert_eq!(restored.occurrence.id, item.occurrence.id);
    assert_eq!(f.store.summary().await.unwrap().completed, 3);
    f.finish().await;
}
#[tokio::test]
async fn bearer_auth_never_trusts_forged_ingress_or_owner_actor_headers() {
    let f = Fixture::new().await;
    assert_eq!(
        f.client
            .get(format!("{}/v1/items", f.origin))
            .header("X-Remote-User-Id", "example-user")
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        f.client
            .get(format!("{}/health", f.origin))
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::OK
    );
    let item = f.create("example-filter").await;
    let response = f
        .client
        .post(format!("{}/v1/items/example-filter/completions", f.origin))
        .bearer_auth(OWNER)
        .header("X-Maintenance-Actor-Id", "ha:forged")
        .header("X-Maintenance-Actor-Name", "Forged User")
        .header("If-Match", "\"1\"")
        .header("Idempotency-Key", "forged-owner")
        .json(&CompleteItem {
            occurrence_id: item.occurrence.id,
        })
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let result: Item = response.json().await.unwrap();
    assert!(matches!(
        result.last_completion.unwrap().actor,
        Some(Actor::System { .. })
    ));
    f.finish().await;
}
#[tokio::test]
async fn integration_permissions_require_authenticated_actor_and_admin_assertion() {
    let f = Fixture::new().await;
    let item = f.create("example-filter").await;
    let url = format!("{}/v1/items/example-filter", f.origin);
    assert_eq!(
        f.client
            .patch(&url)
            .bearer_auth(INTEGRATION)
            .header("X-Maintenance-Actor-Id", "ha:example-user")
            .json(&UpdateItem::default())
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        f.client
            .post(format!("{url}/completions"))
            .bearer_auth(INTEGRATION)
            .header("If-Match", "\"1\"")
            .header("Idempotency-Key", "missing-actor")
            .json(&CompleteItem {
                occurrence_id: item.occurrence.id
            })
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::FORBIDDEN
    );
    let response = f
        .client
        .post(format!("{url}/completions"))
        .bearer_auth(INTEGRATION)
        .header("X-Maintenance-Actor-Id", "ha:example-user")
        .header("X-Maintenance-Actor-Name", "Demo User")
        .header("If-Match", "\"1\"")
        .header("Idempotency-Key", "user-complete")
        .json(&CompleteItem {
            occurrence_id: item.occurrence.id,
        })
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let result: Item = response.json().await.unwrap();
    assert!(
        matches!(result.last_completion.unwrap().actor,Some(Actor::User {id,..}) if id=="ha:example-user")
    );
    let system: System = f
        .client
        .get(format!("{}/v1/system", f.origin))
        .bearer_auth(INTEGRATION)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(matches!(system.credential_role, Role::Integration));
    f.finish().await;
}
#[tokio::test]
async fn ingress_is_peer_scoped_requires_identity_and_protects_mutations() {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(&directory.path().join("test.sqlite3"))
        .await
        .unwrap();
    let rejected = api::start(
        ([127, 0, 0, 1], 0).into(),
        Arc::new(
            Context::new(store.clone(), OWNER).for_ingress(IpAddr::V4(Ipv4Addr::new(192, 0, 2, 1))),
        ),
    )
    .unwrap();
    assert_eq!(
        Client::new()
            .get(format!("http://{}/", rejected.local_addr()))
            .header("X-Remote-User-Id", "example-user")
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::FORBIDDEN
    );
    rejected.close().await.unwrap();
    let allowed = api::start(
        ([127, 0, 0, 1], 0).into(),
        Arc::new(Context::new(store.clone(), OWNER).for_ingress(IpAddr::V4(Ipv4Addr::LOCALHOST))),
    )
    .unwrap();
    let origin = format!("http://{}", allowed.local_addr());
    let client = Client::new();
    assert_eq!(
        client
            .get(format!("{origin}/v1/items"))
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        client
            .post(format!("{origin}/v1/items"))
            .header("X-Remote-User-Id", "example-user")
            .header("Idempotency-Key", "ingress")
            .json(&input("example-filter"))
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::FORBIDDEN
    );
    let response = client
        .post(format!("{origin}/v1/items"))
        .header("X-Remote-User-Id", "example-user")
        .header("X-Maintenance-Request", "1")
        .header("Idempotency-Key", "ingress")
        .json(&input("example-filter"))
        .send()
        .await
        .unwrap();
    assert_eq!(
        response.status(),
        StatusCode::CREATED,
        "{}",
        response.text().await.unwrap()
    );
    let page = client
        .get(format!("{origin}/items/example-filter"))
        .header("X-Ingress-Path", "/api/hassio_ingress/example-session")
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(page.contains("/api/hassio_ingress/example-session/"));
    assert_eq!(
        client
            .get(format!("{origin}/"))
            .header("X-Ingress-Path", "//example.com")
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::BAD_REQUEST
    );
    allowed.close().await.unwrap();
    store.close().await.unwrap();
}
#[derive(Clone, Default)]
struct LogBuffer(Arc<Mutex<Vec<u8>>>);
impl std::io::Write for LogBuffer {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
#[tokio::test]
async fn request_logging_cannot_disclose_paths_queries_tokens_or_error_text() {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(&directory.path().join("test.sqlite3"))
        .await
        .unwrap();
    let output = LogBuffer::default();
    let server = api::start_with_logger(
        ([127, 0, 0, 1], 0).into(),
        Arc::new(Context::new(store.clone(), OWNER)),
        home_maintenance::logging::with_writer(output.clone()),
    )
    .unwrap();
    let logger = home_maintenance::logging::with_writer(output.clone());
    slog::error!(logger,"private example diagnostic";"token"=>OWNER);
    let response = Client::new()
        .get(format!(
            "http://{}/private-example-path?token=private-example-query",
            server.local_addr()
        ))
        .header("Authorization", format!("Bearer {OWNER}"))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    server.close().await.unwrap();
    let output = String::from_utf8(output.0.lock().unwrap().clone()).unwrap();
    assert!(!output.contains("private-example"));
    assert!(!output.contains(OWNER));
    assert!(output.contains("request_completed"));
    store.close().await.unwrap();
}
#[test]
fn generated_openapi() {
    expectorate::assert_contents(
        "openapi/api.json",
        &format!(
            "{}\n",
            serde_json::to_string_pretty(&api::openapi().unwrap()).unwrap()
        ),
    );
}
#[tokio::test]
async fn summary_counts_archived_and_completed_separately() {
    let f = Fixture::new().await;
    let item = f.create("example-filter").await;
    let completed: Item = complete(&f, &item, "summary").await.json().await.unwrap();
    f.store
        .archive(&item.id, completed.revision, true)
        .await
        .unwrap();
    let summary: Summary = f
        .client
        .get(format!("{}/v1/summary", f.origin))
        .bearer_auth(OWNER)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(summary.active, 0);
    assert_eq!(summary.completed, 1);
    f.finish().await;
}

async fn reorderable(f: &Fixture, id: &str) -> Item {
    let item = f.create(id).await;
    f.store
        .update(
            &item.id,
            item.revision,
            UpdateItem {
                reorder_url: Change(Some(Some("https://example.invalid/replacement".into()))),
                specs: Change(Some(vec![home_maintenance::models::Spec {
                    label: "Size".into(),
                    value: "Demonstration medium".into(),
                }])),
                resources: Change(Some(vec![home_maintenance::models::Resource {
                    label: "Instructions".into(),
                    url: "https://example.invalid/instructions".into(),
                }])),
                ..Default::default()
            },
        )
        .await
        .unwrap()
}

async fn reorder(f: &Fixture, item: &Item, key: &str) -> Response {
    f.client
        .post(format!(
            "{}/v1/items/{}/reorders",
            f.origin,
            item.id.as_ref()
        ))
        .bearer_auth(INTEGRATION)
        .header("X-Maintenance-System-Actor", "ha:integration")
        .header("If-Match", format!("\"{}\"", item.revision))
        .header("Idempotency-Key", key)
        .json(&home_maintenance::models::ReorderItem::default())
        .send()
        .await
        .unwrap()
}

#[tokio::test]
async fn reorder_receipts_survive_source_edits_archive_deletion_and_resolution() {
    use home_maintenance::models::{
        Reorder, ReorderReceipt, ReorderResolution, ReorderStatus, ResolveReorder,
    };
    let f = Fixture::new().await;
    let item = reorderable(&f, "example-reorder").await;
    let before = serde_json::to_vec(&f.get(item.id.as_ref()).await).unwrap();
    let response = reorder(&f, &item, "physical-press").await;
    assert_eq!(response.status(), StatusCode::CREATED);
    assert_eq!(response.headers()["cache-control"], "no-store");
    let receipt: ReorderReceipt = response.json().await.unwrap();
    assert_eq!(receipt.item_id, item.id);
    assert_eq!(receipt.item_revision, item.revision);
    assert_eq!(receipt.item.name, item.definition.name);
    assert_eq!(receipt.item.summary, item.definition.summary);
    assert_eq!(receipt.item.description, item.definition.description);
    assert_eq!(receipt.item.specs, item.definition.specs);
    assert_eq!(receipt.item.resources, item.definition.resources);
    assert_eq!(
        receipt.item.reorder_url,
        "https://example.invalid/replacement"
    );
    assert_eq!(receipt.actor.key(), "system:ha:integration");
    assert_eq!(
        serde_json::to_vec(&f.get(item.id.as_ref()).await).unwrap(),
        before
    );
    assert_eq!(f.store.summary().await.unwrap().completed, 0);

    let changed = f
        .store
        .update(
            &item.id,
            item.revision,
            UpdateItem {
                name: Change(Some("Updated example task".into())),
                summary: Change(Some("Inspect a different example".into())),
                description: Change(Some(None)),
                specs: Change(Some(vec![])),
                resources: Change(Some(vec![])),
                reorder_url: Change(Some(None)),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    let replay: ReorderReceipt = reorder(&f, &item, "physical-press")
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(replay, receipt);
    f.store
        .archive(&item.id, changed.revision, true)
        .await
        .unwrap();
    let replay: ReorderReceipt = reorder(&f, &item, "physical-press")
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(replay, receipt);
    let owner = Actor::System {
        id: home_maintenance::models::SystemActor::Owner,
        name: "Owner credential".into(),
    };
    let resolved = f
        .store
        .resolve_reorder(
            receipt.id,
            1,
            ResolveReorder {
                status: ReorderResolution::Dismissed,
            },
            &owner,
        )
        .await
        .unwrap();
    assert_eq!(resolved.status, ReorderStatus::Dismissed);
    sqlx::query("DELETE FROM items WHERE id=?")
        .bind(item.id.as_ref())
        .execute(f.store.pool())
        .await
        .unwrap();
    let response = reorder(&f, &item, "physical-press").await;
    assert_eq!(response.status(), StatusCode::CREATED);
    assert_eq!(response.json::<ReorderReceipt>().await.unwrap(), receipt);
    let response = f
        .client
        .get(format!("{}/v1/reorders/{}", f.origin, receipt.id))
        .bearer_auth(INTEGRATION)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()["etag"], "\"2\"");
    assert_eq!(response.json::<Reorder>().await.unwrap(), resolved);
    assert!(
        f.store
            .reorders(ReorderStatus::Pending, None, 100)
            .await
            .unwrap()
            .items
            .is_empty()
    );
    f.finish().await;
}

#[tokio::test]
async fn reorder_concurrency_actor_scope_and_shared_ledger_distinguish_physical_presses() {
    use home_maintenance::models::ReorderReceipt;
    let f = Fixture::new().await;
    let item = reorderable(&f, "example-concurrent-reorders").await;
    let (first, retry) = tokio::join!(reorder(&f, &item, "press-a"), reorder(&f, &item, "press-a"));
    assert_eq!(first.status(), StatusCode::CREATED);
    assert_eq!(retry.status(), StatusCode::CREATED);
    let first: ReorderReceipt = first.json().await.unwrap();
    assert_eq!(retry.json::<ReorderReceipt>().await.unwrap(), first);
    let (second, third) =
        tokio::join!(reorder(&f, &item, "press-b"), reorder(&f, &item, "press-c"));
    let second: ReorderReceipt = second.json().await.unwrap();
    let third: ReorderReceipt = third.json().await.unwrap();
    assert_ne!(first.id, second.id);
    assert_ne!(second.id, third.id);
    let endpoint = format!("{}/v1/items/{}/reorders", f.origin, item.id.as_ref());
    let owner_response = f
        .client
        .post(&endpoint)
        .bearer_auth(OWNER)
        .header("If-Match", format!("\"{}\"", item.revision))
        .header("Idempotency-Key", "press-a")
        .json(&home_maintenance::models::ReorderItem::default())
        .send()
        .await
        .unwrap();
    assert_eq!(owner_response.status(), StatusCode::CREATED);
    let owned: ReorderReceipt = owner_response.json().await.unwrap();
    assert_ne!(first.id, owned.id);
    assert_eq!(owned.actor.key(), "system:owner");
    // The same actor cannot reuse a receipt key for completion or a changed revision.
    let response = f
        .client
        .post(format!(
            "{}/v1/items/{}/completions",
            f.origin,
            item.id.as_ref()
        ))
        .bearer_auth(INTEGRATION)
        .header("X-Maintenance-System-Actor", "ha:integration")
        .header("If-Match", format!("\"{}\"", item.revision))
        .header("Idempotency-Key", "press-a")
        .json(&CompleteItem {
            occurrence_id: item.occurrence.id,
        })
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::CONFLICT);
    let mut changed_request = item.clone();
    changed_request.revision += 1;
    assert_eq!(
        reorder(&f, &changed_request, "press-a").await.status(),
        StatusCode::CONFLICT
    );
    let other = reorderable(&f, "example-other-reorder").await;
    assert_eq!(
        reorder(&f, &other, "press-a").await.status(),
        StatusCode::CONFLICT
    );
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM reorders")
        .fetch_one(f.store.pool())
        .await
        .unwrap();
    assert_eq!(count, 4);
    assert_eq!(f.get(item.id.as_ref()).await.revision, item.revision);
    f.finish().await;
}

#[tokio::test]
async fn reorder_rejections_do_not_create_a_request_or_poison_a_key() {
    let f = Fixture::new().await;
    let item = f.create("example-no-reorder").await;
    let endpoint = format!("{}/v1/items/{}/reorders", f.origin, item.id.as_ref());
    let empty = home_maintenance::models::ReorderItem::default();
    for body in ["[]", "null", "\"example\""] {
        assert_eq!(
            f.client
                .post(&endpoint)
                .bearer_auth(OWNER)
                .header("If-Match", "\"1\"")
                .header("Idempotency-Key", "wrong-json-shape")
                .header("content-type", "application/json")
                .body(body)
                .send()
                .await
                .unwrap()
                .status(),
            StatusCode::BAD_REQUEST
        );
    }
    assert_eq!(
        f.client
            .post(&endpoint)
            .json(&empty)
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        f.client
            .post(&endpoint)
            .bearer_auth(INTEGRATION)
            .header("If-Match", "\"1\"")
            .header("Idempotency-Key", "untrusted")
            .json(&empty)
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        f.client
            .post(&endpoint)
            .bearer_auth(OWNER)
            .header("Idempotency-Key", "missing-revision")
            .json(&empty)
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::PRECONDITION_REQUIRED
    );
    assert_eq!(
        f.client
            .post(&endpoint)
            .bearer_auth(OWNER)
            .header("If-Match", "\"1\"")
            .json(&empty)
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        f.client
            .post(&endpoint)
            .bearer_auth(OWNER)
            .header("If-Match", "\"1\"")
            .header("Idempotency-Key", "untrusted-content")
            .header("content-type", "application/json")
            .body(r#"{"reorder_url":"https://example.invalid/forged"}"#)
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        reorder(&f, &item, "retry-after-configure").await.status(),
        StatusCode::CONFLICT
    );
    let item = f
        .store
        .update(
            &item.id,
            item.revision,
            UpdateItem {
                reorder_url: Change(Some(Some("https://example.invalid/part".into()))),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(
        reorder(&f, &item, "retry-after-configure").await.status(),
        StatusCode::CREATED
    );
    let mut stale = item.clone();
    stale.revision -= 1;
    assert_eq!(
        reorder(&f, &stale, "stale-press").await.status(),
        StatusCode::PRECONDITION_FAILED
    );
    let archived = f
        .store
        .archive(&item.id, item.revision, true)
        .await
        .unwrap();
    assert_eq!(
        reorder(&f, &archived, "archived-press").await.status(),
        StatusCode::CONFLICT
    );
    let mut missing = item;
    missing.id = "example-missing".to_owned().try_into().unwrap();
    assert_eq!(
        reorder(&f, &missing, "missing-press").await.status(),
        StatusCode::NOT_FOUND
    );
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM reorders")
        .fetch_one(f.store.pool())
        .await
        .unwrap();
    assert_eq!(count, 1);
    f.finish().await;
}

#[tokio::test]
async fn reorder_and_receipt_roll_back_together_when_the_ledger_write_fails() {
    use home_maintenance::models::ReorderReceipt;
    let f = Fixture::new().await;
    let item = reorderable(&f, "example-rollback").await;
    sqlx::query("CREATE TRIGGER fail_reorder_receipt BEFORE INSERT ON idempotency WHEN NEW.key='failing-press' BEGIN SELECT RAISE(ABORT,'test ledger unavailable'); END").execute(f.store.pool()).await.unwrap();
    assert_eq!(
        reorder(&f, &item, "failing-press").await.status(),
        StatusCode::INTERNAL_SERVER_ERROR
    );
    let requests: i64 = sqlx::query_scalar("SELECT count(*) FROM reorders")
        .fetch_one(f.store.pool())
        .await
        .unwrap();
    let receipts: i64 =
        sqlx::query_scalar("SELECT count(*) FROM idempotency WHERE key='failing-press'")
            .fetch_one(f.store.pool())
            .await
            .unwrap();
    assert_eq!((requests, receipts), (0, 0));
    sqlx::query("DROP TRIGGER fail_reorder_receipt")
        .execute(f.store.pool())
        .await
        .unwrap();
    let response = reorder(&f, &item, "failing-press").await;
    assert_eq!(response.status(), StatusCode::CREATED);
    let first: ReorderReceipt = response.json().await.unwrap();
    assert_eq!(
        reorder(&f, &item, "failing-press")
            .await
            .json::<ReorderReceipt>()
            .await
            .unwrap(),
        first
    );
    f.finish().await;
}

#[tokio::test]
async fn reorder_listing_and_terminal_resolution_are_bounded_and_concurrent_safe() {
    use home_maintenance::models::{
        Reorder, ReorderReceipt, ReorderResolution, ReorderStatus, ResolveReorder,
    };
    let f = Fixture::new().await;
    let item = reorderable(&f, "example-paged-reorders").await;
    let mut ids = Vec::new();
    for index in 0..5 {
        let receipt: ReorderReceipt = reorder(&f, &item, &format!("page-press-{index}"))
            .await
            .json()
            .await
            .unwrap();
        ids.push(receipt.id);
    }
    let list_url = format!("{}/v1/reorders", f.origin);
    let first: Page<Reorder> = f
        .client
        .get(&list_url)
        .bearer_auth(INTEGRATION)
        .query(&[("limit", "2")])
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(
        first
            .items
            .iter()
            .map(|record| record.request.id)
            .collect::<Vec<_>>(),
        ids[..2]
    );
    let cursor = first.next_page.unwrap();
    let resolution = format!("{}/v1/reorders/{}/resolution", f.origin, ids[0]);
    let fulfilled = ResolveReorder {
        status: ReorderResolution::Fulfilled,
    };
    assert_eq!(
        f.client
            .put(&resolution)
            .bearer_auth(INTEGRATION)
            .header("X-Maintenance-System-Actor", "ha:integration")
            .header("If-Match", "\"1\"")
            .json(&fulfilled)
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::FORBIDDEN
    );
    let (one, two) = tokio::join!(
        f.client
            .put(&resolution)
            .bearer_auth(OWNER)
            .header("If-Match", "\"1\"")
            .json(&fulfilled)
            .send(),
        f.client
            .put(&resolution)
            .bearer_auth(OWNER)
            .header("If-Match", "\"1\"")
            .json(&ResolveReorder {
                status: ReorderResolution::Dismissed
            })
            .send()
    );
    let one = one.unwrap();
    let two = two.unwrap();
    assert!([one.status(), two.status()].contains(&StatusCode::OK));
    assert!([one.status(), two.status()].contains(&StatusCode::PRECONDITION_FAILED));
    let resolved = if one.status() == StatusCode::OK {
        one.json::<Reorder>().await.unwrap()
    } else {
        two.json::<Reorder>().await.unwrap()
    };
    assert_eq!(resolved.revision, 2);
    assert!(resolved.resolved_at.is_some());
    assert_eq!(resolved.resolved_by.as_ref().unwrap().key(), "system:owner");
    let same_status = match resolved.status {
        ReorderStatus::Fulfilled => ReorderResolution::Fulfilled,
        ReorderStatus::Dismissed => ReorderResolution::Dismissed,
        ReorderStatus::Pending => panic!("resolution remained pending"),
    };
    let repeated = f
        .client
        .put(&resolution)
        .bearer_auth(OWNER)
        .header("If-Match", "\"2\"")
        .json(&ResolveReorder {
            status: same_status,
        })
        .send()
        .await
        .unwrap();
    assert_eq!(repeated.status(), StatusCode::OK);
    assert_eq!(repeated.json::<Reorder>().await.unwrap(), resolved);
    let other_status = match same_status {
        ReorderResolution::Fulfilled => ReorderResolution::Dismissed,
        ReorderResolution::Dismissed => ReorderResolution::Fulfilled,
    };
    assert_eq!(
        f.client
            .put(&resolution)
            .bearer_auth(OWNER)
            .header("If-Match", "\"2\"")
            .json(&ResolveReorder {
                status: other_status
            })
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::CONFLICT
    );
    let next: Page<Reorder> = f
        .client
        .get(&list_url)
        .bearer_auth(OWNER)
        .query(&[("limit", "2"), ("page_token", cursor.as_str())])
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(
        next.items
            .iter()
            .map(|record| record.request.id)
            .collect::<Vec<_>>(),
        ids[2..4]
    );
    let final_page: Page<Reorder> = f
        .client
        .get(&list_url)
        .bearer_auth(OWNER)
        .query(&[
            ("limit", "2"),
            ("page_token", next.next_page.as_deref().unwrap()),
        ])
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(final_page.items[0].request.id, ids[4]);
    assert!(final_page.next_page.is_none());
    for query in [
        vec![("status", "fulfilled"), ("page_token", cursor.as_str())],
        vec![("limit", "0")],
        vec![("limit", "201")],
        vec![("page_token", "invalid")],
        vec![("status", "unknown")],
    ] {
        assert_eq!(
            f.client
                .get(&list_url)
                .bearer_auth(OWNER)
                .query(&query)
                .send()
                .await
                .unwrap()
                .status(),
            StatusCode::BAD_REQUEST
        );
    }
    f.finish().await;
}

/// Own the real child even if an assertion fails, so tests never leave a server behind.
struct BackendProcess(std::process::Child);
impl BackendProcess {
    async fn start(database: &std::path::Path, address: SocketAddr) -> Self {
        let child = std::process::Command::new(env!("CARGO_BIN_EXE_home-maintenance"))
            .args(["--bind", &address.to_string(), "--database"])
            .arg(database)
            .env("MAINTENANCE_API_TOKEN", OWNER)
            .env("MAINTENANCE_INTEGRATION_TOKEN", INTEGRATION)
            .env_remove("MAINTENANCE_INGRESS_BIND")
            .env_remove("SUPERVISOR_TOKEN")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap();
        let mut process = Self(child);
        let client = Client::new();
        for _ in 0..100 {
            assert!(
                process.0.try_wait().unwrap().is_none(),
                "backend exited during startup"
            );
            if client
                .get(format!("http://{address}/health"))
                .send()
                .await
                .is_ok_and(|response| response.status().is_success())
            {
                return process;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        panic!("backend did not become ready");
    }
    fn crash(&mut self) {
        self.0.kill().unwrap();
        self.0.wait().unwrap();
    }
}
impl Drop for BackendProcess {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[tokio::test]
async fn reorder_lost_response_survives_a_real_backend_process_crash_and_restart() {
    use home_maintenance::models::{Reorder, ReorderReceipt};
    use std::io::Write as _;
    let directory = tempfile::tempdir().unwrap();
    let database = directory.path().join("process.sqlite3");
    let store = Store::open(&database).await.unwrap();
    let mut create = input("example-process-reorder");
    create.definition.reorder_url = Some("https://example.invalid/replacement".into());
    let actor = Actor::System {
        id: home_maintenance::models::SystemActor::Owner,
        name: "Owner credential".into(),
    };
    let item = store.create(create, &actor, "seed-process").await.unwrap();
    store.close().await.unwrap();
    let listener = std::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    let address = listener.local_addr().unwrap();
    drop(listener);
    let mut process = BackendProcess::start(&database, address).await;
    let mut connection = std::net::TcpStream::connect(address).unwrap();
    write!(connection, "POST /v1/items/{}/reorders HTTP/1.1\r\nHost: {address}\r\nAuthorization: Bearer {INTEGRATION}\r\nX-Maintenance-System-Actor: ha:integration\r\nIf-Match: \"{}\"\r\nIdempotency-Key: lost-process-response\r\nContent-Type: application/json\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{{}}", item.id.as_ref(), item.revision).unwrap();
    connection.flush().unwrap();
    // The client never reads the response. Observe the transaction independently
    // before killing the actual process, without relying on an in-memory Store.
    let observer = Store::open(&database).await.unwrap();
    let mut committed = None;
    for _ in 0..100 {
        let receipt: Option<String> = sqlx::query_scalar("SELECT receipt FROM reorders")
            .fetch_optional(observer.pool())
            .await
            .unwrap();
        if let Some(receipt) = receipt {
            committed = Some(serde_json::from_str::<ReorderReceipt>(&receipt).unwrap());
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    let original = committed.expect("request was not committed");
    drop(connection);
    process.crash();
    observer.close().await.unwrap();
    let mut restarted = BackendProcess::start(&database, address).await;
    let client = Client::new();
    let response = client
        .post(format!(
            "http://{address}/v1/items/{}/reorders",
            item.id.as_ref()
        ))
        .bearer_auth(INTEGRATION)
        .header("X-Maintenance-System-Actor", "ha:integration")
        .header("If-Match", format!("\"{}\"", item.revision))
        .header("Idempotency-Key", "lost-process-response")
        .json(&home_maintenance::models::ReorderItem::default())
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::CREATED);
    assert_eq!(response.json::<ReorderReceipt>().await.unwrap(), original);
    let page: Page<Reorder> = client
        .get(format!("http://{address}/v1/reorders"))
        .bearer_auth(INTEGRATION)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(page.items.len(), 1);
    assert_eq!(page.items[0].request, original);
    restarted.crash();
}

fn migration_item(id: &str) -> home_maintenance::models::MigrationItem {
    use home_maintenance::models::{ItemStatus, MigrationItem, SourceRef};
    let mut definition = input(id);
    definition.due = None;
    definition.definition.schedule = Schedule::Once;
    MigrationItem {
        source: SourceRef {
            system: "example-source".into(),
            collection: "example-list".into(),
            id: format!("Opaque {id} / original"),
        },
        item: definition,
        status: ItemStatus::NeedsAction,
        archived: false,
        history: vec![],
    }
}
async fn migration_request(
    f: &Fixture,
    input: &home_maintenance::models::Migration,
    preview: bool,
    key: &str,
) -> Response {
    f.client
        .post(format!(
            "{}/v1/migrations{}",
            f.origin,
            if preview { "/preview" } else { "" }
        ))
        .bearer_auth(OWNER)
        .header("Idempotency-Key", key)
        .json(input)
        .send()
        .await
        .unwrap()
}
#[tokio::test]
async fn unknown_due_once_standing_and_recurring_have_distinct_completion_lifecycles() {
    use home_maintenance::models::ItemStatus;
    let f = Fixture::new().await;
    for (id, schedule) in [
        ("example-once", Schedule::Once),
        ("example-standing", Schedule::Standing),
        ("example-recurring", Schedule::new(10, 0, 0).unwrap()),
    ] {
        let mut definition = input(id);
        definition.due = None;
        definition.definition.schedule = schedule;
        let response = f
            .client
            .post(format!("{}/v1/items", f.origin))
            .bearer_auth(OWNER)
            .header("Idempotency-Key", id)
            .json(&definition)
            .send()
            .await
            .unwrap();
        assert_eq!(
            response.status(),
            StatusCode::CREATED,
            "{}",
            response.text().await.unwrap()
        );
        let item: Item = response.json().await.unwrap();
        assert_eq!(item.occurrence.due, None);
        assert_eq!(item.status, ItemStatus::NeedsAction);
        let response = complete(&f, &item, &format!("complete-{id}")).await;
        assert_eq!(
            response.status(),
            StatusCode::OK,
            "{}",
            response.text().await.unwrap()
        );
        let completed: Item = response.json().await.unwrap();
        let event = completed.last_completion.as_ref().unwrap();
        assert!(event.completed_at.is_some());
        assert!(event.actor.is_some());
        assert_eq!(event.previous_due, None);
        match schedule {
            Schedule::Once => {
                assert_eq!(completed.status, ItemStatus::Completed);
                assert_eq!(completed.occurrence.id, item.occurrence.id);
                assert_eq!(event.next_due, None);
                assert_eq!(
                    complete(&f, &completed, "complete-closed").await.status(),
                    StatusCode::CONFLICT
                );
                assert_eq!(
                    set_step(&f, &completed, Uuid::from_u128(1), true, "check-closed")
                        .await
                        .status(),
                    StatusCode::CONFLICT
                );
            }
            Schedule::Standing => {
                assert_eq!(completed.status, ItemStatus::NeedsAction);
                assert_ne!(completed.occurrence.id, item.occurrence.id);
                assert_eq!(completed.occurrence.due, None);
                assert_eq!(event.next_due, None);
            }
            Schedule::Recurring { interval } => {
                assert_eq!(completed.status, ItemStatus::NeedsAction);
                assert_ne!(completed.occurrence.id, item.occurrence.id);
                assert_eq!(
                    completed.occurrence.due,
                    Some(
                        interval
                            .next(
                                event
                                    .completed_at
                                    .unwrap()
                                    .with_timezone(&completed.definition.timezone.0)
                                    .date_naive()
                            )
                            .unwrap()
                    )
                );
            }
        }
        let replay: Item = complete(&f, &item, &format!("complete-{id}"))
            .await
            .error_for_status()
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(
            serde_json::to_vec(&completed).unwrap(),
            serde_json::to_vec(&replay).unwrap()
        );
    }
    let summary = f.store.summary().await.unwrap();
    assert_eq!(summary.active, 2);
    assert_eq!(summary.due_soon, 1);
    assert_eq!(summary.overdue, 0);
    assert_eq!(summary.completed, 3);
    // Omitted schedule defaults to a once task, and omitted due stays unknown.
    let response = f.client.post(format!("{}/v1/items",f.origin)).bearer_auth(OWNER).header("Idempotency-Key","minimal-default")
        .header("Content-Type","application/json").body(r#"{"id":"example-default","name":"Example","summary":"Inspect example","timezone":"Etc/UTC"}"#).send().await.unwrap();
    assert_eq!(response.status(), StatusCode::CREATED);
    let item: Item = response.json().await.unwrap();
    assert_eq!(item.definition.schedule, Schedule::Once);
    assert_eq!(item.occurrence.due, None);
    f.finish().await;
}
#[tokio::test]
async fn dated_then_unknown_pagination_and_due_clearing_are_null_safe() {
    let f = Fixture::new().await;
    for (id, due) in [
        ("example-undated-a", None),
        ("example-early", Some("2031-01-01")),
        ("example-undated-b", None),
        ("example-late", Some("2032-01-01")),
    ] {
        let mut item = input(id);
        item.due = due.map(|date| NaiveDate::parse_from_str(date, "%Y-%m-%d").unwrap());
        f.store
            .create(
                item,
                &Actor::System {
                    id: home_maintenance::models::SystemActor::Owner,
                    name: "Owner credential".into(),
                },
                id,
            )
            .await
            .unwrap();
    }
    let mut cursor = None;
    let mut ids = Vec::new();
    loop {
        let mut request = f
            .client
            .get(format!("{}/v1/items?limit=1", f.origin))
            .bearer_auth(OWNER);
        if let Some(value) = &cursor {
            request = request.query(&[("page_token", value)]);
        }
        let page: Page<Item> = request
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap()
            .json()
            .await
            .unwrap();
        ids.extend(page.items.iter().map(|item| item.id.as_ref().to_owned()));
        cursor = page.next_page;
        if cursor.is_none() {
            break;
        }
    }
    assert_eq!(
        ids,
        [
            "example-early",
            "example-late",
            "example-undated-a",
            "example-undated-b"
        ]
    );
    let early = f.get("example-early").await;
    let response = f
        .client
        .patch(format!("{}/v1/items/example-early", f.origin))
        .bearer_auth(OWNER)
        .header("If-Match", format!("\"{}\"", early.revision))
        .json(&UpdateItem {
            due: Change(Some(None)),
            ..Default::default()
        })
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let cleared: Item = response.json().await.unwrap();
    assert_eq!(cleared.occurrence.due, None);
    assert_eq!(f.store.summary().await.unwrap().active, 4);
    f.finish().await;
}
#[tokio::test]
async fn migration_preview_apply_history_and_replays_preserve_known_and_unknown_facts() {
    use home_maintenance::models::{
        ImportedCompletion, ItemStatus, Migration, MigrationAction, MigrationReport, SourceRef,
    };
    let f = Fixture::new().await;
    let mut active = migration_item("example-migrated");
    active.item.definition.schedule = Schedule::Standing;
    let known_time = chrono::DateTime::parse_from_rfc3339("2025-02-03T04:05:06+02:00")
        .unwrap()
        .to_utc();
    let event_id = Uuid::from_u128(1234);
    let event_occurrence = Uuid::from_u128(5678);
    active.history = vec![
        ImportedCompletion {
            id: Some(event_id),
            occurrence_id: Some(event_occurrence),
            source: Some(SourceRef {
                system: "example-source".into(),
                collection: "history".into(),
                id: "Original event / AbC".into(),
            }),
            completed_at: Some(known_time),
            ..Default::default()
        },
        ImportedCompletion::default(),
    ];
    let mut closed = migration_item("example-closed");
    closed.status = ItemStatus::Completed;
    let input = Migration {
        items: vec![active.clone(), closed.clone()],
    };
    let preview = migration_request(&f, &input, true, "unused-preview").await;
    assert_eq!(preview.status(), StatusCode::OK);
    assert_eq!(preview.headers()["cache-control"], "no-store");
    let preview: MigrationReport = preview.json().await.unwrap();
    assert!(preview.applicable);
    assert!(
        preview
            .items
            .iter()
            .all(|item| item.action == MigrationAction::Created)
    );
    for (table, query) in [
        ("items", "SELECT count(*) FROM items"),
        ("completions", "SELECT count(*) FROM completions"),
        (
            "migration_sources",
            "SELECT count(*) FROM migration_sources",
        ),
        ("idempotency", "SELECT count(*) FROM idempotency"),
    ] {
        let rows: i64 = sqlx::query_scalar(query)
            .fetch_one(f.store.pool())
            .await
            .unwrap();
        assert_eq!(rows, 0, "preview wrote {table}");
    }
    // Concurrent retries model a response lost before the client could receive it.
    let (first, retry) = tokio::join!(
        migration_request(&f, &input, false, "migration-attempt"),
        migration_request(&f, &input, false, "migration-attempt")
    );
    assert_eq!(first.status(), StatusCode::CREATED);
    assert_eq!(retry.status(), StatusCode::CREATED);
    let receipt: MigrationReport = first.json().await.unwrap();
    assert_eq!(receipt, retry.json::<MigrationReport>().await.unwrap());
    let item = f.get("example-migrated").await;
    assert_eq!(item.source, Some(active.source));
    assert_eq!(item.status, ItemStatus::NeedsAction);
    assert_eq!(item.occurrence.due, None);
    let newest = item.last_completion.as_ref().unwrap();
    assert_eq!(newest.completed_at, None);
    assert_eq!(newest.actor, None);
    assert_eq!(newest.occurrence_id, None);
    assert_eq!(newest.previous_due, None);
    assert_eq!(newest.next_due, None);
    let page: Page<Completion> = f
        .client
        .get(format!("{}/v1/items/example-migrated/events", f.origin))
        .bearer_auth(OWNER)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(page.items.len(), 2);
    let known = &page.items[1];
    assert_eq!(known.id, event_id);
    assert_eq!(known.occurrence_id, Some(event_occurrence));
    assert_eq!(known.completed_at, Some(known_time));
    assert_eq!(known.actor, None);
    assert_eq!(known.source, active.history[0].source);
    let completed = f.get("example-closed").await;
    assert_eq!(completed.status, ItemStatus::Completed);
    assert_eq!(completed.occurrence.due, None);
    assert!(completed.last_completion.is_none());
    let history: Page<Completion> = f
        .client
        .get(format!("{}/v1/items/example-closed/events", f.origin))
        .bearer_auth(OWNER)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(history.items.is_empty());
    assert_eq!(
        complete(&f, &completed, "closed-no-event").await.status(),
        StatusCode::CONFLICT
    );
    let summary = f.store.summary().await.unwrap();
    assert_eq!(summary.active, 1);
    assert_eq!(summary.completed, 2);
    assert_eq!(summary.overdue, 0);
    assert_eq!(summary.due_soon, 0);
    let unchanged: MigrationReport = migration_request(&f, &input, true, "unused")
        .await
        .json()
        .await
        .unwrap();
    assert!(
        unchanged
            .items
            .iter()
            .all(|item| item.action == MigrationAction::Unchanged)
    );
    f.store
        .update(
            &item.id,
            item.revision,
            UpdateItem {
                name: Change(Some("Edited current item".into())),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    let replay: MigrationReport = migration_request(&f, &input, false, "migration-attempt")
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(replay, receipt);
    let fresh: MigrationReport = migration_request(&f, &input, false, "migration-again")
        .await
        .json()
        .await
        .unwrap();
    assert!(
        fresh
            .items
            .iter()
            .all(|item| item.action == MigrationAction::Unchanged)
    );
    assert_eq!(
        f.get("example-migrated").await.definition.name,
        "Edited current item"
    );
    let mut changed = input.clone();
    changed.items[0].item.definition.summary = "Changed migration content".into();
    assert_eq!(
        migration_request(&f, &changed, false, "migration-attempt")
            .await
            .status(),
        StatusCode::CONFLICT
    );
    let conflicting: MigrationReport = migration_request(&f, &changed, true, "unused")
        .await
        .json()
        .await
        .unwrap();
    assert!(!conflicting.applicable);
    assert_eq!(conflicting.items[0].action, MigrationAction::Conflict);
    assert_eq!(
        migration_request(&f, &changed, false, "different-migration")
            .await
            .status(),
        StatusCode::CONFLICT
    );
    let mut colliding_history = migration_item("example-history-collision");
    colliding_history.history = vec![active.history[0].clone()];
    let colliding_history = Migration {
        items: vec![colliding_history],
    };
    let conflict: MigrationReport = migration_request(&f, &colliding_history, true, "unused")
        .await
        .json()
        .await
        .unwrap();
    assert!(!conflict.applicable);
    assert!(
        conflict.items[0]
            .conflicts
            .iter()
            .any(|reason| reason == "event id already exists")
    );
    assert!(
        conflict.items[0]
            .conflicts
            .iter()
            .any(|reason| reason == "event occurrence id already exists")
    );
    assert!(
        conflict.items[0]
            .conflicts
            .iter()
            .any(|reason| reason == "event source identity already exists")
    );
    assert_eq!(
        migration_request(&f, &colliding_history, false, "collision-with-history")
            .await
            .status(),
        StatusCode::CONFLICT
    );
    // A cold reopen retains mapping and immutable receipts after current item edits.
    let database = f.directory.path().join("maintenance.sqlite3");
    f.server.close().await.unwrap();
    f.store.close().await.unwrap();
    let reopened = Store::open(&database).await.unwrap();
    let owner = Actor::System {
        id: home_maintenance::models::SystemActor::Owner,
        name: "Owner credential".into(),
    };
    assert_eq!(
        reopened
            .migrate(input.clone(), &owner, "migration-attempt")
            .await
            .unwrap(),
        receipt
    );
    assert_eq!(
        reopened
            .events(&item.id, None, 100)
            .await
            .unwrap()
            .items
            .len(),
        2
    );
    reopened.close().await.unwrap();
}
#[tokio::test]
async fn migration_conflicts_are_batched_atomic_and_cannot_claim_a_live_occurrence() {
    use home_maintenance::models::{ImportedCompletion, Migration, MigrationReport};
    let f = Fixture::new().await;
    let existing = f.create("example-existing").await;
    let mut claimed = migration_item("example-claim");
    claimed.history.push(ImportedCompletion {
        occurrence_id: Some(existing.occurrence.id),
        ..Default::default()
    });
    let attempt = Migration {
        items: vec![migration_item("example-before-conflict"), claimed],
    };
    let preview: MigrationReport = migration_request(&f, &attempt, true, "unused")
        .await
        .json()
        .await
        .unwrap();
    assert!(!preview.applicable);
    assert!(
        preview.items[1]
            .conflicts
            .iter()
            .any(|reason| reason.contains("event occurrence id already exists"))
    );
    assert_eq!(
        migration_request(&f, &attempt, false, "claim-attempt")
            .await
            .status(),
        StatusCode::CONFLICT
    );
    assert!(matches!(
        f.store.get(&attempt.items[0].item.id).await,
        Err(home_maintenance::error::Error::NotFound)
    ));
    assert_eq!(
        complete(&f, &existing, "existing-still-completable")
            .await
            .status(),
        StatusCode::OK
    );
    let mut duplicate = migration_item("example-duplicate");
    let repeated = Uuid::from_u128(999);
    duplicate.history = vec![
        ImportedCompletion {
            id: Some(repeated),
            ..Default::default()
        },
        ImportedCompletion {
            id: Some(repeated),
            ..Default::default()
        },
    ];
    let duplicate = Migration {
        items: vec![duplicate],
    };
    let report: MigrationReport = migration_request(&f, &duplicate, true, "unused")
        .await
        .json()
        .await
        .unwrap();
    assert!(!report.applicable);
    assert_eq!(
        migration_request(&f, &duplicate, false, "duplicate-event")
            .await
            .status(),
        StatusCode::CONFLICT
    );
    let first = Migration {
        items: vec![migration_item("example-raced")],
    };
    assert!(
        migration_request(&f, &first, true, "unused")
            .await
            .json::<MigrationReport>()
            .await
            .unwrap()
            .applicable
    );
    f.create("example-raced").await;
    assert_eq!(
        migration_request(&f, &first, false, "stale-preview")
            .await
            .status(),
        StatusCode::CONFLICT
    );
    let duplicated = Migration {
        items: vec![
            migration_item("example-source-collision"),
            migration_item("example-source-collision"),
        ],
    };
    assert!(
        !migration_request(&f, &duplicated, true, "unused")
            .await
            .json::<MigrationReport>()
            .await
            .unwrap()
            .applicable
    );
    f.finish().await;
}
#[tokio::test]
async fn migration_requires_admin_and_validates_before_writing() {
    use home_maintenance::models::Migration;
    let f = Fixture::new().await;
    let input = Migration {
        items: vec![migration_item("example-auth")],
    };
    for path in ["migrations", "migrations/preview"] {
        let response = f
            .client
            .post(format!("{}/v1/{path}", f.origin))
            .bearer_auth(INTEGRATION)
            .header("X-Maintenance-System-Actor", "ha:integration")
            .header("Idempotency-Key", "forbidden-migration")
            .json(&input)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
    }
    assert_eq!(
        f.client
            .post(format!("{}/v1/migrations", f.origin))
            .bearer_auth(OWNER)
            .json(&input)
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::BAD_REQUEST
    );
    let invalid = Migration { items: vec![] };
    assert_eq!(
        migration_request(&f, &invalid, false, "invalid-migration")
            .await
            .status(),
        StatusCode::BAD_REQUEST
    );
    assert_eq!(f.store.list(false, None, 200).await.unwrap().items.len(), 0);
    f.finish().await;
}

#[tokio::test]
async fn simultaneous_migrations_cannot_overwrite_one_target_or_duplicate_history() {
    use home_maintenance::models::{
        ImportedCompletion, Migration, MigrationAction, MigrationReport,
    };
    let f = Fixture::new().await;
    let mut original = migration_item("example-concurrent");
    original.history.push(ImportedCompletion::default());
    let input = Migration {
        items: vec![original.clone()],
    };
    let (first, second) = tokio::join!(
        migration_request(&f, &input, false, "concurrent-first"),
        migration_request(&f, &input, false, "concurrent-second")
    );
    assert_eq!(first.status(), StatusCode::CREATED);
    assert_eq!(second.status(), StatusCode::CREATED);
    let first: MigrationReport = first.json().await.unwrap();
    let second: MigrationReport = second.json().await.unwrap();
    assert_ne!(first.items[0].action, second.items[0].action);
    assert!([first.items[0].action, second.items[0].action].contains(&MigrationAction::Created));
    assert!([first.items[0].action, second.items[0].action].contains(&MigrationAction::Unchanged));
    assert_eq!(
        f.store
            .events(&original.item.id, None, 100)
            .await
            .unwrap()
            .items
            .len(),
        1
    );
    assert_eq!(f.store.list(false, None, 100).await.unwrap().items.len(), 1);
    f.finish().await;
}
