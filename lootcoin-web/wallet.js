import init, { Wallet } from "./lootcoin-wallet/pkg/lootcoin_wallet.js";
import { addressToWordcode } from "./wordcode.js";
import { attachTooltip } from "./tooltip.js";

// bech32 alphabet used in lootcoin addresses (loot1…)
const BECH32_CHARS = 'qpzry9x8gf2tvdw0s3jn54khce6mua7l';
function isValidAddress(addr) {
  return typeof addr === 'string' &&
    addr.length === 63 &&
    addr.startsWith('loot1') &&
    [...addr.slice(5)].every(c => BECH32_CHARS.includes(c));
}

let wallet = null;

// Pagination state
let txOffset = 0;
const TX_LIMIT = 10;
let txHasMore = false;

// Cached network stats from /chain/head and /mempool/fee-estimate
let networkStats = null;
let feeEstimate = null;

const byId = (id) => document.getElementById(id);
function base() { return window.LOOTCOIN_NODE_URL.replace(/\/+$/, ""); }

function setWalletInfo(w) {
  wallet = w;
  const addr = wallet.address();
  byId("wordcode").textContent = addressToWordcode(addr);
  byId("address").textContent = addr;
  byId("walletEmpty").style.display = "none";
  byId("walletLoaded").style.display = "";
  // Show and expand send and history cards
  byId("sendCard").style.display = "";
  byId("historyCard").style.display = "";
  byId("sendCard").classList.remove("collapsed");
  byId("historyCard").classList.remove("collapsed");
  txOffset = 0;
  queryBalance();
}


/** Format a number with thousands separators. */
function fmt(n) {
  return Number(n).toLocaleString();
}

/** Mirrors the miner's is_eligible formula: eligible_after = 120 / fee blocks. */
const GUARANTEE_AFTER = 120;

function feeWaitBlocks(fee) {
  if (fee <= 0) return Infinity;
  return Math.max(0, Math.floor(GUARANTEE_AFTER / fee) - 1);
}

/** Simple time estimate for the confirm modal. */
function settlementEstimate(fee) {
  if (fee <= 0) return "—";
  const busy = feeEstimate && feeEstimate.utilization >= 1.0;
  const blocks = feeWaitBlocks(fee);
  if (!busy || blocks === 0) return "Next block";
  const blockSecs = (networkStats && networkStats.avg_block_time_secs) || 60;
  const secs = (blocks + 1) * blockSecs;
  if (secs < 90) return `~${Math.round(secs)}s`;
  const mins = secs / 60;
  if (mins < 90) return `~${Math.round(mins)} min`;
  return `~${(mins / 60).toFixed(1).replace(/\.0$/, "")} h`;
}

/** "High", "Medium", or "Low" based on which preset bracket the fee falls into. */
function contributionTier(fee) {
  if (fee >= 120) return "High";
  if (fee >= 12)  return "Medium";
  return "Low";
}

async function fetchNetworkStats() {
  try {
    const [headRes, estimateRes] = await Promise.all([
      fetch(`${base()}/chain/head`),
      fetch(`${base()}/mempool/fee-estimate?target_blocks=0`),
    ]);
    if (headRes.ok) networkStats = await headRes.json();
    if (estimateRes.ok) feeEstimate = await estimateRes.json();
    updateFeeHint();
  } catch {
    // network unavailable — keep stale stats
  }
}

function updateFeeHint() {
  const fee = Number.parseInt(byId("fee").value, 10);
  const hint = byId("feeHint");
  const presets = document.querySelectorAll(".btn-preset");

  // Highlight matching preset
  presets.forEach(btn => {
    btn.classList.toggle("active", Number.parseInt(btn.dataset.fee, 10) === fee);
  });

  if (Number.isNaN(fee) || fee < 0) {
    hint.textContent = "";
    hint.className = "fee-hint";
    return;
  }
  if (fee === 0) {
    hint.textContent = "Zero-fee transactions are never included.";
    hint.className = "fee-hint fee-hint-error";
    return;
  }

  const busy = feeEstimate && feeEstimate.utilization >= 1.0;
  const tier = contributionTier(fee);
  const blocks = feeWaitBlocks(fee);

  // Append market context when the network is busy.
  const recommendNote = busy && feeEstimate
    ? ` Median fee: ${fmt(feeEstimate.median_fee)}. Recommended for next block: ${fmt(feeEstimate.recommended_fee)}.`
    : "";

  if (!busy) {
    hint.textContent = "Network is below capacity — this transaction will be included in the next block.";
    hint.className = "fee-hint fee-hint-good";
  } else if (blocks === 0) {
    hint.textContent = `Network is at capacity — ${tier} contribution, included in the next block.${recommendNote}`;
    hint.className = "fee-hint fee-hint-good";
  } else {
    hint.textContent = `Network is at capacity — ${tier} contribution, included within ${blocks} block${blocks === 1 ? "" : "s"}.${recommendNote}`;
    hint.className = blocks <= 10 ? "fee-hint fee-hint-good" : "fee-hint fee-hint-warn";
  }
}

