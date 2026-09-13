// Copyright (c) 2026, The WrkzCoin developers
//
// Please see the included LICENSE file for more information

//! The prompts of `src/zedwallet++/GetInput.cpp`, `Utilities::confirm`
//! (`utilities/Input.cpp:14`) and `ZedUtilities::getScanHeight`
//! (`zedwallet++/Utilities.cpp:30`).
//!
//! Each keeps asking until the answer is usable, and each returns
//! [`Answer::Cancel`] on `cancel` or on end of input — the C++'s "fixes
//! infinite looping when someone does a ctrl + c".

use wrkz_primitives::base58::Base58Error;

use super::format::{format_amount, TICKER};
use super::term::{information, success, warning, Terminal};
use crate::file::{SecretKey, WalletError};
use crate::transfer::validate_payment_id;

/// A prompt either answered or cancelled.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Answer<T> {
    Given(T),
    Cancel,
}

impl<T> Answer<T> {
    pub fn is_cancel(&self) -> bool {
        matches!(self, Answer::Cancel)
    }
}

/// `Utilities::confirm(msg)`: `(Y/n)`, defaulting to yes.
pub fn confirm(term: &mut dyn Terminal, msg: &str) -> bool {
    confirm_default(term, msg, true)
}

/// `Utilities::confirm(msg, defaultToYes)`. The upper-case letter is the
/// default, which is what hitting enter gives.
pub fn confirm_default(term: &mut dyn Terminal, msg: &str, default_to_yes: bool) -> bool {
    let suffix = if default_to_yes { " (Y/n): " } else { " (y/N): " };
    loop {
        term.write(&information(format!("{msg}{suffix}")));
        term.flush();

        let Some(answer) = term.read_line() else {
            // No more input; take the default rather than loop for ever.
            return default_to_yes;
        };

        match answer.chars().next().map(|c| c.to_ascii_lowercase()) {
            None => return default_to_yes,
            Some('y') => return true,
            Some('n') => return false,
            Some(_) => {
                term.line(&format!(
                    "{}{}{}",
                    warning("Bad input: "),
                    information(&answer),
                    warning(" - please enter either Y or N.")
                ));
            }
        }
    }
}

/// `getAddress(msg, integratedAddressesAllowed, cancelAllowed)`
/// (`GetInput.cpp:107`), including the three tips it prints for the three
/// common mistakes.
pub fn get_address(
    term: &mut dyn Terminal,
    msg: &str,
    integrated_allowed: bool,
    cancel_allowed: bool,
) -> Answer<String> {
    loop {
        term.write(&information(msg));
        term.flush();

        let Some(address) = term.read_line() else {
            return Answer::Cancel;
        };
        let address = address.trim().to_string();

        if address.is_empty() {
            continue;
        }
        if address == "cancel" && cancel_allowed {
            return Answer::Cancel;
        }

        match validate_address(&address, integrated_allowed) {
            Ok(()) => return Answer::Given(address),
            Err(e) => {
                term.line(&format!("{}{}", warning("Invalid address: "), warning(e.to_string())));
                print_address_tip(term, &e);
            }
        }
    }
}

/// `validateAddresses({address}, integratedAddressesAllowed)`
/// (`ValidateParameters.cpp:338`), which the library already implements in the
/// C++'s order.
pub fn validate_address(address: &str, integrated_allowed: bool) -> Result<(), WalletError> {
    crate::transfer::validate_address(address, integrated_allowed).map(|_| ())
}

fn print_address_tip(term: &mut dyn Terminal, error: &WalletError) {
    let WalletError::InvalidAddress(reason) = error else { return };
    match reason {
        Base58Error::WrongPrefix(_) => term.line(&format!(
            "{}{}{}{}.",
            information("Tip: Use a "),
            success(TICKER),
            information(" address starting with "),
            success(wrkz_primitives::constants::ADDRESS_PREFIX)
        )),
        Base58Error::WrongAddressLength(_) => term.line(&format!(
            "{}{}{}{}{}{}{}",
            information("Tip: Valid lengths are "),
            success(wrkz_primitives::constants::STANDARD_ADDRESS_LENGTH.to_string()),
            information(" (standard), "),
            success(wrkz_primitives::constants::INTEGRATED_ADDRESS_LENGTH.to_string()),
            information(" (integrated-short), "),
            success(wrkz_primitives::constants::INTEGRATED_ADDRESS_LENGTH_LONG.to_string()),
            information(" (integrated-long).")
        )),
        Base58Error::InvalidCharacter { .. } | Base58Error::InvalidLength(_) => {
            term.line(&information("Tip: Remove spaces/symbols and ensure it is pure base58 text."))
        }
        _ => {}
    }
}

