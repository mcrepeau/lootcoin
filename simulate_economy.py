#!/usr/bin/env python3
"""
Lootcoin economic simulator.

Models pot balance, coin supply, inflation, and miner earnings under
different transaction-volume scenarios.

Incentive structure:
  - Users pay the minimum fee (2 coins) when blocks are not full.
  - Fees rise only when demand exceeds block capacity (fee market).
  - 50% of each block's fees goes directly to the miner (floor division).
  - The remainder accumulates in the lottery pot.
  - Every block with ≥1 non-coinbase tx earns a deferred lottery ticket.
  - Tickets settle at block height + REVEAL_BLOCKS (= +100).
  - Single draw at settlement; prize = pot / DIVISOR (flat, tier-dependent).
  - Coinbase (1 coin/block) is new supply.

Accounting invariant:
  - total_supply = circulating + pot  (always)
  - Fees are paid from circulating coins; they cannot exceed the circulating supply.
  - Coinbase adds 1 coin/block to circulating (and to total_supply).
  - Lottery payouts move coins from pot to circulating.
  - The pot can therefore never exceed total_supply.

Usage:
    python simulate_economy.py                    # all scenarios, interactive plot
    python simulate_economy.py --scenario thriving
    python simulate_economy.py --blocks 200000
    python simulate_economy.py --no-plot
    python simulate_economy.py --save-dir docs/charts   # save PNGs for embedding
"""

import math
import random
import argparse
import sys
import os
from dataclasses import dataclass
from typing import Callable

try:
    import matplotlib
    import matplotlib.pyplot as plt
    import matplotlib.gridspec as gridspec
    HAS_MATPLOTLIB = True
except ImportError:
    HAS_MATPLOTLIB = False


# ── Chain constants (must match lootcoin-core/src/lottery.rs) ─────────────────

REVEAL_BLOCKS    = 100   # tickets settle at created_height + REVEAL_BLOCKS
SETTLE_DELAY     = REVEAL_BLOCKS

MAX_BLOCK_TXS    = 240    # hard cap on non-coinbase transactions per block
COINBASE_REWARD  = 1      # new coins minted per block
MIN_FEE          = 2      # minimum fee; ensures both miner and pot each get ≥1 coin per tx
GUARANTEE_AFTER  = 120    # fee required to guarantee next-block inclusion

GENESIS_POT      = 99_000_000
GENESIS_SUPPLY   = 100_000_000   # 1M genesis wallet + 99M seeded pot

# Fee split: fees split 50/50 between miner and pot using floor division.
# MIN_FEE=2 guarantees both sides always receive ≥1 coin; odd remainders go to pot.
MINER_FEE_SHARE  = 0.50

# Lottery prize divisors: prize = pot / DIVISOR  (flat, no tx_count scaling)
#
# Pot equilibrium: pot_eq = (1 - MINER_FEE_SHARE) × MIN_FEE × avg_txs_per_block
#                           / Σ(prob_i / divisor_i)
# With MINER_FEE_SHARE=0.50, MIN_FEE=2, avg_txs≈240 (full blocks), Σ≈2.28e-6:
#   pot_eq ≈ 0.50 × 2 × 240 / 2.28e-6 ≈ 105M  (full blocks)
#   pot_eq ≈ 0.50 × 2 × 120 / 2.28e-6 ≈  52M  (half-full blocks)
SMALL_DIVISOR    = 400_000
MEDIUM_DIVISOR   =  30_000
LARGE_DIVISOR    =   2_000
JACKPOT_DIVISOR  =     500


# ── Lottery ───────────────────────────────────────────────────────────────────

@dataclass
class Tier:
    name:        str
    probability: float   # fraction in [0, 1]
    divisor:     int     # 0 for the no-win tier


LOTTERY_TIERS: list[Tier] = [
    Tier("none",    0.6200, 0),
    Tier("small",   0.3625, SMALL_DIVISOR),
    Tier("medium",  0.0167, MEDIUM_DIVISOR),
    Tier("large",   0.0007, LARGE_DIVISOR),
    Tier("jackpot", 0.0001, JACKPOT_DIVISOR),
]

WIN_PROBABILITY = sum(t.probability for t in LOTTERY_TIERS if t.divisor != 0)