function renderTxList(txs, myAddr) {
  const list = byId("txList");
  if (txs.length === 0) {
    list.innerHTML = `<p class="empty-state">No transactions on this page.</p>`;
    return;
  }

  list.innerHTML = txs.map(tx => {
    const isCoinbase = !tx.sender;
    const isIn = tx.receiver === myAddr;
    const isOut = tx.sender === myAddr;

    let badgeClass, badgeText, amountClass, amountText, counterpart;

    if (isCoinbase) {
      badgeClass = "tx-badge-coinbase";
      badgeText = "REWARD";
      amountClass = "tx-amount-in";
      amountText = `+${fmt(tx.amount)}`;
      counterpart = "Mining reward";
    } else if (tx.sender === "lottery") {
      badgeClass = "tx-badge-lottery";
      badgeText = "LOTTERY";
      amountClass = "tx-amount-in";
      amountText = `+${fmt(tx.amount)}`;
      counterpart = "Lottery payout";
    } else if (isOut && isIn) {
      badgeClass = "tx-badge-self";
      badgeText = "SELF";
      amountClass = "";
      amountText = fmt(tx.amount);
      counterpart = "To self";
    } else if (isOut) {
      badgeClass = "tx-badge-debit";
      badgeText = "OUT";
      amountClass = "tx-amount-out";
      amountText = `-${fmt(tx.amount)}`;
      counterpart = `To: <span class="wordcode-tip" data-address="${tx.receiver}">${addressToWordcode(tx.receiver)}</span>`;
    } else {
      badgeClass = "tx-badge-in";
      badgeText = "IN";
      amountClass = "tx-amount-in";
      amountText = `+${fmt(tx.amount)}`;
      counterpart = `From: <span class="wordcode-tip" data-address="${tx.sender}">${addressToWordcode(tx.sender)}</span>`;
    }

    const feeLine = (!isCoinbase && isOut && tx.fee > 0)
      ? ` · Fee: ${fmt(tx.fee)}`
      : "";

    return `
      <div class="tx-item">
        <div class="tx-left">
          <span class="tx-badge ${badgeClass}">${badgeText}</span>
          <span class="tx-amount ${amountClass}">${amountText}</span>
        </div>
        <div class="tx-right">
          <div class="tx-counterpart">${counterpart}</div>
          <div class="tx-meta">Block #${tx.block_index}${feeLine}</div>
        </div>
      </div>`;
  }).join("");
}

// ── Address tooltip (history panel) ──────────────────────────────────────────

attachTooltip(byId("txList"), byId("addrTooltip"));

function renderPagination() {
  const pag = byId("txPagination");
  const page = Math.floor(txOffset / TX_LIMIT) + 1;
  byId("txPageInfo").textContent = `Page ${page}`;
  byId("txPrev").disabled = txOffset === 0;
  byId("txNext").disabled = !txHasMore;
  pag.style.display = "flex";
}

async function loadHistory() {
  const addr = byId("address").textContent;
  if (!addr || addr === "(none)") return;

  // Fetch one extra to detect whether a next page exists
  const fetchLimit = TX_LIMIT + 1;
  const res = await fetch(
    `${base()}/address/${addr}/transactions?offset=${txOffset}&limit=${fetchLimit}`
  );
  if (!res.ok) {
    byId("txList").innerHTML = `<p class="empty-state">Failed to load history (${res.status}).</p>`;
    return;
  }
  const data = await res.json();
  txHasMore = data.length > TX_LIMIT;
  const page = txHasMore ? data.slice(0, TX_LIMIT) : data;
  renderTxList(page, addr);
  renderPagination();
}

async function queryBalance() {
  const addr = byId("address").textContent;
  if (!addr || addr === "(none)") return;
  const res = await fetch(`${base()}/balance/${addr}`);
  if (!res.ok) {
    byId("balance").textContent = `error ${res.status}`;
    byId("pendingInfo").style.display = "none";
  } else {
    const data = await res.json();
    byId("balance").textContent = fmt(data.balance);
    // Show spendable balance only when pending transactions reduce it.
    if (data.spendable_balance < data.balance) {
      byId("spendable").textContent = fmt(data.spendable_balance);
      byId("pendingInfo").style.display = "";
    } else {
      byId("pendingInfo").style.display = "none";
    }
  }
  loadHistory();
}

