### Bytes per second, whole-feed subscriber (SYNTHETIC data, base regime)

178 instruments, 600 ticks of 100 ms after a 50-tick warm-up. Ratio is payload bytes relative to JSON. Range is across the calm, base and busy regimes.

| Encoding | KB/s | vs JSON | Saving | Range across regimes | Wire, modelled |
|---|---:|---:|---:|---|---:|
| JSON as sent today | 659.3 | 1.000 | 0% | 1.000 to 1.000 | 1.000 |
| JSON + permessage-deflate, no context takeover | 423.3 | 0.642 | 36% | 0.641 to 0.642 | 0.709 |
| JSON + zstd with a trained dictionary | 182.4 | 0.277 | 72% | 0.262 to 0.279 | 0.408 |
| JSON + permessage-deflate, context takeover | 161.7 | 0.245 | 75% | 0.245 to 0.247 | 0.383 |
| Binary, full snapshots | 97.1 | 0.147 | 85% | 0.147 to 0.148 | 0.125 |
| Binary + deltas (exact round trip) | 46.2 | 0.070 | 93% | 0.062 to 0.087 | 0.059 |
| Binary + deltas, unchanged books skipped | 26.1 | 0.040 | 96% | 0.028 to 0.064 | 0.034 |
| ... and books conflated to 1 s | 20.0 | 0.030 | 97% | 0.026 to 0.035 | 0.026 |
| Binary + deltas + permessage-deflate | 22.5 | 0.034 | 97% | 0.026 to 0.050 | 0.029 |

- Unchanged books between consecutive 100 ms publishes (an assumption-driven SYNTHETIC output): calm 95%, base 85%, busy 63%.
- An SBE-style fixed-width layout (Binance `DepthSnapshotStreamEvent` shape) would be 0.498 of JSON for books; the varint layout here is 0.134.
- One-instrument subscribers, wire model: binary + deltas 0.225 of JSON, JSON + permessage-deflate 0.283. Small frames are dominated by TCP/TLS overhead, so the saving shrinks.
- Joining the whole binary feed costs 34.7 KB once per connection (directory and snapshots).
- Raw-JSON fallbacks across all runs: 0. Every synthetic message round-tripped exactly.
- Signed order requests: 491 bytes as JSON, 121 as binary; the signature goes from 134 characters of hex to 65 bytes. Requests are inbound, which AWS does not charge for; responses were not measured.

### CPU and memory (MEASURED on one machine, SYNTHETIC inputs)

Intel(R) Core(TM) i7-9700K CPU @ 3.60GHz, 8 cores, WSL2 kernel 6.18.54.1-microsoft-standard-WSL2, load average 1.15 2.00 2.53 at start. These are serialization and copy costs, not network latency.

| Per message | CPU ns |
|---|---:|
| Parse JSON into a generic value (what a JSON client pays) | 2,847 |
| Transcode JSON to binary + deltas (gateway, once per message) | 3,788 |
| Decode binary and apply to local state (client) | 270 |
| Decode, apply and rebuild the exact JSON text | 799 |
| permessage-deflate, per connection | 13,398 |
| zstd with dictionary, once per message | 1,914 |
| Verify one order signature (secp256k1 recover, pure Rust) | 175,487 |

| Fan-out, CPU per connection per second of feed | microseconds |
|---|---:|
| Binary: transcode once, copy shared bytes | 44.3 |
| JSON as is: copy shared bytes | 116.6 |
| permessage-deflate without context: compress once, copy | 375.6 |
| permessage-deflate with context: compress per connection | 25,512.4 |

Per-connection compressor state, live heap after traffic: 376 KiB for permessage-deflate with context. Binary encode-once keeps no per-connection codec state.

### Redis and Dragonfly pub/sub (MEASURED on loopback)

| Server | Byte-exact (7 hostile payloads, PUBLISH and SET/GET) | p50 / p99 publish-to-receive, 2000 msg/s, one subscriber | Pipelined ceiling, binary msgs/s, one subscriber |
|---|---|---|---:|
| Dragonfly v2.1 | yes, all | 171 / 860 us | 214,377 |
| Redis 7.0.15 | yes, all | 186 / 926 us | 676,172 |

The synthetic whole feed is 1,960 messages per second.

### Decoder mutation testing

2,000,000 seeded mutations (byte flips, truncation, junk insertion, random frames), each applied after a valid snapshot frame; not coverage-guided: 0 panics.

### Monthly cost by lever (MODEL)

List prices checked 2026-11-07 are an upper bound. Bills are illustrative. Encoding ratios are from the SYNTHETIC base regime.

| Lever | $1,000/month | $10,000/month | $30,000/month |
|---|---:|---:|---:|
| Implied traffic | 10 TB | 118 TB | 455 TB |
| Today: JSON over AWS egress | $1,000 | $10,000 | $30,000 |
| Binary, full snapshots | $154 | $1,677 | $6,045 |
| Binary + deltas (exact) | $77 | $818 | $3,028 |
| + skip unchanged books (if sent today) | $47 | $467 | $1,740 |
| + conflate books to 1 s (opt-in) | $38 | $359 | $1,348 |
| JSON + permessage-deflate | $251 | $2,752 | $9,515 |
| JSON + zstd dictionary | $282 | $3,097 | $10,627 |
| Binary + deltas, one-instrument subscribers (wire model) | $231 | $2,529 | $8,795 |
| JSON + permessage-deflate, one-instrument subscribers (wire model) | $289 | $3,170 | $10,860 |
| Direct Connect to the CDN | $504 + unknown | $3,373 + unknown | $13,776 + unknown |
| CloudFront instead of current CDN | $957 | $9,056 | $25,601 |
| Relay tier outside AWS (JSON) | $431 | $561 | $1,583 |
| Binary + deltas AND relay tier | $133 | $141 | $169 |
| Relay tier (JSON), feed 10x larger | $3,110 | $3,239 | $7,762 |
| Binary + deltas AND relay tier, feed 10x larger | $329 | $336 | $365 |
| Binary orders | $995 | $9,995 | $29,995 |
