# Fuzz targets

libFuzzer targets for every parser that consumes bytes from a peer, a
daemon, a database or a file. See `scripts/fuzz.sh` for how to run them
(Linux, nightly toolchain, `cargo-fuzz`). The `corpus/` directories are
seed inputs taken from `spec/vectors/`; any crash found under `artifacts/`
must be added to the crate's tests as a regression before it is fixed.

| Target | Parser | Property |
| --- | --- | --- |
| `block` | `BlockTemplate::from_bytes` | no panic; byte-exact round trip; hashing total |
| `transaction` | `Transaction`, `TransactionPrefix`, `BaseTransaction` | same |
| `tx_extra` | `parse_extra`, `parse_extra_wallet` | total functions |
| `kv` | `kv::decode` / `kv::encode` | no panic; bounded memory; round trip |
| `p2p_messages` | the `wrkz_p2p::msg` parsers | total functions |
| `pow_diff` | the Rust hashing of `wrkz-pow` (Keccak, finalizers, tree hash, Chukwa, `cn_turtle_lite_v2`, `cn_upx`) | identical to the reference C of `wrkz-pow-ref` |