/** Show the confirmation modal and resolve true/false when the user decides. */
function confirmSend(receiver, amount, fee) {
  return new Promise(resolve => {
    byId("confirmWordcode").textContent = addressToWordcode(receiver);
    byId("confirmAddress").textContent  = receiver;
    byId("confirmAmount").textContent   = fmt(amount);
    byId("confirmFee").textContent      = fmt(fee);
    byId("confirmWait").textContent     = settlementEstimate(Number(fee));
    byId("confirmOverlay").style.display = "flex";

    const cleanup = (result) => {
      byId("confirmOverlay").style.display = "none";
      byId("confirmSend").removeEventListener("click", onConfirm);
      byId("confirmCancel").removeEventListener("click", onCancel);
      resolve(result);
    };
    const onConfirm = () => cleanup(true);
    const onCancel  = () => cleanup(false);
    byId("confirmSend").addEventListener("click", onConfirm);
    byId("confirmCancel").addEventListener("click", onCancel);
  });
}

async function submitTransaction() {
  const receiver = byId("receiver").value.trim();

  const amount = BigInt(byId("amount").value);
  const fee = BigInt(byId("fee").value);

  const resultEl = byId("sendResult");
  const setErr = (msg) => { resultEl.textContent = msg; resultEl.className = "send-result result-err"; };

  if (!wallet) { setErr("No wallet loaded."); return; }
  if (!receiver) { setErr("Receiver address required."); return; }
  if (!isValidAddress(receiver)) { setErr("Invalid address — must be a lootcoin address starting with loot1."); return; }
  if (receiver === byId("address").textContent) { setErr("Cannot send to your own address."); return; }

  if (!await confirmSend(receiver, amount, fee)) return;

  const submission = wallet.sign_transaction(receiver, amount, fee);
  const payload = JSON.parse(submission);

  const res = await fetch(`${base()}/transactions`, {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify(payload),
  });

  if (!res.ok) {
    const text = await res.text();
    const el = byId("sendResult");
    el.textContent = `Error ${res.status}: ${text}`;
    el.className = "send-result result-err";
    return;
  }
  const data = await res.json();
  const el = byId("sendResult");
  el.textContent = `Sent ${fmt(data.amount)} to ${addressToWordcode(data.receiver)}`;
  el.className = "send-result result-ok";
  // Refresh balance and history after a successful send
  queryBalance();
}

