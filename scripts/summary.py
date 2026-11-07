"""Builds results/summary.md from the JSON results. Standard library only.

README.md embeds this file between the summary markers, and
scripts/check_numbers.py fails if the two drift apart.
"""

import json
import sys
from pathlib import Path

R = Path(__file__).resolve().parent.parent / "results"


def load(name):
    return json.loads((R / name).read_text())


def pct_less(ratio):
    return f"{(1 - ratio) * 100:.0f}%"


def main():
    s, t, b, c, fz = load("sizes.json"), load("timing.json"), load("broker.json"), load("cost_model.json"), load("fuzz.json")
    reg = s["regimes"]
    base = reg["base"]["whole_feed_subscriber"]
    j = base["json"]["payload_bytes"]
    jw = base["json"]["wire_bytes_modelled"]
    one = reg["base"]["one_instrument_subscribers_summed"]
    names = [
        ("json", "JSON as sent today"),
        ("json_pmd_no_context_takeover", "JSON + permessage-deflate, no context takeover"),
        ("json_zstd_dictionary", "JSON + zstd with a trained dictionary"),
        ("json_pmd_context_takeover", "JSON + permessage-deflate, context takeover"),
        ("binary_snapshots", "Binary, full snapshots"),
        ("binary_deltas", "Binary + deltas (exact round trip)"),
        ("binary_deltas_suppress_unchanged", "Binary + deltas, unchanged books skipped"),
        ("binary_deltas_suppress_conflate_1s", "... and books conflated to 1 s"),
        ("binary_deltas_plus_pmd", "Binary + deltas + permessage-deflate"),
    ]
    out = []
    w = out.append
    w("### Bytes per second, whole-feed subscriber (SYNTHETIC data, base regime)\n")
    w(f"{reg['base']['instruments']} instruments, {s['measured_ticks']} ticks of 100 ms after a {s['warmup_ticks_excluded']}-tick warm-up. "
      "Ratio is payload bytes relative to JSON. Range is across the calm, base and busy regimes.\n")
    w("| Encoding | KB/s | vs JSON | Saving | Range across regimes | Wire, modelled |")
    w("|---|---:|---:|---:|---|---:|")
    for k, label in names:
        r = base[k]["payload_bytes"] / j
        rng = [reg[x]["whole_feed_subscriber"][k]["payload_bytes"] / reg[x]["whole_feed_subscriber"]["json"]["payload_bytes"] for x in reg]
        wire = base[k]["wire_bytes_modelled"] / jw
        w(f"| {label} | {base[k]['payload_bytes_per_s']/1000:.1f} | {r:.3f} | {pct_less(r)} | {min(rng):.3f} to {max(rng):.3f} | {wire:.3f} |")
    bs = reg["base"]["book_stats"]
    w("")
    w(f"- Unchanged books between consecutive 100 ms publishes (an assumption-driven SYNTHETIC output): "
      + ", ".join(f"{x} {reg[x]['book_stats']['unchanged_fraction']*100:.0f}%" for x in ["calm", "base", "busy"]) + ".")
    w(f"- An SBE-style fixed-width layout (Binance `DepthSnapshotStreamEvent` shape) would be {bs['sbe_fixed_layout_bytes']/bs['json_bytes']:.3f} of JSON for books; the varint layout here is {base['binary_snapshots']['by_family_payload']['book']/base['json']['by_family_payload']['book']:.3f}.")
    w(f"- One-instrument subscribers, wire model: binary + deltas {one['binary_deltas']['wire_bytes_modelled']/one['json']['wire_bytes_modelled']:.3f} of JSON, "
      f"JSON + permessage-deflate {one['json_pmd_context_takeover']['wire_bytes_modelled']/one['json']['wire_bytes_modelled']:.3f}. "
      "Small frames are dominated by TCP/TLS overhead, so the saving shrinks.")
    w(f"- Joining the whole binary feed costs {reg['base']['join_bytes_whole_feed_binary']/1000:.1f} KB once per connection (directory and snapshots).")
    w(f"- Raw-JSON fallbacks across all runs: {sum(reg[x]['raw_fallbacks'] for x in reg)}. Every synthetic message round-tripped exactly.")
    o = s["orders"]
    w(f"- Signed order requests: {o['mean_json_bytes']:.0f} bytes as JSON, {o['mean_binary_bytes']:.0f} as binary; the signature goes from {o['signature_json_chars']} characters of hex to 65 bytes. Requests are inbound, which AWS does not charge for; responses were not measured.\n")

    w("### CPU and memory (MEASURED on one machine, SYNTHETIC inputs)\n")
    m = t["machine"]
    w(f"{m['cpu'].strip()}, {m['cores']} cores, WSL2 kernel {m['kernel']}, load average {m['load_avg_1_5_15_before']} at start. "
      "These are serialization and copy costs, not network latency.\n")
    w("| Per message | CPU ns |")
    w("|---|---:|")
    pm = t["per_message"]
    for k, label in [("serde_json_parse_to_value", "Parse JSON into a generic value (what a JSON client pays)"),
                     ("transcode_json_to_binary_deltas", "Transcode JSON to binary + deltas (gateway, once per message)"),
                     ("decode_binary_and_apply", "Decode binary and apply to local state (client)"),
                     ("decode_binary_apply_and_render_json", "Decode, apply and rebuild the exact JSON text"),
                     ("pmd_compress_context_takeover", "permessage-deflate, per connection"),
                     ("zstd_dictionary_compress", "zstd with dictionary, once per message")]:
        w(f"| {label} | {pm[k]['cpu_ns_per_item_median']:,.0f} |")
    po = t["per_order"]
    w(f"| Verify one order signature (secp256k1 recover, pure Rust) | {po['verify_signature_secp256k1_recover']['cpu_ns_per_item_median']:,.0f} |")
    w("")
    fo = t["fan_out_one_second_of_feed"]["results"]
    w("| Fan-out, CPU per connection per second of feed | microseconds |")
    w("|---|---:|")
    for k, label in [("binary_transcode_once_then_copy", "Binary: transcode once, copy shared bytes"),
                     ("json_as_is_copy_to_each_connection", "JSON as is: copy shared bytes"),
                     ("json_pmd_no_context_compress_once_then_copy", "permessage-deflate without context: compress once, copy"),
                     ("json_pmd_context_takeover_per_connection", "permessage-deflate with context: compress per connection")]:
        w(f"| {label} | {fo[k]['cpu_ns_per_item_median']/1000:,.1f} |")
    w(f"\nPer-connection compressor state, live heap after traffic: {t['per_connection_state_bytes']['pmd_context_takeover_live_heap']/1024:.0f} KiB for permessage-deflate with context. Binary encode-once keeps no per-connection codec state.\n")

    w("### Redis and Dragonfly pub/sub (MEASURED on loopback)\n")
    w("| Server | Byte-exact (7 hostile payloads, PUBLISH and SET/GET) | p50 / p99 publish-to-receive, 2000 msg/s, one subscriber | Pipelined ceiling, binary msgs/s, one subscriber |")
    w("|---|---|---|---:|")
    for name, r in b["servers"].items():
        if not r.get("tested"):
            w(f"| {name} | UNTESTED: {r.get('reason')} | | |")
            continue
        ex = r["byte_exactness"]
        ok = all(v["publish_exact"] and v["set_get_exact"] for v in ex.values())
        lat = r["publish_to_receive_loopback"]["binary_messages_2000_per_s"]
        cap = r["pipelined_ceiling_one_subscriber"]["binary_messages"]["received_msgs_per_s"]
        ver = r["version"].split()
        label = f"Dragonfly {ver[1].split('-')[0]}" if name == "dragonfly" else f"Redis {ver[2].removeprefix('v=')}"
        w(f"| {label} | {'yes, all' if ok else 'NO'} | {lat['p50_us']:.0f} / {lat['p99_us']:.0f} us | {cap:,.0f} |")
    w(f"\nThe synthetic whole feed is {b['synthetic_feed_messages_per_s']:,} messages per second.\n")
    w(f"### Decoder mutation testing\n\n{fz['iterations']:,} seeded mutations (byte flips, truncation, junk insertion, random frames), each applied after a valid snapshot frame; not coverage-guided: {fz['panics']} panics.\n")

    w("### Monthly cost by lever (MODEL)\n")
    w("List prices checked 2026-11-07 are an upper bound. Bills are illustrative. Encoding ratios are from the SYNTHETIC base regime.\n")
    sc = c["scenarios"]
    levers = [r["lever"] for r in sc[0]["rows"]]
    w("| Lever | " + " | ".join(f"${x['illustrative_bill_usd']:,}/month" for x in sc) + " |")
    w("|---|" + "---:|" * len(sc))
    w("| Implied traffic | " + " | ".join(f"{x['implied_gb_per_month']/1024:,.0f} TB" for x in sc) + " |")
    for i, lv in enumerate(levers):
        cells = []
        for x in sc:
            r = x["rows"][i]
            cell = f"${r['monthly_usd']:,.0f}"
            if r.get("unknown_extra"):
                cell += " + unknown"
            cells.append(cell)
        w(f"| {lv} | " + " | ".join(cells) + " |")
    text = "\n".join(out) + "\n"
    (R / "summary.md").write_text(text)
    if "--update-readme" in sys.argv:
        readme = R.parent / "README.md"
        doc = readme.read_text()
        a, b = "<!-- summary:start -->\n", "<!-- summary:end -->"
        head, rest = doc.split(a, 1)
        readme.write_text(head + a + text + b + rest.split(b, 1)[1])


if __name__ == "__main__":
    main()
