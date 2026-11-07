//! Signed orders. The public docs define an EIP-712 signature over an `Action`
//! struct, and the exchange re-derives the signed words from the request. So
//! a binary order is correct only if it rebuilds those words exactly.
//!
//! What the signature covers:
//! `subaccountId, nonce, module, keccak(data), expiry, owner, signer`, where
//! `data` is seven ABI words: `asset, sub_id, limit_price, amount, max_fee,
//! recipient_id, is_bid`. Prices are 18-decimal fixed point.
//!
//! Three of those are not carried in the order. `module` is a constant,
//! `asset` and `sub_id` come from the instrument, and `owner` comes from the
//! subaccount. Everything else is carried exactly: decimals as `Dec`, the
//! signature as 65 raw bytes in place of 132 characters of hex.

use crate::dec::Dec;
use crate::error::Error;
use crate::json::{addr_text, parse_addr, parse_hex_into, push_hex, push_json_str};
use crate::msg::*;
use k256::ecdsa::{RecoveryId, Signature, SigningKey, VerifyingKey};
use serde::Deserialize;
use sha3::{Digest, Keccak256};
use std::fmt::Write;

pub const TRADE_MODULE: [u8; 20] = hex20("B8D20c2B7a1Ad2EE33Bc50eF10876eD3035b5e7b");
/// `verifyingContract` from the public signing docs.
pub const MATCHING: [u8; 20] = hex20("eB8d770ec18DB98Db922E9D83260A585b9F0DeAD");
pub const CHAIN_MAINNET: u64 = 1;
pub const CHAIN_TESTNET: u64 = 11155111;

const fn nib(c: u8) -> u8 {
    match c {
        b'0'..=b'9' => c - b'0',
        b'a'..=b'f' => c - b'a' + 10,
        b'A'..=b'F' => c - b'A' + 10,
        _ => panic!("bad hex"),
    }
}

const fn hex20(s: &str) -> [u8; 20] {
    let b = s.as_bytes();
    let mut out = [0u8; 20];
    let mut i = 0;
    while i < 20 {
        out[i] = (nib(b[2 * i]) << 4) | nib(b[2 * i + 1]);
        i += 1;
    }
    out
}

fn keccak(data: &[u8]) -> [u8; 32] {
    Keccak256::digest(data).into()
}

fn word_u128(v: u128) -> [u8; 32] {
    let mut w = [0u8; 32];
    w[16..].copy_from_slice(&v.to_be_bytes());
    w
}

/// `int256` two's complement.
fn word_i128(v: i128) -> [u8; 32] {
    let mut w = [if v < 0 { 0xff } else { 0 }; 32];
    w[16..].copy_from_slice(&v.to_be_bytes());
    w
}

fn word_addr(a: &[u8; 20]) -> [u8; 32] {
    let mut w = [0u8; 32];
    w[12..].copy_from_slice(a);
    w
}

pub fn action_typehash() -> [u8; 32] {
    keccak(b"Action(uint256 subaccountId,uint256 nonce,address module,bytes data,uint256 expiry,address owner,address signer)")
}

pub fn domain_separator(chain_id: u64) -> [u8; 32] {
    let mut enc = Vec::with_capacity(160);
    enc.extend_from_slice(&keccak(b"EIP712Domain(string name,string version,uint256 chainId,address verifyingContract)"));
    enc.extend_from_slice(&keccak(b"Matching"));
    enc.extend_from_slice(&keccak(b"1.0"));
    enc.extend_from_slice(&word_u128(chain_id as u128));
    enc.extend_from_slice(&word_addr(&MATCHING));
    keccak(&enc)
}

/// What the gateway looks up, because it is signed but not sent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SignedContext {
    pub asset: [u8; 20],
    pub sub_id: u128,
    pub owner: [u8; 20],
    pub chain_id: u64,
}

fn e18(d: &Dec) -> Result<i128, Error> {
    // The docs: more than 12 fractional digits can never verify.
    if d.s > 12 {
        return Err(Error::BadScale);
    }
    d.to_e18().ok_or(Error::DecimalOverflow)
}

