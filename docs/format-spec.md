# mdbin v1 wire format

All integers are LEB128 varints unless a width is given. A varint must be minimal: a longer encoding of the same value is rejected, so every value has exactly one wire form. `zigzag` maps signed values to unsigned ones (0, -1, 1, -2, ...).

## Frame

```
frame   := 0xB1 message*                     high nibble 0xB = magic, low nibble 1 = version
message := len type:u8 flags:u8 instrument seq ts:zigzag body
```

| Field | Meaning |
|---|---|
| `len` | Bytes after `len`. Used for batching and for skipping unknown types. Trailing bytes inside `len` are ignored, which lets a later minor version append fields. |
| `type` | See the table below. |
| `flags` | `0x01` RESYNC: this snapshot answers a resync request. |
| `instrument` | ID from an `InstrumentDef` message. 0 when the message has no instrument. |
| `seq` | Contiguous per stream. A stream is (instrument, family, channel). |
| `ts` | Snapshot: absolute ms since the epoch. Delta: ms since the previous message on the stream. |

A message's bytes do not depend on its frame, so one encoding can be shared by every client.

## Types

| Type | Name | Body |
|---|---|---|
| 1 | BookSnapshot | `chan:u8 publish_id price_col:u8 qty_col:u8 n_bids n_asks prices... qtys...` |
| 2 | BookDelta | `chan:u8 publish_id_delta:zigzag price_col:u8 qty_col:u8 n_ops op...` |
| 3 | TickerSnapshot | `chan:u8 mask null_mask t_offset:zigzag value...` |
| 4 | TickerDelta | Same layout, with only the changed fields in `mask` |
| 5 | Trades | `chan:u8 n trade...` |
| 6 | InstrumentDef | `len name` |
| 7 | ResyncRequest | `family:u8 chan:u8` (client to server) |
| 8 | RawJson | The original JSON text, for anything this codec cannot carry exactly |
| 16 | NewOrder | See "Orders" |
| 17 | OrderAck | `rpc_id accepted:u8 order_id:16 reason` |

`chan` for books is `group_index << 2 | depth_index`, over groups (1, 10, 100) and depths (1, 10, 20, 100). For tickers it is the interval index, over (100, 1000) ms.

## Decimals

A value is `Dec { m: mantissa, s: fractional digits }`, taken from the JSON text exactly: `"1.50"` is (150, 2) and `"1.5"` is (15, 1). Only `-?(0|[1-9][0-9]*)(\.[0-9]+)?` with at most 15 fractional digits and 36 digits in total is accepted. Anything else falls back to RawJson.

On its own, a decimal is written as `varint(zigzag(m) << 4 | s)`.

A column of values (book prices, book quantities) shares a scale. The column byte is `mode << 4 | scale`:

| Mode | Meaning |
|---|---|
| 0 Fixed | Every value has exactly `scale` digits. Mantissas are stored at that scale. |
| 1 Trim | Every value has trailing zeros trimmed. Mantissas are stored at `scale` and trimmed again on decode. |
| 2 PerValue | Mixed forms. Each value is a self-describing decimal. |

Book snapshot prices are deltas: down the bid side from 0, then up the ask side from the best bid. Book delta ops are `varint(zigzag(price - previous op price) << 2 | remove << 1 | is_ask)`, followed by the quantity unless the op removes a level.

## Ticker fields

Bit positions in `mask`: 0 `a`, 1 `A`, 2 `b`, 3 `B`, 4 `M`, 5 `I`, 6 `f`, 7 `minp`, 8 `maxp`, 9-16 `stats` (`c h l n oi p pr v`), 17-27 `option_pricing` (`ai bi d df f g i m r t v`). Bit 28 of `null_mask` means `option_pricing` is null.

## Trades

Per trade: one bits byte (bit 0 buy, bit 1 maker, bit 2 wallet is EIP-55), nine decimals, then `subaccount_id`, `timestamp`, `trade_id`, `quote_id` and `rfq_id`, then a 20-byte wallet. An ID is tagged: 0 null, 1 raw 16-byte UUID, 2 text.

## Orders

`rpc_id bits subaccount_id limit_price amount max_fee nonce:u128 signer:20 signature:65 expiry [label]`

The EIP-712 signature covers `subaccountId, nonce, module, keccak(data), expiry, owner, signer`. `data` is `asset, sub_id, limit_price, amount, max_fee, recipient_id, is_bid` as 18-decimal words. The binary order carries every signed field it can exactly, and the signature as 65 raw bytes. The gateway supplies `module` (a constant), `asset` and `sub_id` (from the instrument), and `owner` (from the subaccount). `crates/mdbin/tests/orders.rs` checks the published `ACTION_TYPEHASH` and domain separators, a JSON to binary to JSON round trip that still verifies, and that changing any signed field breaks verification.

## Decoder limits

Frames are at most 1 MiB, books at most 1,024 levels per side, trades at most 1,024 per message, and text at most 256 bytes. Every count is checked against the bytes remaining before anything is allocated.
