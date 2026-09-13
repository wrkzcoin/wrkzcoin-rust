// Copyright (c) 2026, The WrkzCoin developers
//
// Please see the included LICENSE file for more information

//! Byte comparison of our block template against the live C++ daemon's.
//!
//! `cargo test -p wrkz-mempool -- --ignored` (needs outbound TCP to
//! `node-fin.wrkz.work:17856`). Point it elsewhere with `WRKZ_DAEMON`.
//!
//! # What is compared, and what is not
//!
//! The daemon is asked for `getblocktemplate` with the fixture address of
//! `crates/wrkz-wallet/tests/fixtures/README.md` and `reserve_size = 8`. We
//! then build **our** template for the same height, with the daemon's own
//! parent hash, difficulty and timestamp, an empty transaction list, and a
//! fixed coinbase transaction key.
//!
//! Compared, and required to be equal:
//!
//! | Field | Why it can be compared |
//! | --- | --- |
//! | `major_version`, `minor_version` | height rule and `Core.cpp:2346` |
//! | `previous_block_hash` | fed from the daemon's blob; asserted to round trip |
//! | the whole parent block, byte for byte | major 0, minor 0, a zero parent hash, `transactionCount = 1`, empty branches, a default-constructed coinbase whose extra is `03 21 00 ‖ 32 zero bytes` (spec/12, "Things that look like bugs") |
//! | coinbase `version`, `unlock_time`, inputs | `height + 40` and one `BaseInput{height}` |
//! | coinbase output amounts | the reward decomposition; only when the daemon's pool was empty, since fees change it |
//! | coinbase `extra` layout and length | `01 ‖ R ‖ 02 ‖ 08 ‖ 8 reserved bytes`, with `R` masked |
//! | `difficulty`, `height`, `reserved_offset` | the RPC's own three numbers |
//! | the full template blob, with the transaction public key and every coinbase output key zeroed | everything above at once, including the varint widths |
//!
//! **Not** compared, because they cannot be:
//!
//! - the **timestamp**: `time(nullptr)` at the daemon. We take the daemon's
//!   value so that the blob lengths (and therefore `reserved_offset`) line up;
//!   the assertion is only that ours round trips.
//! - the **nonce**: 0 in a fresh template on both sides, asserted, not built.
//! - the **transaction public key** and the **coinbase output keys** derived
//!   from it: `Currency::constructMinerTx` calls `generateKeyPair()`. Both are
//!   masked to zeros before the byte comparison.
//! - the **transaction list**: two nodes' pools are never the same set. The
//!   test retries until the daemon offers an empty template; if the daemon's
//!   pool stays busy it falls back to comparing everything that does not
//!   depend on the pool and says so.
//!
//! Two chain values are taken as constants rather than read from a database we
//! do not have here, and both are asserted rather than assumed:
//!
//! - `median_size` is `calculateCumulativeBlocksizeLimit(height) / 2`, which is
//!   `max(median of the last 100 block sizes, 100,000)`. Real blocks are a few
//!   hundred bytes, so the granted full reward zone wins; the test asserts that
//!   the daemon's own coinbase pays what a median of 100,000 produces.
//! - `already_generated_coins` is unused above `FIXED_REWARD_V1_HEIGHT`
//!   (1,500,000), where the base reward is the flat 1,000,000; the test asserts
//!   the height is above it.

use wrkz_mempool::{build_template_from_context, TemplateContext, TemplateOptions};
use wrkz_primitives::block::BlockTemplate;
use wrkz_primitives::constants::*;

const DEFAULT_DAEMON: &str = "http://node-fin.wrkz.work:17856";
const ADDRESS: &str =
    "WrkzaiGnz9chESkaqwevEPRjtnU46TRBRY6z3EZ7fXymcE6dTbHDSWJ3mCgQ2Vdzxca6v5FEwodPCExuJuAmtVj86gVkpDFBue";
const RESERVE_SIZE: usize = 8;
/// How many times to ask before giving up on catching an empty pool.
const ATTEMPTS: usize = 4;

struct DaemonTemplate {
    blob: Vec<u8>,
    block: BlockTemplate,
    height: u64,
    difficulty: u64,
    reserved_offset: u64,
}

