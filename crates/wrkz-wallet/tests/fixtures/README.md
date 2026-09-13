# Wallet file fixtures

**These are real wallet files written by the C++ code, not by this port.**

They were produced on 2026-09-09 by a Windows build of the C++
`wrkz-wallet-api.exe` (WRKZCoin v0.4.8.280, the same
`WalletBackend::saveWalletJSONToDisk` that the C++ `wrkz-wallet` CLI uses), driven over its
HTTP API with no daemon running:

```sh
wrkz-wallet-api.exe --no-console -r <throwaway> -p 18856 --threads 1

# spec05-seed.wallet
curl -H 'X-API-KEY: <throwaway>' -d '{"filename":"spec05-seed.wallet","password":"password",
      "scanHeight":4213000,"daemonHost":"127.0.0.1","daemonPort":17856,
      "mnemonicSeed":"<the 25 words below>"}' http://127.0.0.1:18856/wallet/import/seed

# spec05-subwallets.wallet: the same wallet, then two /addresses/create calls and PUT /save
# spec05-view.wallet: /wallet/import/view with the view key and the address below

# each .json beside a .wallet
curl -H 'X-API-KEY: <throwaway>' -d '{"filename":"<name>.json"}' http://127.0.0.1:18856/export/json
```

`POST /export/json` writes `WalletBackend::toJSON()` verbatim, so each `.json` is
the exact plaintext inside the matching `.wallet` (plus the trailing `std::endl`,
which is CRLF on Windows — the tests strip it).

## Password

`password` for all three. **No fixture is a real wallet**: every key here comes
from `spec/vectors/primitives.txt`, is published in
`spec/05-addresses-keys-mnemonics.md`, and has never held funds.

## Keys

```text
mnemonic       eluded ceiling theatrics orange mixture epoxy viewpoint oatmeal aggravate
               tell different dating intended richly slower inundate ridges slug inundate
               ridges slug were rotate rudely viewpoint
spend secret   243d1bb4f6adfeb83a74196e321732fc10111111111111111111111111111101
spend public   857eed804ff087b97f87848f6493e87257a8c5203cb9f422f6e7a7d8a4d299f3
view secret    779e4dd2c49ac3c0b2edcd1b843c795b7d6eb51457125bb9c90339b752f23700
view public    0489cb98c7108372eaff2cdeddc5e76166b017a847537bf8499d61465395e942
address        WrkzaiGnz9chESkaqwevEPRjtnU46TRBRY6z3EZ7fXymcE6dTbHDSWJ3mCgQ2Vdzxca6v5FEwodPCExuJuAmtVj86gVkpDFBue
```

## The files

| File | What it holds |
| --- | --- |
| `spec05-seed.wallet` | One subwallet, imported from the seed with `scanHeight` 4,213,000. No inputs, no transactions. `subWalletIndexCounter` 0. |
| `spec05-subwallets.wallet` | The same wallet plus deterministic subwallets 1 and 2 (`/addresses/create` twice), `subWalletIndexCounter` 2. Their keys are the index 1 and 2 vectors of `spec/vectors/primitives.txt`, which is what makes this file a subwallet-derivation vector as well as a format fixture. |
| `spec05-view.wallet` | A view-only wallet for the same address: `isViewWallet` true and `privateSpendKey` all zeros. |

Subwallet keys in `spec05-subwallets.wallet`:

```text
index 1  secret 2c7d88e6b43bb83f7215ecc744e73589d8d1a841e7ab8f26672c5490c1aa2b0a
         public 2c1c4f98aed340fd311ab7d1fe51a1c2e879fde0eb74695e3d10b33d62cc5086
         WrkzQdNHHbjcLRmmnmCVGHEew2YuXAZwgLULczrHRdrRbB7D23QWHA63mCgQ2Vdzxca6v5FEwodPCExuJuAmtVj86gVkjzSFJF
index 2  secret b5f66b6627238ac68d776a1319f785243feb071a3a11cf611c6bc69e0b40a20e
         public 560f3e3b47ffd155f6a42b9764464b751f63f2269dc22e775dc67c543097c456
         WrkzVMsEUdkjnkdnZnBChhCkwwLZ3CgoWZUodFDEc9yRSPL77sKUkRt3mCgQ2Vdzxca6v5FEwodPCExuJuAmtVj86gVkp2zfWK
```

The two `syncStartTimestamp` values in that file are the wall clock at the moment
the subwallets were created, so it is the one fixture whose bytes cannot be
reproduced from the seed alone. Nothing in the tests depends on their value.

## The other direction

The fixtures prove we read what the C++ writes. The reverse was checked by hand
with the same binary on 2026-09-09, and is repeatable with the ignored test
`interop_write_a_wallet_for_the_cpp_wallet_to_open` in `../wallet_file.rs`: it
writes a wallet with two subwallets, an unspent input with a cached
`privateEphemeral`, an unconfirmed incoming amount, a transaction with a payment
id, a `txPrivateKeys` entry and a block hash checkpoint. `wrkz-wallet-api`
opened that file (HTTP 200), reported both addresses, the transaction with its
payment id and transfer, and balances of 1,234,567 unlocked / 500 locked — and
its own `POST /export/json` of it came back **byte-identical** to the JSON this
port wrote.
