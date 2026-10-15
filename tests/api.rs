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
        due: NaiveDate::from_ymd_opt(2031, 2, 17).unwrap(),
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
    assert_eq!(event.occurrence_id, before.occurrence.id);
    assert_eq!(
        event.next_due,
        first
            .definition
            .schedule
            .next(
                event
                    .completed_at
                    .with_timezone(&first.definition.timezone.0)
                    .date_naive()
            )
            .unwrap()
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
        Actor::System { .. }
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
        matches!(result.last_completion.unwrap().actor,Actor::User {id,..} if id=="ha:example-user")
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