fn fetch(url: &str) -> DaemonTemplate {
    let body = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "getblocktemplate",
        "params": { "wallet_address": ADDRESS, "reserve_size": RESERVE_SIZE },
    });
    let response: serde_json::Value = ureq::post(&format!("{url}/json_rpc"))
        .timeout(std::time::Duration::from_secs(30))
        .send_json(body)
        .expect("the daemon answered")
        .into_json()
        .expect("the answer is JSON");
    let result = response.get("result").unwrap_or_else(|| panic!("no result in {response}"));
    assert_eq!(result["status"], "OK", "daemon status: {result}");
    let hex_blob = result["blocktemplate_blob"].as_str().expect("blocktemplate_blob");
    let blob = hex::decode(hex_blob).expect("blocktemplate_blob is hex");
    let block = BlockTemplate::from_bytes(&blob).expect("the daemon's template parses");
    DaemonTemplate {
        blob,
        block,
        height: result["height"].as_u64().expect("height"),
        difficulty: result["difficulty"].as_u64().expect("difficulty"),
        reserved_offset: result["reserved_offset"].as_u64().expect("reserved_offset"),
    }
}

/// Zero the two random things in a template: the coinbase transaction public
/// key in `extra`, and every output key derived from it.
fn mask(block: &BlockTemplate) -> Vec<u8> {
    let mut b = block.clone();
    let extra = &mut b.base_transaction.prefix.extra;
    assert_eq!(extra.first(), Some(&wrkz_primitives::tx::TX_EXTRA_TAG_PUBKEY), "extra starts with the public key");
    assert!(extra.len() >= 33);
    extra[1..33].fill(0);
    for output in &mut b.base_transaction.prefix.outputs {
        output.key = [0u8; 32];
    }
    b.to_bytes().expect("the masked template re-serializes")
}

fn diff(ours: &[u8], theirs: &[u8]) -> String {
    let mut out = String::new();
    if ours.len() != theirs.len() {
        out.push_str(&format!("length: ours {} vs daemon {}\n", ours.len(), theirs.len()));
    }
    let at = ours.iter().zip(theirs).position(|(a, b)| a != b);
    match at {
        None if ours.len() == theirs.len() => out.push_str("identical\n"),
        None => out.push_str("one is a prefix of the other\n"),
        Some(at) => {
            let from = at.saturating_sub(16);
            let to = (at + 16).min(ours.len().min(theirs.len()));
            out.push_str(&format!("first difference at byte {at}\n"));
            out.push_str(&format!("  ours   {}\n", hex::encode(&ours[from..to])));
            out.push_str(&format!("  daemon {}\n", hex::encode(&theirs[from..to])));
            out.push_str(&format!("  {}^\n", " ".repeat(9 + 2 * (at - from))));
        }
    }
    out
}