def lottery_draw(rng: random.Random, pot: int) -> tuple[str, int]:
    """Return (tier_name, amount). 'none' returns amount=0."""
    roll = rng.random()
    cumulative = 0.0
    for tier in LOTTERY_TIERS:
        cumulative += tier.probability
        if roll < cumulative:
            if tier.divisor == 0:
                return tier.name, 0
            return tier.name, pot // tier.divisor
    # Floating-point safety: return last tier.
    t = LOTTERY_TIERS[-1]
    return t.name, pot // t.divisor


# ── Scenarios ─────────────────────────────────────────────────────────────────

Fn = float | Callable[[int], float]


def _resolve(val: Fn, block: int) -> float:
    return val(block) if callable(val) else val


@dataclass
class Scenario:
    label:        str
    color:        str
    tx_per_block: Fn   # mean tx demand per block (Poisson); fee is always MIN_FEE (2)
    description:  str = ""


SCENARIOS: dict[str, Scenario] = {
    "dormant": Scenario(
        label="Dormant",
        color="#9E9E9E",
        tx_per_block=2.0,
        description="~2 txs/block — very little activity, fee=2",
    ),
    "low_activity": Scenario(
        label="Low activity",
        color="#2196F3",
        tx_per_block=8.0,
        description="~8 txs/block — small community, fee=2",
    ),
    "thriving": Scenario(
        label="Thriving",
        color="#FF9800",
        tx_per_block=60.0,
        description="~60 txs/block — healthy network, fee=2 (blocks 25% full)",
    ),
    "busy": Scenario(
        label="Busy",
        color="#FF5722",
        tx_per_block=120.0,
        description="~120 txs/block — popular network, blocks half full, fee=2",
    ),
}


# ── Simulation ────────────────────────────────────────────────────────────────

@dataclass
class BlockRecord:
    height:       int
    tx_count:     int
    avg_fee:      float  # average fee per tx this block
    fee_income:   int
    fee_rebate:   int    # MINER_FEE_SHARE fraction paid directly to miner
    payout:       int    # lottery payout (0 = no-win or no ticket)
    tier:         str    # winning tier name, or "" if none
    pot:          int
    circulating:  int    # coins outside the pot (= total_supply - pot)
    total_supply: int


def _poisson(rng: random.Random, lam: float) -> int:
    if lam <= 0:
        return 0
    L = math.exp(-min(lam, 700.0))
    k, p = 0, 1.0
    while p > L:
        k += 1
        p *= rng.random()
    return k - 1


def simulate(blocks: int, scenario: Scenario, seed: int) -> list[BlockRecord]:
    rng          = random.Random(seed)
    pot          = GENESIS_POT
    circulating  = GENESIS_SUPPLY - GENESIS_POT
    total_supply = GENESIS_SUPPLY

    pending_tickets: set[int] = set()   # heights with an unsettled ticket
    records: list[BlockRecord] = []

    for height in range(1, blocks + 1):
        # ── Transactions ───────────────────────────────────────────────────────
        mean_tx  = _resolve(scenario.tx_per_block, height)
        tx_count = min(_poisson(rng, mean_tx), MAX_BLOCK_TXS)

        # Each transaction pays the minimum fee of MIN_FEE (2) coins.
        # Fees are paid from the circulating supply and cannot exceed it.
        raw_fee_income = tx_count * MIN_FEE
        fee_income     = min(raw_fee_income, circulating)
        avg_fee        = fee_income / tx_count if tx_count > 0 else 0.0

        # ── Fee split (floor division: equal halves, odd remainder to pot) ─────
        fee_rebate   = fee_income // 2   # stays in circulation
        fee_to_pot   = fee_income - fee_rebate  # moves from circulating to pot
        pot         += fee_to_pot
        circulating -= fee_to_pot   # fee_rebate stays in circulation (miner spends it)

        # ── Coinbase (new coin minted to miner) ────────────────────────────────
        circulating  += COINBASE_REWARD
        total_supply += COINBASE_REWARD

        # ── Issue ticket ──────────────────────────────────────────────────────
        if tx_count > 0:
            pending_tickets.add(height)

        # ── Settle ticket from REVEAL_BLOCKS ago ──────────────────────────────
        payout = 0
        tier   = ""
        settle_height = height - SETTLE_DELAY
        if settle_height in pending_tickets:
            pending_tickets.discard(settle_height)
            tier_name, amount = lottery_draw(rng, pot)
            if tier_name != "none" and amount > 0:
                payout       = min(amount, pot)
                pot         -= payout
                circulating += payout   # winner's coins move from pot to circulation
                tier         = tier_name

        records.append(BlockRecord(
            height       = height,
            tx_count     = tx_count,
            avg_fee      = avg_fee,
            fee_income   = fee_income,
            fee_rebate   = fee_rebate,
            payout       = payout,
            tier         = tier,
            pot          = pot,
            circulating  = circulating,
            total_supply = total_supply,
        ))

    return records


