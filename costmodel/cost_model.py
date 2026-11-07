"""Monthly transfer cost by lever.

    bill  ~  subscribers x messages x bytes x $/GB

Inputs:
  results/sizes.json  byte ratios measured on SYNTHETIC data (crates/bench sizes)
  results/timing.json CPU per connection, measured on one machine
  costmodel/pricing.toml public list prices checked 2026-11-07 (an upper bound)

The $1k/$10k/$30k bills are ILLUSTRATIVE: the model backs out how much
traffic each would imply at list price, then prices that traffic under each
lever. Nothing here is a real company's bill.
"""

from __future__ import annotations

import csv
import json
import math
import sys
import tomllib
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
RESULTS = ROOT / "results"
SCENARIOS = [1_000, 10_000, 30_000]
CHART_SCENARIO = 10_000
SECONDS_PER_MONTH = 730 * 3600

# ---- Assumptions (not measurements). Each is printed into the results.
ASSUMPTIONS = {
    "all_of_the_bill_is_market_data_egress_via_one_nlb": True,
    "relay_monthly_capacity_tb": 100.0,  # ~30% of a 1 Gbit/s port, per relay
    "relay_included_tb": 1.0,  # low end of Hetzner's published US cloud range
    "direct_connect_port_max_utilisation": 0.5,
    "orders_per_second_for_order_lever": 1000,
}


def tiered(gb: float, edges: list[float], prices: list[float]) -> float:
    cost, lo = 0.0, 0.0
    for edge, price in zip(edges + [math.inf], prices):
        if gb > lo:
            cost += (min(gb, edge) - lo) * price
        lo = edge
    return cost


class Prices:
    def __init__(self, p: dict):
        self.p = p

    def egress(self, gb: float) -> float:
        e = self.p["aws_egress"]
        return tiered(max(0.0, gb - e["free_tier_gb"]), e["tiers_gb"], e["tier_usd_per_gb"])

    def nlb(self, gb: float) -> float:
        n = self.p["aws_nlb"]
        return n["usd_per_hour"] * n["hours_per_month"] + gb / n["gb_per_nlcu_hour"] * n["usd_per_nlcu_hour"]

    def aws_bill(self, gb: float) -> float:
        return self.egress(gb) + self.nlb(gb)

    def gb_for_bill(self, usd: float) -> float:
        lo, hi = 0.0, 1e9
        for _ in range(200):
            mid = (lo + hi) / 2
            lo, hi = (mid, hi) if self.aws_bill(mid) < usd else (lo, mid)
        return lo

    def direct_connect(self, gb: float) -> tuple[float, str]:
        d = self.p["aws_direct_connect"]
        gbps = gb * 8 / SECONDS_PER_MONTH / ASSUMPTIONS["direct_connect_port_max_utilisation"]
        if gbps <= 1:
            port, name = d["port_1g_usd_per_hour"], "1G"
        else:
            ports = math.ceil(gbps / 10)
            port, name = ports * d["port_10g_usd_per_hour"], f"{ports}x10G"
        return gb * d["usd_per_gb_out"] + port * 730 + self.nlb(gb), name

    def cloudfront(self, gb: float) -> float:
        c = self.p["aws_cloudfront"]
        return tiered(gb, c["us_tiers_gb"], c["us_tier_usd_per_gb"]) + self.nlb(gb)

    def relay_tier(self, client_gb: float, feed_gb_per_relay: float) -> tuple[float, dict]:
        """AWS sends one copy of the feed to each relay; relays serve clients."""
        h = self.p["hetzner"]
        cap_gb = ASSUMPTIONS["relay_monthly_capacity_tb"] * 1024
        relays = max(2, math.ceil(client_gb / cap_gb))  # at least two for redundancy
        aws_gb = relays * feed_gb_per_relay
        per_relay_tb = client_gb / relays / 1024
        overage_tb = max(0.0, per_relay_tb - ASSUMPTIONS["relay_included_tb"]) * relays
        relay_cost = relays * h["dedicated_vcpu_cloud_from_eur_per_month"] * h["eur_to_usd"]
        overage = overage_tb * h["overage_usd_per_tb_as_stated"]
        total = self.aws_bill(aws_gb) + relay_cost + overage
        return total, {"relays": relays, "aws_gb": aws_gb, "relay_servers_usd": relay_cost, "relay_overage_usd": overage}


