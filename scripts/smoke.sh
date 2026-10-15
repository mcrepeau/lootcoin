#!/usr/bin/env bash
# smoke.sh — end-to-end smoke test against a running Lootcoin stack.
#
# Exercises the wiring between the node, faucet, miner, and the wallet CLI:
#   1. node and faucet reachable
#   2. fresh wallet created with `lc`
#   3. faucet dispense, then wait for a mined block to confirm it
#   4. signed send to a second wallet, then wait for confirmation
#   5. balances and chain height verified
#
# Fresh wallets are created in a temp directory (removed on exit), so the
# faucet's per-address cooldown never interferes with repeated runs.
#
# Usage:
#   scripts/smoke.sh
#
# Environment:
#   SMOKE_NODE_URL     node API base URL (default http://127.0.0.1:3001)
#   SMOKE_FAUCET_URL   faucet base URL   (default http://127.0.0.1:3030)
#   SMOKE_LC           path to the `lc` binary (default: build it with cargo)
#   SMOKE_TIMEOUT      seconds to wait per confirmation (default 360 — a few
#                      blocks at the 60 s target)
#   SMOKE_ALLOW_REMOTE set to 1 to permit non-localhost targets. The test
#                      creates and spends coins; don't point it at someone
#                      else's chain.
#
# Example (docker-compose stack from the quickstart):
#   docker compose up -d
#   scripts/smoke.sh

set -euo pipefail

NODE="${SMOKE_NODE_URL:-http://127.0.0.1:3001}"
FAUCET="${SMOKE_FAUCET_URL:-http://127.0.0.1:3030}"
CONFIRM_TIMEOUT="${SMOKE_TIMEOUT:-360}"
SEND_AMOUNT=50
SEND_FEE=12

# ── helpers ──────────────────────────────────────────────────────────────────

die() { echo "SMOKE FAIL: $*" >&2; exit 1; }
ok()  { echo "  ok: $*"; }

json_num() { # json_num <json> <key> — extract an integer field
  printf '%s' "$1" | grep -o "\"$2\"[[:space:]]*:[[:space:]]*[0-9]*" | head -n1 | grep -o '[0-9]*$'
}

json_hex() { # json_hex <json> <key> — extract a hex string field
  printf '%s' "$1" | grep -o "\"$2\"[[:space:]]*:[[:space:]]*\"[0-9a-f]*\"" | head -n1 | sed 's/.*"\([0-9a-f]*\)"$/\1/'
}

case "$NODE" in
  http://127.0.0.1*|http://localhost*|http://[::1]*) : ;;
  *) [ "${SMOKE_ALLOW_REMOTE:-0}" = "1" ] || die "refusing to run against '$NODE' — set SMOKE_ALLOW_REMOTE=1 to override" ;;
esac
case "$FAUCET" in
  http://127.0.0.1*|http://localhost*|http://[::1]*) : ;;
  *) [ "${SMOKE_ALLOW_REMOTE:-0}" = "1" ] || die "refusing to run against '$FAUCET' — set SMOKE_ALLOW_REMOTE=1 to override" ;;
esac

# ── locate or build the `lc` binary ───────────────────────────────────────────

if [ -n "${SMOKE_LC:-}" ]; then
  LC_BIN="$SMOKE_LC"
elif command -v lc >/dev/null 2>&1; then
  LC_BIN="$(command -v lc)"
elif [ -x target/release/lc ]; then
  LC_BIN=target/release/lc
elif [ -x target/release/lc.exe ]; then
  LC_BIN=target/release/lc.exe
else
  echo "  ..: building \`lc\` (cargo build --release -p lootcoin-wallet --bin lc)" >&2
  cargo build --release -p lootcoin-wallet --bin lc >&2 || die "could not build the lc binary"
  if [ -x target/release/lc.exe ]; then LC_BIN=target/release/lc.exe; else LC_BIN=target/release/lc; fi
fi

# ── 1. stack reachable ───────────────────────────────────────────────────────

echo "1. stack reachable"

HEAD="$(curl -fsS -m 10 "$NODE/chain/head" 2>/dev/null)" || die "node unreachable at $NODE — is the stack up? (docker compose up -d)"
HEIGHT0="$(json_num "$HEAD" height)"
POT="$(json_num "$HEAD" pot)"
[ -n "$HEIGHT0" ] || die "could not parse chain height from: $HEAD"
ok "node: height $HEIGHT0, pot $POT"

STATUS="$(curl -fsS -m 10 "$FAUCET/status" 2>/dev/null)" || die "faucet unreachable at $FAUCET"
DISPENSE_AMOUNT="$(json_num "$STATUS" dispense_amount)"
[ -n "$DISPENSE_AMOUNT" ] || die "could not parse dispense_amount from: $STATUS"
ok "faucet: dispenses $DISPENSE_AMOUNT coins"

# ── 2. fresh wallets ─────────────────────────────────────────────────────────

echo "2. wallets"
WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT

