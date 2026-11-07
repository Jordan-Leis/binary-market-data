# Cost model

```
monthly bill  ~  subscribers x messages/s x bytes/message x seconds x $/GB
```

`costmodel/cost_model.py` takes an illustrative bill ($1k, $10k or $30k a month) and assumes all of it is AWS internet egress plus NLB processing for market data. That assumption makes every saving an upper bound. The script finds the monthly traffic that would produce that bill at list price, then prices the same traffic under each lever.

## Prices (`costmodel/pricing.toml`, checked 2026-11-07)

- **AWS data transfer out, us-east-1:** $0.09/GB for the first 10 TB, $0.085 for the next 40 TB, $0.07 for the next 100 TB, and $0.05 after that. The first 100 GB are free.
- **AWS data transfer in:** $0.00/GB. This is why orders, which flow inward, barely affect the transfer bill.
- **NLB:** $0.0225 per hour, plus $0.006 per NLCU-hour. For TCP, one NLCU is 1 GB of processed bytes per hour (or 800 new connections per second, or 100,000 active connections, whichever is highest).
- **Direct Connect:** $0.02/GB out, plus port hours ($0.30/h for 1G, $2.25/h for 10G). It also needs an interconnect with the CDN. Cloudflare's is listed as Enterprise-only, with no public price, so that cost is **unknown**.
- **CloudFront:** AWS to CloudFront is $0.00/GB. CloudFront's US list tiers start at $0.085/GB. It supports WebSockets on any distribution. Flat-rate plans include up to 50 TB, and whether WebSocket traffic fits those plans is **unknown**.
- **Hetzner:** overage of €1 ($1.20 by Hetzner's own figure) per TB beyond the included traffic. Included traffic is 20 TB in the EU, 1 to 8 TB in the US (depending on plan), and unlimited on 1 Gbit dedicated servers.
- **Others:** Oracle (10 TB free, then $0.0085/GB), DigitalOcean ($0.01/GiB over the pooled allowance), GCP ($0.08 to $0.12/GiB to North America).

These are list prices. Negotiated discounts would lower every AWS figure.

## Levers

| Lever | What changes | Evidence |
|---|---|---|
| Binary snapshots, deltas, skipping unchanged books, conflation | Bytes per message | Byte ratios measured on SYNTHETIC data (`results/sizes.json`). Skipping unchanged books saves nothing if the venue already skips them. |
| permessage-deflate, zstd with a dictionary | Bytes per message | Measured on the same SYNTHETIC data. permessage-deflate's per-connection CPU and memory come from `results/timing.json`. |
| Direct Connect | $/GB | Modelled. Interconnect and colocation costs unknown. |
| CloudFront | $/GB, and replaces the current CDN | Modelled. WebSocket request charges not modelled. |
| Relay tier outside AWS | Copies leaving AWS: one per relay instead of one per subscriber | Modelled. Relays on Hetzner, each carrying up to 100 TB a month (an assumption, about 30% of a 1 Gbit/s port), with at least 2 for redundancy and US included traffic at the low end (1 TB). |
| Binary orders | Inbound bytes | Inbound transfer is free, so only NLB processing changes. 1,000 orders/s is an assumption. |

## Not modelled

- Compute for relays beyond the listed server price.
- Engineering and operating cost.
- Latency added by an extra hop.
- The CDN's own bill.
- Request charges.
- Cross-AZ traffic.
- Any traffic other than market data.