def load_ratios(sizes: dict) -> dict:
    """Payload bytes relative to JSON, per regime, for a whole-feed subscriber."""
    out = {}
    for regime, r in sizes["regimes"].items():
        v = r["whole_feed_subscriber"]
        j = v["json"]["payload_bytes"]
        jw = v["json"]["wire_bytes_modelled"]
        out[regime] = {k: x["payload_bytes"] / j for k, x in v.items()}
        out[regime]["_wire"] = {k: x["wire_bytes_modelled"] / jw for k, x in v.items()}
        s = r["one_instrument_subscribers_summed"]
        out[regime]["_one_instrument_wire"] = {
            k: x["wire_bytes_modelled"] / s["json"]["wire_bytes_modelled"] for k, x in s.items()
        }
    return out


def main() -> None:
    prices = Prices(tomllib.loads((ROOT / "costmodel" / "pricing.toml").read_text()))
    sizes = json.loads((RESULTS / "sizes.json").read_text())
    timing = json.loads((RESULTS / "timing.json").read_text())
    ratios = load_ratios(sizes)
    base = ratios["base"]

    feed_bps = sizes["regimes"]["base"]["whole_feed_subscriber"]["json"]["payload_bytes_per_s"]
    feed_gb_month = feed_bps * SECONDS_PER_MONTH / 1e9
    bin_feed_gb_month = feed_gb_month * base["binary_deltas"]
    pmd_cpu_s_per_conn_s = (
        timing["fan_out_one_second_of_feed"]["results"]["json_pmd_context_takeover_per_connection"][
            "cpu_ns_per_item_median"
        ]
        / 1e9
    )
    pmd_heap = timing["per_connection_state_bytes"]["pmd_context_takeover_live_heap"]

    # (key, label, kind, ratio or callable). kind: measured | modelled
    encoding_levers = [
        ("binary_snapshots", "Binary, full snapshots", "measured"),
        ("binary_deltas", "Binary + deltas (exact)", "measured"),
        ("binary_deltas_suppress_unchanged", "+ skip unchanged books (if sent today)", "measured"),
        ("binary_deltas_suppress_conflate_1s", "+ conflate books to 1 s (opt-in)", "measured"),
        ("json_pmd_context_takeover", "JSON + permessage-deflate", "measured"),
        ("json_zstd_dictionary", "JSON + zstd dictionary", "measured"),
    ]

    scenarios = []
    for bill in SCENARIOS:
        gb = prices.gb_for_bill(bill)
        subs = gb / feed_gb_month
        rows = [{"lever": "Today: JSON over AWS egress", "kind": "baseline", "monthly_usd": prices.aws_bill(gb), "note": ""}]
        for key, label, kind in encoding_levers:
            lo = min(ratios[r][key] for r in ratios)
            hi = max(ratios[r][key] for r in ratios)
            note = f"bytes x{base[key]:.3f} (regimes {lo:.3f}-{hi:.3f})"
            if key in base["_one_instrument_wire"]:
                note += f"; one-instrument subscribers, wire model: x{base['_one_instrument_wire'][key]:.3f}"
            if key == "json_pmd_context_takeover":
                cores = subs * pmd_cpu_s_per_conn_s
                note += f"; costs ~{pmd_cpu_s_per_conn_s*1000:.0f} ms CPU per connection-second (~{cores:.1f} cores at this scale) and ~{pmd_heap/1024:.0f} KiB per connection"
            if key == "json_zstd_dictionary":
                note += "; not a WebSocket standard, so every client needs a custom decoder"
            rows.append({"lever": label, "kind": kind, "ratio": base[key], "monthly_usd": prices.aws_bill(gb * base[key]), "note": note})
        dx, port = prices.direct_connect(gb)
        rows.append({"lever": "Direct Connect to the CDN", "kind": "modelled", "monthly_usd": dx,
                     "note": f"$0.02/GB + {port} port + NLB; PLUS UNKNOWN CDN interconnect cost (for example, Cloudflare Network Interconnect is Enterprise-only with no public price) and colocation fees", "unknown_extra": True})
        rows.append({"lever": "CloudFront instead of current CDN", "kind": "modelled", "monthly_usd": prices.cloudfront(gb),
                     "note": "AWS to CloudFront is $0/GB; CloudFront US list tiers; replaces the CDN's WAF/DDoS role; flat-rate plans cap at 50 TB unless re-priced (UNKNOWN)"})
        relay, det = prices.relay_tier(gb, feed_gb_month)
        rows.append({"lever": "Relay tier outside AWS (JSON)", "kind": "modelled", "monthly_usd": relay,
                     "note": f"{det['relays']} relays on Hetzner at list price; AWS sends one feed copy per relay ({det['aws_gb']:.0f} GB); relay overage ${det['relay_overage_usd']:.0f}", "detail": det})
        relay_b, det_b = prices.relay_tier(gb * base["binary_deltas"], bin_feed_gb_month)
        rows.append({"lever": "Binary + deltas AND relay tier", "kind": "modelled", "monthly_usd": relay_b,
                     "note": f"{det_b['relays']} relays; clients get binary. Serving JSON clients from relays would add relay egress", "detail": det_b})
        order_gb = ASSUMPTIONS["orders_per_second_for_order_lever"] * (
            sizes["orders"]["mean_json_bytes"] - sizes["orders"]["mean_binary_bytes"]) * SECONDS_PER_MONTH / 1e9
        rows.append({"lever": "Binary orders", "kind": "modelled",
                     "monthly_usd": prices.aws_bill(gb) - order_gb * prices.p["aws_nlb"]["usd_per_nlcu_hour"],
                     "note": f"inbound transfer is $0/GB, so only NLB processing changes: {order_gb:.0f} GB/month fewer at {ASSUMPTIONS['orders_per_second_for_order_lever']} orders/s saves ${order_gb*0.006:.2f}"})
        for r in rows:
            r["saving_vs_today_usd"] = rows[0]["monthly_usd"] - r["monthly_usd"]
        scenarios.append({"illustrative_bill_usd": bill, "implied_gb_per_month": gb,
                          "implied_whole_feed_json_subscribers": subs, "rows": rows})

    other = {
        "relay_host_egress_usd_per_tb_list": {
            "aws_us_east_1_first_tier": 0.09 * 1024,
            "hetzner_overage": prices.p["hetzner"]["overage_usd_per_tb_as_stated"],
            "oracle_after_10tb": prices.p["oracle"]["usd_per_gb_after"] * 1024,
            "digitalocean_over_pool": prices.p["digitalocean"]["usd_per_gib_over_pool"] * 1024,
            "gcp_premium_na_10tib_plus": prices.p["gcp"]["usd_per_gib"][2] * 1024,
        },
        "note": "Per-TB list prices for the host of a relay tier. Matching is assumed to stay in AWS; only the public fan-out moves.",
    }
    doc = {
        "label": "MODEL. Byte ratios measured on SYNTHETIC data; prices are public list prices checked "
                 + prices.p["checked"] + " (upper bound); bills are illustrative.",
        "assumptions": ASSUMPTIONS,
        "whole_feed_json_gb_per_subscriber_month": feed_gb_month,
        "scenarios": scenarios,
        "other_providers": other,
    }
    (RESULTS / "cost_model.json").write_text(json.dumps(doc, indent=2) + "\n")
    with (RESULTS / "cost_model.csv").open("w", newline="") as f:
        w = csv.writer(f)
        w.writerow(["illustrative_bill_usd", "lever", "kind", "monthly_usd", "saving_vs_today_usd", "note"])
        for s in scenarios:
            for r in s["rows"]:
                w.writerow([s["illustrative_bill_usd"], r["lever"], r["kind"], round(r["monthly_usd"]), round(r["saving_vs_today_usd"]), r["note"]])
    chart(next(s for s in scenarios if s["illustrative_bill_usd"] == CHART_SCENARIO), prices.p["checked"])
    print(f"wrote {RESULTS/'cost_model.json'}, cost_model.csv, monthly_cost_by_lever.png", file=sys.stderr)


