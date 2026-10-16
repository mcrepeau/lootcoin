// HTML escaping for values interpolated into innerHTML templates.
//
// Chain data is attacker-influenced in principle: consensus validates only
// the coinbase receiver's address format, so a crafted block can carry
// arbitrary strings as an ordinary transaction's receiver. Everything that
// arrives from the API and lands in markup goes through escapeHtml().

export function escapeHtml(s) {
  return String(s)
    .replaceAll("&", "&amp;")
    .replaceAll("<", "&lt;")
    .replaceAll(">", "&gt;")
    .replaceAll('"', "&quot;")
    .replaceAll("'", "&#39;");
}
