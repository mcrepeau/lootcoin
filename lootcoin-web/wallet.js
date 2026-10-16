import init, { Wallet } from "./lootcoin-wallet/pkg/lootcoin_wallet.js";
import { addressToWordcode } from "./wordcode.js";
import { attachTooltip } from "./tooltip.js";
import { nodeFetch, subscribeEvents } from "./node.js";
import { hasVault, readVaultMeta, saveVault, unlockVault, forgetVault } from "./vault.js";

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

// Cached network stats from /chain/head
let networkStats = null;

const byId = (id) => document.getElementById(id);

// ── Device vault UI state ──────────────────────────────────────────────
// Reflects the vault's existence and whether it matches the loaded wallet
// in the three surfaces: the unlock card, the save/forget buttons, and
// the button labels (saving a different wallet replaces the saved one).
function shortAddr(a) {
  return a && a.length > 8 ? a.slice(0, 8) + "…" : a;
}

function updateVaultUI() {
  const meta = readVaultMeta();
  const vaultExists = meta !== null;
  const unlock = byId("unlockSection");
  if (vaultExists && !wallet) {
    byId("unlockSavedAddr").textContent = meta.addr ? shortAddr(meta.addr) : "a wallet";
    unlock.style.display = "";
    byId("walletEmptyDefault").style.display = "none";
  } else {
    unlock.style.display = "none";
    byId("walletEmptyDefault").style.display = "";
  }
  const saveBtn = byId("saveToDevice");
  const forgetBtn = byId("forgetDeviceWallet");
  if (wallet) {
    const mine = vaultExists && meta.addr === wallet.address();
    saveBtn.style.display = mine ? "none" : "";
    saveBtn.textContent = vaultExists && !mine
      ? "Save this wallet on this device (replaces the saved one)"
      : "Save on this device";
    forgetBtn.style.display = vaultExists ? "" : "none";
  } else {
    saveBtn.style.display = "none";
    forgetBtn.style.display = "none";
  }
}

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
  updateVaultUI();
  void queryBalance();
}


/** Format a number with thousands separators. */
function fmt(n) {
  return Number(n).toLocaleString();
}

/** Mirrors the miner's is_eligible formula: eligible_after = 120 / fee blocks. */
const GUARANTEE_AFTER = 120;
const MAX_BLOCK_TXS = 240;

function feeWaitBlocks(fee) {
  if (fee <= 0) return Infinity;
  return Math.max(0, Math.floor(GUARANTEE_AFTER / fee) - 1);
}

