#!/usr/bin/env bash
# Generates the upgrade-test fixtures in tests/fixtures/upgrade/<version>/.
#
# For every released version it runs that version's container image with an
# empty data directory, completes setup and seeds a rich, deterministic
# dataset through that version's own HTTP UI and REST API (users with roles
# and 2FA, API tokens, monitors of every type the version accepts, groups,
# tags, notification channels with secrets, status pages with sections and
# incidents, maintenance windows, remote agents, remote browsers, settings),
# lets it run for a while against local targets so heartbeats and events
# exist, stops it and saves:
#
#   vigilant-vole.db  the database, vacuumed into a single file
#   manifest.json     credentials, row counts and a few exact values that
#                     tests/upgrade.rs checks after upgrading
#
# Usage:
#   tools/upgrade-fixtures/generate.sh                 # v0.1.0 … v0.4.0
#   tools/upgrade-fixtures/generate.sh v0.5.0          # one (new) release
#
# Needs Docker and network access (to pull the release images). Everything
# else (curl, jq, sqlite3, oathtool, openssl) runs in a helper container, so
# this works the same from Linux, macOS and Git Bash on Windows.
#
# Environment:
#   IMAGE_REPO   image to test (default ghcr.io/ferdinand99/vigilant-vole)
#   OUT_DIR      where fixtures go (default <repo>/tests/fixtures/upgrade)
#   RUN_SECONDS  how long each instance collects heartbeats (default 75)
#
# Internal modes (run inside the helper container):
#   generate.sh --seed BASE_URL SEED_JSON
#   generate.sh --finish DB SEED_JSON OUT_DIR [IMAGE]
set -euo pipefail

# Git Bash would rewrite container paths such as /data into Windows paths.
export MSYS_NO_PATHCONV=1

DEFAULT_VERSIONS=(v0.1.0 v0.2.0 v0.3.0 v0.4.0)
TOOLS_IMAGE=vv-upgrade-fixture-tools
ADMIN_USER='admin'
ADMIN_PASSWORD='Vole-Upgrade-Pass-2026!'
USER_PASSWORD='Vole-Member-Pass-2026!'

log() { printf '[fixtures] %s\n' "$*" >&2; }
die() { log "error: $*"; exit 1; }

# ---------------------------------------------------------------------------
# Host mode
# ---------------------------------------------------------------------------

# Path as the Docker daemon sees it (Windows paths under Git Bash).
host_path() {
    if command -v cygpath >/dev/null 2>&1; then
        cygpath -m "$1"
    else
        printf '%s\n' "$1"
    fi
}

build_tools_image() {
    log "building helper image $TOOLS_IMAGE"
    docker build -q -t "$TOOLS_IMAGE" - >/dev/null <<'DOCKERFILE'
FROM alpine:3.20
RUN apk add --no-cache bash curl jq sqlite openssl oath-toolkit-oathtool coreutils
DOCKERFILE
}

