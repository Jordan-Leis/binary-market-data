//! Measures what the public docs do not state, from any JSONL file of
//! subscription notifications (one per line). Run on the committed SYNTHETIC
//! sample by `make reproduce`; meant to be run by a venue on its own feed.
//!
//!   analyze <file.jsonl> [out.json]

use serde_json::{json, Value};
use std::collections::HashMap;
use std::io::BufRead;

#[derive(Default)]
struct Fam {
    messages: u64,
    bytes: u64,
    numeric_values: u64,
    string_values: u64,
}

fn walk(v: &Value, f: &mut Fam) {
    match v {
        Value::Number(_) => f.numeric_values += 1,
        Value::String(_) => f.string_values += 1,
        Value::Array(a) => a.iter().for_each(|x| walk(x, f)),
        Value::Object(o) => o.values().for_each(|x| walk(x, f)),
        _ => {}
    }
}

fn median(v: &mut [i64]) -> Option<i64> {
    v.sort_unstable();
    v.get(v.len() / 2).copied()
}

fn main() {
    let mut args = std::env::args().skip(1);
    let path = args.next().expect("usage: analyze <file.jsonl> [out.json]");
    let out = args.next();
    let file = std::io::BufReader::new(std::fs::File::open(&path).expect("open input"));
    let mut fams: HashMap<String, Fam> = HashMap::new();
    let mut last_ts: HashMap<String, i64> = HashMap::new();
    let mut gaps: HashMap<String, Vec<i64>> = HashMap::new();
    let mut last_levels: HashMap<String, String> = HashMap::new();
    let (mut books, mut unchanged, mut publish_steps) = (0u64, 0u64, Vec::new());
    let mut last_pub: HashMap<String, i64> = HashMap::new();
    let mut levels_per_side = Vec::new();
    for line in file.lines() {
        let line = line.expect("read line");
        let Ok(v) = serde_json::from_str::<Value>(&line) else { continue };
        let Some(channel) = v.pointer("/params/channel").and_then(Value::as_str) else { continue };
        let fam_name = channel.split('.').next().unwrap_or("").to_owned();
        let f = fams.entry(fam_name.clone()).or_default();
        f.messages += 1;
        f.bytes += line.len() as u64;
        let data = &v["params"]["data"];
        walk(data, f);
        let ts = data.get("timestamp").and_then(Value::as_i64).or_else(|| data.pointer("/0/timestamp").and_then(Value::as_i64));
        if let Some(ts) = ts {
            if let Some(prev) = last_ts.insert(channel.to_owned(), ts) {
                gaps.entry(fam_name.clone()).or_default().push(ts - prev);
            }
        }
        if fam_name == "orderbook" {
            books += 1;
            let lv = format!("{}{}", data["bids"], data["asks"]);
            if last_levels.get(channel).is_some_and(|p| *p == lv) {
                unchanged += 1;
            }
            last_levels.insert(channel.to_owned(), lv);
            if let Some(p) = data.get("publish_id").and_then(Value::as_i64) {
                if let Some(prev) = last_pub.insert(channel.to_owned(), p) {
                    publish_steps.push(p - prev);
                }
            }
            for side in ["bids", "asks"] {
                levels_per_side.push(data[side].as_array().map_or(0, Vec::len) as i64);
            }
        }
    }
    let mut families = serde_json::Map::new();
    for (k, f) in &fams {
        let mut g = gaps.get(k).cloned().unwrap_or_default();
        families.insert(k.clone(), json!({
            "messages": f.messages,
            "mean_bytes": f.bytes as f64 / f.messages.max(1) as f64,
            "string_values": f.string_values,
            "numeric_values": f.numeric_values,
            "median_ms_between_messages_per_channel": median(&mut g),
        }));
    }
    let contiguous = publish_steps.iter().filter(|&&d| d == 1).count();
    let doc = json!({
        "input": path,
        "families": families,
        "orderbook": {
            "messages": books,
            "unchanged_vs_previous_on_same_channel": unchanged as f64 / books.max(1) as f64,
            "every_message_carries_full_levels": "yes by schema: bids and asks are arrays of [price, amount]",
            "median_levels_per_side": median(&mut levels_per_side),
            "publish_id_step_is_1_fraction": contiguous as f64 / publish_steps.len().max(1) as f64,
        },
    });
    let text = serde_json::to_string_pretty(&doc).unwrap() + "\n";
    match out {
        Some(o) => std::fs::write(o, text).expect("write output"),
        None => print!("{text}"),
    }
}
