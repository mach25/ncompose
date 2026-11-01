#!/bin/sh
# One-time init of a private PostgreSQL cluster for the DANN example
# (equivalent of the postgres image's POSTGRES_DB/USER/PASSWORD env).
set -eu
# Usage: setup-db.sh [DATA_DIR]   (default: .data/pg next to this script)
PGDATA="${1:-$(dirname "$0")/.data/pg}"
mkdir -p "$PGDATA"
PGDATA="$(cd "$PGDATA" && pwd)"
if [ -f "$PGDATA/PG_VERSION" ]; then
  echo "already initialised: $PGDATA"; exit 0
fi
PW=$(mktemp); trap 'rm -f "$PW"' EXIT; echo dann > "$PW"
initdb -D "$PGDATA" -U dann --pwfile="$PW" --auth-local=trust --auth-host=scram-sha-256 >/dev/null
# Services connect from their own loopback IPs (127.10.0.x), like containers do.
echo "host all all 127.0.0.0/8 scram-sha-256" >> "$PGDATA/pg_hba.conf"
# The unix socket lives in the shared filesystem (not isolated), so keep it private
# to avoid clashing with a host PostgreSQL's /tmp/.s.PGSQL.5432.
echo "unix_socket_directories = '$PGDATA'" >> "$PGDATA/postgresql.conf"
echo "CREATE DATABASE dann OWNER dann;" | postgres --single -D "$PGDATA" postgres >/dev/null 2>&1
echo "initialised $PGDATA (user dann / password dann / db dann)"
