# Synthetic data: what it is and what it assumes

**Every market data number in this repo comes from a simulation.** No data was captured from any exchange.

## Why there is no live capture

The target API is public and needs no key, but its Data Usage Policy (updated 30 June 2026) prohibits collecting data "by automated means, whether through bots, crawlers, spiders, scripts, API polling, WebSocket capture, or otherwise" without a separate written agreement, and applies "whether or not the relevant endpoint requires authentication". It also prohibits publishing the data or building derived datasets from it. The terms name no exception for a test environment. So this project made no connection to the live or test endpoints, and `crates/synth` generates data from the published field definitions instead.

The cost of that choice is real: the values that most affect the savings cannot be measured here. The table below separates what the docs state from what is assumed. `crates/bench/src/bin/analyze.rs` measures all of the assumed values from any JSONL file, so the operator of a venue can replace them with real figures from its own feed.

## What comes from public documentation

| Item | Source |
|---|---|
| Channel names and parameters: group 1/10/100, depth 1/10/20/100, ticker interval 100/1000 ms | Subscriptions reference |
| Book payload: `[price, amount]` string pairs, `publish_id` (uint64), `timestamp` | AsyncAPI `OrderbookPayloadDoc` |
| Ticker payload: every key, including `stats` and `option_pricing` | AsyncAPI `TickerSlimSnapshot` |
| Trade payload: every key, delivered as a JSON array | AsyncAPI `PublicTrade` |
| Decimals are strings, "up to 12 fractional digits" | API reference |
| Instrument name grammar | Instrument Names page |
| Order fields and the EIP-712 signing scheme | `private/order`, Action Signing pages |

## What is assumed

| Assumption | Value | Why it matters | Effect if wrong |
|---|---|---|---|
| Book cadence | 100 ms | The docs say "periodically" without a number. 100 ms is the fastest documented ticker interval | Bytes per second scale directly with cadence |
| Unchanged books are still published | Yes | The docs do not say | **If unchanged books are already skipped, the suppression lever is worth nothing, because that saving is already in the bill** |
| Book message type | Full snapshot each time | The payload has no delta fields, and an SDK example calls it a snapshot | If deltas are already sent, the JSON baseline is smaller than modelled |
| Universe | 2 perps + 176 options | Unknown | Changes totals, and how far apart repeats sit for per-connection compression |
| Levels | Perps fill the requested depth; options 1 to 5 per side | Unknown | Sparse option books shrink both JSON and binary |
| Tick sizes and amount precision | See `UNDERLYINGS` in `crates/synth/src/lib.rs` | `tick_size` is a field the docs define, but its live values were not read | Small effect on varint widths |
| Number text | Trailing zeros trimmed, as in documented examples (`"3100.5"`) | Unknown in general | The format handles the other forms; JSON gets a little longer |
| Greek digits | 12 (the documented maximum), also run at 6 | Unknown | Long decimals are high entropy, so this moves ticker results |
| Key order | Schema order | Unknown | Binary to JSON is byte-exact for this order; real key order may differ, which changes bytes but not values |
| Message mix | See `docs/cost-model.md` | Unknown | Sets how much each channel counts |

## Regimes

Three regimes bracket the unknown market activity. **Their parameters were committed before any size was measured and were not changed afterwards.** `git log -- crates/synth/src/lib.rs` shows the commit order against `results/`.

| Parameter | calm | base | busy |
|---|---|---|---|
| Annualised volatility | 40% | 70% | 120% |
| Option requote threshold (ticks) | 4 | 3 | 2 |
| Option quote refresh per second | 0.05 | 0.2 | 0.8 |
| Perp level change chance per tick | 2% | 6% | 15% |
| Perp trades per second | 0.2 | 0.5 | 2.0 |
| Option trades per second | 0.002 | 0.01 | 0.05 |

A simulation cannot show that a real book behaves like this one. What it can do is exercise the codec with the documented shapes, and show how sensitive each saving is to activity. The size of the headers, decimals and addresses depends on the format and is fairly stable. **The delta and suppression savings depend on how often books change, so treat those as ranges, not measurements.**
