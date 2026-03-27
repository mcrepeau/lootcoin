/**
 * Attaches wordcode address tooltip behaviour to a container element.
 * Any `.wordcode-tip[data-address]` descendants will show the full address
 * on hover and copy it to the clipboard on click.
 *
 * @param {HTMLElement} container  - Element that contains `.wordcode-tip` spans.
 * @param {HTMLElement} tooltipEl  - The shared tooltip DOM node.
 */
export function attachTooltip(container, tooltipEl) {
  container.addEventListener("mouseover", (e) => {
    const target = e.target.closest(".wordcode-tip");
    if (!target) return;
    tooltipEl.textContent = target.dataset.address;
    tooltipEl.style.opacity = "1";
  });

  container.addEventListener("mousemove", (e) => {
    const target = e.target.closest(".wordcode-tip");
    if (!target) { tooltipEl.style.opacity = "0"; return; }
    const gap = 10;
    const tw = tooltipEl.offsetWidth;
    const th = tooltipEl.offsetHeight;
    let x = e.clientX + gap;
    if (x + tw > window.innerWidth - 8) x = e.clientX - tw - gap;
    let y = e.clientY + gap;
    if (y + th > window.innerHeight - 8) y = e.clientY - th - gap;
    tooltipEl.style.left = `${x}px`;
    tooltipEl.style.top  = `${y}px`;
  });

  container.addEventListener("mouseout", (e) => {
    if (!e.target.closest(".wordcode-tip")) return;
    tooltipEl.style.opacity = "0";
  });

  container.addEventListener("click", (e) => {
    const target = e.target.closest(".wordcode-tip");
    if (!target) return;
    navigator.clipboard.writeText(target.dataset.address).then(() => {
      const prev = tooltipEl.textContent;
      tooltipEl.textContent = "copied!";
      setTimeout(() => { tooltipEl.textContent = prev; }, 1000);
    });
  });
}
