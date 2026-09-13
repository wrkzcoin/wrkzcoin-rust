# 10 - Wallet: file format, sync, transaction construction, C API

Source files: `src/walletbackend/WalletBackend.cpp` (open/save lines
496-754, JSON lines 1935-1990), `Constants.h`, `WalletSynchronizer.cpp`,
`BlockDownloader.cpp`, `SynchronizationStatus.cpp`, `Transfer.cpp`,
`src/subwallets/SubWallets.cpp`, `SubWallet.cpp`, `include/WalletTypes.h`,
`include/walletcapi/wallet_capi.h`, `src/walletcapi/wallet_capi.cpp`,
`src/errors/Errors.h`, `src/config/WalletConfig.h`,
`src/utilities/Utilities.cpp`. Reference docs: `docs/docs/guides/wallet-apps.md`,
`docs/docs/guides/building-wallet-apps.md`, `docs/docs/guides/encrypted-payment-ids.md`,
`docs/docs/guides/error-codes.md`.

This is stage 2. It depends on stages 1 documents and on
`06-transactions.md` and `09-rpc-and-wallet-sync.md`.

## Wallet file

`WalletBackend::saveWalletJSONToDisk` (line 630) and `openWallet` (line 496):

    bytes 0..64    IS_A_WALLET_IDENTIFIER (plaintext, 64 bytes; 01-constants.md)
    bytes 64..80   salt (16 random bytes, plaintext)
    bytes 80..     AES-128-CBC(PKCS#7, key = PBKDF2-HMAC-SHA256(password, salt, 500000, 16), iv = salt)
                   of  IS_CORRECT_PASSWORD_IDENTIFIER (26 bytes) ‖ wallet JSON

Open: check the 64-byte marker (else "not a wallet file"; the C++ code then
tries the legacy `WalletGreen` format and converts it in place after a
backup copy named `old-version-backup-<file>`); read the salt; derive; decrypt;
any failure is `WRONG_PASSWORD`; check the 26-byte marker; parse JSON.
Save: fresh salt every time, write to `<file>.tmp`, rename over the
original. The file is saved on close, on demand, and periodically during
sync.

The wallet API's own password (not the file password) is hashed with the
same PBKDF2 at 10,000 iterations.

## Wallet JSON (`WALLET_FILE_FORMAT_VERSION` 0)

```json
{"walletFileFormatVersion": 0,
 "subWallets": {
   "publicSpendKeys": [hex, ...],
   "subWalletIndexCounter": n,               // highest deterministic index handed out
   "subWallet": [
     {"walletIndex": n,                       // 0 for the primary and for imported keys
      "publicSpendKey": hex, "privateSpendKey": hex,   // all zeros in a view wallet
      "address": "Wrkz...",
      "syncStartTimestamp": n, "syncStartHeight": n,   // one of them is 0
      "isPrimaryAddress": bool,
      "unspentInputs": [TransactionInput], "lockedInputs": [TransactionInput], "spentInputs": [TransactionInput],
      "unconfirmedIncomingAmounts": [{"amount": n, "key": hex, "parentTransactionHash": hex}]}],
   "transactions": [Transaction], "lockedTransactions": [Transaction],
   "privateViewKey": hex, "isViewWallet": bool,
   "txPrivateKeys": [{"transactionHash": hex, "txPrivateKey": hex}]},
 "walletSynchronizer": {
   "transactionSynchronizerStatus": {
      "blockHashCheckpoints": [hex, ...],     // newest first, one every 5000 blocks
      "lastKnownBlockHashes": [hex, ...],     // newest first, at most 50
      "lastKnownBlockHeight": n},
   "startTimestamp": n, "startHeight": n, "privateViewKey": hex}}
```

`TransactionInput` (`WalletTypes.h:161`): `keyImage, amount, blockHeight,
transactionPublicKey, transactionIndex, globalOutputIndex, key, spendHeight,
unlockTime, parentTransactionHash` and optional `privateEphemeral` (the
one-time secret, cached so spending does not need to re-derive it).
`Transaction` (`WalletTypes.h:360`): `transfers: [{"publicKey", "amount"}]`
(signed amounts per owned spend key), `hash, fee, blockHeight, timestamp,
paymentID, unlockTime, isCoinbaseTransaction`. Hex strings are lowercase.
A port MUST read and write this exact schema so existing files keep
working across implementations; unknown keys should be preserved or
ignored, never rejected.

## Sync algorithm

Three cooperating pieces: `BlockDownloader` fetches, `WalletSynchronizer`
scans, `SubWallets` stores. The observable behaviour a port must keep:

### Request construction (`BlockDownloader::getBlockCheckpoints`, line 326)

`blockHashCheckpoints` = hashes of the up-to-50 most recently *downloaded
but unprocessed* blocks (newest first), padded with the most recently
*processed* hashes up to 50, followed by the sparse 5000-block checkpoints
from the status. The daemon resumes after the first hash it knows, so a
reorganisation shallower than the recent list is recovered automatically:
the response starts below the wallet's height and the wallet unwinds
(below).

`startHeight`/`startTimestamp` come from the earliest subwallet start
(`SubWallets::getMinInitialSyncStart`, `SubWallets.cpp:296`): a new wallet
records its creation timestamp and, once `/info` answers, converts it to
`min(network height, timestampToScanHeight)` (`WalletBackend::init`, line 760);
`timestampToScanHeight(t) = (t − 1529831318) / 60 − 10000`, floored at 0.
Restores take the user's scan height.

### Batch size (`Nigel.cpp:433-469`)

Start at 100 blocks; double after each success up to 1000 (or the
daemon's limit learned from a `400`); halve on an empty or failed answer;
never shrink after a `429`.

### Parallel windows (`BlockDownloader::downloadBlocksInParallel`, line 664)

When the daemon advertises `heightRange` and the wallet is more than
`4 · window + 4 · 180` blocks behind the tip, it requests 4 consecutive
height windows at once (`syncRequestConcurrency`), each of
`batch · 20` heights capped at 50,000, with no checkpoints, and stores them
in order; a window that fails or stops short ends the run. Windows are
address-by-height only, so this is used far enough below the tip that no
reorganisation can reach (`CRYPTONOTE_MAX_ALT_BLOCK_DEPTH` 180).

### Answer validation (`BlockDownloader::downloadBlocks`, line 373)

- a daemon whose `lite_start_height` is above the wallet's covered height
  stops sync with a "gap" error rather than skipping blocks;
- when coinbases are not skipped and no timestamp start is in use, the
  first returned height MUST be `max(coveredTo + 1, startHeight)`; a
  higher first height is retried up to 3 times (a reorganisation at the
  tip looks like this), then reported as a gap;
- a timestamp start is resolved to a height from the first block received.

### Scanning (`WalletSynchronizer::processTransactionOutputs`, line 645)

For every transaction in a block (coinbase included unless coinbase
scanning is off), compute `D = 8·a·R` from the tx public key and the
private view key once, then for each output index `i`:
`B' = underive(D, i, output key)`; if `B'` is one of the container's spend
public keys the output is ours. For a non-view wallet derive the one-time
secret `x = Hs(D‖i) + b` and the key image `x·Hp(P)` and store both
(`SubWallet::getTxInputKeyImage`, `SubWallet.cpp:61`).

Ownership is decided by the receiving subwallet's spend key; there is no
"is this output for me" tag. A view wallet stores inputs without key
images and cannot see them being spent.

### Global indexes

`getwalletsyncdata` outputs carry no global index. For each block with
owned outputs the wallet asks `/get_global_indexes_for_range` for the
10-block window containing it (`getLowerBound/getUpperBound` to a
multiple of 10) and fills `globalOutputIndex` from the transaction hash and
output position. Three failures in a row leave the index unset with a
warning; spending that input later fails until a full node resolves it.

### Applying a block (`completeBlockProcessing`, line 368)

1. If `blockHeight <= wallet height` and `blockHeight != 0`, a fork was
   detected: remove every transaction at or above that height, move inputs
   received there out of every list, un-spend inputs spent there, and put
   sent transactions with a payment id back into the locked list so their
   plaintext payment id survives (`SubWallets::removeForkedTransactions`,
   line 836).
2. Every 2880 blocks, prune spent inputs spent more than 2880 blocks ago.
3. For each transaction: sum owned outputs per spend key (positive), and
   for each key input whose key image the container owns subtract its
   amount (negative) and mark it spent at this height. A transaction with
   any non-zero transfer is recorded; fee = inputs − outputs.
   Coinbase transactions record fee 0 and `isCoinbaseTransaction`.
4. Decrypt the payment id (`decryptPaymentID`, line 530): a 16-hex-char id
   is decrypted with `D` from the tx public key and the private view key
   *only if we did not spend any input*; if we did, report empty (the
   record kept from send time supplies it, `SubWallets::addTransaction`).
   64-hex-char ids pass through.
5. Store inputs, then record the block hash and height in the
   synchronization status (`storeBlockHash`: pushes to the 50-entry recent
   list, and to the sparse list when 5000 blocks have passed).
6. Fire `onTransaction` per new transaction and `onSynced` when the block
   height reaches the network height.

### Locked transactions

Every 60 s (or whenever a block arrives) while synced, ask
`/get_transactions_status` for the hashes in `lockedTransactions`; hashes
the daemon reports as unknown are cancelled: the transaction is dropped
and its locked inputs return to unspent.

## Balances and spendability

`SubWallet::getBalance` (line 135): unlocked = unspent inputs whose
`unlockTime` has passed; locked = the rest plus `unconfirmedIncomingAmounts`.
`Utilities::isInputUnlocked(unlockTime, height)` (`Utilities.cpp:45`):
`unlockTime == 0` → unlocked; `>= 500,000,000` → unix time, unlocked if
`now + 60 >= unlockTime`; else unlocked if `height + 1 >= unlockTime`.
Inputs used by an in-flight send live in `lockedInputs` until the
transaction is seen in a block (then spent) or cancelled (then unspent).

## Transaction construction (`SendTransaction::sendTransactionAdvancedWithMixin`, `Transfer.cpp:189`)

Inputs: destinations `[(address, amount)]`, mixin, fee type (minimum, per
byte, or fixed), payment id, subwallets to take from, change address,
unlock time, arbitrary extra data, `sendAll`, and whether to relay.

1. **Defaults**: change to the primary address; `unlockTime = networkHeight
   + (height > 1,500,000 ? 20 : 40) + 15`.
2. **Validate** (`errors/ValidateParameters.cpp`): addresses, amounts
   (≥ 1000 atomic, `WalletConfig::minimumSend`), mixin within the tier for
   the network height, fee, payment id format, balance.
3. **Integrated addresses** are split into address + payment id; a short
   payment id requires exactly one destination and is encrypted to that
   destination's view key with the transaction key
   (`03-crypto-primitives.md`).
4. **Input selection** (`SubWallets::getSpendableTransactionInputs`, line
   530): all unlocked inputs of the chosen subwallets, sorted largest
   first, bucketed by decimal digit count, then taken round-robin one per
   bucket starting from the smallest bucket, smallest amount within each
   bucket first. Inputs are added one at a time until the sum covers the
   amount (plus the fee for fixed fees).
5. **Fee loop** for per-byte fees (`tryMakeFeePerByteTransaction`, line
   575): estimate the size (`Utilities::estimateTransactionSize`, line
   356), build the transaction, measure the real size, recompute the fee
   with `getTransactionFee(size, height, rate)`; if the built fee is too
   low, rebuild with the larger fee; if the inputs no longer cover it, add
   another input and repeat. With `sendAll` the amount, not the change, is
   reduced.
6. **Destinations** (`setupDestinations`, line 791): append change, then
   split every amount into decimal denominations
   (`splitAmountIntoDenominations`: each non-zero decimal digit × its power
   of ten; a denomination above `MAX_OUTPUT_SIZE_CLIENT` is split into ten
   or more equal pieces); at most 90 outputs.
7. **Ring members** (`prepareRingParticipants`, line 980): sort inputs by
   amount; `/getrandom_outs` with `mixin + 1` per amount; per input drop
   the entry that is our own output, take the first `mixin` others, sort by
   global index, insert the real output at its sorted position and
   remember that position; convert to relative offsets. If any amount has
   too few decoys the send fails with `NOT_ENOUGH_FAKE_OUTPUTS`, and
   `sendTransactionAdvanced` retries at the largest achievable mixin, then
   at the network minimum (`Utilities::nextFallbackMixin`).
8. **Inputs** (`setupInputs`, line 1096): key image, amount, offsets; the
   one-time secret from the cached `privateEphemeral` or re-derived.
9. **Outputs** (`setupOutputs`, line 1177): sort destinations by amount
   ascending; generate a random transaction key `(r, R)`; output `i` gets
   `P_i = Hs(8·r·A_i ‖ i)·G + B_i` for that destination's keys.
10. **Extra**: `01 ‖ R`, then if a payment id or extra data exists
    `02 ‖ len ‖ [03 ‖ 8 bytes encrypted id | 00 ‖ 32 bytes long id] [7f ‖ varint len ‖ data]`.
11. **Transaction proof of work** (`06-transactions.md`): needed when
    `networkHeight < 1,500,000` or the fee is below 10000; appends
    `04 ‖ 8-byte nonce` to extra and searches. GUI wallets may delegate the
    search to a `wrkz-txpow-server` and re-verify the answer
    (`TxPowClient`, `TXPOWSERVER.md`); the CLI, API and service never do.
12. **Sign** (`generateRingSignatures`, line 1221): prefix hash over the
    final prefix (extra included), one ring signature per input; verify
    each immediately; the C++ code also round-trips the serialization and
    re-verifies.
13. **Checks**: serialized size ≤ `getMaxTxSize(height)` =
    `min(100000 + height·102400/525600, 125000) − 600`; every output amount
    is in `PRETTY_AMOUNTS`; the fee is within `[expected, 2·expected]` for
    per-byte fees or exactly the fixed fee (`verifyTransactionFee`, line 1688).
14. **Relay or prepare**: `/sendrawtransaction`; on success record an
    unconfirmed outgoing transaction with negative transfers per spending
    key and positive change, lock the inputs, store our own outputs as
    unconfirmed incoming, and keep `r` in `txPrivateKeys`. A prepared
    transaction (`send_transaction = false`) is cached under its hash and
    can be relayed later (`sendPreparedTransaction`), which re-checks that
    the inputs are still spendable.

**Sweep** (`WalletBackend::sweepToAddress`): builds as many transactions as
needed to move an amount (or everything) in size-limited batches through
the same path, re-weighing the fee against each finished transaction.

**Fusion** (`SubWallets::getFusionTransactionInputs`, line 615): shuffle
unlocked inputs, bucket by digit count, prefer a bucket with at least 12
inputs, take up to the count that fits in 30,000 bytes; build a
zero-fee transaction whose outputs are the decomposition of the input sum
(`06-transactions.md`, fusion rules) with the 320,000-difficulty proof of
work. The current front ends do not expose fusion; the C API has no call
for it.

## The C API (`include/walletcapi/wallet_capi.h`)

Every front end that is not the CLI uses these 57 functions through
FFI (Flutter desktop and mobile) or through the WASM build's JS bridge
(web). Signatures are in the header; the contract a port must keep:

| Group | Functions |
| --- | --- |
| lifecycle | `wallet_capi_api_version` (1), `wallet_capi_version_string`, `wallet_open`, `wallet_create`, `wallet_restore_from_seed`, `wallet_restore_from_keys`, `wallet_restore_view`, `wallet_delete_file`, `wallet_close`, `wallet_save` |
| sync | `wallet_get_sync_status` (wallet, local daemon, network heights), `wallet_sync_step` (single-threaded drive for WASM), `wallet_get_status_json`, `wallet_daemon_online`, `wallet_get_node_info_json`, `wallet_swap_node`, `wallet_reset`, `wallet_test_node` |
| read | `wallet_get_transactions_json`, `wallet_get_primary_address`, `wallet_get_addresses_json`, `wallet_get_total_balance`, `wallet_get_balance_for_address`, `wallet_get_balances_json`, `wallet_get_transactions_status_json`, `wallet_is_view_wallet` |
| keys | `wallet_get_private_view_key`, `wallet_get_spend_keys_json`, `wallet_get_mnemonic_seed`, `wallet_get_mnemonic_seed_for_address`, `wallet_get_tx_private_key`, `wallet_change_password`, `wallet_export_json` |
| subwallets | `wallet_add_subwallet_json`, `wallet_import_subwallet_from_key`, `wallet_import_subwallet_from_index`, `wallet_delete_subwallet` |
| send | `wallet_send_basic`, `wallet_send_advanced_json`, `wallet_send_prepared`, `wallet_delete_prepared`, `wallet_sweep_to_address`, `wallet_estimate_sweep`, `wallet_create_integrated_address`, `wallet_get_pow_status`, `wallet_set_tx_pow_server`, `wallet_test_tx_pow_server` |
| plumbing | `wallet_string_free`, `wallet_poll_event` (`WALLET_EVENT_SYNCED`, `WALLET_EVENT_TRANSACTION`), `wallet_error_code_to_string`, `wallet_last_error_message`, `wallet_clear_last_error_message`, `wallet_set_log_level`, `wallet_take_logs_json`, `wallet_clear_logs`, `wallet_set_scan_coinbase` |

Conventions: every call returns a `wallet_status_t` error code from
`src/errors/Errors.h` (0 = `SUCCESS`; the numbering is part of the
contract because the apps switch on it, and `docs/docs/guides/error-codes.md`
lists it); strings are returned as `char **out, size_t *len` and freed
with `wallet_string_free`; JSON in and out for anything structured
(`wallet_send_advanced_json` takes
`{"destinations":[{"address","amount"}],"mixin","fee"|"feePerByte","paymentID","subWalletsToTakeFrom","changeAddress","unlockTime","extraData","sendAll"}`
and returns `{"transactionHash","fee","mixin","defaultMixin",...}`; see
`wallet_capi.cpp` for each shape). `sync_threads = 0` means no background
threads and the host drives `wallet_sync_step` (the web build);
`wallet_open` with `sync_threads = 0` on a desktop build never syncs, which
has bitten the apps before.

The Flutter apps live in `extras/desktop-wallet`, `extras/mobile-wallet`
and `extras/web-wallet` (Dart, plus `extras/web-wallet-wasm/wallet_bridge.js`
which is duplicated per app and must be kept in step). A port that
replaces `libwallet_capi` must keep the exported names, the error codes
and the JSON shapes, or update all three apps together.

## What is not consensus but must match

- Wallets built by the port and by the C++ code must be interchangeable:
  same file, same seed, same addresses, same subwallet indexes.
- Transactions built by the port must be indistinguishable on the wire
  from the C++ wallet's: same extra layout and field order, sorted
  outputs, sorted ring members with relative offsets, denominations from
  `PRETTY_AMOUNTS`, unlock time formula, fee rounding. Any deviation is a
  fingerprint that weakens every user's privacy.
- The daemon requests must be the same, including the checkpoint list
  shape and the 10-block obscurity window for global indexes.

## Acceptance for this document

1. Open the C++ wallet file `vectors/` does not include one (it would hold
   keys); instead create a wallet with the C++ `wrkz-wallet` CLI, open it with
   the port, and compare `wallet_export_json` output field by field; then
   the reverse.
2. Restore the seed from `05-addresses-keys-mnemonics.md` in both
   implementations and compare primary address and subwallets 1, 2, 5.
3. Sync a fresh view wallet for a known mainnet address from a chosen
   height in both implementations and compare the transaction lists and
   balances.
4. Build a transaction with the port with fixed test randomness for the
   tx key, ring scalars and decoy choice, and confirm the C++ daemon
   accepts it (`/sendrawtransaction` on a private test node, or by
   running the C++ validator on it in a test).
5. The three Flutter apps run unmodified against the port's
   `libwallet_capi`.
