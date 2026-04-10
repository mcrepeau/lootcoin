import { addressToWordcode } from "./wordcode.js";
import { attachTooltip } from "./tooltip.js";
import { nodeFetch } from "./node.js";

const BATCH = 15;
const TX_BATCH = 20;

const byId = (id) => document.getElementById(id);

let chainHeight = 0;
let lowestLoadedIndex  = null;
let highestLoadedIndex = null; // track top of loaded range for live prepend
let isLoading = false;
let allLoaded = false;

function fmtTime(ts) {
  return new Date(ts * 1000).toLocaleString();
}

function fmtDuration(secs) {
  if (secs < 90) return `${Math.round(secs)}s`;
  const mins = secs / 60;
  if (mins < 90) return `${mins.toFixed(0)} min`;
  return `${(mins / 60).toFixed(1).replace(/\.0$/, "")} h`;
}

function truncHash(h) {
  if (!h || h.length <= 16) return h;
  return h.slice(0, 8) + "…" + h.slice(-6);
}

function fmt(n) {
  return Number(n).toLocaleString();
}

function fmtHashrate(hps) {
  if (hps >= 1e12) return `${(hps / 1e12).toFixed(2)} TH/s`;
  if (hps >= 1e9)  return `${(hps / 1e9).toFixed(2)} GH/s`;
  if (hps >= 1e6)  return `${(hps / 1e6).toFixed(2)} MH/s`;
  if (hps >= 1e3)  return `${(hps / 1e3).toFixed(2)} kH/s`;
  return `${Math.round(hps)} H/s`;
}

function hex(bytes) {
  return Array.from(bytes, b => b.toString(16).padStart(2, "0")).join("");
}

// ── Chain stats ───────────────────────────────────────────────────────────────

async function loadHead() {
  try {
    const res = await nodeFetch(`/chain/head`);
    if (!res.ok) return;
    const data = await res.json();
    const prevHeight = chainHeight;
    chainHeight = data.height;
    byId("chainHeight").textContent = fmt(data.height);
    byId("chainDiff").textContent = `${data.difficulty.toFixed(2)} bits`;
    byId("chainHash").textContent = data.latest_hash_hex;
    byId("chainMempool").textContent = fmt(data.mempool_size);
    byId("chainBlockTime").textContent = data.avg_block_time_secs != null
      ? fmtDuration(data.avg_block_time_secs)
      : "—";
    byId("chainHashrate").textContent = data.avg_block_time_secs
      ? fmtHashrate(Math.pow(2, data.difficulty) / data.avg_block_time_secs)
      : "—";
    byId("chainPot").textContent = data.pot != null ? fmt(data.pot) : "—";

    // Prepend any new blocks that arrived since the last tick.
    if (highestLoadedIndex !== null && chainHeight > prevHeight) {
      await loadNewBlocks();
    }
  } catch {
    byId("chainHeight").textContent = "unreachable";
  }
}

/** Fetch blocks above highestLoadedIndex and prepend them to the list. */
async function loadNewBlocks() {
  const from  = highestLoadedIndex + 1;
  const limit = chainHeight - from; // chainHeight = index of next block to mine
  if (limit <= 0) return;
  try {
    const res = await nodeFetch(`/blocks?from=${from}&limit=${limit}`);
    if (!res.ok) return;
    const blocks = await res.json();
    if (blocks.length === 0) return;
    const list = getOrCreateList();
    // Prepend oldest-first so that the final prepend (newest block) lands at the top.
    for (let i = 0; i < blocks.length; i++) {
      list.prepend(createBlockEl(blocks[i]));
    }
    highestLoadedIndex = blocks[blocks.length - 1].index;
  } catch { /* ignore — next tick will retry */ }
}

// ── Block rendering ───────────────────────────────────────────────────────────