def chart(s: dict, checked: str) -> None:
    import matplotlib

    matplotlib.use("Agg")
    matplotlib.rcParams["svg.hashsalt"] = "mdbin"
    import matplotlib.pyplot as plt
    from matplotlib.patches import Patch

    surface, ink, ink2, grid = "#fcfcfb", "#0b0b0b", "#52514e", "#e4e3df"
    color = {"baseline": "#8a8984", "measured": "#2a78d6", "modelled": "#eb6834"}
    keep = [
        "Today: JSON over AWS egress",
        "JSON + permessage-deflate",
        "Binary, full snapshots",
        "Binary + deltas (exact)",
        "+ skip unchanged books (if sent today)",
        "CloudFront instead of current CDN",
        "Direct Connect to the CDN",
        "Relay tier outside AWS (JSON)",
        "Binary + deltas AND relay tier",
    ]
    rows = [r for k in keep for r in s["rows"] if r["lever"] == k]
    fig, ax = plt.subplots(figsize=(9, 5.2), dpi=150)
    fig.patch.set_facecolor(surface)
    ax.set_facecolor(surface)
    y = list(range(len(rows)))[::-1]
    for yi, r in zip(y, rows):
        ax.barh(yi, r["monthly_usd"], height=0.55, color=color[r["kind"]], edgecolor=surface, linewidth=2)
        label = f"${r['monthly_usd']:,.0f}"
        if r.get("unknown_extra"):
            label += " + unknown interconnect"
        ax.text(r["monthly_usd"] + s["rows"][0]["monthly_usd"] * 0.012, yi, label, va="center", ha="left", color=ink, fontsize=9)
    ax.set_yticks(y)
    ax.set_yticklabels([r["lever"] for r in rows], color=ink, fontsize=9.5)
    ax.set_xlim(0, s["rows"][0]["monthly_usd"] * 1.32)
    ax.xaxis.set_major_formatter(matplotlib.ticker.FuncFormatter(lambda v, _: f"${v:,.0f}"))
    ax.tick_params(axis="x", colors=ink2, labelsize=8.5)
    ax.tick_params(axis="y", length=0)
    ax.grid(axis="x", color=grid, linewidth=0.8)
    ax.set_axisbelow(True)
    for side in ["top", "right", "left"]:
        ax.spines[side].set_visible(False)
    ax.spines["bottom"].set_color(grid)
    ax.set_title(
        f"Monthly cost by lever, for an illustrative ${s['illustrative_bill_usd']:,} / month market data bill",
        loc="left", color=ink, fontsize=11.5, pad=26,
    )
    ax.text(0, 1.02, f"Implied traffic {s['implied_gb_per_month']/1024:,.0f} TB/month at AWS list prices (checked {checked}); lower is better",
            transform=ax.transAxes, color=ink2, fontsize=8.5)
    legend = [
        Patch(color=color["baseline"], label="Today (list-price model)"),
        Patch(color=color["measured"], label="Byte ratio measured on SYNTHETIC data"),
        Patch(color=color["modelled"], label="Modelled from public list prices"),
    ]
    ax.legend(handles=legend, loc="lower right", frameon=False, fontsize=8.5, labelcolor=ink2)
    fig.text(0.01, 0.01, "Encoding bars: payload bytes, whole-feed subscriber, base regime. See results/cost_model.csv for every lever, range and caveat.",
             color=ink2, fontsize=7.5)
    fig.tight_layout(rect=(0, 0.03, 1, 1))
    fig.savefig(RESULTS / "monthly_cost_by_lever.png", facecolor=surface, metadata={"Software": None})
    plt.close(fig)


if __name__ == "__main__":
    main()