async function main() {
  await init(); // load wasm

  // Collapsible cards — clicking the header toggles the body.
  // New/Open buttons sit in the wallet card header; stopPropagation
  // prevents their clicks from also toggling the card.
  document.querySelectorAll(".collapsible-card").forEach(card => {
    card.querySelector(".card-header").addEventListener("click", () => {
      card.classList.toggle("collapsed");
    });
  });
  byId("newWallet").addEventListener("click", () => {
    const w = new Wallet();
    const phrase = w.mnemonic_phrase();
    const words = phrase.split(" ");

    // Render numbered word grid
    const grid = byId("mnemonicGrid");
    grid.innerHTML = words.map((word, i) =>
      `<div class="mnemonic-word"><span class="mnemonic-num">${i + 1}</span>${word}</div>`
    ).join("");

    // Reset confirmation state
    byId("mnemonicSaved").checked = false;
    byId("closeNewWallet").disabled = true;
    byId("newWalletOverlay").style.display = "flex";

    const close = () => {
      byId("newWalletOverlay").style.display = "none";
      byId("closeNewWallet").removeEventListener("click", close);
      setWalletInfo(w);
    };
    byId("mnemonicSaved").addEventListener("change", function onCheck() {
      byId("closeNewWallet").disabled = !this.checked;
    });
    byId("closeNewWallet").addEventListener("click", close);
  });

  byId("copyMnemonic").addEventListener("click", async () => {
    const phrase = Array.from(byId("mnemonicGrid").querySelectorAll(".mnemonic-word"))
      .map(el => el.textContent.replace(/^\d+/, "").trim())
      .join(" ");
    try {
      await navigator.clipboard.writeText(phrase);
      const btn = byId("copyMnemonic");
      btn.textContent = "Copied!";
      setTimeout(() => { btn.textContent = "Copy to Clipboard"; }, 1500);
    } catch {}
  });

  byId("openWallet").addEventListener("click", () => {
    byId("openWalletPhrase").value = "";
    byId("openWalletInput").value = "";
    byId("openWalletError").textContent = "";
    byId("hexInputArea").style.display = "none";
    byId("openWalletOverlay").querySelector("label.modal-label").style.display = "";
    byId("openWalletPhrase").style.display = "";
    byId("toggleHexInput").textContent = "Use secret key instead (advanced)";
    byId("openWalletOverlay").style.display = "flex";
    byId("openWalletPhrase").focus();

    const cleanup = () => {
      byId("openWalletOverlay").style.display = "none";
      byId("openWalletConfirm").removeEventListener("click", onConfirm);
      byId("openWalletCancel").removeEventListener("click", onCancel);
    };
    const onConfirm = () => {
      byId("openWalletError").textContent = "";
      const phrase = byId("openWalletPhrase").value.trim();
      const hex   = byId("openWalletInput").value.trim();
      // Prefer phrase if filled; fall back to hex if the hex area is visible
      if (phrase) {
        try {
          setWalletInfo(Wallet.from_mnemonic(phrase));
          cleanup();
        } catch (e) {
          byId("openWalletError").textContent = String(e) || "Invalid recovery phrase.";
        }
      } else if (hex) {
        try {
          setWalletInfo(Wallet.from_secret_key_hex(hex));
          cleanup();
        } catch {
          byId("openWalletError").textContent = "Invalid secret key.";
        }
      } else {
        byId("openWalletError").textContent = "Enter your recovery phrase.";
      }
    };
    const onCancel = () => cleanup();
    byId("openWalletConfirm").addEventListener("click", onConfirm);
    byId("openWalletCancel").addEventListener("click", onCancel);
  });

  byId("toggleHexInput").addEventListener("click", () => {
    const area = byId("hexInputArea");
    const visible = area.style.display !== "none";
    const phraseLabel = byId("openWalletOverlay").querySelector("label.modal-label");
    const phraseTextarea = byId("openWalletPhrase");
    area.style.display = visible ? "none" : "";
    phraseLabel.style.display = visible ? "" : "none";
    phraseTextarea.style.display = visible ? "" : "none";
    byId("toggleHexInput").textContent = visible
      ? "Use secret key instead (advanced)"
      : "Use recovery phrase instead";
    if (!visible) byId("openWalletInput").focus();
    else byId("openWalletPhrase").focus();
  });

  byId("openWalletPhrase").addEventListener("keydown", (e) => {
    if (e.key === "Enter") { e.preventDefault(); byId("openWalletConfirm").click(); }
  });
  byId("openWalletInput").addEventListener("keydown", (e) => {
    if (e.key === "Enter") byId("openWalletConfirm").click();
  });

  byId("copyAddress").addEventListener("click", async () => {
    const addr = byId("address").textContent;
    if (!addr || addr === "(none)") return;
    try {
      await navigator.clipboard.writeText(addr);
      const btn = byId("copyAddress");
      btn.classList.add("copied");
      setTimeout(() => btn.classList.remove("copied"), 1500);
    } catch {}
  });

  byId("closeWallet").addEventListener("click", () => {
    wallet = null;
    txOffset = 0;
    byId("walletLoaded").style.display = "none";
    byId("walletEmpty").style.display = "";
    byId("wordcode").textContent = "(none)";
    byId("address").textContent = "(none)";
    byId("balance").textContent = "—";
    byId("sendCard").style.display = "none";
    byId("historyCard").style.display = "none";
    byId("sendCard").classList.add("collapsed");
    byId("historyCard").classList.add("collapsed");
    byId("txList").innerHTML = '<p class="empty-state">Load a wallet to see transactions.</p>';
    byId("txPagination").style.display = "none";
    byId("sendResult").textContent = "";
    byId("sendResult").className = "send-result";
  });

  byId("getBalance").addEventListener("click", queryBalance);
  byId("sendTx").addEventListener("click", submitTransaction);

  byId("fee").addEventListener("input", updateFeeHint);
  document.querySelectorAll(".btn-preset").forEach(btn => {
    btn.addEventListener("click", () => {
      byId("fee").value = btn.dataset.fee;
      updateFeeHint();
    });
  });

  // Fetch live network stats and refresh balance every 30s
  fetchNetworkStats();
  setInterval(fetchNetworkStats, 30_000);
  setInterval(() => { if (wallet) queryBalance(); }, 30_000);

  updateFeeHint();

  // Console helper for advanced users — not exposed in the UI.
  // Open DevTools and run: lootcoin.secretKey()
  window.lootcoin = {
    secretKey() {
      if (!wallet) { console.warn("No wallet loaded."); return; }
      console.log(wallet.secret_key_hex());
    }
  };

  byId("txPrev").addEventListener("click", () => {
    txOffset = Math.max(0, txOffset - TX_LIMIT);
    loadHistory();
  });

  byId("txNext").addEventListener("click", () => {
    if (txHasMore) {
      txOffset += TX_LIMIT;
      loadHistory();
    }
  });
}

main();