generate_version() {
    local version=$1 repo=$2 out_root=$3 script_dir=$4 run_seconds=$5
    local tag=${version#v}
    local image="$repo:$tag"
    local suffix="$$-${tag//./-}"
    local net="vv-fixtures-$suffix" app="vv-fixtures-app-$suffix"
    local work
    work=$(mktemp -d)

    # shellcheck disable=SC2064 # expand now: the names are fixed per version
    trap "docker rm -fv '$app' >/dev/null 2>&1 || true; docker network rm '$net' >/dev/null 2>&1 || true; rm -rf '$work'" RETURN EXIT

    log "$version: pulling $image"
    docker pull -q "$image" >/dev/null
    docker network create "$net" >/dev/null
    # NET_RAW for ping monitors; no published ports, the seeder joins the network.
    docker run -d --name "$app" --network "$net" --network-alias app \
        --cap-add NET_RAW -e TZ=UTC "$image" >/dev/null

    log "$version: seeding"
    docker run --rm --network "$net" \
        -e RUN_SECONDS="$run_seconds" -e FIXTURE_VERSION="$version" \
        -v "$(host_path "$script_dir"):/tools:ro" \
        -v "$(host_path "$work"):/work" \
        "$TOOLS_IMAGE" bash /tools/generate.sh --seed http://app:3001 /work/seed.json

    log "$version: stopping"
    docker stop -t 30 "$app" >/dev/null
    mkdir -p "$work/data"
    docker cp "$app:/data/." "$(host_path "$work/data")" >/dev/null

    local image_id
    image_id=$(docker image inspect --format '{{index .RepoDigests 0}}' "$image" 2>/dev/null || echo "$image")
    docker run --rm \
        -v "$(host_path "$script_dir"):/tools:ro" \
        -v "$(host_path "$work"):/work" \
        "$TOOLS_IMAGE" bash /tools/generate.sh --finish /work/data/vigilant-vole.db /work/seed.json /work/out "$image_id"

    local dest="$out_root/$version"
    mkdir -p "$dest"
    cp "$work/out/vigilant-vole.db" "$work/out/manifest.json" "$dest/"
    log "$version: wrote $dest ($(wc -c <"$dest/vigilant-vole.db") bytes)"
}

host_main() {
    command -v docker >/dev/null || die "docker is required"
    local script_dir repo_root
    script_dir=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
    repo_root=$(cd "$script_dir/../.." && pwd)
    local repo=${IMAGE_REPO:-ghcr.io/ferdinand99/vigilant-vole}
    local out_root=${OUT_DIR:-$repo_root/tests/fixtures/upgrade}
    local run_seconds=${RUN_SECONDS:-75}
    local versions=("$@")
    if [ ${#versions[@]} -eq 0 ]; then
        versions=("${DEFAULT_VERSIONS[@]}")
    fi
    for v in "${versions[@]}"; do
        [[ $v =~ ^v[0-9]+\.[0-9]+\.[0-9]+$ ]] || die "versions look like v0.4.0 (got '$v')"
    done

    build_tools_image
    for v in "${versions[@]}"; do
        generate_version "$v" "$repo" "$out_root" "$script_dir" "$run_seconds"
    done
    docker image rm "$TOOLS_IMAGE" >/dev/null 2>&1 || true
    log "done: ${versions[*]}"
}

# ---------------------------------------------------------------------------
# Seed mode (inside the helper container)
# ---------------------------------------------------------------------------

BASE=""
TOKEN=""
JAR=/tmp/admin.cookies
RESP=""
CODE=""
SEED='{}'
FAILURES=/tmp/rejected.txt
UNSUPPORTED=/tmp/unsupported.txt

# Remembers a value in the seed record: seed_set '.path' JSON
seed_set() { SEED=$(jq -c --argjson v "$2" "$1 = \$v" <<<"$SEED"); }
seed_str() { seed_set "$1" "$(jq -cn --arg v "$2" '$v')"; }

# REST call with the admin token: call METHOD PATH [JSON]; sets RESP and CODE.
call() {
    local method=$1 path=$2 body=${3-} out
    local args=(-sS -X "$method" -H "Authorization: Bearer $TOKEN" -w '\n%{http_code}')
    if [ -n "$body" ]; then
        args+=(-H 'Content-Type: application/json' --data-binary "$body")
    fi
    out=$(curl "${args[@]}" "$BASE$path")
    CODE=${out##*$'\n'}
    RESP=${out%$'\n'*}
}

# Form post through the UI as the cookie holder: ui JAR PATH FORM-ARGS...
ui() {
    local jar=$1 path=$2
    shift 2
    curl -sS -b "$jar" -c "$jar" -H 'HX-Request: true' "$@" "$BASE$path"
}

# POST that drops fields this version doesn't know and retries. Fields in
# REQUIRES (space separated) must survive, else the item is skipped.
# create PATH JSON [REQUIRES] — prints the response body on success.
create() {
    local path=$1 body=$2 requires=${3-} unknown f r _
    for _ in 1 2 3 4 5 6; do
        call POST "$path" "$body"
        case $CODE in
            200 | 201) printf '%s' "$RESP"; return 0 ;;
            404 | 405)
                log "  skipped $path: this version has no such endpoint"
                printf '%s\n' "$path" >>"$UNSUPPORTED"
                return 1
                ;;
        esac
        if grep -q 'unknown variant' <<<"$RESP"; then
            log "  skipped ($(jq -r '.name // .title // ""' <<<"$body")): this version doesn't know $(jq -r '.monitor_type // .kind // "it"' <<<"$body")"
            jq -r '.monitor_type // .kind // "?"' <<<"$body" >>"$UNSUPPORTED"
            return 1
        fi
        unknown=$(jq -r '.error.fields // {} | to_entries[] | select(.value | test("^Unknown field")) | .key' <<<"$RESP" 2>/dev/null || true)
        if [ -z "$unknown" ]; then
            break
        fi
        for f in $unknown; do
            for r in $requires; do
                if [ "$f" = "$r" ]; then
                    log "  skipped ($(jq -r '.name // .title // ""' <<<"$body")): needs $f, which this version lacks"
                    return 1
                fi
            done
            body=$(jq -c --arg f "$f" 'del(.[$f])' <<<"$body")
        done
    done
    local what
    what=$(jq -r '.name // .title // "item"' <<<"$body")
    log "  rejected $path ($what): $CODE $(jq -c '.error // .' <<<"$RESP" 2>/dev/null || printf '%s' "$RESP")"
    printf '%s %s\n' "$path" "$what" >>"$FAILURES"
    return 1
}

wait_healthy() {
    local i
    for i in $(seq 1 60); do
        if curl -fsS "$BASE/healthz" >/dev/null 2>&1; then
            return 0
        fi
        sleep 1
    done
    die "the app didn't become healthy"
}

setup_admin() {
    local code
    code=$(curl -sS -o /dev/null -w '%{http_code}' -c "$JAR" -H 'HX-Request: true' \
        --data-urlencode "username=$ADMIN_USER" \
        --data-urlencode "password=$ADMIN_PASSWORD" \
        --data-urlencode "confirm=$ADMIN_PASSWORD" \
        --data-urlencode 'site_name=Vole Fixture' \
        --data-urlencode 'timezone=Europe/Oslo' \
        "$BASE/setup")
    [ "$code" = 303 ] || die "setup failed ($code)"
    seed_str .credentials.admin.username "$ADMIN_USER"
    seed_str .credentials.admin.password "$ADMIN_PASSWORD"
}

# Creates an API token through the tokens page; prints the token.
ui_token() {
    local jar=$1 name=$2 scope=$3 page
    page=$(ui "$jar" /settings/tokens --data-urlencode "name=$name" -d "scope=$scope" -d expires=never)
    grep -oE 'vv_[A-Za-z0-9]{20,}' <<<"$page" | sed -n 1p
}

login() {
    local jar=$1 user=$2 password=$3
    rm -f "$jar"
    curl -sS -o /dev/null -c "$jar" -H 'HX-Request: true' \
        --data-urlencode "username=$user" --data-urlencode "password=$password" "$BASE/login"
}

seed_users() {
    log "users"
    local u
    # username|display name|role
    for u in "vera|Vera Viewer|viewer" "eddie|Eddie Editor|editor" "ada|Ada Admin|admin" "dave|Dave Departed|viewer"; do
        IFS='|' read -r name display role <<<"$u"
        ui "$JAR" /settings/users --data-urlencode "username=$name" \
            --data-urlencode "display_name=$display" -d "role=$role" \
            --data-urlencode "password=$USER_PASSWORD" -o /dev/null
    done
    seed_str .credentials.user_password "$USER_PASSWORD"

    # Dave is disabled. Ids follow creation order on a fresh database:
    # admin 1, vera 2, eddie 3, ada 4, dave 5.
    ui "$JAR" /settings/users/5/disable -X POST -o /dev/null

    # Eddie signs in, picks a language and turns on two-factor authentication.
    local ej=/tmp/eddie.cookies page secret code codes
    login "$ej" eddie "$USER_PASSWORD"
    ui "$ej" /settings/security/profile --data-urlencode 'display_name=Eddie Editor' -d language=nb -o /dev/null
    ui "$ej" /settings/security/2fa/setup -X POST -o /dev/null
    page=$(curl -sS -b "$ej" "$BASE/settings/security")
    secret=$(grep -oE 'id="totp-secret">[^<]+' <<<"$page" | sed 's/.*>//; s/ //g' | sed -n 1p)
    [ -n "$secret" ] || die "no TOTP secret on the security page"
    code=$(oathtool --totp -b "$secret")
    page=$(ui "$ej" /settings/security/2fa/enable -d "code=$code")
    codes=$(grep -oE 'id="recovery-codes"[^>]*>[^<]+' <<<"$page" | sed 's/.*>//' | sed -n 1p)
    seed_str .credentials.totp.username eddie
    seed_str .credentials.totp.secret "$secret"
    seed_str .credentials.totp.recovery_code "${codes%% *}"

    # API tokens: a write token for the admin (used for everything below)
    # and a read-only one for Vera.
    TOKEN=$(ui_token "$JAR" "fixture admin" write)
    [ -n "$TOKEN" ] || die "couldn't create an API token"
    seed_str .credentials.admin_token "$TOKEN"
    call GET /api/v1/me
    seed_set .ids.admin "$(jq '.id' <<<"$RESP")"
    local vj=/tmp/vera.cookies vt
    login "$vj" vera "$USER_PASSWORD"
    vt=$(ui_token "$vj" "vera read" read)
    seed_str .credentials.viewer_token "$vt"
}

seed_settings() {
    log "settings"
    call PATCH /api/v1/settings '{"retention_days": 120, "base_url": "http://vole.fixture.test", "metrics_enabled": true, "badges_public": true}'
    [ "$CODE" = 200 ] || log "  settings: $CODE $RESP"
}

seed_tags() {
    log "tags"
    local t
    for t in '{"name":"production","color":"green"}' '{"name":"env","color":"blue"}' '{"name":"database","color":"purple"}' '{"name":"legacy","color":"gray"}'; do
        create /api/v1/tags "$t" >/dev/null || true
    done
}

# Values for fields whose format a provider checks (by "kind.key").
CHANNEL_VALUES='{
  "discord.url": "https://discord.com/api/webhooks/000000/fixture-secret",
  "googlechat.url": "https://chat.googleapis.com/v1/spaces/FIXTURE/messages?key=fixture&token=secret",
  "matrix.room_id": "!fixture:matrix.org",
  "webex.room_id": "ops@example.com",
  "telegram.bot_token": "123456:ABC-fixture-secret",
  "twilio.account_sid": "AC00000000000000000000000000fixture"
}'

# One channel per provider kind; required and secret fields get harmless
# values. None of them is linked to a monitor, so nothing is ever sent.
seed_channels() {
    log "notification channels"
    call GET /api/v1/notification-providers
    local providers=$RESP kind body
    for kind in $(jq -r '.data[].kind' <<<"$providers"); do
        body=$(jq -c --arg kind "$kind" --argjson known "$CHANNEL_VALUES" '
            .data[] | select(.kind == $kind) |
            {
              name: ("Fixture " + .label),
              kind: .kind,
              is_default: false,
              apply_existing: false,
              config: ([.fields[] | select(.required or .secret or ($known[$kind + "." + .key] != null)) |
                {key: .key, value: (
                  if $known[$kind + "." + .key] then $known[$kind + "." + .key]
                  elif .type == "url" then "http://127.0.0.1:9/" + $kind + "/" + .key
                  elif .type == "secret" or .secret then "secret-" + $kind + "-" + .key
                  elif .type == "number" then "1"
                  elif .type == "bool" or .type == "checkbox" then true
                  elif (.key | test("^(from|to|email|recipients?)$")) and ($kind == "email") then "ops@example.com"
                  elif (.key | test("^(from|to|number|recipients|phone)$")) then "+15550000001"
                  elif .key == "host" then "127.0.0.1"
                  elif .key == "port" then "9"
                  elif ((.options // []) | length) > 0 then .options[0].value // .options[0]
                  else "fixture-" + .key end)}] | from_entries)
            }' <<<"$providers")
        create /api/v1/notification-channels "$body" >/dev/null || true
    done
    # The two the monitors use: local, unreachable targets.
    local id
    if id=$(create /api/v1/notification-channels '{"name":"Local webhook","kind":"webhook","config":{"url":"http://127.0.0.1:9/hook"},"is_default":true,"apply_existing":false}'); then
        seed_set .ids.webhook "$(jq '.id' <<<"$id")"
    fi
    if id=$(create /api/v1/notification-channels '{"name":"Escalation webhook","kind":"webhook","config":{"url":"http://127.0.0.1:9/escalate"},"active":false,"is_default":false,"apply_existing":false}'); then
        seed_set .ids.escalation "$(jq '.id' <<<"$id")"
    fi
    seed_str .values.channel_secret "http://127.0.0.1:9/hook"
}

seed_agents() {
    log "agents"
    local page token
    page=$(ui "$JAR" /settings/agents -d name=edge-oslo -d location=Oslo)
    token=$(grep -oE 'vva_[A-Za-z0-9]{20,}' <<<"$page" | sed -n 1p)
    seed_str .credentials.agent_token "$token"
    ui "$JAR" /settings/agents -d name=edge-bergen -d location=Bergen -o /dev/null
}

seed_browsers() {
    log "remote browsers"
    local r
    if r=$(create /api/v1/remote-browsers '{"name":"Local Chrome","url":"ws://127.0.0.1:9222"}' 2>/dev/null); then
        seed_set .ids.browser "$(jq '.id' <<<"$r")"
    fi
}

seed_status_page_shell() {
    log "status pages"
    local r
    r=$(create /api/v1/status-pages '{
        "slug": "fixture", "title": "Fixture status", "description": "Everything the fixture runs",
        "custom_domain": "status.fixture.test", "published": true, "theme": "dark",
        "show_tags": true, "show_uptime": true, "show_response": true, "show_powered_by": false,
        "footer_text": "Run by the fixture team", "custom_css": ".hero { color: #123456; }",
        "refresh_seconds": 60,
        "analytics": {"enabled": true, "unique_visitors": true, "provider": "plausible",
                      "site_id": "status.fixture.test", "script_url": "https://plausible.io/js/script.js"},
        "logo": "data:image/png;base64,iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNk+M9QDwADhgGAWjR9awAAAABJRU5ErkJggg=="}') || die "status page"
    seed_set .ids.page "$(jq '.id' <<<"$r")"
    # Password protection (v0.3.0+; older versions ignore the field).
    r=$(create /api/v1/status-pages '{"slug":"internal","title":"Internal","published":true,"theme":"light","password":"Page-Secret-2026!"}') || true
    seed_str .credentials.page_password 'Page-Secret-2026!'
    seed_set .ids.page_internal "$(jq '.id' <<<"$r")"
    seed_str .values.page_slug fixture
    seed_str .values.custom_domain status.fixture.test
    seed_str .values.custom_css '.hero { color: #123456; }'
}

# The monitors. "parent", "notify", "escalate", "requires" and "browser" are
# instructions for this script; everything else goes to the API.
MONITORS=$(cat <<'JSON'
{"name":"Core services","monitor_type":"group","description":"Local checks"}
{"name":"Website","monitor_type":"http","target":"http://127.0.0.1:3001/healthz","interval_seconds":20,"retry_interval_seconds":20,"timeout_seconds":5,"retries":1,"http_method":"GET","accepted_status_codes":"200-299","max_redirects":5,"http_headers":{"X-Fixture":"1","Authorization":"Bearer header-secret-123"},"description":"The app's own health endpoint","tags":["production",{"name":"env","value":"prod"}],"parent":"Core services","notify":["Local webhook"],"escalate":["Escalation webhook"],"expiry_notify":true,"expiry_days":21,"resend_interval":3,"escalation_minutes":15,"flap_detection":true}
{"name":"Broken page","monitor_type":"http","target":"http://127.0.0.1:3001/definitely-missing","interval_seconds":20,"retries":0,"accepted_status_codes":"200","tags":["production"],"parent":"Core services","notify":["Local webhook"]}
{"name":"Basic auth API","monitor_type":"http","target":"http://127.0.0.1:3001/api/v1/health","interval_seconds":30,"http_method":"POST","http_body":"{\"ping\":true}","http_body_encoding":"json","basic_auth_user":"apiuser","basic_auth_pass":"basic-pass-456","auth_method":"basic","ignore_tls":true,"accepted_status_codes":"200-499"}
{"name":"OAuth API","monitor_type":"http","target":"http://127.0.0.1:9/oauth","interval_seconds":60,"active":false,"auth_method":"oauth2","oauth_token_url":"http://127.0.0.1:9/token","oauth_client_id":"fixture-client","oauth_client_secret":"oauth-secret-789","oauth_scope":"read write","oauth_audience":"vole","oauth_client_auth":"client_secret_post","requires":"auth_method"}
{"name":"mTLS API","monitor_type":"http","target":"https://127.0.0.1:9/mtls","interval_seconds":60,"active":false,"auth_method":"mtls","tls_client_cert":"@CERT@","tls_client_key":"@KEY@","tls_ca_cert":"@CERT@","requires":"auth_method tls_client_cert"}
{"name":"Keyword","monitor_type":"keyword","target":"http://127.0.0.1:3001/healthz","interval_seconds":20,"keyword":"ok","tags":[{"name":"env","value":"staging"}],"parent":"Core services"}
{"name":"Keyword absent","monitor_type":"keyword","target":"http://127.0.0.1:3001/healthz","interval_seconds":30,"keyword":"error","keyword_invert":true}
{"name":"JSON query","monitor_type":"json_query","target":"http://127.0.0.1:3001/api/status-page/fixture","interval_seconds":30,"json_path":"$.title","json_operator":"==","json_expected":"Fixture status"}
{"name":"TCP app port","monitor_type":"tcp","target":"127.0.0.1:3001","interval_seconds":20,"parent":"Core services"}
{"name":"TCP closed port","monitor_type":"tcp","target":"127.0.0.1:9","interval_seconds":20,"retries":0,"upside_down":true}
{"name":"Ping loopback","monitor_type":"ping","target":"127.0.0.1","interval_seconds":20}
{"name":"Tailscale node","monitor_type":"tailscale_ping","target":"fixture-node","interval_seconds":60,"active":false}
{"name":"DNS","monitor_type":"dns","target":"localhost","interval_seconds":60,"dns_record_type":"A","dns_resolver":"127.0.0.11","dns_expected":"127.0.0.1"}
{"name":"Nightly backup","monitor_type":"push","interval_seconds":60,"tags":["database"]}
{"name":"Docker container","monitor_type":"docker","target":"vigilant-vole","interval_seconds":60,"docker_host":"unix:///var/run/docker.sock"}
{"name":"WebSocket","monitor_type":"websocket","target":"ws://127.0.0.1:9/socket","interval_seconds":60}
{"name":"Domain expiry","monitor_type":"domain_expiry","target":"example.com","interval_seconds":86400,"active":false,"expiry_days":30}
{"name":"NTP","monitor_type":"ntp","target":"127.0.0.1:123","interval_seconds":60}
{"name":"Steam server","monitor_type":"steam","target":"127.0.0.1:27015","interval_seconds":60,"active":false}
{"name":"Game server","monitor_type":"game_server","target":"minecraft://127.0.0.1:25565","interval_seconds":60,"active":false}
{"name":"TLS certificate","monitor_type":"tls","target":"127.0.0.1:9","interval_seconds":60,"expiry_notify":true,"expiry_days":10}
{"name":"SMTP","monitor_type":"smtp","target":"127.0.0.1:9","interval_seconds":60}
{"name":"Redis","monitor_type":"redis","target":"redis://127.0.0.1:9/0","interval_seconds":60,"basic_auth_pass":"redis-pass","tags":["database"]}
{"name":"Postgres","monitor_type":"postgres","target":"postgres://127.0.0.1:9/vole","interval_seconds":60,"basic_auth_user":"vole","basic_auth_pass":"pg-pass","tags":["database"]}
{"name":"MySQL","monitor_type":"mysql","target":"mysql://127.0.0.1:9/vole","interval_seconds":60,"basic_auth_user":"vole","basic_auth_pass":"my-pass","tags":["database"]}
{"name":"MQTT","monitor_type":"mqtt","target":"mqtt://127.0.0.1:9","interval_seconds":60,"basic_auth_user":"mqtt-user","basic_auth_pass":"mqtt-pass"}
{"name":"Kafka","monitor_type":"kafka","target":"127.0.0.1:9","interval_seconds":60}
{"name":"RabbitMQ","monitor_type":"rabbitmq","target":"amqp://127.0.0.1:9/","interval_seconds":60,"basic_auth_user":"guest","basic_auth_pass":"rabbit-pass"}
{"name":"gRPC","monitor_type":"grpc","target":"127.0.0.1:9","interval_seconds":60}
{"name":"gRPC keyword","monitor_type":"grpc_keyword","target":"grpc://127.0.0.1:9/grpc.health.v1.Health/Check","interval_seconds":60,"keyword":"SERVING"}
{"name":"RADIUS","monitor_type":"radius","target":"127.0.0.1:1812","interval_seconds":60,"secret":"radius-shared-secret","basic_auth_user":"radius-user","basic_auth_pass":"radius-pass"}
{"name":"SNMP","monitor_type":"snmp","target":"127.0.0.1:161","interval_seconds":60,"secret":"snmp-community"}
{"name":"SQL Server","monitor_type":"sqlserver","target":"sqlserver://127.0.0.1:9/master","interval_seconds":60,"basic_auth_user":"sa","basic_auth_pass":"Sql-pass-1","tags":["database"]}
{"name":"MongoDB","monitor_type":"mongodb","target":"mongodb://127.0.0.1:9/admin","interval_seconds":60,"basic_auth_user":"vole","basic_auth_pass":"mongo-pass","tags":["database"]}
{"name":"Real browser","monitor_type":"real_browser","target":"http://127.0.0.1:3001/login","interval_seconds":120,"active":false,"browser_selector":"form","browser":true}
{"name":"Agent-checked site","monitor_type":"http","target":"http://127.0.0.1:3001/healthz","interval_seconds":30,"run_on_agents":true,"confirm_with_agents":true}
{"name":"Paused legacy","monitor_type":"tcp","target":"127.0.0.1:2222","interval_seconds":120,"active":false,"tags":["legacy"]}
JSON
)

seed_monitors() {
    log "monitors"
    local cert key
    openssl req -x509 -newkey ec -pkeyopt ec_paramgen_curve:P-256 -nodes -days 3650 \
        -subj /CN=vole-fixture -keyout /tmp/key.pem -out /tmp/cert.pem 2>/dev/null
    cert=$(cat /tmp/cert.pem)
    key=$(cat /tmp/key.pem)
    local ids='{}' line spec body requires parent r id
    while IFS= read -r line; do
        [ -n "$line" ] || continue
        spec=$(jq -c --arg cert "$cert" --arg key "$key" \
            'walk(if . == "@CERT@" then $cert elif . == "@KEY@" then $key else . end)' <<<"$line")
        requires=$(jq -r '.requires // ""' <<<"$spec")
        parent=$(jq -r '.parent // ""' <<<"$spec")
        body=$(jq -c --argjson ids "$ids" --argjson ch "$(jq -c '.ids' <<<"$SEED")" '
            (if .parent then .parent_id = $ids[.parent] else . end)
            | (if .browser and $ch.browser then .remote_browser_id = $ch.browser else . end)
            | .notifications = ([(.notify // [])[] | {"Local webhook": $ch.webhook, "Escalation webhook": $ch.escalation}[.] | select(.) | {channel_id: ., escalation: false}]
                               + [(.escalate // [])[] | {"Local webhook": $ch.webhook, "Escalation webhook": $ch.escalation}[.] | select(.) | {channel_id: ., escalation: true}])
            | del(.parent, .notify, .escalate, .requires, .browser)' <<<"$spec")
        if [ -n "$parent" ] && [ "$(jq --arg p "$parent" '.[$p] // empty' <<<"$ids")" = "" ]; then
            body=$(jq -c 'del(.parent_id)' <<<"$body")
        fi
        if r=$(create /api/v1/monitors "$body" "$requires"); then
            id=$(jq '.id' <<<"$r")
            ids=$(jq -c --arg n "$(jq -r '.name' <<<"$spec")" --argjson id "$id" '.[$n] = $id' <<<"$ids")
            if [ "$(jq -r '.monitor_type' <<<"$r")" = push ]; then
                seed_str .credentials.push_token "$(jq -r '.push_token' <<<"$r")"
                seed_set .ids.push_monitor "$id"
            fi
        fi
    done <<<"$MONITORS"
    seed_set .ids.monitors "$ids"
    log "  created $(jq 'length' <<<"$ids") monitors"
}

seed_page_layout() {
    local page ids
    page=$(jq '.ids.page' <<<"$SEED")
    ids=$(jq -c '.ids.monitors' <<<"$SEED")
    call PUT "/api/v1/status-pages/$page/sections" "$(jq -c '[
        {name: "Website", monitor_ids: [.["Website"], .["Keyword"], .["JSON query"]] | map(select(.))},
        {name: "Infrastructure", monitor_ids: [.["Core services"], .["TCP app port"], .["Ping loopback"], .["Nightly backup"]] | map(select(.))},
        {name: "Databases", monitor_ids: [.["Postgres"], .["MySQL"], .["Redis"]] | map(select(.))}
    ]' <<<"$ids")"
    [ "$CODE" = 200 ] || log "  sections: $CODE $RESP"
    local internal
    internal=$(jq '.ids.page_internal' <<<"$SEED")
    call PUT "/api/v1/status-pages/$internal/sections" "$(jq -c '[{name: "", monitor_ids: [.["Broken page"]] | map(select(.))}]' <<<"$ids")"

    log "incidents"
    local r
    r=$(create "/api/v1/status-pages/$page/incidents" '{"title":"Database maintenance overran","body":"Writes were slow for **20 minutes**.","severity":"warning","pinned":true}') || true
    if [ -n "${r:-}" ]; then
        call PATCH "/api/v1/status-pages/$page/incidents/$(jq '.id' <<<"$r")" '{"body":"Resolved: the migration finished.","severity":"success","resolved":true}'
    fi
    create "/api/v1/status-pages/$page/incidents" '{"title":"Elevated error rates","body":"We are looking into it.","severity":"danger","pinned":true}' >/dev/null || true
    seed_str .values.incident_title "Elevated error rates"
}

seed_maintenance() {
    log "maintenance"
    local ids page
    ids=$(jq -c '.ids.monitors' <<<"$SEED")
    page=$(jq '.ids.page' <<<"$SEED")
    create /api/v1/maintenance "$(jq -c --argjson page "$page" '{title: "Manual switch work", description: "Swapping the core switch", strategy: "manual", active: true, monitor_ids: [.["TCP closed port"]] | map(select(.)), status_page_ids: [$page]}' <<<"$ids")" >/dev/null || true
    create /api/v1/maintenance "$(jq -c '{title: "Datacenter move", strategy: "once", start_at: "2031-03-01 22:00:00", end_at: "2031-03-02 04:00:00", timezone: "UTC", monitor_ids: [.["Postgres"], .["MySQL"]] | map(select(.))}' <<<"$ids")" >/dev/null || true
    create /api/v1/maintenance "$(jq -c '{title: "Weekly patching", strategy: "weekly", weekdays: "2,4", time_from: "22:00", time_to: "23:30", timezone: "Europe/Oslo", monitor_ids: [.["Website"]] | map(select(.))}' <<<"$ids")" >/dev/null || true
    create /api/v1/maintenance '{"title":"Nightly backups","strategy":"daily","time_from":"02:00","time_to":"02:30","timezone":"Europe/Oslo","active":false}' >/dev/null || true
}

# Lets the instance run: checks happen on their own; the push monitor and an
# agent report in on the way.
collect_heartbeats() {
    local seconds=${RUN_SECONDS:-75} push agent agent_monitor i status
    push=$(jq -r '.credentials.push_token // ""' <<<"$SEED")
    agent=$(jq -r '.credentials.agent_token // ""' <<<"$SEED")
    agent_monitor=$(jq -r '.ids.monitors["Agent-checked site"] // ""' <<<"$SEED")
    log "collecting heartbeats for ${seconds}s"
    for i in $(seq 1 $((seconds / 5))); do
        if [ -n "$push" ]; then
            status=up
            if [ $((i % 6)) -eq 0 ]; then status=down; fi
            curl -sS -o /dev/null "$BASE/api/push/$push?status=$status&msg=run-$i&ping=$((10 + i))" || true
        fi
        # Page views for the built-in analytics (v0.3.0+).
        curl -sS -o /dev/null -H "Referer: https://news.example/item-$((i % 3))" "$BASE/status/fixture" || true
        if [ -n "$agent" ] && [ -n "$agent_monitor" ] && [ $((i % 2)) -eq 0 ]; then
            curl -sS -o /dev/null -H "Authorization: Bearer $agent" -H 'x-agent-version: fixture' \
                -H 'Content-Type: application/json' \
                --data-binary "[{\"monitor_id\": $agent_monitor, \"status\": \"up\", \"response_time_ms\": $((40 + i)), \"message\": \"200 OK\"}]" \
                "$BASE/api/agent/v1/results" || true
        fi
        sleep 5
    done
}

seed_main() {
    BASE=$1
    local out=$2
    wait_healthy
    setup_admin
    seed_users
    seed_settings
    seed_tags
    seed_channels
    seed_agents
    seed_browsers
    seed_status_page_shell
    seed_monitors
    seed_page_layout
    seed_maintenance
    collect_heartbeats
    call GET /api/v1/health
    seed_str .version "$(jq -r '.version' <<<"$RESP")"
    touch "$FAILURES" "$UNSUPPORTED"
    seed_set .rejected "$(jq -R . <"$FAILURES" | jq -cs .)"
    seed_set .unsupported "$(jq -R . <"$UNSUPPORTED" | jq -cs .)"
    printf '%s\n' "$SEED" >"$out"
    if [ -s "$FAILURES" ]; then
        log "rejected items:"
        sed 's/^/  /' "$FAILURES" >&2
    fi
}

# ---------------------------------------------------------------------------
# Finish mode: compact the database and write the manifest
# ---------------------------------------------------------------------------

finish_main() {
    local db=$1 seed=$2 out=$3 image=${4-}
    mkdir -p "$out"
    rm -f "$out/vigilant-vole.db"
    # Fold the WAL in, then write a compact single-file copy.
    sqlite3 "$db" 'PRAGMA wal_checkpoint(TRUNCATE);' >/dev/null
    sqlite3 "$db" "VACUUM INTO '$out/vigilant-vole.db'"
    local fixture="$out/vigilant-vole.db" counts tables t n migrations values
    counts='{}'
    tables=$(sqlite3 "$fixture" "SELECT name FROM sqlite_master WHERE type = 'table' AND name NOT LIKE 'sqlite_%' ORDER BY name")
    for t in $tables; do
        n=$(sqlite3 "$fixture" "SELECT COUNT(*) FROM \"$t\"")
        counts=$(jq -c --arg t "$t" --argjson n "$n" '.[$t] = $n' <<<"$counts")
    done
    migrations=$(sqlite3 -json "$fixture" 'SELECT name FROM schema_migrations ORDER BY name' | jq -c 'map(.name)')
    values=$(sqlite3 -json "$fixture" "
        SELECT
          (SELECT json_group_array(json_object('username', username, 'role', role, 'active', active, 'totp', totp_enabled)) FROM (SELECT * FROM users ORDER BY id)) AS users,
          (SELECT json_group_array(json_object('name', name, 'type', monitor_type, 'active', active)) FROM (SELECT * FROM monitors ORDER BY id)) AS monitors,
          (SELECT value FROM settings WHERE key = 'site_name') AS site_name
    " | jq -c '.[0] | {users: (.users | fromjson), monitors: (.monitors | fromjson), site_name}')
    jq -n --argjson seed "$(cat "$seed")" --argjson counts "$counts" \
        --argjson migrations "$migrations" --argjson values "$values" \
        --arg image "$image" '
        {
          version: $seed.version,
          image: $image,
          generated_at: (now | todate),
          generator: "tools/upgrade-fixtures/generate.sh",
          migrations: $migrations,
          credentials: $seed.credentials,
          ids: $seed.ids,
          values: ($seed.values + $values),
          counts: $counts,
          not_in_this_version: $seed.unsupported,
          rejected: $seed.rejected
        }' >"$out/manifest.json"
}

case ${1-} in
    --seed) shift; seed_main "$@" ;;
    --finish) shift; finish_main "$@" ;;
    -h | --help) sed -n '2,32p' "$0" ;;
    *) host_main "$@" ;;
esac