# ── Analysis ──────────────────────────────────────────────────────────────────

def rolling_avg(data: list[float], window: int) -> list[float]:
    out = []
    for i, v in enumerate(data):
        lo = max(0, i - window + 1)
        chunk = data[lo:i + 1]
        out.append(sum(chunk) / len(chunk))
    return out


def annualized_inflation(supply: int, blocks_per_year: int = 525_600) -> float:
    return blocks_per_year / supply * 100.0


def print_summary(records: list[BlockRecord], scenario: Scenario, blocks: int):
    n            = len(records)
    final        = records[-1]
    total_fees   = sum(r.fee_income for r in records)
    total_rebate = sum(r.fee_rebate for r in records)
    total_paid   = sum(r.payout    for r in records)
    total_miner  = total_rebate + total_paid
    wins         = [r for r in records if r.tier]

    days    = n * 60 / 86400
    pot_pct = final.pot / GENESIS_POT * 100

    tier_names  = [t.name for t in LOTTERY_TIERS if t.divisor != 0]
    tier_counts = {t: sum(1 for r in records if r.tier == t) for t in tier_names}
    tier_totals = {t: sum(r.payout for r in records if r.tier == t) for t in tier_names}

    pct = lambda n, d: f"{n / d * 100:.1f}%" if d else "—"

    avg_tx_count    = sum(r.tx_count for r in records) / n
    full_blocks_pct = sum(1 for r in records if r.tx_count == MAX_BLOCK_TXS) / n * 100

    print(f"  Scenario         : {scenario.label} — {scenario.description}")
    print(f"  Blocks           : {n:,}  (~{days:.1f} days at 60 s/block)")
    print(f"  Avg txs/block    : {avg_tx_count:.1f}  "
          f"(blocks at capacity: {full_blocks_pct:.1f}%)")
    print(f"  Final pot        : {final.pot:>15,}  ({pot_pct:.1f}% of genesis)")
    print(f"  Circulating      : {final.circulating:>15,}  "
          f"({final.circulating / final.total_supply * 100:.1f}% of supply)")
    print(f"  Total supply     : {final.total_supply:>15,}  "
          f"(+{final.total_supply - GENESIS_SUPPLY:,} from coinbase)")
    print(f"  Inflation (curr) : {annualized_inflation(final.total_supply):.4f}%/year")
    print()
    print(f"  Total fees collected    : {total_fees:>15,}")
    print(f"  Direct rebate → miners  : {total_rebate:>15,}  ({pct(total_rebate, total_fees)} of fees)")
    print(f"  Fees → pot              : {total_fees - total_rebate:>15,}  ({pct(total_fees - total_rebate, total_fees)} of fees)")
    print(f"  Lottery → miners        : {total_paid:>15,}  ({pct(total_paid, total_fees)} of fees)")
    print(f"  Total miners received   : {total_miner:>15,}  ({pct(total_miner, total_fees)} of fees)")
    print(f"  Net pot change          : {final.pot - GENESIS_POT:>+15,}")
    print()
    win_pct = len(wins) / n * 100
    print(f"  Lottery win rate : {win_pct:.2f}%  (expected {WIN_PROBABILITY * 100:.2f}%)")
    print()
    print(f"  Outcomes by tier:")
    for t in tier_names:
        c = tier_counts[t]
        w = tier_totals[t]
        avg_w = w // c if c else 0
        freq  = f"every {n // c:.0f} blocks" if c else "never"
        print(f"    {t:<8}  {c:>5} wins  ({freq})  avg payout {avg_w:>10,}  total {w:>14,}")


# ── Plotting ──────────────────────────────────────────────────────────────────

TIER_COLORS = {
    "small":   "#78909C",
    "medium":  "#42A5F5",
    "large":   "#AB47BC",
    "jackpot": "#EF5350",
}


def _save_or_show(fig, path: str | None, suffix: str):
    """Save figure to path/suffix.png if path given, otherwise show interactively."""
    if path:
        out = os.path.join(path, suffix + ".png")
        fig.savefig(out, dpi=150, bbox_inches="tight")
        print(f"  Saved {out}")
        plt.close(fig)
    else:
        plt.show()


