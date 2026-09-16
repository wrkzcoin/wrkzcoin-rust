# Fuzz targets

libFuzzer targets for the parsers that consume bytes this node did not
produce: a peer's frames, a daemon's answers, a downloaded file, a database
handed over, and an unauthenticated client's HTTP request. See
`scripts/fuzz.sh` for how to run them (Linux, nightly toolchain,
`cargo-fuzz`). The `corpus/` directories are seed inputs taken from
`spec/vectors/` and from the wallet fixtures; any crash found under
`artifacts/` must be added to the crate's tests as a regression before it is
fixed.

| Target | Parser | Reads bytes from | Property |
| --- | --- | --- | --- |
| `block` | `BlockTemplate::from_bytes` | a peer | no panic; byte-exact round trip; hashing total |
| `transaction` | `Transaction`, `TransactionPrefix`, `BaseTransaction` | a peer | same |
| `tx_extra` | `parse_extra`, `parse_extra_wallet` | a peer | total functions |
| `kv` | `kv::decode` / `kv::encode` | a peer, a database | no panic; bounded memory; round trip |
| `p2p_messages` | the `wrkz_p2p::msg` parsers | a peer | total functions |
| `pow_diff` | the Rust hashing of `wrkz-pow` (Keccak, finalizers, tree hash, Chukwa, `cn_turtle_lite_v2`, `cn_upx`) | a peer | identical to the reference C of `wrkz-pow-ref` |
| `json` | `wrkz_rpc::json::parse` | an RPC client | total within its limits; what parses re-serializes and parses back the same |
| `http` | `wrkz_rpc::http::read_request` | an RPC client | total; nothing past `HttpLimits` is accepted; no allocation sized from a declared `Content-Length` |
| `wallet_file` | `Wallet::from_json_bytes` | a wallet file | total; a document that opens writes back to one that opens identically |
| `db_records` | `wrkz_storage::records`, `::codec`, `wrkz_chain::records` | a C++ or port database | total; no allocation sized from a declared length |
| `peer_state` | `PeerManager::decode` (`p2pstate.wrkz.bin`) | the data directory | total; the loaded lists stay inside the peerlist limits |
| `lite_snapshot` | `snapshot::container::Reader` and `snapshot::records::decode` | a downloaded `.litesnap` | total and bounded; truncated, reordered and over-declared frames are errors |

Not fuzzed, and deliberately: the encrypted wallet container
(`decode_wallet_file`). Every input past its magic identifier costs 500,000
PBKDF2 iterations (spec/03), so a campaign would spend all its time in the key
derivation. Its framing is covered by `crates/wrkz-wallet/tests/wallet_file.rs`.
