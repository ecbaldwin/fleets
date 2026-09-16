#!/usr/bin/env bash
# A minimal dynamic inventory script implementing the ansible contract:
#   --list           -> full inventory with _meta.hostvars
#   --host <name>    -> that host's vars (fallback; _meta makes this unnecessary)
set -euo pipefail

case "${1:-}" in
  --list)
    cat <<'JSON'
{
  "_meta": {
    "hostvars": {
      "app01": {"role": "app", "port": 9000},
      "app02": {"role": "app", "port": 9000},
      "cache01": {"role": "cache"}
    }
  },
  "appservers": {
    "hosts": ["app01", "app02"],
    "vars": {"tier": "frontend"}
  },
  "cacheservers": ["cache01"],
  "production": {
    "children": ["appservers", "cacheservers"]
  }
}
JSON
    ;;
  --host)
    # Fallback path; _meta already covers this, but implement for completeness.
    case "${2:-}" in
      app01|app02) echo '{"role": "app", "port": 9000}' ;;
      cache01) echo '{"role": "cache"}' ;;
      *) echo '{}' ;;
    esac
    ;;
  *)
    echo "usage: $0 --list | --host <name>" >&2
    exit 1
    ;;
esac