def plot_pot_balance(all_records: dict[str, list[BlockRecord]], save_dir: str | None = None):
    """Pot balance over time for all scenarios."""
    fig, ax = plt.subplots(figsize=(10, 5))
    for key, records in all_records.items():
        s = SCENARIOS[key]
        ax.plot([r.height for r in records], [r.pot / 1e6 for r in records],
                color=s.color, lw=1.5, label=s.label)
    ax.axhline(GENESIS_POT / 1e6, color="black", lw=0.8, ls=":", alpha=0.5,
               label="Genesis pot (99M)")
    ax.set_ylabel("Pot balance (M coins)")
    ax.set_xlabel("Block height")
    ax.set_title("Lottery pot balance under different activity levels", fontweight="bold")
    ax.set_ylim(bottom=0)
    ax.legend(fontsize=9)
    ax.grid(alpha=0.25)
    fig.tight_layout()
    _save_or_show(fig, save_dir, "pot_balance")


def plot_supply_inflation(all_records: dict[str, list[BlockRecord]], save_dir: str | None = None):
    """Total supply and annualized inflation rate (scenario-independent)."""
    ref = list(all_records.values())[0]
    hs  = [r.height for r in ref]

    fig, ax_sup = plt.subplots(figsize=(10, 5))
    ax_sup.plot(hs, [r.total_supply / 1e6 for r in ref], color="#3F51B5", lw=1.5,
                label="Total supply")
    ax_i = ax_sup.twinx()
    ax_i.plot(hs, [annualized_inflation(r.total_supply) for r in ref],
              color="#F44336", lw=1.2, ls="--", label="Inflation %/yr")
    ax_i.set_ylabel("Annualized inflation (%/yr)", color="#F44336")
    ax_i.tick_params(axis="y", labelcolor="#F44336")
    ax_sup.set_ylabel("Total supply (M coins)")
    ax_sup.set_xlabel("Block height")
    ax_sup.set_title("Coin supply & annualized inflation (scenario-independent)", fontweight="bold")
    l1, lb1 = ax_sup.get_legend_handles_labels()
    l2, lb2 = ax_i.get_legend_handles_labels()
    ax_sup.legend(l1 + l2, lb1 + lb2, fontsize=9)
    ax_sup.grid(alpha=0.25)
    fig.tight_layout()
    _save_or_show(fig, save_dir, "supply_inflation")


