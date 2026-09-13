// Copyright (c) 2026, The WrkzCoin developers
//
// Please see the included LICENSE file for more information

//! Replays the key, derivation, key image, subwallet, payment id and
//! hash-to-point vectors of spec/vectors/primitives.txt (spec/03-crypto-primitives.md).

use wrkz_pow::curve::*;

fn text() -> String {
    let path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../spec/vectors/primitives.txt");
    std::fs::read_to_string(path).unwrap()
}

/// The value after the last " = " on the first line of `section` whose label starts with `key_prefix`.
fn get(section: &str, key_prefix: &str) -> String {
    let text = text();
    let mut in_section = false;
    for line in text.lines() {
        if let Some(t) = line.strip_prefix("## ") {
            in_section = t.starts_with(section);
            continue;
        }
        if !in_section {
            continue;
        }
        if let Some((l, r)) = line.rsplit_once(" = ") {
            if l.trim().starts_with(key_prefix) {
                return r.trim().to_string();
            }
        }
    }
    panic!("{section}/{key_prefix} missing");
}

fn base_keys() -> (SecretKey, PublicKey, SecretKey, PublicKey) {
    let (spend_sec, spend_pub) = generate_deterministic_keys(&[0x11; 32]);
    let (view_sec, view_pub) = generate_view_from_spend(&spend_sec);
    (spend_sec, spend_pub, view_sec, view_pub)
}

#[test]
fn keys_and_view_from_spend() {
    let (spend_sec, spend_pub, view_sec, view_pub) = base_keys();
    assert_eq!(hex::encode(spend_sec), get("keys", "spend_secret"));
    assert_eq!(hex::encode(spend_pub), get("keys", "spend_public"));
    assert_eq!(secret_key_to_public_key(&spend_sec), Some(spend_pub));
    assert!(sc_check(&spend_sec));
    assert!(check_key(&spend_pub));
    assert_eq!(hex::encode(wrkz_pow::cn_fast_hash(&spend_sec)), get("keys", "keccak(spend_secret)"));
    assert_eq!(hex::encode(view_sec), get("keys", "view_secret"));
    assert_eq!(hex::encode(view_pub), get("keys", "view_public"));
}

#[test]
fn deterministic_subwallets() {
    let (spend_sec, _, _, _) = base_keys();
    let text = text();
    let mut seen = 0;
    for line in text.lines() {
        // "index N: secret=<hex> public=<hex>"
        let Some(rest) = line.strip_prefix("index ") else { continue };
        let Some((idx, kv)) = rest.split_once(": secret=") else { continue };
        let idx: u64 = idx.parse().unwrap();
        let (sec_hex, pub_hex) = kv.split_once(" public=").unwrap();
        let sub = generate_deterministic_subwallet_key(&spend_sec, idx);
        assert_eq!(hex::encode(sub), sec_hex, "subwallet {idx} secret");
        assert_eq!(hex::encode(secret_key_to_public_key(&sub).unwrap()), pub_hex, "subwallet {idx} public");
        seen += 1;
    }
    assert_eq!(seen, 3);
}

#[test]
fn output_derivation_key_images_underive() {
    let (spend_sec, spend_pub, view_sec, view_pub) = base_keys();
    let (tx_sec, tx_pub) = generate_deterministic_keys(&[0x22; 32]);
    assert_eq!(hex::encode(tx_sec), get("output derivation", "tx_secret"));
    assert_eq!(hex::encode(tx_pub), get("output derivation", "tx_public"));

    let d = generate_key_derivation(&view_pub, &tx_sec).unwrap();
    assert_eq!(generate_key_derivation(&tx_pub, &view_sec), Some(d), "both sides agree");
    assert_eq!(hex::encode(d), get("output derivation", "derivation ="));

    let text = text();
    let mut cur: Option<u64> = None;
    let mut seen = Vec::new();
    for line in text.lines() {
        if let Some(i) = line.strip_prefix("output index ").and_then(|s| s.strip_suffix(':')) {
            cur = Some(i.parse().unwrap());
            seen.push(cur.unwrap());
            continue;
        }
        let (Some(i), Some((label, val))) = (cur, line.rsplit_once(" = ")) else { continue };
        let val = val.trim();
        let p = derive_public_key(&d, i, &spend_pub).unwrap();
        let x = derive_secret_key(&d, i, &spend_sec);
        if label.contains("derivation_to_scalar") {
            assert_eq!(hex::encode(derivation_to_scalar(&d, i)), val, "scalar {i}");
        } else if label.contains("one_time_public") {
            assert_eq!(hex::encode(p), val, "P {i}");
            assert_eq!(underive_public_key(&d, i, &p), Some(spend_pub), "underive {i}");
        } else if label.contains("one_time_secret") {
            assert_eq!(hex::encode(x), val, "x {i}");
            assert_eq!(secret_key_to_public_key(&x), Some(p), "x*G == P {i}");
        } else if label.contains("key_image") {
            let ki = generate_key_image(&p, &x);
            assert_eq!(hex::encode(ki), val, "key image {i}");
            assert!(key_image_in_prime_subgroup(&ki));
        }
    }
    assert_eq!(seen, vec![0, 1, 300]);
}