/// The seven words of the order `data`.
pub fn encode_action_data(o: &NewOrder, ctx: &SignedContext) -> Result<[u8; 224], Error> {
    let recipient = u128::try_from(o.subaccount_id).map_err(|_| Error::BadEnum)?;
    let words = [
        word_addr(&ctx.asset),
        word_u128(ctx.sub_id),
        word_i128(e18(&o.limit_price)?),
        word_i128(e18(&o.amount)?),
        word_i128(e18(&o.max_fee)?),
        word_u128(recipient),
        word_u128(o.is_buy as u128),
    ];
    let mut out = [0u8; 224];
    for (i, w) in words.iter().enumerate() {
        out[i * 32..(i + 1) * 32].copy_from_slice(w);
    }
    Ok(out)
}

/// The digest the signer signs: `keccak(0x1901 || domain || actionHash)`.
pub fn order_digest(o: &NewOrder, ctx: &SignedContext) -> Result<[u8; 32], Error> {
    let data = encode_action_data(o, ctx)?;
    let subaccount = u128::try_from(o.subaccount_id).map_err(|_| Error::BadEnum)?;
    let expiry = u128::try_from(o.signature_expiry_sec).map_err(|_| Error::BadEnum)?;
    let mut enc = Vec::with_capacity(256);
    enc.extend_from_slice(&action_typehash());
    enc.extend_from_slice(&word_u128(subaccount));
    enc.extend_from_slice(&word_u128(o.nonce));
    enc.extend_from_slice(&word_addr(&TRADE_MODULE));
    enc.extend_from_slice(&keccak(&data));
    enc.extend_from_slice(&word_u128(expiry));
    enc.extend_from_slice(&word_addr(&ctx.owner));
    enc.extend_from_slice(&word_addr(o.signer.bytes()));
    let action_hash = keccak(&enc);
    let mut msg = Vec::with_capacity(66);
    msg.extend_from_slice(&[0x19, 0x01]);
    msg.extend_from_slice(&domain_separator(ctx.chain_id));
    msg.extend_from_slice(&action_hash);
    Ok(keccak(&msg))
}

pub fn address_of(key: &VerifyingKey) -> [u8; 20] {
    let point = key.to_encoded_point(false);
    let hash = keccak(&point.as_bytes()[1..]);
    let mut a = [0u8; 20];
    a.copy_from_slice(&hash[12..]);
    a
}

/// `r || s || v` with `v` in {27, 28}, the form signing libraries emit.
pub fn sign_digest(key: &SigningKey, digest: &[u8; 32]) -> Result<[u8; 65], Error> {
    let (sig, rec) = key.sign_prehash_recoverable(digest).map_err(|_| Error::BadSignature)?;
    let mut out = [0u8; 65];
    out[..64].copy_from_slice(&sig.to_bytes());
    out[64] = rec.to_byte() + 27;
    Ok(out)
}

pub fn recover_signer(digest: &[u8; 32], sig: &[u8; 65]) -> Result<[u8; 20], Error> {
    let s = Signature::from_slice(&sig[..64]).map_err(|_| Error::BadSignature)?;
    let v = sig[64].checked_sub(27).ok_or(Error::BadSignature)?;
    let rec = RecoveryId::from_byte(v).ok_or(Error::BadSignature)?;
    let key = VerifyingKey::recover_from_prehash(digest, &s, rec).map_err(|_| Error::BadSignature)?;
    Ok(address_of(&key))
}