def plot_fee_flow(all_records: dict[str, list[BlockRecord]], detail_key: str,
                  save_dir: str | None = None):
    """Fee flow breakdown for the detail scenario."""
    det    = all_records[detail_key]
    window = max(1, len(det) // 60)
    hs_det = [r.height for r in det]
    rebate_ra  = rolling_avg([r.fee_rebate for r in det], window)
    pot_ra     = rolling_avg([r.fee_income - r.fee_rebate for r in det], window)
    lottery_ra = rolling_avg([r.payout for r in det], window)

    fig, ax = plt.subplots(figsize=(10, 5))
    ax.fill_between(hs_det, rebate_ra,  color="#4CAF50", alpha=0.5, label="Direct rebate → miner")
    ax.fill_between(hs_det, pot_ra,     color="#2196F3", alpha=0.3, label="Fees → pot")
    ax.fill_between(hs_det, lottery_ra, color="#F44336", alpha=0.5, label="Lottery drain")
    ax.plot(hs_det, rebate_ra,  color="#4CAF50", lw=1.0)
    ax.plot(hs_det, pot_ra,     color="#2196F3", lw=1.0)
    ax.plot(hs_det, lottery_ra, color="#F44336", lw=1.0)
    ax.set_ylabel("Coins per block (rolling avg)")
    ax.set_xlabel("Block height")
    ax.set_title(f"Fee flow breakdown — {SCENARIOS[detail_key].label}", fontweight="bold")
    ax.legend(fontsize=9)
    ax.grid(alpha=0.25)
    fig.tight_layout()
    _save_or_show(fig, save_dir, "fee_flow")


def plot_lottery_payouts(all_records: dict[str, list[BlockRecord]], detail_key: str,
                          save_dir: str | None = None):
    """Cumulative lottery payouts by tier for the detail scenario."""
    det        = all_records[detail_key]
    hs_det     = [r.height for r in det]
    tier_names = [t.name for t in LOTTERY_TIERS if t.divisor != 0]
    cumulative = {t: [] for t in tier_names}
    running    = {t: 0   for t in tier_names}
    for r in det:
        if r.tier in running:
            running[r.tier] += r.payout
        for t in tier_names:
            cumulative[t].append(running[t] / 1e6)

    fig, ax = plt.subplots(figsize=(10, 5))
    ax.stackplot(
        hs_det,
        *[cumulative[t] for t in tier_names],
        labels=[t.capitalize() for t in tier_names],
        colors=[TIER_COLORS.get(t, "#999") for t in tier_names],
        alpha=0.75,
    )
    ax.set_ylabel("Cumulative lottery payout (M coins)")
    ax.set_xlabel("Block height")
    ax.set_title(f"Cumulative lottery payouts by tier — {SCENARIOS[detail_key].label}",
                 fontweight="bold")
    ax.legend(fontsize=9, loc="upper left")
    ax.grid(alpha=0.25)
    fig.tight_layout()
    _save_or_show(fig, save_dir, "lottery_payouts")



def plot_all_combined(all_records: dict[str, list[BlockRecord]], detail_key: str):
    """Legacy combined 4-panel figure shown interactively."""
    fig = plt.figure(figsize=(14, 10))
    fig.suptitle("Lootcoin economic simulation", fontsize=13, fontweight="bold")
    gs = gridspec.GridSpec(2, 2, hspace=0.42, wspace=0.32, top=0.93, bottom=0.07)

    # Panel A: pot balance
    ax_pot = fig.add_subplot(gs[0, 0])
    for key, records in all_records.items():
        s = SCENARIOS[key]
        ax_pot.plot([r.height for r in records], [r.pot / 1e6 for r in records],
                    color=s.color, lw=1.2, label=s.label)
    ax_pot.axhline(GENESIS_POT / 1e6, color="black", lw=0.8, ls=":", alpha=0.5,
                   label="Genesis pot")
    ax_pot.set_ylabel("Pot balance (M coins)")
    ax_pot.set_ylim(bottom=0)
    ax_pot.set_xlabel("Block height")
    ax_pot.set_title("Pot balance — all scenarios", fontsize=10)
    ax_pot.legend(fontsize=7)
    ax_pot.grid(alpha=0.25)

    # Panel B: supply + inflation
    ref    = list(all_records.values())[0]
    hs     = [r.height for r in ref]
    ax_sup = fig.add_subplot(gs[0, 1])
    ax_sup.plot(hs, [r.total_supply / 1e6 for r in ref], color="#3F51B5", lw=1.3,
                label="Total supply")
    ax_sup.set_ylabel("Total supply (M coins)")
    ax_i = ax_sup.twinx()
    ax_i.plot(hs, [annualized_inflation(r.total_supply) for r in ref],
              color="#F44336", lw=1.0, ls="--", label="Inflation %/yr")
    ax_i.set_ylabel("Annualized inflation (%/yr)", color="#F44336")
    ax_i.tick_params(axis="y", labelcolor="#F44336")
    ax_sup.set_xlabel("Block height")
    ax_sup.set_title("Supply & inflation (scenario-independent)", fontsize=10)
    l1, lb1 = ax_sup.get_legend_handles_labels()
    l2, lb2 = ax_i.get_legend_handles_labels()
    ax_sup.legend(l1 + l2, lb1 + lb2, fontsize=7)
    ax_sup.grid(alpha=0.25)

    # Panel C: fee flow
    ax_flow = fig.add_subplot(gs[1, 0])
    det    = all_records[detail_key]
    window = max(1, len(det) // 60)
    hs_det = [r.height for r in det]
    rebate_ra  = rolling_avg([r.fee_rebate for r in det], window)
    pot_ra     = rolling_avg([r.fee_income - r.fee_rebate for r in det], window)
    lottery_ra = rolling_avg([r.payout for r in det], window)
    ax_flow.fill_between(hs_det, rebate_ra,  color="#4CAF50", alpha=0.5, label="Direct rebate")
    ax_flow.fill_between(hs_det, pot_ra,     color="#2196F3", alpha=0.3, label="Fees → pot")
    ax_flow.fill_between(hs_det, lottery_ra, color="#F44336", alpha=0.5, label="Lottery drain")
    ax_flow.plot(hs_det, rebate_ra,  color="#4CAF50", lw=1.0)
    ax_flow.plot(hs_det, pot_ra,     color="#2196F3", lw=1.0)
    ax_flow.plot(hs_det, lottery_ra, color="#F44336", lw=1.0)
    ax_flow.set_ylabel("Coins per block (rolling avg)")
    ax_flow.set_xlabel("Block height")
    ax_flow.set_title(f"Fee flow — {SCENARIOS[detail_key].label}", fontsize=10)
    ax_flow.legend(fontsize=7)
    ax_flow.grid(alpha=0.25)

    # Panel D: cumulative lottery payouts
    ax_tier = fig.add_subplot(gs[1, 1])
    tier_names = [t.name for t in LOTTERY_TIERS if t.divisor != 0]
    cumulative = {t: [] for t in tier_names}
    running    = {t: 0   for t in tier_names}
    for r in det:
        if r.tier in running:
            running[r.tier] += r.payout
        for t in tier_names:
            cumulative[t].append(running[t] / 1e6)
    ax_tier.stackplot(
        hs_det,
        *[cumulative[t] for t in tier_names],
        labels=[t.capitalize() for t in tier_names],
        colors=[TIER_COLORS.get(t, "#999") for t in tier_names],
        alpha=0.75,
    )
    ax_tier.set_ylabel("Cumulative lottery payout (M coins)")
    ax_tier.set_xlabel("Block height")
    ax_tier.set_title(f"Cumulative lottery payouts — {SCENARIOS[detail_key].label}", fontsize=10)
    ax_tier.legend(fontsize=7, loc="upper left")
    ax_tier.grid(alpha=0.25)

    plt.show()


# ── Entry point ───────────────────────────────────────────────────────────────

def main():
    sys.stdout.reconfigure(encoding="utf-8")
    parser = argparse.ArgumentParser(
        description=__doc__,
        formatter_class=argparse.RawDescriptionHelpFormatter,
    )
    parser.add_argument("--blocks",   type=int, default=100_000,
                        help="Blocks to simulate (default: 100 000, ~69 days)")
    parser.add_argument("--scenario", choices=list(SCENARIOS.keys()),
                        help="Run a single scenario (default: all scenarios)")
    parser.add_argument("--detail",   choices=list(SCENARIOS.keys()),
                        default="thriving",
                        help="Scenario shown in detail panels of the plot (default: thriving)")
    parser.add_argument("--seed",     type=int, default=42)
    parser.add_argument("--no-plot",  action="store_true",
                        help="Print summaries only, skip all plots")
    parser.add_argument("--save-dir", metavar="DIR",
                        help="Save individual chart PNGs to DIR instead of showing interactively")
    args = parser.parse_args()

    scenarios = (
        {args.scenario: SCENARIOS[args.scenario]}
        if args.scenario
        else SCENARIOS
    )

    print(f"Lootcoin simulation — {args.blocks:,} blocks  "
          f"(~{args.blocks * 60 / 86400:.1f} days)\n"
          f"Fee split: {int(MINER_FEE_SHARE * 100)}% direct to miner, "
          f"{int((1 - MINER_FEE_SHARE) * 100)}% to pot  |  "
          f"Accounting: circulating + pot = total_supply (always)\n")

    all_records: dict[str, list[BlockRecord]] = {}
    for key, s in scenarios.items():
        print(f"── {s.label} {'─' * (55 - len(s.label))}")
        records = simulate(args.blocks, s, args.seed)
        all_records[key] = records
        print_summary(records, s, args.blocks)
        print()

    if args.no_plot:
        return

    if not HAS_MATPLOTLIB:
        print("matplotlib not found — pip install matplotlib")
        sys.exit(1)

    save_dir = args.save_dir
    if save_dir:
        os.makedirs(save_dir, exist_ok=True)
        # Use non-interactive backend when saving files
        matplotlib.use("Agg")

    detail_key = args.detail if args.detail in all_records else list(all_records.keys())[0]

    if save_dir:
        print(f"\nSaving charts to {save_dir}/")
        plot_pot_balance(all_records, save_dir)
        plot_supply_inflation(all_records, save_dir)
        if detail_key in all_records:
            plot_fee_flow(all_records, detail_key, save_dir)
            plot_lottery_payouts(all_records, detail_key, save_dir)
    else:
        plot_all_combined(all_records, detail_key)


if __name__ == "__main__":
    main()