"$LC_BIN" new --wallet "$WORK/w1.json" >/dev/null || die "lc new failed for wallet 1"
ADDR1="$("$LC_BIN" address --wallet "$WORK/w1.json")"
"$LC_BIN" new --wallet "$WORK/w2.json" >/dev/null || die "lc new failed for wallet 2"
ADDR2="$("$LC_BIN" address --wallet "$WORK/w2.json")"
case "$ADDR1" in loot1*) : ;; *) die "lc produced an unexpected address: '$ADDR1'" ;; esac
case "$ADDR2" in loot1*) : ;; *) die "lc produced an unexpected address: '$ADDR2'" ;; esac
[ "${#ADDR1}" -eq 63 ] && [ "${#ADDR2}" -eq 63 ] || die "unexpected address lengths: '${#ADDR1}' / '${#ADDR2}'"
ok "w1 ${ADDR1:0:12}…, w2 ${ADDR2:0:12}…"

# ── 3. faucet dispense + confirmation ─────────────────────────────────────────

echo "3. faucet dispense to w1"
RESP="$(curl -sS -m 10 -X POST -H 'Content-Type: application/json' \
  -d "{\"address\":\"$ADDR1\"}" "$FAUCET/faucet" 2>/dev/null)" || die "faucet request failed"
echo "$RESP" | grep -q '"txid"' || die "faucet rejected the dispense: $RESP"
AMT="$(json_num "$RESP" amount)"
[ -n "$AMT" ] && [ "$AMT" -ge 1 ] || die "faucet returned no amount: $RESP"
ok "dispensed $AMT coins (txid $(json_hex "$RESP" txid | cut -c1-12)…)"

echo "  ..: waiting up to ${CONFIRM_TIMEOUT}s for a block to confirm the dispense"
W1_BAL=""
deadline=$((SECONDS + CONFIRM_TIMEOUT))
while [ $SECONDS -lt $deadline ]; do
  BODY="$(curl -fsS -m 10 "$NODE/balance/$ADDR1" 2>/dev/null)" || { sleep 5; continue; }
  BAL="$(json_num "$BODY" balance)"
  if [ -n "$BAL" ] && [ "$BAL" -ge "$AMT" ]; then W1_BAL="$BAL"; break; fi
  sleep 5
done
[ -n "$W1_BAL" ] || die "w1 balance did not confirm within ${CONFIRM_TIMEOUT}s — is the miner running?"
ok "w1 confirmed balance: $W1_BAL"

# ── 4. signed send + confirmation ────────────────────────────────────────────

echo "4. send $SEND_AMOUNT coins w1 → w2 (fee $SEND_FEE)"
"$LC_BIN" send "$ADDR2" "$SEND_AMOUNT" --fee "$SEND_FEE" --yes \
  --wallet "$WORK/w1.json" --node "$NODE" >/dev/null || die "lc send failed"

echo "  ..: waiting up to ${CONFIRM_TIMEOUT}s for a block to confirm the send"
W2_BAL=""
deadline=$((SECONDS + CONFIRM_TIMEOUT))
while [ $SECONDS -lt $deadline ]; do
  BODY="$(curl -fsS -m 10 "$NODE/balance/$ADDR2" 2>/dev/null)" || { sleep 5; continue; }
  BAL="$(json_num "$BODY" balance)"
  if [ -n "$BAL" ] && [ "$BAL" -ge "$SEND_AMOUNT" ]; then W2_BAL="$BAL"; break; fi
  sleep 5
done
[ -n "$W2_BAL" ] || die "w2 never received the coins — send not confirmed within ${CONFIRM_TIMEOUT}s"

# ── 5. verify ────────────────────────────────────────────────────────────────

echo "5. verify"
# w1 spent exactly amount + fee; nothing else touches a fresh address.
BODY="$(curl -fsS -m 10 "$NODE/balance/$ADDR1" 2>/dev/null)" || die "balance check failed"
W1_AFTER="$(json_num "$BODY" balance)"
EXPECTED_AFTER=$((AMT - SEND_AMOUNT - SEND_FEE))
[ "$W1_AFTER" = "$EXPECTED_AFTER" ] || die "w1 balance is $W1_AFTER, expected $EXPECTED_AFTER ($AMT − $SEND_AMOUNT − $SEND_FEE)"
ok "w1 balance: $W1_AFTER (dispensed − sent − fee)"

HEAD1="$(curl -fsS -m 10 "$NODE/chain/head" 2>/dev/null)" || die "chain head unreachable"
HEIGHT1="$(json_num "$HEAD1" height)"
[ "$HEIGHT1" -gt "$HEIGHT0" ] || die "chain height did not advance ($HEIGHT0 → $HEIGHT1) — is the miner running?"
ok "chain advanced: $HEIGHT0 → $HEIGHT1"

echo
echo "SMOKE PASS: faucet → mine → send → mine, balances exact, chain advanced ($HEIGHT0 → $HEIGHT1)"
