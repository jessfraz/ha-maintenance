set shell := ["bash", "-euo", "pipefail", "-c"]

default: test

public-tree:
    ./scripts/check-public-tree

test: public-tree
    cd home-maintenance && go test -race ./... && go vet ./...
