// Device vault — encrypted wallet persistence in localStorage.
//
// The wallet's secret material (recovery phrase or secret key) is encrypted
// with a user-chosen password using WebCrypto: PBKDF2-SHA256 derives an
// AES-GCM key, and only the ciphertext is written to localStorage. Nothing
// readable ever touches storage, and a wrong password simply fails to
// decrypt (AES-GCM authentication) — there is no stored password to compare.
//
// The record also stores the wallet's public address in the clear so the UI
// can label the saved wallet; addresses are public on-chain anyway.
//
// This is a convenience, never a backup: clearing browser data removes the
// vault entirely. The 12-word recovery phrase remains the canonical backup.

const VAULT_KEY = "lootcoin-wallet-v1";
// OWASP-recommended iteration count for PBKDF2-HMAC-SHA256 (2023 guidance).
const PBKDF2_ITERATIONS = 600_000;
const crypto = globalThis.crypto;
const encoder = new TextEncoder();
const decoder = new TextDecoder();

function bytesToB64(bytes) {
  let s = "";
  for (const b of bytes) s += String.fromCharCode(b);
  return btoa(s);
}

function b64ToBytes(b64) {
  const bin = atob(b64);
  const bytes = new Uint8Array(bin.length);
  for (let i = 0; i < bin.length; i++) bytes[i] = bin.charCodeAt(i);
  return bytes;
}

async function deriveKey(password, salt) {
  const base = await crypto.subtle.importKey(
    "raw", encoder.encode(password), "PBKDF2", false, ["deriveKey"]
  );
  return crypto.subtle.deriveKey(
    { name: "PBKDF2", salt, iterations: PBKDF2_ITERATIONS, hash: "SHA-256" },
    base,
    { name: "AES-GCM", length: 256 },
    false,
    ["encrypt", "decrypt"]
  );
}

/** True if a wallet vault exists in localStorage. */
export function hasVault() {
  try {
    return localStorage.getItem(VAULT_KEY) !== null;
  } catch {
    return false;
  }
}

/** Public metadata of the saved vault (its address), or null if none. */
export function readVaultMeta() {
  try {
    const rec = JSON.parse(localStorage.getItem(VAULT_KEY));
    return rec ? { addr: rec.addr ?? null } : null;
  } catch {
    return null;
  }
}

/**
 * Encrypt and store the wallet payload under a password.
 * payload: { type: "mnemonic" | "hex", value: string }
 * addr: the wallet's public address, stored in the clear for UI labelling.
 */
export async function saveVault(payload, addr, password) {
  const salt = crypto.getRandomValues(new Uint8Array(16));
  const iv = crypto.getRandomValues(new Uint8Array(12));
  const key = await deriveKey(password, salt);
  const ct = new Uint8Array(await crypto.subtle.encrypt(
    { name: "AES-GCM", iv },
    key,
    encoder.encode(JSON.stringify(payload))
  ));
  localStorage.setItem(VAULT_KEY, JSON.stringify({
    v: 1,
    kdf: "PBKDF2-SHA256",
    iter: PBKDF2_ITERATIONS,
    salt: bytesToB64(salt),
    iv: bytesToB64(iv),
    ct: bytesToB64(ct),
    addr,
  }));
}

/**
 * Decrypt and return the stored payload, or null if no vault exists,
 * the password is wrong, or the stored data is corrupted.
 */
export async function unlockVault(password) {
  let rec;
  try {
    rec = JSON.parse(localStorage.getItem(VAULT_KEY));
  } catch {
    return null;
  }
  if (!rec || rec.v !== 1) return null;
  try {
    const key = await deriveKey(password, b64ToBytes(rec.salt));
    const plain = await crypto.subtle.decrypt(
      { name: "AES-GCM", iv: b64ToBytes(rec.iv) },
      key,
      b64ToBytes(rec.ct)
    );
    return JSON.parse(decoder.decode(plain));
  } catch {
    return null; // wrong password or tampered ciphertext
  }
}

/** Remove the saved wallet from this device. */
export function forgetVault() {
  try {
    localStorage.removeItem(VAULT_KEY);
  } catch { /* nothing to remove */ }
}