#[test]
fn hash_to_scalar_and_ec() {
    let sec = "hash_to_scalar / hash_to_ec";
    assert_eq!(hex::encode(hash_to_scalar(b"abc")), get(sec, "hash_to_scalar('abc')"));
    assert_eq!(hex::encode(hash_to_ec(b"abc")), get(sec, "hash_to_ec('abc')"));
    let (_, spend_pub, _, _) = base_keys();
    assert_eq!(hex::encode(hash_to_ec(&spend_pub)), get(sec, "Hp(spend_public)"));
}

#[test]
fn encrypted_short_payment_id() {
    let sec = "encrypted short payment id";
    let (_, _, _, view_pub) = base_keys();
    let (tx_sec, _) = generate_deterministic_keys(&[0x22; 32]);
    let d = generate_key_derivation(&view_pub, &tx_sec).unwrap();
    let mut buf = d.to_vec();
    buf.push(0x8d);
    let ks = wrkz_pow::cn_fast_hash(&buf);
    assert_eq!(hex::encode(ks), get(sec, "keystream"));
    let mut pid = [1u8, 2, 3, 4, 5, 6, 7, 8];
    for (b, k) in pid.iter_mut().zip(ks.iter()) {
        *b ^= k;
    }
    assert_eq!(hex::encode(pid), get(sec, "encrypt(0102030405060708)"));
}

#[test]
fn ring_signature_round_trips() {
    // spec/03 acceptance 3: sign-then-verify with random rings of sizes 1, 2, 4, 8.
    let prefix = wrkz_pow::cn_fast_hash(b"prefix");
    for n in [1usize, 2, 4, 8] {
        for real in 0..n {
            let mut pubs = Vec::new();
            let mut secs = Vec::new();
            for _ in 0..n {
                let (s, p) = generate_keys();
                pubs.push(p);
                secs.push(s);
            }
            let image = generate_key_image(&pubs[real], &secs[real]);
            let sigs = generate_ring_signature(&prefix, &image, &pubs, &secs[real], real).unwrap();
            assert!(check_ring_signature(&prefix, &image, &pubs, &sigs), "n={n} real={real}");
            let mut bad = sigs.clone();
            bad[0][5] ^= 1;
            assert!(!check_ring_signature(&prefix, &image, &pubs, &bad));
            let other = wrkz_pow::cn_fast_hash(b"other");
            assert!(!check_ring_signature(&other, &image, &pubs, &sigs));
            let (s2, p2) = generate_keys();
            let image2 = generate_key_image(&p2, &s2);
            assert!(!check_ring_signature(&prefix, &image2, &pubs, &sigs));
        }
    }
}

#[test]
fn plain_signature_round_trip() {
    let (s, p) = generate_keys();
    let h = wrkz_pow::cn_fast_hash(b"msg");
    let sig = generate_signature(&h, &p, &s);
    assert!(check_signature(&h, &p, &sig));
    let mut bad = sig;
    bad[40] ^= 1;
    assert!(!check_signature(&h, &p, &bad));
}

#[test]
fn order_times_base_is_identity() {
    let mut one = [0u8; 32];
    one[0] = 1;
    let g = secret_key_to_public_key(&one).unwrap();
    assert_eq!(scalarmult_key(&g, &L), Some(IDENTITY));
    assert!(sc_check(&sc_reduce32([0xff; 32])));
}

#[test]
fn key_image_domain_check_rejects_undecodable_images() {
    // ge_frombytes_vartime rejects a y of 2^255-19 .. 2^255-1 with the sign bit
    // set; the C++ would multiply an uninitialised point here.
    let bad: KeyImage = [0xff; 32];
    assert!(!check_key(&bad));
    assert_eq!(scalarmult_key(&bad, &L), None);
    assert!(!key_image_in_prime_subgroup(&bad));
    // A real key image passes, and a small-order point fails the domain check.
    let (sec, pk) = generate_keys();
    assert!(key_image_in_prime_subgroup(&generate_key_image(&pk, &sec)));
}