/** Simple time estimate for the confirm modal. */
function settlementEstimate(fee) {
  if (fee <= 0) return "—";
  const busy = networkStats && networkStats.mempool_size >= MAX_BLOCK_TXS;
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
    const res = await nodeFetch(`/chain/head`);
    if (res.ok) networkStats = await res.json();
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

  const busy = networkStats && networkStats.mempool_size >= MAX_BLOCK_TXS;
  const tier = contributionTier(fee);
  const blocks = feeWaitBlocks(fee);

  // Append market context when the network is busy.
  const recommendNote = busy && networkStats.mempool_median_fee != null
    ? ` Median fee: ${fmt(networkStats.mempool_median_fee)}.`
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
  try {
    const res = await nodeFetch(
      `/address/${addr}/transactions?offset=${txOffset}&limit=${fetchLimit}`
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
  } catch {
    byId("txList").innerHTML = `<p class="empty-state">Failed to load history (node unreachable).</p>`;
  }
}

async function queryBalance() {
  const addr = byId("address").textContent;
  if (!addr || addr === "(none)") return;
  try {
    const res = await nodeFetch(`/balance/${addr}`);
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
  } catch {
    byId("balance").textContent = "unreachable";
    byId("pendingInfo").style.display = "none";
  }
  void loadHistory();
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

  let res;
  try {
    res = await nodeFetch(`/transactions`, {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify(payload),
    });
  } catch (e) {
    setErr(`Network error: ${e.message || e}`);
    return;
  }

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
  void queryBalance();
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

  // ── Device vault: unlock, save, forget ────────────────────────────
  updateVaultUI();

  async function onUnlockWallet() {
    const pw = byId("unlockPassword").value;
    const err = byId("unlockError");
    err.textContent = "";
    if (!pw) { err.textContent = "Enter your password."; return; }
    byId("unlockWallet").disabled = true;
    try {
      const payload = await unlockVault(pw);
      if (!payload) {
        err.textContent = "Incorrect password — or the saved data is corrupted.";
        return;
      }
      let w;
      if (payload.type === "mnemonic") w = Wallet.from_mnemonic(payload.value);
      else if (payload.type === "hex") w = Wallet.from_secret_key_hex(payload.value);
      if (!w) { err.textContent = "The saved wallet could not be restored."; return; }
      byId("unlockPassword").value = "";
      setWalletInfo(w);
    } finally {
      byId("unlockWallet").disabled = false;
    }
  }
  byId("unlockWallet").addEventListener("click", () => { void onUnlockWallet(); });
  byId("unlockPassword").addEventListener("keydown", (e) => {
    if (e.key === "Enter") byId("unlockWallet").click();
  });

  const forgetSaved = () => {
    if (confirm("Remove the saved wallet from this device? You will need your recovery phrase to open it again.")) {
      forgetVault();
      updateVaultUI();
    }
  };
  byId("forgetSavedWallet").addEventListener("click", forgetSaved);
  byId("forgetDeviceWallet").addEventListener("click", forgetSaved);

  byId("saveToDevice").addEventListener("click", () => {
    byId("saveVaultPass").value = "";
    byId("saveVaultPass2").value = "";
    byId("saveVaultError").textContent = "";
    byId("saveVaultOverlay").style.display = "flex";
    byId("saveVaultPass").focus();
  });
  byId("saveVaultCancel").addEventListener("click", () => {
    byId("saveVaultOverlay").style.display = "none";
  });
  async function onSaveVaultConfirm() {
    const pw = byId("saveVaultPass").value;
    const pw2 = byId("saveVaultPass2").value;
    const err = byId("saveVaultError");
    err.textContent = "";
    if (pw.length < 8) { err.textContent = "Password must be at least 8 characters."; return; }
    if (pw !== pw2) { err.textContent = "Passwords do not match."; return; }
    byId("saveVaultConfirm").disabled = true;
    try {
      const phrase = wallet.mnemonic_phrase();
      const payload = phrase
        ? { type: "mnemonic", value: phrase }
        : { type: "hex", value: wallet.secret_key_hex() };
      await saveVault(payload, wallet.address(), pw);
      byId("saveVaultOverlay").style.display = "none";
      updateVaultUI();
    } catch (e) {
      err.textContent = "Could not save: " + (e && e.message ? e.message : e);
    } finally {
      byId("saveVaultConfirm").disabled = false;
    }
  }
  byId("saveVaultConfirm").addEventListener("click", () => { void onSaveVaultConfirm(); });
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

  async function onCopyMnemonic() {
    const phrase = Array.from(byId("mnemonicGrid").querySelectorAll(".mnemonic-word"))
      .map(el => el.textContent.replace(/^\d+/, "").trim())
      .join(" ");
    try {
      await navigator.clipboard.writeText(phrase);
      const btn = byId("copyMnemonic");
      btn.textContent = "Copied!";
      setTimeout(() => { btn.textContent = "Copy to Clipboard"; }, 1500);
    } catch {}
  }
  byId("copyMnemonic").addEventListener("click", () => { void onCopyMnemonic(); });

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

  async function onCopyAddress() {
    const addr = byId("address").textContent;
    if (!addr || addr === "(none)") return;
    try {
      await navigator.clipboard.writeText(addr);
      const btn = byId("copyAddress");
      btn.classList.add("copied");
      setTimeout(() => btn.classList.remove("copied"), 1500);
    } catch {}
  }
  byId("copyAddress").addEventListener("click", () => { void onCopyAddress(); });

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
    updateVaultUI();
  });

  byId("getBalance").addEventListener("click", () => { void queryBalance(); });
  byId("sendTx").addEventListener("click", () => { void submitTransaction(); });

  byId("fee").addEventListener("input", updateFeeHint);
  document.querySelectorAll(".btn-preset").forEach(btn => {
    btn.addEventListener("click", () => {
      byId("fee").value = btn.dataset.fee;
      updateFeeHint();
    });
  });

  // Fetch live network stats and refresh balance every 30s — the fallback
  // for anything the live event stream misses (or when SSE is blocked).
  void fetchNetworkStats();
  setInterval(() => { void fetchNetworkStats(); }, 30_000);
  setInterval(() => { if (wallet) void queryBalance(); }, 30_000);

  // Live updates: subscribe to the node's event stream and refresh the
  // moment a block lands or one of our own transactions appears in the
  // mempool, instead of waiting for the 30s poll.
  subscribeEvents({
    onBlock: () => {
      void fetchNetworkStats();
      if (wallet) void queryBalance();
    },
    onTransaction: (tx) => {
      const myAddr = byId("address").textContent;
      if (wallet && myAddr !== "(none)" && (tx.sender === myAddr || tx.receiver === myAddr)) {
        void fetchNetworkStats();
        void queryBalance();
      }
    },
  });

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
    void loadHistory();
  });

  byId("txNext").addEventListener("click", () => {
    if (txHasMore) {
      txOffset += TX_LIMIT;
      void loadHistory();
    }
  });
}

void main();
