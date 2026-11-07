# Format research: what was read, and what was borrowed or rejected

This covers three public specifications. Each claim below cites the document it came from, and all were read on 2026-11-07. This is not a survey of what trading firms do in private. It covers only what these documents say.

## Sources read

| Source | What it is |
|---|---|
| [Binance SBE market data streams](https://github.com/binance/binance-spot-api-docs/blob/master/sbe-market-data-streams.md), the [stream schema `stream_1_0.xml`](https://github.com/binance/binance-spot-api-docs/blob/master/sbe/schemas/stream_1_0.xml), the [SBE FAQ](https://github.com/binance/binance-spot-api-docs/blob/master/faqs/sbe_faq.md), and the local order book section of [WebSocket streams](https://github.com/binance/binance-spot-api-docs/blob/master/web-socket-streams.md) | A large crypto venue's public binary market data |
| [FIX Simple Binary Encoding v1.0](https://github.com/FIXTradingCommunity/fix-simple-binary-encoding/tree/master/v1-0-STANDARD/doc): Introduction, Field Encoding, Message Structure, Schema Extension | The FIX Trading Community standard that Binance's schema is written in |
| [Nasdaq TotalView-ITCH 5.0](https://www.nasdaqtrader.com/content/technicalsupport/specifications/dataproducts/NQTVITCHspecification.pdf), [MoldUDP64 1.00](https://www.nasdaqtrader.com/content/technicalsupport/specifications/dataproducts/moldudp64.pdf), [SoupBinTCP](https://www.nasdaqtrader.com/content/technicalsupport/specifications/dataproducts/soupbintcp.pdf) | An equities exchange feed and its two transports |

## What each one says that matters here

**Binance SBE streams.**
- Market data arrives in WebSocket binary frames. Subscription requests and responses stay JSON in text frames, and so does the `serverShutdown` event.
- Prices and quantities are an `int64` mantissa with an `int8` exponent, and each message carries one exponent for prices and one for quantities (`priceExponent`, `qtyExponent`).
- The message header is four `uint16`s: `blockLength`, `templateId`, `schemaId` and `version`.
- Best bid/ask streams use "auto-culling": under load, an outdated event for a symbol is dropped in favour of the newer one.
- Diff-depth events carry first and last update IDs. A client keeps a local book by buffering events, fetching a snapshot, and dropping events at or below the snapshot's `lastUpdateId`.
- Schemas are versioned by `id` (breaking change) and `version` (non-breaking change), and a deprecated schema stays supported for at least 6 months.

**FIX SBE 1.0.**
- Its design goals include a "preference for fixed positions and fixed length fields, supporting direct access to data".
- The decimal type is a mantissa with an exponent, and optional fields use a reserved null value.
- "SBE messages have no defined message delimiter." The separate Simple Open Framing Header provides "an overall message size including headers to support framing" plus an encoding identifier.
- `blockLength` lets a decoder skip fields added by a later schema version.

**Nasdaq ITCH 5.0 and its transports.**
- ITCH integers are big-endian, and prices are integers with implied decimals (`Price (4)`, `Price (8)`).
- Timestamps take 6 bytes: nanoseconds since midnight.
- Each message is "variable in length based on the message type", with a one-byte type first.
- Instruments are a two-byte "stock locate code", "assigned each day, starting with a value of 1, and communicated via the Stock Directory message".
- MoldUDP64 sends each packet "only once regardless of the number of listeners". "Multiple messages may also be aggregated into a single network packet to reduce network traffic."
- A MoldUDP64 packet header is session (10 bytes), sequence number (8) and message count (2), and each message block starts with a 2-byte length. Listeners detect loss by sequence number and "re-request missed packets".
- SoupBinTCP prefixes each packet with a 2-byte length. It carries no sequence number in sequenced packets: "both client and server compute the sequence number locally by counting messages".

## What this format borrows

| Idea | From | Where it shows up |
|---|---|---|
| Binary market data, JSON control messages | Binance SBE streams | Subscribe and responses stay JSON text; data is binary (`crates/gateway`) |
| Decimal as integer mantissa plus exponent, never floats | FIX SBE, Binance, ITCH | `Dec { m, s }` keeps the scale exactly as it was in the text, so JSON round-trips byte for byte |
| One scale shared by many values | Binance `priceExponent` / `qtyExponent` | A book carries one price scale and one quantity scale (`Column`) |
| Small integer instrument IDs from a directory message | ITCH stock locate and Stock Directory | `InstrumentDef` messages; the header carries a varint ID instead of the name |
| One-byte message type first | ITCH | `type` in the message header |
| Per-message length, several messages per packet | MoldUDP64 message blocks | `len` varint; one WebSocket frame carries every message of a 100 ms tick |
| Send once, regardless of listener count | MoldUDP64 | Encode once per message in the gateway; the relay-tier cost lever |
| Sequence numbers, gap detection, snapshot recovery | Binance local book procedure, MoldUDP64 re-request | Per-stream `seq`, `Replica` gap detection, `ResyncRequest` with `FLAG_RESYNC` snapshots |
| Drop stale updates under load rather than queue them | Binance auto-culling | A lagging client gets fresh snapshots instead of a backlog |
| Version in every frame, skippable extensions | FIX SBE `blockLength`, Binance schema versioning | Magic/version byte; unknown types and trailing fields skipped by `len` |

## What this format rejects, and why

| Idea | From | Why not here |
|---|---|---|
| Fixed positions and fixed-width fields | FIX SBE design goal | SBE optimises for direct access and decode speed. This project optimises for bytes on the wire, because the bill is per GB. Varints and deltas give up random access in exchange for size. On the synthetic data, an SBE-shaped book (Binance's `DepthSnapshotStreamEvent` layout) is about half the size of JSON, while this layout is about a seventh (see README). |
| 8-byte header of four `uint16`s | FIX SBE / Binance | A 100 ms delta is often under 20 bytes, so 8 bytes of header would be a large share of it. The header here is varints: about 7 to 10 bytes in total, including sequence and timestamp. |
| Big-endian fixed integers | ITCH | Varints make byte order irrelevant. |
| Implied decimals fixed per field | ITCH `Price (4)` | Decimal places vary by instrument here, and the text form must survive exactly, so the scale travels with the data. |
| Implicit sequence numbers counted by the client | SoupBinTCP | Each client receives a filtered subset of streams, so counting received messages cannot reveal a gap in one stream. Sequence numbers are explicit and per stream. |
| Reserved null values | FIX SBE | A presence bitmask costs nothing for required fields, and cannot be confused with a real value. |
| UDP multicast | MoldUDP64 | Clients connect over the public internet through a CDN, so TCP and WebSocket are given. |

## What the length field is for

WebSocket frames and ZMQ messages already have boundaries, so a length is not needed to find the end of a single message. Here it earns its place twice. First, it makes batching possible: one frame per tick per connection, which saves the per-frame WebSocket, TLS and TCP/IP overhead (see the wire model in the README). Second, it lets an older client skip a message type it does not know. A message's bytes never depend on the frame they sit in: timestamps are relative to the previous message of the same stream, not to the frame. So the gateway encodes each message once and concatenates the same bytes into a different frame for each client.
