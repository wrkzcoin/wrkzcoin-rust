// Copyright (c) 2026, The WrkzCoin developers
//
// Please see the included LICENSE file for more information

//! Throwaway keys for a simnet: somewhere to mine to, and the keys a wallet
//! imports to spend what was mined. Nothing here is for mainnet funds.

use wrkz_pow::curve;
use wrkz_primitives::base58;

/// A spend key, the view key derived from it, and their address.
#[derive(Clone, Debug)]
pub struct SimKeys {
    pub spend_secret: [u8; 32],
    pub spend_public: [u8; 32],
    pub view_secret: [u8; 32],
    pub view_public: [u8; 32],
    /// The standard `Wrkz…` address. A simnet uses mainnet's address prefix,
    /// so this is also a valid mainnet address — whose coins exist only on
    /// the simnet.
    pub address: String,
}

impl SimKeys {
    /// Fresh random keys.
    pub fn random() -> Self {
        let (spend_secret, spend_public) = curve::generate_keys();
        let (view_secret, view_public) = curve::generate_view_from_spend(&spend_secret);
        let address = base58::standard_address(&spend_public, &view_public);
        SimKeys { spend_secret, spend_public, view_secret, view_public, address }
    }

    pub fn spend_secret_hex(&self) -> String {
        hex::encode(self.spend_secret)
    }

    pub fn view_secret_hex(&self) -> String {
        hex::encode(self.view_secret)
    }
}
