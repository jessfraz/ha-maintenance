set shell := ["bash", "-euo", "pipefail", "-c"]

default: check

public-tree:
    ./scripts/check-public-tree

versions:
    ./scripts/check-release-version

rust:
    cargo fmt --check
    cargo clippy --locked --all --benches --tests --examples --all-features -- -D warnings
    cargo test --locked --all-features

ha:
    cargo build --locked --bin home-maintenance
    uv sync --frozen
    uv run --frozen ruff check custom_components tests_ha scripts/check-public-tree scripts/check-release-version
    uv run --frozen ruff format --check custom_components tests_ha scripts/check-public-tree scripts/check-release-version
    uv run --frozen ty check --error-on-warning custom_components tests_ha
    uv run --frozen ty check --error-on-warning --python-version 3.11 scripts/check-public-tree scripts/check-release-version
    MAINTENANCE_TEST_BINARY="{{justfile_directory()}}/target/debug/home-maintenance" uv run --frozen pytest tests_ha

check: public-tree versions rust ha

test: check

security:
    cargo deny check advisories licenses sources

openapi:
    mkdir -p work
    cargo run --locked -- --openapi work/openapi.json

container:
    docker build --tag ha-maintenance:smoke .
    ./scripts/container-smoke.sh ha-maintenance:smoke "$(docker image inspect ha-maintenance:smoke --format '{{{{.Architecture}}')"