function renderTxRows(txs) {
  return txs.map(tx => {
    const isCoinbase = !tx.sender;
    let badgeClass, badgeText, amountClass, amountText, counterpart;

    const wc = (address) =>
      `<span class="wordcode-tip" data-address="${address}">${addressToWordcode(address)}</span>`;

    if (isCoinbase) {
      badgeClass = "tx-badge-coinbase";
      badgeText = "MINTED";
      amountClass = "tx-amount-in";
      amountText = `+${fmt(tx.amount)}`;
      counterpart = `To: ${wc(tx.receiver)}`;
    } else if (tx.sender === "lottery") {
      badgeClass = "tx-badge-lottery";
      badgeText = "PAYOUT";
      amountClass = "tx-amount-in";
      amountText = `+${fmt(tx.amount)}`;
      counterpart = `Lottery → ${wc(tx.receiver)}`;
    } else {
      badgeClass = "tx-badge-out";
      badgeText = "TX";
      amountClass = "";
      amountText = fmt(tx.amount);
      counterpart = `${wc(tx.sender)} → ${wc(tx.receiver)}`;
    }

    const feeLine = (!isCoinbase && tx.fee > 0) ? ` · Fee: ${fmt(tx.fee)}` : "";

    return `
      <div class="tx-item">
        <div class="tx-left">
          <span class="tx-badge ${badgeClass}">${badgeText}</span>
          <span class="tx-amount ${amountClass}">${amountText}</span>
        </div>
        <div class="tx-right">
          <div class="tx-counterpart">${counterpart}</div>
          <div class="tx-meta">${feeLine || "&nbsp;"}</div>
        </div>
      </div>`;
  }).join("");
}

function createBlockEl(block) {
  const payouts = (block.lottery_payouts || []).map(p => ({
    sender: "lottery", receiver: p.receiver, amount: p.amount, fee: 0,
  }));
  const allRows = [...block.transactions, ...payouts];

  const payoutLabel = payouts.length > 0
    ? ` · ${payouts.length} lottery payout${payouts.length !== 1 ? "s" : ""}`
    : "";
  const txLabel = `${block.transactions.length} tx${block.transactions.length !== 1 ? "s" : ""}${payoutLabel}`;

  const item = document.createElement("div");
  item.className = "block-item";
  item.innerHTML = `
    <div class="block-header">
      <span class="block-index">#${block.index}</span>
      <div class="block-meta">
        <span class="block-txcount">${txLabel}</span>
        <span class="block-time">${fmtTime(block.timestamp)}</span>
        <span class="block-hash">${truncHash(hex(block.hash))}</span>
      </div>
      <span class="block-chevron">▼</span>
    </div>
    <div class="block-txs tx-list">
      ${renderTxRows(allRows)}
    </div>`;

  const header  = item.querySelector(".block-header");
  const txsEl   = item.querySelector(".block-txs");
  const chevron = item.querySelector(".block-chevron");
  header.addEventListener("click", () => {
    const open = txsEl.classList.toggle("open");
    chevron.classList.toggle("open", open);
  });

  return item;
}

// ── Infinite scroll ───────────────────────────────────────────────────────────

function getOrCreateList() {
  const container = byId("blockList");
  let list = container.querySelector(".block-list");
  if (!list) {
    list = document.createElement("div");
    list.className = "block-list";
    container.replaceChildren(list);
  }
  return list;
}

/** Returns true if the sentinel is currently visible inside the scroll container. */
function sentinelInView() {
  const main     = document.querySelector(".explorer-main");
  const sentinel = byId("scrollSentinel");
  if (!main || !sentinel) return false;
  const mainBottom     = main.scrollTop + main.clientHeight;
  const sentinelOffset = sentinel.offsetTop - main.offsetTop;
  return sentinelOffset <= mainBottom;
}

async function loadMoreBlocks() {
  if (isLoading || allLoaded || chainHeight === 0) return;
  isLoading = true;

  const end   = lowestLoadedIndex ?? chainHeight;
  const from  = Math.max(0, end - BATCH);
  const limit = end - from;

  if (limit <= 0) {
    allLoaded = true;
    isLoading = false;
    showEndMarker();
    return;
  }

  try {
    const res = await nodeFetch(`/blocks?from=${from}&limit=${limit}`);
    if (!res.ok) { isLoading = false; return; }
    const blocks = await res.json();

    blocks.reverse(); // newest first
    const list = getOrCreateList();
    for (const block of blocks) list.appendChild(createBlockEl(block));

    // blocks[0] is the highest-index block after reversing
    if (highestLoadedIndex === null && blocks.length > 0) {
      highestLoadedIndex = blocks[0].index;
    }
    lowestLoadedIndex = from;
    if (from === 0) {
      allLoaded = true;
      showEndMarker();
    }
  } catch {
    isLoading = false;
    return;
  }

  isLoading = false;

  // If the sentinel is still on-screen after this batch, keep loading
  // (handles the case where initial content doesn't fill the viewport).
  if (!allLoaded && sentinelInView()) {
    await loadMoreBlocks();
  }
}

