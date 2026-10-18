#!/usr/bin/env bash
set -euo pipefail
umask 077

repo_root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)

image=${1:?Usage: container-smoke.sh IMAGE EXPECTED_ARCH}
expected_arch=${2:?Usage: container-smoke.sh IMAGE EXPECTED_ARCH}
actual_arch=$(docker image inspect "$image" --format '{{.Architecture}}')
if [[ $actual_arch != "$expected_arch" ]]; then
    echo "Expected $expected_arch image, got $actual_arch" >&2
    exit 1
fi

scratch=$(mktemp -d "${TMPDIR:-/tmp}/maintenance-smoke.XXXXXX")
container="maintenance-smoke-${RANDOM}-${RANDOM}"
volume="$container-data"
restored_volume="$container-restored"
# Fictional, disposable credentials. No provider or Home Assistant is contacted.
token='container-smoke-owner-fixture-only'
integration_token='container-smoke-integration-fixture-only'
step_id='30000000-0000-4000-8000-000000000001'
other_step_id='30000000-0000-4000-8000-000000000002'

cleanup() {
    status=$?
    if [[ $status -ne 0 ]]; then
        docker logs "$container" >&2 || true
    fi
    docker rm --force "$container" >/dev/null 2>&1 || true
    docker volume rm "$volume" "$restored_volume" >/dev/null 2>&1 || true
    rm -rf "$scratch"
    exit "$status"
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

request() {
    local method=$1 path=$2 expected=$3 actual
    shift 3
    actual=$(curl --silent --show-error --max-time 5 \
        --request "$method" --output "$scratch/response.json" \
        --dump-header "$scratch/headers" --write-out '%{http_code}' \
        "$base_url$path" "$@")
    if [[ $actual != "$expected" ]]; then
        # Response bodies can contain personal data or credentials. Report only
        # the failing operation and status, even in disposable smoke tests.
        echo "$method $path: expected HTTP $expected, got $actual" >&2
        return 1
    fi
}

wait_for_api() {
    for _ in {1..60}; do
        if curl --fail --silent --max-time 1 "$base_url/health" \
            --output /dev/null; then
            return
        fi
        sleep 0.25
    done
    echo 'Container did not become ready' >&2
    return 1
}

check_process_user() {
    local uid
    uid=$(docker exec "$container" cat /proc/1/status | awk '$1 == "Uid:" { print $2 }')
    if [[ $uid != 10001 ]]; then
        echo "Expected server uid10001, got $uid" >&2
        return 1
    fi
}

stop_container() {
    local exit_code
    docker stop --time 10 "$container" >/dev/null
    exit_code=$(docker inspect "$container" --format '{{.State.ExitCode}}')
    if [[ $exit_code != 0 ]]; then
        echo "Server did not stop cleanly: exit $exit_code" >&2
        return 1
    fi
}

start_container() {
    docker start "$container" >/dev/null
    base_url="http://$(docker port "$container" 8080/tcp)"
    wait_for_api
    check_process_user
}

etag() {
    awk 'tolower($1) == "etag:" {gsub("\r", "", $2); print $2}' "$scratch/headers"
}

check_item_contract() {
    python3 - "$scratch" "$1" "$occurrence" "${next_occurrence:-}" "$step_id" "$other_step_id" <<'PY'
import json
import sys
from datetime import datetime, timedelta
from pathlib import Path
from typing import Literal, TypedDict
from uuid import UUID


class Actor(TypedDict):
    kind: Literal["user", "system"]
    id: str
    name: str


class StepCompletion(TypedDict):
    completed_at: str
    actor: Actor


class Step(TypedDict):
    id: str
    summary: str
    completion: StepCompletion | None


class Occurrence(TypedDict):
    id: str
    due: str
    checklist: list[Step]


class Completion(TypedDict):
    occurrence_id: str
    completed_at: str
    previous_due: str
    next_due: str
    checklist: list[Step]


class Interval(TypedDict):
    days: int
    months: int
    years: int


class RecurringSchedule(TypedDict):
    kind: Literal["recurring"]
    interval: Interval


class Item(TypedDict):
    id: str
    name: str
    summary: str
    description: str | None
    archived: bool
    timezone: str
    schedule: RecurringSchedule
    occurrence: Occurrence
    last_completion: Completion | None


class History(TypedDict):
    items: list[Completion]
    next_page: str | None


scratch = Path(sys.argv[1])
mode, occurrence, next_occurrence, first_id, second_id = sys.argv[2:]
if mode == "history":
    history: History = json.loads((scratch / "response.json").read_text())
    expected: Completion = json.loads((scratch / "completion.json").read_text())
    assert history["items"] == [expected], "Completion history changed"
    assert history["next_page"] is None
    sys.exit(0)

item: Item = json.loads((scratch / "response.json").read_text())
assert item["id"] == "example-filter"
assert item["name"] == "Example filter"
assert item["summary"] == "Replace the example filter"
assert item["description"] == "Example task with a partial checklist."
assert item["timezone"] == "Etc/UTC" and item["archived"] is False
assert item["schedule"] == {
    "kind": "recurring", "interval": {"days": 47, "months": 0, "years": 0}
}
assert str(UUID(item["occurrence"]["id"])) == item["occurrence"]["id"]
steps = item["occurrence"]["checklist"]
assert [(step["id"], step["summary"]) for step in steps] == [
    (first_id, "Inspect the example filter"),
    (second_id, "Fit the example replacement"),
]
for step in steps:
    checked = step["completion"]
    if checked is not None:
        timestamp = datetime.fromisoformat(checked["completed_at"].replace("Z", "+00:00"))
        assert timestamp.utcoffset() == timedelta(0)

if mode in {"fresh", "partial"}:
    assert item["occurrence"]["id"] == occurrence
    assert item["occurrence"]["due"] == "2031-02-17"
    assert item["last_completion"] is None
    assert steps[1]["completion"] is None
    if mode == "fresh":
        assert steps[0]["completion"] is None
    else:
        checked = steps[0]["completion"]
        assert checked is not None
        assert checked["actor"] == {
            "kind": "user", "id": "ha:example-user", "name": "Example user"
        }
else:
    assert item["occurrence"]["id"] == next_occurrence != occurrence
    completed = item["last_completion"]
    assert completed is not None and completed["occurrence_id"] == occurrence
    completed_at = datetime.fromisoformat(completed["completed_at"].replace("Z", "+00:00"))
    assert completed["previous_due"] == "2031-02-17"
    assert item["occurrence"]["due"] == completed["next_due"]
    assert completed["next_due"] == (completed_at.date() + timedelta(days=47)).isoformat()
    partial: list[Step] = json.loads((scratch / "partial-checklist.json").read_text())
    assert completed["checklist"] == partial, "Partial checklist snapshot changed"
    if mode == "reset":
        assert all(step["completion"] is None for step in steps)
    else:
        assert mode == "persisted"
        current: list[Step] = json.loads((scratch / "current-checklist.json").read_text())
        assert steps == current, "Current checklist did not survive restart or restore"
        assert steps[0]["completion"] is None
        checked = steps[1]["completion"]
        assert checked is not None and checked["actor"]["kind"] == "system"
        assert checked["actor"]["id"] == "owner"
PY
}

check_item() {
    request GET /v1/items/example-filter 200 --header "Authorization: Bearer $token"
    check_item_contract persisted
    request GET /v1/items/example-filter/events 200 --header "Authorization: Bearer $token"
    check_item_contract history
    check_reorder
}

check_reorder() {
    # Replaying the accepted press must keep its immutable acknowledgement,
    # including after resolution and later changes to the source task.
    request POST /v1/items/example-filter/reorders 201 \
        --header "Authorization: Bearer $integration_token" \
        --header 'X-Maintenance-System-Actor: ha:integration' \
        --header 'Content-Type: application/json' \
        --header 'Idempotency-Key: smoke-reorder-filter' \
        --header "If-Match: $reorder_revision" --data '{}'
    cmp --silent "$scratch/reorder-receipt.json" "$scratch/response.json"
    request GET "/v1/reorders/$reorder_id" 200 \
        --header "Authorization: Bearer $integration_token"
    cmp --silent "$scratch/resolved-reorder.json" "$scratch/response.json"
    request GET /v1/reorders 200 --header "Authorization: Bearer $integration_token"
    jq --exit-status '.items == [] and .next_page == null' "$scratch/response.json" >/dev/null
    request GET '/v1/reorders?status=fulfilled&limit=1' 200 \
        --header "Authorization: Bearer $integration_token"
    jq --exit-status --slurpfile expected "$scratch/resolved-reorder.json" \
        '.items == $expected and .next_page == null' "$scratch/response.json" >/dev/null
}

check_credentials() {
    local permissions
    permissions=$(docker exec "$container" stat -c '%a:%u:%g' /data/credentials.json)
    if [[ $permissions != 600:10001:10001 ]]; then
        echo "Expected credential mode/owner 600:10001:10001, got $permissions" >&2
        return 1
    fi
}

docker volume create "$volume" >/dev/null
docker run --detach --name "$container" \
    --publish 127.0.0.1::8080 \
    --mount "type=volume,source=$volume,target=/data" \
    --env "MAINTENANCE_API_TOKEN=$token" \
    --env "MAINTENANCE_INTEGRATION_TOKEN=$integration_token" \
    "$image" >/dev/null
base_url="http://$(docker port "$container" 8080/tcp)"
wait_for_api
check_process_user
request GET /v1/system 401
request GET /v1/system 401 --header 'Authorization: Bearer incorrect-fixture'
request GET /v1/system 200 --header "Authorization: Bearer $token"
jq --exit-status '.credential_role == "owner" and (.version | type == "string" and length > 0)' "$scratch/response.json" >/dev/null
request GET /v1/system 200 --header "Authorization: Bearer $integration_token"
jq --exit-status '.credential_role == "integration"' "$scratch/response.json" >/dev/null

request POST /v1/imports 200 \
    --header "Authorization: Bearer $token" \
    --header 'Content-Type: application/json' \
    --header 'Idempotency-Key: smoke-import-examples' \
    --data-binary "@$repo_root/config/items.example.json"
jq --exit-status '(.items | length) == 2' "$scratch/response.json" >/dev/null

request POST /v1/items 201 \
    --header "Authorization: Bearer $token" \
    --header 'Content-Type: application/json' \
    --header 'Idempotency-Key: smoke-create-filter' \
    --data "{\"id\":\"example-filter\",\"name\":\"Example filter\",\"summary\":\"Replace the example filter\",\"description\":\"Example task with a partial checklist.\",\"timezone\":\"Etc/UTC\",\"due\":\"2031-02-17\",\"schedule\":{\"kind\":\"recurring\",\"interval\":{\"days\":47,\"months\":0,\"years\":0}},\"checklist\":[{\"id\":\"$step_id\",\"summary\":\"Inspect the example filter\"},{\"id\":\"$other_step_id\",\"summary\":\"Fit the example replacement\"}],\"reorder_url\":\"https://example.com/filter\"}"
occurrence=$(jq --exit-status --raw-output '.occurrence.id' "$scratch/response.json")
check_item_contract fresh
reorder_revision=$(etag)
cp "$scratch/response.json" "$scratch/reorder-source.json"
request POST /v1/items/example-filter/reorders 201 \
    --header "Authorization: Bearer $integration_token" \
    --header 'X-Maintenance-System-Actor: ha:integration' \
    --header 'Content-Type: application/json' \
    --header 'Idempotency-Key: smoke-reorder-filter' \
    --header "If-Match: $reorder_revision" --data '{}'
cp "$scratch/response.json" "$scratch/reorder-receipt.json"
reorder_id=$(jq --exit-status --raw-output '.id' "$scratch/response.json")
jq --exit-status --slurpfile source "$scratch/reorder-source.json" \
    '.item_id == $source[0].id and .item_revision == $source[0].revision
     and .actor == {kind:"system",id:"ha:integration",name:"Home Assistant integration"}
     and .item == ($source[0] | {name,summary,description,specs,resources,reorder_url})
     and (.requested_at | type == "string" and length > 0)' "$scratch/response.json" >/dev/null
# A reorder must not complete, reschedule, or otherwise mutate its source task.
request GET /v1/items/example-filter 200 --header "Authorization: Bearer $token"
cmp --silent "$scratch/reorder-source.json" "$scratch/response.json"
test "$(etag)" = "$reorder_revision"
request GET "/v1/reorders/$reorder_id" 200 --header "Authorization: Bearer $token"
reorder_resolution_revision=$(etag)
jq --exit-status --slurpfile receipt "$scratch/reorder-receipt.json" \
    '.request == $receipt[0] and .status == "pending" and .revision == 1
     and .resolved_at == null and .resolved_by == null' "$scratch/response.json" >/dev/null
request PUT "/v1/reorders/$reorder_id/resolution" 200 \
    --header "Authorization: Bearer $token" \
    --header 'Content-Type: application/json' \
    --header "If-Match: $reorder_resolution_revision" --data '{"status":"fulfilled"}'
jq --exit-status --slurpfile receipt "$scratch/reorder-receipt.json" \
    '.request == $receipt[0] and .status == "fulfilled" and .revision == 2
     and (.resolved_at | type == "string" and length > 0)
     and .resolved_by.kind == "system" and .resolved_by.id == "owner"' "$scratch/response.json" >/dev/null
cp "$scratch/response.json" "$scratch/resolved-reorder.json"
check_reorder
request GET /v1/items/example-filter 200 --header "Authorization: Bearer $token"
cmp --silent "$scratch/reorder-source.json" "$scratch/response.json"
step_revision=$(etag)
step_body=$(jq --compact-output --null-input --arg occurrence "$occurrence" '{occurrence_id:$occurrence,completed:true}')
# An ordinary authenticated Home Assistant user can check a step without
# gaining administrator access or advancing the occurrence.
request PUT "/v1/items/example-filter/checklist/$step_id" 200 \
    --header "Authorization: Bearer $integration_token" \
    --header 'X-Maintenance-Actor-Id: ha:example-user' \
    --header 'X-Maintenance-Actor-Name: Example user' \
    --header 'Content-Type: application/json' \
    --header 'Idempotency-Key: smoke-check-filter' \
    --header "If-Match: $step_revision" --data "$step_body"
check_item_contract partial
jq '.occurrence.checklist' "$scratch/response.json" > "$scratch/partial-checklist.json"
request PUT "/v1/items/example-filter/checklist/$step_id" 200 \
    --header "Authorization: Bearer $integration_token" \
    --header 'X-Maintenance-Actor-Id: ha:example-user' \
    --header 'X-Maintenance-Actor-Name: Example user' \
    --header 'Content-Type: application/json' \
    --header 'Idempotency-Key: smoke-check-filter' \
    --header "If-Match: $step_revision" --data "$step_body"
check_item_contract partial
request GET /v1/items/example-filter 200 --header "Authorization: Bearer $token"
revision=$(etag)
test -n "$revision"
body=$(jq --compact-output --null-input --arg occurrence "$occurrence" '{occurrence_id:$occurrence}')
request POST /v1/items/example-filter/completions 200 \
    --header "Authorization: Bearer $token" \
    --header 'Content-Type: application/json' \
    --header 'Idempotency-Key: smoke-complete-filter' \
    --header "If-Match: $revision" --data "$body"
next_occurrence=$(jq --exit-status --raw-output '.occurrence.id' "$scratch/response.json")
test "$occurrence" != "$next_occurrence"
check_item_contract reset
jq '.last_completion' "$scratch/response.json" > "$scratch/completion.json"
# Retry the accepted request with its original revision and occurrence. Durable
# idempotency must return that response without completing the next occurrence.
request POST /v1/items/example-filter/completions 200 \
    --header "Authorization: Bearer $token" \
    --header 'Content-Type: application/json' \
    --header 'Idempotency-Key: smoke-complete-filter' \
    --header "If-Match: $revision" --data "$body"
jq --exit-status --arg id "$next_occurrence" '.occurrence.id == $id' "$scratch/response.json" >/dev/null
stale_revision=$(etag)
# A new key cannot apply a step change to the occurrence that just completed.
request PUT "/v1/items/example-filter/checklist/$step_id" 409 \
    --header "Authorization: Bearer $token" \
    --header 'Content-Type: application/json' \
    --header 'Idempotency-Key: smoke-stale-step' \
    --header "If-Match: $stale_revision" --data "$step_body"

# Keep a check on the successor to verify in-progress state independently of
# the immutable completed snapshot through both restarts and cold restore.
request GET /v1/items/example-filter 200 --header "Authorization: Bearer $token"
current_step_revision=$(etag)
current_step_body=$(jq --compact-output --null-input --arg occurrence "$next_occurrence" '{occurrence_id:$occurrence,completed:true}')
request PUT "/v1/items/example-filter/checklist/$other_step_id" 200 \
    --header "Authorization: Bearer $token" \
    --header 'Content-Type: application/json' \
    --header 'Idempotency-Key: smoke-check-successor' \
    --header "If-Match: $current_step_revision" --data "$current_step_body"
jq '.occurrence.checklist' "$scratch/response.json" > "$scratch/current-checklist.json"
check_item

# Supervisor supplies root-owned options. Restart through that path to exercise
# generated pairing credentials and their private ownership on a persistent volume.
stop_container
printf '{}\n' > "$scratch/options.json"
docker cp "$scratch/options.json" "$container:/data/options.json"
docker rm "$container" >/dev/null
docker run --detach --name "$container" \
    --publish 127.0.0.1::8080 \
    --mount "type=volume,source=$volume,target=/data" \
    "$image" >/dev/null
base_url="http://$(docker port "$container" 8080/tcp)"
wait_for_api
check_process_user
check_credentials
docker cp "$container:/data/credentials.json" "$scratch/credentials.json"
jq --exit-status '.owner | type == "string" and length >= 32' "$scratch/credentials.json" >/dev/null
jq --exit-status '.integration | type == "string" and length >= 32' "$scratch/credentials.json" >/dev/null
token=$(jq --exit-status --raw-output '.owner' "$scratch/credentials.json")
integration_token=$(jq --exit-status --raw-output '.integration' "$scratch/credentials.json")
request GET /v1/system 200 --header "Authorization: Bearer $token"
jq --exit-status '.credential_role == "owner"' "$scratch/response.json" >/dev/null
request GET /v1/system 200 --header "Authorization: Bearer $integration_token"
jq --exit-status '.credential_role == "integration"' "$scratch/response.json" >/dev/null
check_item
stop_container
start_container
check_credentials
docker cp "$container:/data/credentials.json" "$scratch/restarted-credentials.json"
cmp --silent "$scratch/credentials.json" "$scratch/restarted-credentials.json"
check_item

# Cold backup: copy the stopped application's complete data volume, restore it
# into a fresh volume, and verify through a fresh running service.
stop_container
docker run --rm --user root --entrypoint /bin/tar \
    --mount "type=volume,source=$volume,target=/data,readonly" \
    "$image" -C /data -cf - . > "$scratch/data.tar"
docker volume create "$restored_volume" >/dev/null
docker run --rm --interactive --user root --entrypoint /bin/tar \
    --mount "type=volume,source=$restored_volume,target=/data" \
    "$image" -C /data -xf - < "$scratch/data.tar"
docker rm "$container" >/dev/null
docker run --detach --name "$container" \
    --publish 127.0.0.1::8080 \
    --mount "type=volume,source=$restored_volume,target=/data" \
    --env "MAINTENANCE_API_TOKEN=$token" \
    --env "MAINTENANCE_INTEGRATION_TOKEN=$integration_token" \
    "$image" >/dev/null
base_url="http://$(docker port "$container" 8080/tcp)"
wait_for_api
check_process_user
check_credentials
check_item
docker cp "$container:/data/credentials.json" "$scratch/restored-credentials.json"
cmp --silent "$scratch/credentials.json" "$scratch/restored-credentials.json"
request POST /v1/items/example-filter/completions 200 \
    --header "Authorization: Bearer $token" \
    --header 'Content-Type: application/json' \
    --header 'Idempotency-Key: smoke-complete-filter' \
    --header "If-Match: $revision" --data "$body"
check_item
request PUT "/v1/items/example-filter/checklist/$other_step_id" 200 \
    --header "Authorization: Bearer $token" \
    --header 'Content-Type: application/json' \
    --header 'Idempotency-Key: smoke-check-successor' \
    --header "If-Match: $current_step_revision" --data "$current_step_body"
check_item_contract persisted
check_item
request GET /v1/items/example-filter 200 --header "Authorization: Bearer $token"
undo_revision=$(etag)
undo_body=$(jq --compact-output --null-input --arg occurrence "$next_occurrence" '{occurrence_id:$occurrence,completed:false}')
request PUT "/v1/items/example-filter/checklist/$other_step_id" 200 \
    --header "Authorization: Bearer $token" \
    --header 'Content-Type: application/json' \
    --header 'Idempotency-Key: smoke-uncheck-successor' \
    --header "If-Match: $undo_revision" --data "$undo_body"
check_item_contract reset
request GET /v1/items/example-filter/events 200 --header "Authorization: Bearer $token"
check_item_contract history

# Migration preserves unknown facts rather than inventing dates or actors.
# Keep this fixture separate from the preceding backup and checklist contracts.
cat > "$scratch/migration.json" <<'JSON'
{
  "items": [
    {
      "source": {"system":"example-source","collection":"example-list","id":"Once / AbC"},
      "item": {"id":"example-migrated-once","name":"Example once","summary":"Inspect the example item","timezone":"Etc/UTC","due":null,"schedule":{"kind":"once"}},
      "status":"needs_action"
    },
    {
      "source": {"system":"example-source","collection":"example-list","id":"Standing / DeF"},
      "item": {"id":"example-migrated-standing","name":"Example standing","summary":"Inspect whenever needed","timezone":"Etc/UTC","due":null,"schedule":{"kind":"standing"}},
      "status":"needs_action"
    },
    {
      "source": {"system":"example-source","collection":"example-list","id":"Completed / GhI"},
      "item": {"id":"example-migrated-completed","name":"Example completed","summary":"A previously completed example","timezone":"Etc/UTC","due":null,"schedule":{"kind":"once"}},
      "status":"completed",
      "history":[{
        "id":"40000000-0000-4000-8000-000000000001",
        "source":{"system":"example-source","collection":"example-history","id":"Event / JkL"},
        "completed_at":"2030-02-03T04:05:06Z",
        "actor":null
      }]
    }
  ]
}
JSON
request POST /v1/migrations/preview 403 \
    --header "Authorization: Bearer $integration_token" \
    --header 'X-Maintenance-System-Actor: ha:integration' \
    --header 'Content-Type: application/json' \
    --data-binary "@$scratch/migration.json"
request POST /v1/migrations/preview 400 \
    --header "Authorization: Bearer $token" \
    --header 'Content-Type: application/json' \
    --data '{"items":[],"unexpected":true}'
request POST /v1/migrations/preview 200 \
    --header "Authorization: Bearer $token" \
    --header 'Content-Type: application/json' \
    --data-binary "@$scratch/migration.json"
jq --exit-status --slurpfile input "$scratch/migration.json" \
    '.applicable == true and (.items | length) == 3
     and ([.items[].source] == [$input[0].items[].source])
     and ([.items[].target_id] == [$input[0].items[].item.id])
     and all(.items[]; .action == "created" and .conflicts == [])
     and [.items[].history_events] == [0,0,1]' "$scratch/response.json" >/dev/null
cp "$scratch/response.json" "$scratch/migration-preview.json"
# An occupied target is reported without changing it or creating other items.
jq '.items[0].item.id = "example-filter"' "$scratch/migration.json" > "$scratch/migration-conflict.json"
request POST /v1/migrations/preview 200 \
    --header "Authorization: Bearer $token" \
    --header 'Content-Type: application/json' \
    --data-binary "@$scratch/migration-conflict.json"
jq --exit-status '.applicable == false and .items[0].action == "conflict"
    and (.items[0].conflicts | length) > 0' "$scratch/response.json" >/dev/null
for id in example-migrated-once example-migrated-standing example-migrated-completed; do
    request GET "/v1/items/$id" 404 --header "Authorization: Bearer $token"
done
request POST /v1/migrations 201 \
    --header "Authorization: Bearer $token" \
    --header 'Content-Type: application/json' \
    --header 'Idempotency-Key: smoke-migration' \
    --data-binary "@$scratch/migration.json"
cmp --silent "$scratch/migration-preview.json" "$scratch/response.json"
cp "$scratch/response.json" "$scratch/migration-receipt.json"
for kind in once standing completed; do
    request GET "/v1/items/example-migrated-$kind" 200 --header "Authorization: Bearer $token"
    jq --exit-status --arg id "example-migrated-$kind" --slurpfile input "$scratch/migration.json" \
        '. as $item | ($input[0].items[] | select(.item.id == $id)) as $source
         | $item.source == $source.source and $item.status == $source.status
         and $item.schedule == $source.item.schedule and $item.occurrence.due == null' \
        "$scratch/response.json" >/dev/null
done
request GET /v1/items/example-migrated-completed/events 200 --header "Authorization: Bearer $token"
jq --exit-status --slurpfile input "$scratch/migration.json" \
    '.next_page == null and (.items | length) == 1
     and .items[0].id == $input[0].items[2].history[0].id
     and .items[0].source == $input[0].items[2].history[0].source
     and .items[0].completed_at == "2030-02-03T04:05:06Z"
     and .items[0].actor == null and .items[0].occurrence_id == null
     and .items[0].previous_due == null and .items[0].next_due == null' "$scratch/response.json" >/dev/null
cp "$scratch/response.json" "$scratch/migration-history.json"
# Once closes its existing occurrence; standing opens a new undated occurrence.
for kind in once standing; do
    request GET "/v1/items/example-migrated-$kind" 200 --header "Authorization: Bearer $token"
    migration_occurrence=$(jq --exit-status --raw-output '.occurrence.id' "$scratch/response.json")
    migration_revision=$(etag)
    migration_body=$(jq --compact-output --null-input --arg occurrence "$migration_occurrence" '{occurrence_id:$occurrence}')
    request POST "/v1/items/example-migrated-$kind/completions" 200 \
        --header "Authorization: Bearer $token" \
        --header 'Content-Type: application/json' \
        --header "Idempotency-Key: smoke-migration-complete-$kind" \
        --header "If-Match: $migration_revision" --data "$migration_body"
    jq --exit-status --arg kind "$kind" --arg occurrence "$migration_occurrence" \
        '.occurrence.due == null and .last_completion.previous_due == null
         and .last_completion.next_due == null
         and .last_completion.occurrence_id == $occurrence
         and (if $kind == "once" then .status == "completed" and .occurrence.id == $occurrence
              else .status == "needs_action" and .occurrence.id != $occurrence end)' \
        "$scratch/response.json" >/dev/null
done
for kind in once standing completed; do
    request GET "/v1/items/example-migrated-$kind" 200 --header "Authorization: Bearer $token"
    cp "$scratch/response.json" "$scratch/migrated-$kind.json"
done
stop_container
start_container
# Replaying the original import must retain the original receipt without
# resetting completed work or appending a duplicate historical event.
request POST /v1/migrations 201 \
    --header "Authorization: Bearer $token" \
    --header 'Content-Type: application/json' \
    --header 'Idempotency-Key: smoke-migration' \
    --data-binary "@$scratch/migration.json"
cmp --silent "$scratch/migration-receipt.json" "$scratch/response.json"
for kind in once standing completed; do
    request GET "/v1/items/example-migrated-$kind" 200 --header "Authorization: Bearer $token"
    cmp --silent "$scratch/migrated-$kind.json" "$scratch/response.json"
done
request GET /v1/items/example-migrated-completed/events 200 --header "Authorization: Bearer $token"
cmp --silent "$scratch/migration-history.json" "$scratch/response.json"
stop_container

echo "Container smoke passed ($expected_arch): authentication, uid10001, private credentials, checklists/history, durable reorders, source-preserving migration, undated once/standing tasks, restart, and cold backup/restore."