/// `getPaymentID(msg, cancelAllowed)` (`GetInput.cpp:167`). An empty answer is
/// "no payment ID", which is a valid answer and not a cancellation.
pub fn get_payment_id(term: &mut dyn Terminal, msg: &str, cancel_allowed: bool) -> Answer<String> {
    loop {
        term.write(&information(msg));
        term.write(&warning(
            "\nWarning: If you were given a payment ID,\nyou MUST use it, or your funds may be lost!\n",
        ));
        term.write("Hit enter for the default of no payment ID: ");
        term.flush();

        let Some(payment_id) = term.read_line() else {
            return Answer::Cancel;
        };
        let payment_id = payment_id.trim().to_string();

        if payment_id == "cancel" && cancel_allowed {
            return Answer::Cancel;
        }
        if payment_id.is_empty() {
            return Answer::Given(payment_id);
        }

        match validate_payment_id(&payment_id) {
            Ok(()) => return Answer::Given(payment_id),
            Err(e) => {
                term.line(&format!("{}{}", warning("Invalid payment ID: "), warning(e.to_string())));
                match e {
                    WalletError::PaymentIdWrongLength(_) => {
                        term.line(&information("Tip: Payment ID must be 16 or 64 hex characters."));
                        term.line(&information("     16 is encrypted to the receiver; 64 is stored in plaintext."));
                    }
                    WalletError::PaymentIdInvalid => {
                        term.line(&information("Tip: Use only hexadecimal characters: 0-9 and a-f."))
                    }
                    _ => {}
                }
            }
        }
    }
}

/// `getHash(msg, cancelAllowed)` (`GetInput.cpp:220`).
pub fn get_hash(term: &mut dyn Terminal, msg: &str, cancel_allowed: bool) -> Answer<String> {
    loop {
        term.write(&information(msg));
        term.flush();

        let Some(hash) = term.read_line() else {
            return Answer::Cancel;
        };
        let hash = hash.trim().to_string();

        if hash == "cancel" && cancel_allowed {
            return Answer::Cancel;
        }

        match crate::api::validate_hash(&hash) {
            Ok(()) => return Answer::Given(hash),
            Err(e) => term.line(&format!("{}{}", warning("Invalid hash: "), warning(e.to_string()))),
        }
    }
}

/// `getAmountToAtomic(msg, cancelAllowed)` (`GetInput.cpp:249`), including the
/// minimum-send check.
pub fn get_amount(term: &mut dyn Terminal, msg: &str, cancel_allowed: bool) -> Answer<u64> {
    loop {
        term.write(&information(msg));
        term.flush();

        let Some(input) = term.read_line() else {
            return Answer::Cancel;
        };

        if input.is_empty() {
            continue;
        }
        let input = input.trim().to_string();
        if input == "cancel" && cancel_allowed {
            return Answer::Cancel;
        }

        match super::format::parse_amount(&input) {
            Err(message) => term.write(&warning(message)),
            Ok(amount) if amount < wrkz_primitives::constants::MINIMUM_SEND => {
                term.write(&warning(format!(
                    "The minimum send allowed is {}!\n",
                    format_amount(wrkz_primitives::constants::MINIMUM_SEND)
                )));
            }
            Ok(amount) => return Answer::Given(amount),
        }
    }
}

/// `getDaemonAddress()` (`GetInput.cpp:326`): `host`, `host:port`, an IPC
/// endpoint, or empty for the default.
///
/// After a TCP address it asks "Does this daemon support SSL?", as the C++ does
/// in a build with TLS (`GetInput.cpp:383`). An IPC address skips that
/// question: there is no TLS on a local socket.
pub fn get_daemon_address(term: &mut dyn Terminal) -> (String, u16, bool) {
    let default = ("127.0.0.1".to_string(), wrkz_primitives::constants::RPC_DEFAULT_PORT, false);

    loop {
        term.write(&information(
            "
Enter the daemon address you want to use.
You can omit the port, and it will default to ",
        ));
        term.write(&information(wrkz_primitives::constants::RPC_DEFAULT_PORT.to_string()));
        term.write(
            ".

Hit enter for the default of localhost: ",
        );
        term.flush();

        let Some(address) = term.read_line() else {
            return default;
        };
        let address = address.trim().to_string();
        if address.is_empty() {
            return default;
        }

        // An IPC endpoint is taken whole: no port, and no SSL question.
        if crate::ipc::is_ipc_address(&address) {
            if let Some(reason) = crate::ipc::unsupported_reason(&address) {
                term.write(&warning(format!(
                    "
{reason}. Try again.
"
                )));
                continue;
            }
            return (address, wrkz_primitives::constants::RPC_DEFAULT_PORT, false);
        }

        let Some((host, port)) = parse_daemon_address(&address) else {
            term.write(&warning(
                "
Invalid daemon address! Try again.
",
            ));
            continue;
        };

        // Only a build with TLS can honour a yes, so only it asks.
        let ssl = if crate::daemon::HTTPS_SUPPORTED {
            confirm_default(term, "Does this daemon support SSL?", false)
        } else {
            false
        };

        return (host, port, ssl);
    }
}