function showEndMarker() {
  const el = byId("scrollSentinel");
  el.textContent = "— genesis —";
  el.classList.add("sentinel-end");
}

// ── Address tab: state ────────────────────────────────────────────────────────

let currentAddress = null;
let txOffset = 0;
let txAllLoaded = false;
let txIsLoading = false;

// ── Address tab: rendering ────────────────────────────────────────────────────

function renderAddrTxRows(txs, address) {
  return txs.map(tx => {
    const isCoinbase = !tx.sender;
    const isLottery = tx.sender === "lottery";
    let badgeClass, badgeText, amountClass, amountText, counterpart;

    const wc = (addr) =>
      `<span class="wordcode-tip" data-address="${addr}">${addressToWordcode(addr)}</span>`;

    if (isCoinbase) {
      badgeClass = "tx-badge-coinbase";
      badgeText = "MINTED";
      amountClass = "tx-amount-in";
      amountText = `+${fmt(tx.amount)}`;
      counterpart = `Block reward → ${wc(tx.receiver)}`;
    } else if (isLottery) {
      badgeClass = "tx-badge-lottery";
      badgeText = "PAYOUT";
      amountClass = "tx-amount-in";
      amountText = `+${fmt(tx.amount)}`;
      counterpart = `Lottery → ${wc(tx.receiver)}`;
    } else if (tx.sender === address && tx.receiver === address) {
      badgeClass = "tx-badge-self";
      badgeText = "SELF";
      amountClass = "";
      amountText = fmt(tx.amount);
      counterpart = `Self transfer`;
    } else if (tx.receiver === address) {
      badgeClass = "tx-badge-in";
      badgeText = "IN";
      amountClass = "tx-amount-in";
      amountText = `+${fmt(tx.amount)}`;
      counterpart = `From: ${wc(tx.sender)}`;
    } else {
      badgeClass = "tx-badge-debit";
      badgeText = "OUT";
      amountClass = "tx-amount-out";
      amountText = `-${fmt(tx.amount)}`;
      counterpart = `To: ${wc(tx.receiver)}`;
    }

    const feePart = tx.fee > 0 ? `Fee: ${fmt(tx.fee)} · ` : "";

    return `
      <div class="tx-item">
        <div class="tx-left">
          <span class="tx-badge ${badgeClass}">${badgeText}</span>
          <span class="tx-amount ${amountClass}">${amountText}</span>
        </div>
        <div class="tx-right">
          <div class="tx-counterpart">${counterpart}</div>
          <div class="tx-meta">${feePart}Block #${tx.block_index}</div>
        </div>
      </div>`;
  }).join("");
}

// ── Address tab: data fetching ────────────────────────────────────────────────

async function searchAddress() {
  const addr = byId("addrInput").value.trim();
  if (!addr) return;

  currentAddress = addr;
  txOffset = 0;
  txAllLoaded = false;
  byId("txList").innerHTML = "";
  byId("txEmpty").hidden = true;
  byId("txLoadMore").hidden = true;
  byId("addrBalanceCard").hidden = true;

  try {
    const res = await nodeFetch(`/balance/${addr}`);
    if (res.ok) {
      const data = await res.json();
      byId("addrWordcode").textContent = addressToWordcode(addr);
      byId("addrBalanceValue").textContent = fmt(data.balance);
      byId("addrBalanceCard").hidden = false;
    }
  } catch { /* ignore */ }

  await loadMoreTxs();
}

async function loadMoreTxs() {
  if (txIsLoading || txAllLoaded || !currentAddress) return;
  txIsLoading = true;
  const btn = byId("txLoadMoreBtn");
  if (btn) btn.disabled = true;

  try {
    const res = await nodeFetch(
      `/address/${currentAddress}/transactions?offset=${txOffset}&limit=${TX_BATCH}`
    );
    if (!res.ok) throw new Error();
    const txs = await res.json();

    if (txs.length === 0 && txOffset === 0) {
      byId("txEmpty").textContent = "No transactions found for this address.";
      byId("txEmpty").hidden = false;
    } else if (txs.length > 0) {
      byId("txList").insertAdjacentHTML("beforeend", renderAddrTxRows(txs, currentAddress));
      txOffset += txs.length;
    }

    if (txs.length < TX_BATCH) {
      txAllLoaded = true;
      byId("txLoadMore").hidden = true;
    } else {
      byId("txLoadMore").hidden = false;
    }
  } catch {
    if (txOffset === 0) {
      byId("txEmpty").textContent = "Could not fetch transactions.";
      byId("txEmpty").hidden = false;
    }
  }

  txIsLoading = false;
  if (btn) btn.disabled = false;
}

