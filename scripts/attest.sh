#!/usr/bin/env bash
# attest.sh — collect a developer checkpoint attestation for TRUSTED_CHECKPOINTS.
#
# Fetches the block hash at <height> from every node listed, requires every
# node to agree, checks reorg-safety depth against each node's tip, and
# prints a ready-to-paste Rust entry for lootcoin-node/src/checkpoints.rs.
#
# Usage:
#   scripts/attest.sh <height> [node_url ...]
#
# If node URLs are omitted, NODE_URLS (comma-separated) is used;
# otherwise http://127.0.0.1:3000.
#
# Example:
#   scripts/attest.sh 271000 http://127.0.0.1:3001 http://127.0.0.1:3002

set -euo pipefail

# ── CLI ─────────────────────────────────────────────────────────────────────

HEIGHT="${1:-}"
if [[ -z "$HEIGHT" ]] || ! [[ "$HEIGHT" =~ ^[0-9]+$ ]]; then
  echo "usage: $0 <height> [node_url ...]" >&2
  exit 1
fi
shift || true

if [[ $# -gt 0 ]]; then
  NODES=("$@")
elif [[ -n "${NODE_URLS:-}" ]]; then
  IFS=',' read -r -a NODES <<< "$NODE_URLS"
else
  NODES=("http://127.0.0.1:3000")
fi

# Snapshots are only written at multiples of CHECKPOINT_INTERVAL (1000);
# /snapshot/<height> cannot serve any other height.
if (( HEIGHT % 1000 != 0 )); then
  echo "error: height $HEIGHT is not a multiple of CHECKPOINT_INTERVAL (1000)." >&2
  echo "       Nodes only snapshot state every 1000 blocks, so a checkpoint at" >&2
  echo "       this height exists in no node's database." >&2
  exit 1
fi

# ── helpers ──────────────────────────────────────────────────────────────────

fetch() { # fetch <url> — curl with timeout; hard-fails on HTTP errors
  local url="$1" body
  if ! body="$(curl -fsS -m 10 "$url" 2>/dev/null)"; then
    echo "error: could not reach $url" >&2
    return 1
  fi
  printf '%s' "$body"
}

json_str() { # json_str <json> <key> — extract a string field
  printf '%s' "$1" | grep -o "\"$2\"[[:space:]]*:[[:space:]]*\"[^\"]*\"" \
    | head -n1 | sed 's/.*"\([^"]*\)"$/\1/'
}

json_num() { # json_num <json> <key> — extract an integer field
  printf '%s' "$1" | grep -o "\"$2\"[[:space:]]*:[[:space:]]*[0-9]*" \
    | head -n1 | grep -o '[0-9]*$'
}

# ── collect hashes from every node ──────────────────────────────────────────

ATTESTED_HASH=""
ATTESTED_FROM=""
declare -A TIPS=()

echo "Attesting checkpoint height $HEIGHT across ${#NODES[@]} node(s)"
echo

for node in "${NODES[@]}"; do
  node="${node%/}"

  head_json="$(fetch "$node/chain/head")" || exit 1
  tip="$(json_num "$head_json" height)"
  work="$(json_str "$head_json" chain_work_hex)"

  hash_json="$(fetch "$node/chain/block-hash/$HEIGHT")" || exit 1
  hash="$(json_str "$hash_json" block_hash_hex)"

  if ! [[ "$hash" =~ ^[0-9a-f]{64}$ ]]; then
    echo "error: $node returned a malformed block hash at height $HEIGHT: '$hash'" >&2
    exit 1
  fi
  if [[ -z "$tip" ]]; then
    echo "error: could not parse tip height from $node" >&2
    exit 1
  fi
  if (( tip < HEIGHT )); then
    echo "error: $node is at height $tip — it has not reached $HEIGHT yet" >&2
    exit 1
  fi

  depth=$(( tip - HEIGHT ))
  if (( depth < 100 )); then
    echo "warning: height $HEIGHT is only $depth block(s) behind $node's tip ($tip)." >&2
    echo "         At least 100 blocks of depth (the reorg window) is recommended" >&2
    echo "         before treating a block as irreversible." >&2
  fi

  echo "  $node"
  echo "    tip height:       $tip"
  echo "    chain work:       ${work:-<unparsed>}"
  echo "    hash at $HEIGHT:  $hash"
  echo

  if [[ -z "$ATTESTED_HASH" ]]; then
    ATTESTED_HASH="$hash"
    ATTESTED_FROM="$node"
  elif [[ "$ATTESTED_HASH" != "$hash" ]]; then
    echo "error: nodes disagree on the hash at height $HEIGHT:" >&2
    echo "         $ATTESTED_FROM → $ATTESTED_HASH" >&2
    echo "         $node → $hash" >&2
    echo "       Do not attest a height at which your nodes are on different" >&2
    echo "       chains. Investigate before publishing anything." >&2
    exit 1
  fi
  TIPS["$node"]="$tip"
done

# Nodes on the same chain may legitimately lag one another; report it.
unique_tips="$(printf '%s\n' "${TIPS[@]}" | sort -u | wc -l | tr -d ' ')"
if (( unique_tips > 1 )); then
  echo "note: nodes are at different tip heights (normal if some lag behind)." >&2
  echo "      The hash at $HEIGHT agrees on all of them, which is what matters." >&2
  echo
fi

# ── output ───────────────────────────────────────────────────────────────────

echo "All ${#NODES[@]} node(s) agree. Add this entry to"
echo "lootcoin-node/src/checkpoints.rs, then rebuild and redeploy:"
echo
echo "pub const TRUSTED_CHECKPOINTS: &[(u64, &str)] = &["
printf '    (%s, "%s"),\n' "$HEIGHT" "$ATTESTED_HASH"
echo "];"
echo
echo "After redeploying, verify the node advertises the snapshot:"
echo
for node in "${NODES[@]}"; do
  echo "  curl -s ${node%/}/snapshots"
done
