#!/bin/sh
set -eu

# Supervisor creates /data and options.json as root. Startup hands ownership
# to the fixed application user before any database or credential is opened.
if [ "$(id -u)" -eq 0 ]; then
    chown maintenance:maintenance /data
    if [ -f /data/options.json ]; then
        chown maintenance:maintenance /data/options.json
    fi
    exec gosu maintenance "$0" "$@"
fi

umask 077
if [ -f /data/options.json ]; then
    set -- --options /data/options.json "$@"
fi
if [ "${MAINTENANCE_ENABLE_INGRESS:-false}" = true ]; then
    export MAINTENANCE_INGRESS_BIND=0.0.0.0:8099
    export MAINTENANCE_INGRESS_PEER=172.30.32.2
fi

exec home-maintenance "$@"