/// True if the signature recovers to the order's declared signer.
pub fn verify_order(o: &NewOrder, ctx: &SignedContext) -> Result<bool, Error> {
    let digest = order_digest(o, ctx)?;
    Ok(recover_signer(&digest, &o.signature)? == *o.signer.bytes())
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RpcJson<'a> {
    id: u64,
    #[serde(borrow)]
    method: &'a str,
    #[serde(borrow)]
    params: OrderJson<'a>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct OrderJson<'a> {
    subaccount_id: i64,
    #[serde(borrow)]
    instrument_name: &'a str,
    #[serde(borrow)]
    direction: &'a str,
    #[serde(borrow)]
    limit_price: &'a str,
    #[serde(borrow)]
    amount: &'a str,
    #[serde(borrow)]
    max_fee: &'a str,
    #[serde(borrow)]
    nonce: &'a str,
    #[serde(borrow)]
    signer: &'a str,
    #[serde(borrow)]
    signature: &'a str,
    signature_expiry_sec: i64,
    #[serde(borrow, default)]
    order_type: Option<&'a str>,
    #[serde(borrow, default)]
    time_in_force: Option<&'a str>,
    #[serde(default)]
    reduce_only: Option<bool>,
    #[serde(default)]
    mmp: Option<bool>,
    // Free text may contain JSON escapes, so it cannot always borrow.
    #[serde(borrow, default)]
    label: Option<std::borrow::Cow<'a, str>>,
}

/// Parses a `private/order` JSON-RPC request. Trigger, algo and fee-extension
/// params are outside this prototype and are refused, so those requests stay
/// on the JSON path.
pub fn parse_order_json(json: &str) -> Result<(String, NewOrder), Error> {
    let r: RpcJson<'_> = serde_json::from_str(json).map_err(|_| Error::NotRepresentable)?;
    if r.method != "private/order" {
        return Err(Error::NotRepresentable);
    }
    let p = r.params;
    let dec = |s: &str| Dec::parse(s).map_err(|_| Error::NotRepresentable);
    let sig_hex = p.signature.strip_prefix("0x").ok_or(Error::NotRepresentable)?;
    if sig_hex.bytes().any(|c| c.is_ascii_uppercase()) {
        return Err(Error::NotRepresentable);
    }
    let mut signature = [0u8; 65];
    parse_hex_into(sig_hex.as_bytes(), &mut signature).map_err(|_| Error::NotRepresentable)?;
    // The nonce is a decimal string, and text must survive exactly.
    let nonce: u128 = p.nonce.parse().map_err(|_| Error::NotRepresentable)?;
    if nonce.to_string() != p.nonce {
        return Err(Error::NotRepresentable);
    }
    if p.label.as_ref().is_some_and(|l| l.len() > MAX_TEXT) {
        return Err(Error::NotRepresentable);
    }
    let order = NewOrder {
        rpc_id: r.id,
        subaccount_id: p.subaccount_id,
        is_buy: match p.direction {
            "buy" => true,
            "sell" => false,
            _ => return Err(Error::NotRepresentable),
        },
        limit_price: dec(p.limit_price)?,
        amount: dec(p.amount)?,
        max_fee: dec(p.max_fee)?,
        nonce,
        signer: parse_addr(p.signer)?,
        signature,
        signature_expiry_sec: p.signature_expiry_sec,
        order_type: match p.order_type {
            None => None,
            Some("limit") => Some(OrderType::Limit),
            Some("market") => Some(OrderType::Market),
            _ => return Err(Error::NotRepresentable),
        },
        time_in_force: match p.time_in_force {
            None => None,
            Some("gtc") => Some(Tif::Gtc),
            Some("post_only") => Some(Tif::PostOnly),
            Some("fok") => Some(Tif::Fok),
            Some("ioc") => Some(Tif::Ioc),
            _ => return Err(Error::NotRepresentable),
        },
        reduce_only: p.reduce_only,
        mmp: p.mmp,
        label: p.label.map(|l| l.into_owned()),
    };
    Ok((p.instrument_name.to_owned(), order))
}

pub fn render_order_json(instrument: &str, o: &NewOrder) -> String {
    let mut s = String::with_capacity(512);
    let _ = write!(
        s,
        r#"{{"id":{},"method":"private/order","params":{{"subaccount_id":{},"instrument_name":"#,
        o.rpc_id, o.subaccount_id
    );
    push_json_str(&mut s, instrument);
    let _ = write!(s, r#","direction":"{}","limit_price":"{}","amount":"{}","max_fee":"{}","nonce":"{}","signer":"{}","signature":"0x"#,
        if o.is_buy { "buy" } else { "sell" },
        o.limit_price.to_text(), o.amount.to_text(), o.max_fee.to_text(), o.nonce, addr_text(&o.signer));
    push_hex(&mut s, &o.signature);
    let _ = write!(s, r#"","signature_expiry_sec":{}"#, o.signature_expiry_sec);
    if let Some(t) = o.order_type {
        let _ = write!(s, r#","order_type":"{}""#, match t { OrderType::Limit => "limit", OrderType::Market => "market" });
    }
    if let Some(t) = o.time_in_force {
        let v = match t {
            Tif::Gtc => "gtc",
            Tif::PostOnly => "post_only",
            Tif::Fok => "fok",
            Tif::Ioc => "ioc",
        };
        let _ = write!(s, r#","time_in_force":"{v}""#);
    }
    if let Some(v) = o.reduce_only {
        let _ = write!(s, r#","reduce_only":{v}"#);
    }
    if let Some(v) = o.mmp {
        let _ = write!(s, r#","mmp":{v}"#);
    }
    if let Some(l) = &o.label {
        s.push_str(r#","label":"#);
        push_json_str(&mut s, l);
    }
    s.push_str("}}");
    s
}
