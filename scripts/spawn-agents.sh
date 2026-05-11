#!/usr/bin/env bash
# spawn-agents.sh — Launch Claude Code agents for each dtmpfs crate via claude-hub.
#
# Usage:
#   ./scripts/spawn-agents.sh [OPTIONS]
#
# Options:
#   --phase 1|2|all   Which agent group to spawn (default: 1)
#                       1 = dtmpfs-proto + dtmpfs-common  (no internal deps)
#                       2 = dtmpfs-meta + dtmpfs-store + dtmpfs-client
#                      all = both phases at once
#   --hub URL          claude-hub base URL (default: http://localhost:8080)
#   --model NAME       Claude model to use (default: sonnet)
#   --dry-run          Print API calls without executing them
#   -h, --help         Show this help

set -euo pipefail

# ── defaults ──────────────────────────────────────────────────────────────────
PHASE="1"
HUB="http://localhost:8080"
MODEL="sonnet"
DRY_RUN=false
WORKSPACE="$(cd "$(dirname "$0")/.." && pwd)"

# ── arg parsing ───────────────────────────────────────────────────────────────
while [[ $# -gt 0 ]]; do
  case "$1" in
    --phase)  PHASE="$2"; shift 2 ;;
    --hub)    HUB="$2";   shift 2 ;;
    --model)  MODEL="$2"; shift 2 ;;
    --dry-run) DRY_RUN=true; shift ;;
    -h|--help)
      sed -n '/^# Usage:/,/^[^#]/{ /^[^#]/d; s/^# \?//p }' "$0"
      exit 0 ;;
    *) echo "Unknown option: $1" >&2; exit 1 ;;
  esac
done

# ── helpers ───────────────────────────────────────────────────────────────────
spawn_session() {
  local name="$1"
  local crate_dir="$2"
  local prompt="$3"

  local payload
  payload=$(printf '{"name":"%s","working_dir":"%s","model":"%s","command":"%s"}' \
    "$name" "$crate_dir" "$MODEL" "$prompt")

  if $DRY_RUN; then
    echo "[DRY-RUN] POST $HUB/api/sessions"
    echo "          name=$name"
    echo "          working_dir=$crate_dir"
    echo "          command=$prompt"
    echo ""
    return
  fi

  local response
  if ! response=$(curl -sf -X POST "$HUB/api/sessions" \
       -H "Content-Type: application/json" \
       -d "$payload" 2>&1); then
    echo "ERROR: failed to create session '$name' (is claude-hub running at $HUB?)" >&2
    echo "       $response" >&2
    return 1
  fi

  echo "  Created: $name"
  echo "    View:  $HUB/#/sessions/$name"
}

# ── crate definitions ─────────────────────────────────────────────────────────
PROTO_DIR="$WORKSPACE/crates/dtmpfs-proto"
COMMON_DIR="$WORKSPACE/crates/dtmpfs-common"
META_DIR="$WORKSPACE/crates/dtmpfs-meta"
STORE_DIR="$WORKSPACE/crates/dtmpfs-store"
CLIENT_DIR="$WORKSPACE/crates/dtmpfs-client"

PROTO_PROMPT="Read CLAUDE.md carefully, then implement every file listed in it. Start with Cargo.toml and build.rs, then the proto files under ../../proto/, then src/lib.rs. When done run: cargo build -p dtmpfs-proto"

COMMON_PROMPT="Read CLAUDE.md carefully, then implement every file listed in it in order: Cargo.toml, src/lib.rs, src/id.rs, src/error.rs, src/config.rs, src/hash.rs. When done run: cargo test -p dtmpfs-common"

META_PROMPT="Read CLAUDE.md carefully. dtmpfs-proto and dtmpfs-common must be built first (run cargo build -p dtmpfs-proto -p dtmpfs-common if needed). Then implement every file listed in CLAUDE.md: Cargo.toml, src/state.rs, src/auth.rs, src/service.rs, src/debug.rs, src/main.rs. When done run: cargo build -p dtmpfs-meta"

STORE_PROMPT="Read CLAUDE.md carefully. dtmpfs-proto and dtmpfs-common must be built first (run cargo build -p dtmpfs-proto -p dtmpfs-common if needed). Then implement every file listed in CLAUDE.md: Cargo.toml, src/state.rs, src/auth.rs, src/heartbeat.rs, src/service.rs, src/debug.rs, src/main.rs. When done run: cargo build -p dtmpfs-store"

CLIENT_PROMPT="Read CLAUDE.md carefully. dtmpfs-proto and dtmpfs-common must be built first (run cargo build -p dtmpfs-proto -p dtmpfs-common if needed). Then implement every file listed in CLAUDE.md: Cargo.toml, src/open_file.rs, src/client.rs, src/cache.rs, src/flush.rs, src/fs.rs, src/main.rs. When done run: cargo build -p dtmpfs-client"

# ── spawn ─────────────────────────────────────────────────────────────────────
echo "dtmpfs agent spawner"
echo "  Workspace : $WORKSPACE"
echo "  Hub       : $HUB"
echo "  Model     : $MODEL"
echo "  Phase     : $PHASE"
if $DRY_RUN; then echo "  Mode      : DRY RUN"; fi
echo ""

spawn_phase1() {
  echo "=== Phase 1: proto + common (no internal deps — safe to run in parallel) ==="
  spawn_session "dtmpfs-proto"  "$PROTO_DIR"  "$PROTO_PROMPT"
  spawn_session "dtmpfs-common" "$COMMON_DIR" "$COMMON_PROMPT"
  echo ""
  echo "Monitor progress at: $HUB"
  echo "When both complete, run:  $0 --phase 2"
  echo ""
}

spawn_phase2() {
  echo "=== Phase 2: meta + store + client (depend on proto + common) ==="
  echo "NOTE: Ensure proto + common are built before agents start compiling."
  echo ""
  spawn_session "dtmpfs-meta"   "$META_DIR"   "$META_PROMPT"
  spawn_session "dtmpfs-store"  "$STORE_DIR"  "$STORE_PROMPT"
  spawn_session "dtmpfs-client" "$CLIENT_DIR" "$CLIENT_PROMPT"
  echo ""
  echo "Monitor progress at: $HUB"
  echo ""
}

case "$PHASE" in
  1)   spawn_phase1 ;;
  2)   spawn_phase2 ;;
  all) spawn_phase1; spawn_phase2 ;;
  *)   echo "ERROR: --phase must be 1, 2, or all" >&2; exit 1 ;;
esac
