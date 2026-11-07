//! A binary order must rebuild exactly what the signature covers. The constants
//! here are the published ones from the public signing docs.

use k256::ecdsa::SigningKey;
use mdbin::dec::Dec;
use mdbin::json::eip55;
use mdbin::msg::*;
use mdbin::order::*;

fn hex32(s: &str) -> [u8; 32] {
    let mut b = [0u8; 32];
    mdbin::json::parse_hex_into(s.as_bytes(), &mut b).unwrap();
    b
}

#[test]
fn signing_constants_match_the_published_values() {
    assert_eq!(action_typehash(), hex32("4d7a9f27c403ff9c0f19bce61d76d82f9aa29f8d6d4b0c5474607d9770d1af17"));
    assert_eq!(domain_separator(CHAIN_MAINNET), hex32("da616dfabb88681b08e1592820a41d55ddc62d68de110e327ae99d734506fe19"));
    assert_eq!(domain_separator(CHAIN_TESTNET), hex32("24d674cd5f2b9d564691c51e9d88f649b99246a2244dd74ce27b96578d773e85"));
}

#[test]
fn eip55_matches_the_reference_vector() {
    // Test vector from EIP-55 itself.
    let mut a = [0u8; 20];
    mdbin::json::parse_hex_into(b"5aaeb6053f3e94c9b9a09f33669435e7ef1beaed", &mut a).unwrap();
    assert_eq!(eip55(&a), "5aAeb6053F3E94C9b9A09f33669435E7Ef1BeAed");
}

/// Test key only. It controls nothing.
fn key() -> SigningKey {
    SigningKey::from_slice(&[0x42u8; 32]).unwrap()
}

fn ctx(owner: [u8; 20]) -> SignedContext {
    SignedContext { asset: [0xAB; 20], sub_id: 39614082545622357598554435968, owner, chain_id: CHAIN_TESTNET }
}

fn signed_order() -> (NewOrder, SignedContext) {
    let k = key();
    let addr = address_of(k.verifying_key());
    let c = ctx(addr);
    let mut o = NewOrder {
        rpc_id: 42,
        subaccount_id: 9,
        is_buy: true,
        limit_price: Dec::parse("310.125").unwrap(),
        amount: Dec::parse("1.5").unwrap(),
        max_fee: Dec::parse("0.01").unwrap(),
        nonce: 1720512000000123456,
        signer: Addr::Checksummed(addr),
        signature: [0; 65],
        signature_expiry_sec: 1720512300,
        order_type: Some(OrderType::Limit),
        time_in_force: Some(Tif::Gtc),
        reduce_only: None,
        mmp: Some(true),
        label: Some("say \"hi\"\n".to_owned()),
    };
    o.signature = sign_digest(&k, &order_digest(&o, &c).unwrap()).unwrap();
    (o, c)
}

fn through_binary(o: &NewOrder) -> NewOrder {
    let h = Header { ty: TYPE_NEW_ORDER, instrument: 3, ..Default::default() };
    let f = encode_frame(&h, &Body::NewOrder(Box::new(o.clone()))).unwrap();
    let m = decode_frame(&f).unwrap();
    match &m[0].body {
        Body::NewOrder(d) => (**d).clone(),
        other => panic!("{other:?}"),
    }
}

#[test]
fn signature_survives_json_to_binary_to_json() {
    let (o, c) = signed_order();
    let json = render_order_json("ETH-20260626-3000-C", &o);

    let (name, parsed) = parse_order_json(&json).unwrap();
    assert_eq!(name, "ETH-20260626-3000-C");
    assert_eq!(parsed, o);

    let decoded = through_binary(&parsed);
    assert_eq!(decoded, o);
    assert_eq!(order_digest(&decoded, &c).unwrap(), order_digest(&o, &c).unwrap());
    assert!(verify_order(&decoded, &c).unwrap());

    assert_eq!(render_order_json(&name, &decoded), json);
}