/// `Utilities::parseDaemonAddressFromString`: `host`, `host:port`, or
/// `[v6]:port`. An IPC endpoint is not one of these — see
/// [`crate::ipc::is_ipc_address`].
pub fn parse_daemon_address(address: &str) -> Option<(String, u16)> {
    let default_port = wrkz_primitives::constants::RPC_DEFAULT_PORT;

    if let Some(rest) = address.strip_prefix('[') {
        let (host, tail) = rest.split_once(']')?;
        if host.is_empty() {
            return None;
        }
        return match tail.strip_prefix(':') {
            None if tail.is_empty() => Some((host.to_string(), default_port)),
            Some(port) => Some((host.to_string(), port.parse().ok()?)),
            None => None,
        };
    }

    match address.rsplit_once(':') {
        None => Some((address.to_string(), default_port)),
        Some((host, port)) if !host.is_empty() => Some((host.to_string(), port.parse().ok()?)),
        Some(_) => None,
    }
}

/// `ZedUtilities::getScanHeight()` (`zedwallet++/Utilities.cpp:30`).
pub fn get_scan_height(term: &mut dyn Terminal) -> u64 {
    term.write("\n");
    loop {
        term.write(&information("What height would you like to begin "));
        term.write(&information("scanning your wallet from?"));
        term.write("\n\nThis can greatly speed up the initial wallet scanning process.\n\n");
        term.write(
            "If you do not know the exact height, err on the side of caution so transactions do not get missed.\n\n",
        );
        term.write(&information("Hit enter for the sub-optimal default "));
        term.write(&information("of zero: "));
        term.flush();

        let Some(input) = term.read_line() else {
            return 0;
        };
        let input: String = input.chars().filter(|c| *c != ',').collect();
        let input = input.trim();

        if input.is_empty() {
            return 0;
        }

        match input.parse::<u64>() {
            Ok(height) => return height,
            Err(_) if input.bytes().all(|c| c.is_ascii_digit()) => {
                term.write(&warning("Input is too large or too small!"));
            }
            Err(_) => {
                term.line(&format!("{}{}", warning("Failed to parse height - input is not "), warning("a number!")));
                term.write("\n");
            }
        }
    }
}

