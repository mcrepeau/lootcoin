// Live data for the home page: the pot display and the floating
// latest-payout toast.
//
// Both refresh the moment a block lands (the node's SSE event stream — see
// subscribeEvents in node.js), with 30s polling as the fallback for
// anything missed while the stream is down or blocked.

import { nodeFetch, subscribeEvents } from './node.js';

const REFRESH_MS = 30_000;
const JACKPOT_DIVISOR = 500; // must match lootcoin-core/src/lottery.rs

function fmtTimeAgo(timestamp) {
  const secs = Math.round((Date.now() / 1000) - timestamp);
  if (secs < 90)         return 'just now';
  const mins = secs / 60;
  if (mins < 90)         return `${Math.round(mins)} min ago`;
  const hours = mins / 60;
  if (hours < 36)        return `${Math.round(hours)} h ago`;
  return `${Math.round(hours / 24)} days ago`;
}

async function refreshPot() {
  const valEl      = document.getElementById('livePotValue');
  const jackEl     = document.getElementById('livePotJackpot');
  const lastJackEl = document.getElementById('liveLastJackpot');
  const pulse      = document.getElementById('livePotPulse');
  try {
    const [headResp, jackpotResp] = await Promise.all([
      nodeFetch('/chain/head'),
      nodeFetch('/lottery/recent-payouts?tier=jackpot&limit=1'),
    ]);
    if (!headResp.ok) throw new Error(`HTTP ${headResp.status}`);
    const data = await headResp.json();
    const pot = Number(data.pot);
    valEl.textContent  = pot.toLocaleString();
    jackEl.textContent = `Jackpot up to ${Math.floor(pot / JACKPOT_DIVISOR).toLocaleString()} coins`;
    pulse.style.background = '#4caf50';

    if (jackpotResp.ok) {
      const jackpots = await jackpotResp.json();
      if (jackpots.length > 0) {
        const j = jackpots[0];
        const when = j.block_timestamp ? fmtTimeAgo(j.block_timestamp) : `block #${j.block_index}`;
        lastJackEl.textContent = `Last jackpot: ${Number(j.amount).toLocaleString()} coins · ${when}`;
      } else {
        lastJackEl.textContent = 'Last jackpot: none yet';
      }
    }
  } catch (_) {
    pulse.style.background = '#ef5350';
  }
}

document.getElementById('potRefreshBtn').addEventListener('click', refreshPot);

// ── Floating toast: latest lottery payout ──────────────────────────────

const payoutToast = document.getElementById('payoutToast');
const payoutToastLink = document.getElementById('payoutToastLink');
let lastPayoutKey = '';

function shortAddr(a) {
  return a && a.length > 8 ? a.slice(0, 8) + '…' : a;
}

function hidePayoutToast() {
  payoutToast.classList.add('hidden');
}

async function refreshLatestPayout() {
  try {
    const res = await nodeFetch('/lottery/recent-payouts?limit=1');
    if (!res.ok) return;
    const payouts = await res.json();
    if (!Array.isArray(payouts) || payouts.length === 0) {
      hidePayoutToast();
      lastPayoutKey = '';
      return;
    }
    const p = payouts[0];
    const when = p.block_timestamp ? fmtTimeAgo(p.block_timestamp) : '';
    payoutToastLink.textContent = shortAddr(p.receiver) + ' won '
      + Number(p.amount).toLocaleString() + ' coins'
      + (when ? ' ' + when : '');
    payoutToastLink.href = 'explorer.html#payout=' + p.block_index;
    const key = p.block_index + ':' + p.receiver + ':' + p.amount;
    if (key !== lastPayoutKey) {
      lastPayoutKey = key;
      // reappear (if dismissed) and flash when a new payout lands
      payoutToast.classList.remove('hidden');
      payoutToast.classList.remove('flash');
      void payoutToast.offsetWidth;
      payoutToast.classList.add('flash');
    }
  } catch (_) { /* keep last render */ }
}

document.getElementById('payoutToastClose')
  .addEventListener('click', hidePayoutToast);

// Instant refresh the moment a new block lands; polling below remains as
// the fallback.
subscribeEvents({
  onBlock: () => { refreshPot(); refreshLatestPayout(); },
});

refreshPot();
refreshLatestPayout();
setInterval(() => { refreshPot(); refreshLatestPayout(); }, REFRESH_MS);