#[test]
fn raw_signature_replaces_hex() {
    let (o, _) = signed_order();
    let json = render_order_json("ETH-PERP", &o);
    let h = Header { ty: TYPE_NEW_ORDER, instrument: 3, ..Default::default() };
    let bin = encode_frame(&h, &Body::NewOrder(Box::new(o.clone()))).unwrap();
    assert!(json.contains("\"signature\":\"0x") && json.len() > 300);
    assert!(bin.windows(65).any(|w| w == o.signature));
    assert!(bin.len() < json.len() / 2);
}

#[test]
fn changing_any_signed_field_breaks_verification() {
    let (o, c) = signed_order();
    let d = |s: &str| Dec::parse(s).unwrap();
    type Tamper = fn(&mut NewOrder, &mut SignedContext);
    let cases: Vec<(&str, Tamper)> = vec![
        ("subaccount_id", |o, _| o.subaccount_id += 1),
        ("nonce", |o, _| o.nonce += 1),
        ("direction", |o, _| o.is_buy = !o.is_buy),
        ("limit_price", |o, _| o.limit_price = Dec::parse("310.126").unwrap()),
        ("amount", |o, _| o.amount = Dec::parse("1.6").unwrap()),
        ("max_fee", |o, _| o.max_fee = Dec::parse("0.02").unwrap()),
        ("expiry", |o, _| o.signature_expiry_sec += 1),
        ("asset", |_, c| c.asset[0] ^= 1),
        ("sub_id", |_, c| c.sub_id += 1),
        ("owner", |_, c| c.owner[0] ^= 1),
        ("chain", |_, c| c.chain_id = CHAIN_MAINNET),
    ];
    for (name, tamper) in cases {
        let (mut t, mut tc) = (o.clone(), c);
        tamper(&mut t, &mut tc);
        assert!(!verify_order(&t, &tc).unwrap_or(false), "{name} is not covered");
    }
    let mut t = o.clone();
    t.signature[10] ^= 1;
    assert!(!verify_order(&t, &c).unwrap_or(false), "signature bit flip");

    // Fields outside the signature can change without breaking it, which is
    // exactly why the gateway must carry them separately and not trust them.
    let mut t = o.clone();
    t.label = None;
    t.time_in_force = Some(Tif::Ioc);
    assert!(verify_order(&t, &c).unwrap());

    // Text form does not change the signed word.
    let mut t = o.clone();
    t.amount = d("1.50");
    assert!(verify_order(&t, &c).unwrap());
}

#[test]
fn the_digest_uses_text_not_floats() {
    // Why no floats: this price survives as text but not as an f64.
    let text = "3100.123456789012";
    let via_float = format!("{:.12}", text.parse::<f64>().unwrap() + 1e-12);
    assert_ne!(Dec::parse(text).unwrap().to_e18(), Dec::parse(&via_float).unwrap().to_e18());
    assert_eq!(Dec::parse(text).unwrap().to_e18(), Some(3100123456789012000000));
}

#[test]
fn more_than_twelve_fractional_digits_is_refused_not_truncated() {
    let (mut o, c) = signed_order();
    o.limit_price = Dec::parse("1.1234567890123").unwrap();
    assert_eq!(order_digest(&o, &c), Err(mdbin::Error::BadScale));
}

#[test]
fn unsupported_order_params_stay_on_the_json_path() {
    let (o, _) = signed_order();
    let json = render_order_json("ETH-PERP", &o).replace("\"mmp\":true", "\"trigger_price\":\"3000\"");
    assert_eq!(parse_order_json(&json).unwrap_err(), mdbin::Error::NotRepresentable);
}

#[test]
fn negative_and_huge_ids_cannot_reach_the_digest() {
    let (mut o, c) = signed_order();
    o.subaccount_id = -1;
    assert!(order_digest(&o, &c).is_err());
    let (mut o, c) = signed_order();
    o.signature[64] = 3;
    assert!(verify_order(&o, &c).is_err());
}