// ── Lottery tab ───────────────────────────────────────────────────────────────

let lotteryLoaded = false;

function renderLotteryRows(payouts) {
  return payouts.map(p => {
    const tierClass = `tx-badge-tier-${p.tier}`;
    const wc = `<span class="wordcode-tip" data-address="${p.receiver}">${addressToWordcode(p.receiver)}</span>`;
    const timePart = p.block_timestamp != null ? ` · ${fmtTime(p.block_timestamp)}` : "";
    return `
      <div class="tx-item">
        <div class="tx-left">
          <span class="tx-badge ${tierClass}">${p.tier.toUpperCase()}</span>
          <span class="tx-amount tx-amount-in">+${fmt(p.amount)}</span>
        </div>
        <div class="tx-right">
          <div class="tx-counterpart">Lottery → ${wc}</div>
          <div class="tx-meta">Block #${p.block_index}${timePart}</div>
        </div>
      </div>`;
  }).join("");
}

async function loadLottery(tier) {
  byId("lotteryList").innerHTML = "";
  byId("lotteryEmpty").hidden = true;
  const tierParam = tier ? `&tier=${tier}` : "";
  try {
    const res = await nodeFetch(`/lottery/recent-payouts?limit=100${tierParam}`);
    if (!res.ok) throw new Error();
    const payouts = await res.json();
    if (payouts.length === 0) {
      byId("lotteryEmpty").textContent = "No payouts found.";
      byId("lotteryEmpty").hidden = false;
    } else {
      byId("lotteryList").innerHTML = renderLotteryRows(payouts);
    }
  } catch {
    byId("lotteryEmpty").textContent = "Could not fetch lottery payouts.";
    byId("lotteryEmpty").hidden = false;
  }
  lotteryLoaded = true;
}

// ── Address tooltip ───────────────────────────────────────────────────────────

attachTooltip(byId("blockList"), byId("addrTooltip"));
attachTooltip(byId("txList"), byId("addrTooltip"));
attachTooltip(byId("lotteryList"), byId("addrTooltip"));

// ── Tab switching ─────────────────────────────────────────────────────────────

function showTab(name) {
  for (const t of ["Blocks", "Txs", "Lottery"]) {
    byId(`tab${t}`).classList.toggle("active", t.toLowerCase() === name);
  }
  byId("blocksPanel").hidden  = name !== "blocks";
  byId("txPanel").hidden      = name !== "txs";
  byId("lotteryPanel").hidden = name !== "lottery";
}

byId("tabBlocks").addEventListener("click",  () => showTab("blocks"));
byId("tabTxs").addEventListener("click",     () => showTab("txs"));
byId("tabLottery").addEventListener("click", () => {
  showTab("lottery");
  if (!lotteryLoaded) loadLottery("");
});

// Tier filter pills
for (const btn of document.querySelectorAll(".tier-filter-btn")) {
  btn.addEventListener("click", () => {
    document.querySelectorAll(".tier-filter-btn").forEach(b => b.classList.remove("active"));
    btn.classList.add("active");
    lotteryLoaded = false;
    loadLottery(btn.dataset.tier);
  });
}

// ── Scroll listener ───────────────────────────────────────────────────────────

document.querySelector(".explorer-main").addEventListener("scroll", () => {
  if (!allLoaded && !isLoading && sentinelInView()) loadMoreBlocks();
});

// ── Full reset ────────────────────────────────────────────────────────────────

async function reset() {
  lowestLoadedIndex  = null;
  highestLoadedIndex = null;
  allLoaded = false;
  isLoading = false;
  const sentinel = byId("scrollSentinel");
  sentinel.textContent = "";
  sentinel.classList.remove("sentinel-end");
  byId("blockList").innerHTML = '<p class="empty-state">Loading…</p>';
  await loadHead();
  await loadMoreBlocks();
}

// ── Event wiring ──────────────────────────────────────────────────────────────

byId("refreshBtn").addEventListener("click", reset);
byId("addrSearchBtn").addEventListener("click", searchAddress);
byId("addrInput").addEventListener("keydown", (e) => { if (e.key === "Enter") searchAddress(); });
byId("txLoadMoreBtn").addEventListener("click", loadMoreTxs);

// Auto-refresh stats only — don't disrupt scroll position
setInterval(loadHead, 15_000);

// Initial load
reset();