/// `getPrivateKey(outputMsg)` (`Open.cpp:270`): 64 hex characters that land on
/// the curve. Never echoed back and never logged.
pub fn get_private_key(term: &mut dyn Terminal, msg: &str) -> Answer<SecretKey> {
    loop {
        term.write(&information(msg));
        term.flush();

        let Some(input) = term.read_line() else {
            return Answer::Cancel;
        };
        let input = input.trim();

        if input.len() != 64 {
            term.write("\n");
            term.line(&format!(
                "{}{}",
                warning("Invalid private key, should be 64 "),
                warning("characters! Try again.")
            ));
            term.write("\n");
            continue;
        }

        let Some(key) = SecretKey::from_hex(input) else {
            term.line(&format!(
                "{}{}",
                warning("Invalid private key, it is not a valid "),
                warning("hex string! Try again.")
            ));
            term.write("\n");
            continue;
        };

        if wrkz_pow::curve::secret_key_to_public_key(key.as_bytes()).is_none() {
            term.write("\n");
            term.line(&format!("{}{}", warning("Invalid private key, is not on the "), warning("ed25519 curve!")));
            term.line(&format!("{}{}", warning("Probably a typo - ensure you entered "), warning("it correctly.")));
            term.write("\n");
            continue;
        }

        return Answer::Given(key);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::term::ScriptedTerminal;

    const ADDRESS: &str =
        "WrkzaiGnz9chESkaqwevEPRjtnU46TRBRY6z3EZ7fXymcE6dTbHDSWJ3mCgQ2Vdzxca6v5FEwodPCExuJuAmtVj86gVkpDFBue";

    #[test]
    fn confirm_takes_the_default_on_enter() {
        let mut term = ScriptedTerminal::new([""]);
        assert!(confirm(&mut term, "Are you sure?"));
        assert!(term.output.contains("Are you sure? (Y/n): "));

        let mut term = ScriptedTerminal::new([""]);
        assert!(!confirm_default(&mut term, "Really?", false));
        assert!(term.output.contains("Really? (y/N): "));
    }

    #[test]
    fn confirm_rejects_junk_and_asks_again() {
        let mut term = ScriptedTerminal::new(["maybe", "y"]);
        assert!(confirm(&mut term, "Is this correct?"));
        assert!(term.output.contains("Bad input: maybe - please enter either Y or N."));
    }

    #[test]
    fn an_address_prompt_rejects_a_bad_address_and_tips() {
        let mut term = ScriptedTerminal::new(["TRTLnope", ADDRESS]);
        assert_eq!(get_address(&mut term, "To?: ", true, true), Answer::Given(ADDRESS.into()));
        assert!(term.output.contains("Invalid address: "));
    }

    #[test]
    fn cancel_is_honoured_everywhere_it_is_allowed() {
        let mut term = ScriptedTerminal::new(["cancel"]);
        assert!(get_address(&mut term, "To?: ", true, true).is_cancel());
        let mut term = ScriptedTerminal::new(["cancel"]);
        assert!(get_amount(&mut term, "How much?: ", true).is_cancel());
        let mut term = ScriptedTerminal::new(["cancel"]);
        assert!(get_payment_id(&mut term, "Payment ID?", true).is_cancel());
        let mut term = ScriptedTerminal::new(["cancel"]);
        assert!(get_hash(&mut term, "Hash?: ", true).is_cancel());
    }

    #[test]
    fn an_empty_payment_id_means_none() {
        let mut term = ScriptedTerminal::new([""]);
        assert_eq!(get_payment_id(&mut term, "Payment ID?", true), Answer::Given(String::new()));
        assert!(term.output.contains("you MUST use it, or your funds may be lost!"));
    }

    #[test]
    fn an_amount_below_the_minimum_is_refused() {
        let mut term = ScriptedTerminal::new(["0.01", "10"]);
        assert_eq!(get_amount(&mut term, "How much?: ", true), Answer::Given(1000));
        assert!(term.output.contains("The minimum send allowed is 10.00 WRKZ!"));
    }

    #[test]
    fn the_scan_height_prompt_takes_commas_and_defaults_to_zero() {
        let mut term = ScriptedTerminal::new(["4,200,000"]);
        assert_eq!(get_scan_height(&mut term), 4_200_000);
        let mut term = ScriptedTerminal::new([""]);
        assert_eq!(get_scan_height(&mut term), 0);
    }

    #[test]
    fn a_daemon_address_parses_with_and_without_a_port() {
        assert_eq!(parse_daemon_address("127.0.0.1"), Some(("127.0.0.1".into(), 17856)));
        assert_eq!(parse_daemon_address("node.example:1234"), Some(("node.example".into(), 1234)));
        assert_eq!(parse_daemon_address("[::1]:1234"), Some(("::1".into(), 1234)));
        assert_eq!(parse_daemon_address("node.example:notaport"), None);
    }

    #[test]
    fn the_daemon_prompt_takes_the_default_the_address_and_an_ipc_endpoint() {
        let mut term = ScriptedTerminal::new([""]);
        assert_eq!(get_daemon_address(&mut term), ("127.0.0.1".into(), 17856, false));

        // A TCP address, then the SSL question in a build that has TLS.
        let script: Vec<&str> =
            if crate::daemon::HTTPS_SUPPORTED { vec!["node.example:1234", "n"] } else { vec!["node.example:1234"] };
        let mut term = ScriptedTerminal::new(script);
        assert_eq!(get_daemon_address(&mut term), ("node.example".into(), 1234, false));
        if crate::daemon::HTTPS_SUPPORTED {
            assert!(term.output.contains("Does this daemon support SSL? (y/N): "));
        }

        // Yes to SSL, where the build can honour it.
        if crate::daemon::HTTPS_SUPPORTED {
            let mut term = ScriptedTerminal::new(["node.example", "y"]);
            assert_eq!(get_daemon_address(&mut term), ("node.example".into(), 17856, true));
        }
    }

    #[test]
    fn an_ipc_endpoint_is_taken_whole_or_refused_with_a_reason() {
        if crate::ipc::supported() {
            let mut term = ScriptedTerminal::new(["/run/wrkz/daemon.sock"]);
            let (host, _, ssl) = get_daemon_address(&mut term);
            assert_eq!(host, "/run/wrkz/daemon.sock");
            assert!(!ssl, "a local socket is never asked about TLS");
            assert!(!term.output.contains("SSL"));
        } else {
            let mut term = ScriptedTerminal::new(["/run/wrkz/daemon.sock", ""]);
            assert_eq!(get_daemon_address(&mut term), ("127.0.0.1".into(), 17856, false));
            assert!(term.output.contains("local IPC sockets are not available on this platform. Try again."));
        }
    }

    #[test]
    fn a_private_key_prompt_never_echoes_the_key_back() {
        let secret = "243d1bb4f6adfeb83a74196e321732fc10111111111111111111111111111101";
        let mut term = ScriptedTerminal::new(["tooshort", secret]);
        let answer = get_private_key(&mut term, "Enter your private spend key: ");
        assert!(matches!(answer, Answer::Given(_)));
        assert!(!term.output.contains(secret), "the key was echoed: {}", term.output);
        assert!(term.output.contains("should be 64 characters! Try again."));
    }
}