#[test]
#[ignore = "needs outbound TCP to the live daemon (WRKZ_DAEMON, default node-fin.wrkz.work:17856)"]
fn our_template_matches_the_live_daemons_bytes() {
    let url = std::env::var("WRKZ_DAEMON").unwrap_or_else(|_| DEFAULT_DAEMON.to_string());

    let mut daemon = fetch(&url);
    for _ in 1..ATTEMPTS {
        if daemon.block.transaction_hashes.is_empty() {
            break;
        }
        daemon = fetch(&url);
    }
    let empty_pool = daemon.block.transaction_hashes.is_empty();
    println!(
        "daemon {url}: height {}, difficulty {}, reserved_offset {}, {} pooled transaction(s), blob {} bytes",
        daemon.height,
        daemon.difficulty,
        daemon.reserved_offset,
        daemon.block.transaction_hashes.len(),
        daemon.blob.len()
    );

    assert!(
        daemon.height >= FIXED_REWARD_V1_HEIGHT,
        "above {FIXED_REWARD_V1_HEIGHT} the base reward is flat, which is what lets this test skip alreadyGeneratedCoins"
    );

    // Everything `Core::getBlockTemplate` reads off the chain, taken from the
    // daemon's own answer (see the module docs for the two constants).
    let major_version = block_major_version_for_index(daemon.height);
    let median_size = full_reward_zone(major_version) as u64;
    let ctx = TemplateContext {
        height: daemon.height,
        previous_block_hash: daemon.block.previous_block_hash,
        difficulty: daemon.difficulty,
        major_version,
        minor_version: wrkz_mempool::chain::minor_version_for(major_version),
        median_size,
        max_cumulative_size: max_block_cumulative_size(daemon.height),
        already_generated_coins: 0,
        // We adopt the daemon's timestamp verbatim, so there is nothing to
        // clamp against.
        timestamp_median: None,
    };
    let options = TemplateOptions {
        tx_key: Some(wrkz_pow::curve::generate_deterministic_keys(&[0x5a; 32])),
        now: Some(daemon.block.timestamp),
    };
    let ours =
        build_template_from_context(&ctx, &[], ADDRESS, RESERVE_SIZE, None, &options).expect("our template builds");

    // --- the three RPC numbers ---------------------------------------------
    assert_eq!(ours.height, daemon.height, "height");
    assert_eq!(ours.difficulty, daemon.difficulty, "difficulty");
    if empty_pool {
        assert_eq!(ours.reserved_offset, daemon.reserved_offset, "reserved_offset");
    }
    // The reserve really is at the offset, on both sides.
    assert_eq!(&ours.blob[ours.reserved_offset as usize..][..RESERVE_SIZE], &[0u8; RESERVE_SIZE]);
    assert_eq!(&daemon.blob[daemon.reserved_offset as usize..][..RESERVE_SIZE], &[0u8; RESERVE_SIZE]);

    // --- header -------------------------------------------------------------
    assert_eq!(ours.block.major_version, daemon.block.major_version, "major_version");
    assert_eq!(ours.block.minor_version, daemon.block.minor_version, "minor_version");
    assert_eq!(ours.block.previous_block_hash, daemon.block.previous_block_hash, "previous_block_hash");
    assert_eq!(daemon.block.nonce, 0, "a fresh daemon template has nonce 0");
    assert_eq!(ours.block.nonce, 0, "so does ours");
    assert_eq!(ours.block.timestamp, daemon.block.timestamp, "we adopted the daemon's timestamp");

    // --- the parent block, byte for byte -----------------------------------
    let ours_parent = ours.block.parent_hashing_blob(false).expect("parent blob");
    let daemon_parent = daemon.block.parent_hashing_blob(false).expect("parent blob");
    assert_eq!(
        hex::encode(&ours_parent),
        hex::encode(&daemon_parent),
        "parent block bytes (including the merge-mining tag)\n{}",
        diff(&ours_parent, &daemon_parent)
    );
    let parent = daemon.block.parent_block.as_ref().expect("v2+ template has a parent block");
    assert_eq!(parent.major_version, 0, "Core.cpp:2365 writes 0, not 1");
    assert_eq!(parent.minor_version, 0);
    assert_eq!(parent.previous_block_hash, [0u8; 32]);
    assert_eq!(parent.transaction_count, 1);
    assert_eq!(hex::encode(&parent.base_transaction().prefix.extra), format!("032100{}", "00".repeat(32)));

    // --- the coinbase -------------------------------------------------------
    let ours_cb = &ours.block.base_transaction.prefix;
    let daemon_cb = &daemon.block.base_transaction.prefix;
    assert_eq!(ours_cb.version, daemon_cb.version, "coinbase version");
    assert_eq!(ours_cb.unlock_time, daemon_cb.unlock_time, "coinbase unlock_time");
    assert_eq!(daemon_cb.unlock_time, daemon.height + CRYPTONOTE_MINED_MONEY_UNLOCK_WINDOW);
    assert_eq!(ours_cb.inputs, daemon_cb.inputs, "coinbase inputs");
    assert_eq!(ours_cb.extra.len(), daemon_cb.extra.len(), "coinbase extra length");
    // `01 ‖ R ‖ 02 ‖ len ‖ reserve`
    assert_eq!(daemon_cb.extra[0], wrkz_primitives::tx::TX_EXTRA_TAG_PUBKEY);
    assert_eq!(daemon_cb.extra[33], wrkz_primitives::tx::TX_EXTRA_NONCE);
    assert_eq!(daemon_cb.extra[34] as usize, RESERVE_SIZE);
    assert_eq!(&daemon_cb.extra[35..], &[0u8; RESERVE_SIZE]);
    assert_eq!(ours_cb.extra[33..], daemon_cb.extra[33..], "coinbase extra past the public key");

    if empty_pool {
        let amounts: Vec<u64> = daemon_cb.outputs.iter().map(|o| o.amount).collect();
        let ours_amounts: Vec<u64> = ours_cb.outputs.iter().map(|o| o.amount).collect();
        assert_eq!(ours_amounts, amounts, "coinbase output decomposition");
        // The median assumption: a flat reward, undamaged by the size penalty.
        let expected =
            wrkz_chain::reward::get_block_reward(major_version, median_size, ours.cumulative_size, 0, 0, daemon.height)
                .expect("the template is far inside the penalty-free zone")
                .reward;
        assert_eq!(amounts.iter().sum::<u64>(), expected, "the daemon's reward matches a median of {median_size}");

        // --- and finally the whole blob, masked -----------------------------
        let ours_masked = mask(&ours.block);
        let daemon_masked = mask(&daemon.block);
        assert_eq!(
            hex::encode(&ours_masked),
            hex::encode(&daemon_masked),
            "template blob with the transaction public key and the coinbase output keys zeroed\n{}",
            diff(&ours_masked, &daemon_masked)
        );
        println!("every compared field matched, including {} masked template bytes", ours_masked.len());
    } else {
        println!(
            "the daemon's pool held {} transaction(s) on every one of {ATTEMPTS} attempts: the coinbase outputs, \
             the reward and the full blob were not compared (they depend on the fees). Everything else matched.",
            daemon.block.transaction_hashes.len()
        );
    }
}
