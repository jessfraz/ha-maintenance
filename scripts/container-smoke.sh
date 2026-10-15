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

check_item() {
    request GET /v1/items/example-filter 200 --header "Authorization: Bearer $token"
    jq --exit-status --arg occurrence "$next_occurrence" \
        '.id == "example-filter" and .name == "Example filter" and
         .occurrence.id == $occurrence and .archived == false and
         .schedule.days == 47 and .timezone == "Etc/UTC"' \
        "$scratch/response.json" >/dev/null
    request GET /v1/items/example-filter/events 200 --header "Authorization: Bearer $token"
    jq --exit-status '(.items | length) == 1' "$scratch/response.json" >/dev/null
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
    --data '{"id":"example-filter","name":"Example filter","timezone":"Etc/UTC","due":"2031-02-17","schedule":{"days":47,"months":0,"years":0}}'
occurrence=$(jq --exit-status --raw-output '.occurrence.id' "$scratch/response.json")
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
# Retry the accepted request with its original revision and occurrence. Durable
# idempotency must return that response without completing the next occurrence.
request POST /v1/items/example-filter/completions 200 \
    --header "Authorization: Bearer $token" \
    --header 'Content-Type: application/json' \
    --header 'Idempotency-Key: smoke-complete-filter' \
    --header "If-Match: $revision" --data "$body"
jq --exit-status --arg id "$next_occurrence" '.occurrence.id == $id' "$scratch/response.json" >/dev/null
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
stop_container

echo "Container smoke passed ($expected_arch): authentication, uid10001, private credentials, atomic completion/retry, restart, and cold backup/restore."
