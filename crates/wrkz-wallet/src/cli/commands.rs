// Copyright (c) 2026, The WrkzCoin developers
//
// Please see the included LICENSE file for more information

//! The command tables of `src/zedwallet++/Commands.cpp`, and the menu parser
//! of `Menu.h`'s `parseCommand`.

use super::format::TICKER;
use super::term::{information, success, warning, Terminal};

/// `Command` (`Commands.h:11`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Command {
    pub name: &'static str,
    pub description: String,
}

/// `AdvancedCommand` (`Commands.h:21`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AdvancedCommand {
    pub name: &'static str,
    pub description: String,
    /// Whether a view-only wallet may run it.
    pub view_wallet_support: bool,
    /// Whether it is in the advanced half of the list. Nothing filters on this
    /// any more — `help` and `advanced` both print every command — but it is
    /// what the C++ records.
    pub advanced: bool,
}

fn cmd(name: &'static str, description: &str) -> Command {
    Command { name, description: description.to_string() }
}

fn adv(name: &'static str, description: String, view: bool, advanced: bool) -> AdvancedCommand {
    AdvancedCommand { name, description, view_wallet_support: view, advanced }
}

/// `startupCommands()` (`Commands.cpp:13`).
pub fn startup_commands() -> Vec<Command> {
    vec![
        cmd("open", "Open a wallet already on your system"),
        cmd("create", "Create a new wallet"),
        cmd("seed_restore", "Restore a wallet using a seed phrase of words"),
        cmd("key_restore", "Restore a wallet using a view and spend key"),
        cmd("view_wallet", "Import a view only wallet"),
        cmd("exit", "Exit the program"),
    ]
}

/// `nodeDownCommands()` (`Commands.cpp:25`).
pub fn node_down_commands() -> Vec<Command> {
    vec![
        cmd("try_again", "Try to connect to the node again"),
        cmd("continue", "Continue to the wallet interface regardless"),
        cmd("swap_node", "Specify a new daemon address/port to connect to"),
        cmd("exit", "Exit the program"),
    ]
}

/// `allCommands()` (`Commands.cpp:35`), in its order.
pub fn all_commands() -> Vec<AdvancedCommand> {
    vec![
        adv("help", "[General] List all commands".into(), true, false),
        adv("advanced", "[General] Alias for help (list all commands)".into(), true, false),
        adv("exit", "[General] Exit and save your wallet".into(), true, false),
        adv("status", "[Wallet/Network Info] Display sync status and network hashrate".into(), true, true),
        adv("refresh", "[Wallet/Network Info] Retry syncing with daemon now".into(), true, false),
        adv("swap_node", "[Wallet/Network Info] Specify a new daemon address/port to sync from".into(), true, true),
        adv("address", "[Wallet Info] Display your payment address".into(), true, false),
        adv("addr", "[Wallet Info] Alias for address".into(), true, true),
        adv("balance", format!("[Wallet Info] Display how much {TICKER} you have"), true, false),
        adv("bal", "[Wallet Info] Alias for balance".into(), true, true),
        adv("incoming_transfers", "[Transactions] Show incoming transfers".into(), true, true),
        adv("in", "[Transactions] Alias for incoming_transfers".into(), true, true),
        adv("outgoing_transfers", "[Transactions] Show outgoing transfers".into(), false, true),
        adv("out", "[Transactions] Alias for outgoing_transfers".into(), false, true),
        adv("list_transfers", "[Transactions] Show all transfers".into(), false, true),
        adv("txs", "[Transactions] Show all transfers in one-line format".into(), false, true),
        adv("txs_full", "[Transactions] Show all transfers with full details".into(), false, true),
        adv("transfer", format!("[Transactions] Send {TICKER} to someone"), false, false),
        adv(
            "ab_send",
            format!("[Transactions / Address Book] Send {TICKER} to someone in your address book"),
            false,
            true,
        ),
        adv("send_all", "[Transactions] Send all your balance to someone".into(), false, true),
        adv(
            "sweep",
            "[Transactions] Sweep a specific amount to an address in multiple transactions (no fusion)".into(),
            false,
            true,
        ),
        adv(
            "sweep_all",
            "[Transactions] Sweep entire balance to an address in multiple transactions (no fusion)".into(),
            false,
            true,
        ),
        adv("get_tx_private_key", "[Transactions] Get the private key of a transaction".into(), true, true),
        adv(
            "check_tx",
            "[Transactions] Check wallet + node status for a tx hash (use: check_tx <hash>)".into(),
            true,
            true,
        ),
        adv(
            "decode_integrated",
            "[Transactions] Decode integrated address to standard address + payment ID".into(),
            true,
            true,
        ),
        adv("ab_add", "[Address Book] Add a person to your address book".into(), true, true),
        adv("ab_delete", "[Address Book] Delete a person in your address book".into(), true, true),
        adv("ab_list", "[Address Book] List everyone in your address book".into(), true, true),
        adv(
            "make_integrated_address",
            "[Address / Payment Tools] Make a combined address + payment ID".into(),
            true,
            true,
        ),
        adv("backup", "[Security & Recovery] Backup your private keys and/or seed".into(), true, false),
        adv("change_password", "[Security] Change your wallet password".into(), true, true),
        adv("save", "[Maintenance] Save your wallet state".into(), true, true),
        adv("save_csv", "[Export] Save all wallet transactions to a CSV file".into(), true, true),
        adv("reset", "[Maintenance] Recheck the chain from zero for transactions".into(), true, true),
        adv("set_log_level", "[Maintenance] Alter the logging level".into(), true, true),
    ]
}

/// `allViewWalletCommands()`: every command a view wallet may run.
pub fn all_view_wallet_commands() -> Vec<AdvancedCommand> {
    all_commands().into_iter().filter(|c| c.view_wallet_support).collect()
}

/// `basicCommands()` / `advancedCommands()`, kept because the C++ has them even
/// though nothing calls them since `help` started printing the whole list.
pub fn basic_commands() -> Vec<AdvancedCommand> {
    all_commands().into_iter().filter(|c| !c.advanced).collect()
}

/// See [`basic_commands`].
pub fn advanced_commands() -> Vec<AdvancedCommand> {
    all_commands().into_iter().filter(|c| c.advanced).collect()
}

/// `setLogLevel`'s own little menu (`CommandImplementations.cpp:985`).
pub fn log_levels() -> Vec<Command> {
    vec![
        cmd("Trace", "Display extremely detailed logging output"),
        cmd("Debug", "Display highly detailed logging output"),
        cmd("Info", "Display detailed logging output"),
        cmd("Warning", "Display only warning and error logging output"),
        cmd("Fatal", "Display only error logging output"),
        cmd("Disabled", "Don't display any logging output"),
    ]
}

////////////////////////
/* PRINTING           */
////////////////////////

/// Anything that can appear in a numbered menu.
pub trait Listable {
    fn command_name(&self) -> &str;
    fn description(&self) -> &str;
}

impl Listable for Command {
    fn command_name(&self) -> &str {
        self.name
    }
    fn description(&self) -> &str {
        &self.description
    }
}

impl Listable for AdvancedCommand {
    fn command_name(&self) -> &str {
        self.name
    }
    fn description(&self) -> &str {
        &self.description
    }
}

/// `printCommands` (`Menu.h:129`): a blank line, then ` N\t<name padded to 25><description>`,
/// then a blank line.
pub fn print_commands<T: Listable>(term: &mut dyn Terminal, commands: &[T]) {
    term.write("\n");
    for (i, command) in commands.iter().enumerate() {
        let name = format!("{:<25}", command.command_name());
        term.line(&format!(
            "{}{}\t{}{}",
            information(" "),
            information((i + 1).to_string()),
            success(name),
            command.description()
        ));
    }
    term.write("\n");
}

////////////////////////
/* PARSING            */
////////////////////////

/// What [`parse_command`] decided.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Selection {
    /// A command name, with any inline argument still attached — `check_tx` and
    /// `decode_integrated` keep theirs, and their argument keeps its case.
    Command(String),
    /// End of input, which the C++ reads as `exit`.
    Exit,
}

/// `parseCommand` (`Menu.h:26`): a number is one-based into the list, a name is
/// matched case-insensitively, and anything else reprints the menu and asks
/// again.
pub fn parse_command<T: Listable>(
    term: &mut dyn Terminal,
    printable: &[T],
    available: &[T],
    prompt: &str,
) -> Selection {
    loop {
        term.write(&information(prompt));
        term.flush();

        let Some(raw) = term.read_line() else {
            // Ctrl+C or a closed stdin.
            return Selection::Exit;
        };
        let raw = raw.trim().to_string();
        let lower = raw.to_lowercase();

        if lower.is_empty() {
            continue;
        }

        if lower == "exit" {
            return Selection::Command("exit".to_string());
        }

        // A number selects by position. `std::stoi` accepts a leading number
        // with trailing junk; this is stricter, and a name is tried next
        // anyway.
        if let Ok(n) = lower.parse::<i64>() {
            let index = n - 1;
            if index < 0 || index >= available.len() as i64 {
                term.line(&format!(
                    "{}{}{}{}{}",
                    warning("Bad input, expected a command name, "),
                    warning("or number from "),
                    information("1"),
                    warning(" to "),
                    information(available.len().to_string())
                ));
                print_commands(term, printable);
                continue;
            }
            return Selection::Command(available[index as usize].command_name().to_string());
        }

        let (name_raw, argument) = match raw.find(' ') {
            Some(pos) => (&raw[..pos], Some(&raw[pos..])),
            None => (raw.as_str(), None),
        };
        let name = name_raw.to_lowercase();

        let Some(found) = available.iter().find(|c| c.command_name().to_lowercase() == name) else {
            term.line(&format!("Unknown command: {}", warning(&lower)));
            print_commands(term, printable);
            continue;
        };

        // Only these two take an inline argument, and its case is preserved.
        if let Some(argument) = argument {
            if name == "check_tx" || name == "decode_integrated" {
                return Selection::Command(format!("{name}{argument}"));
            }
        }

        return Selection::Command(found.command_name().to_lowercase());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::term::ScriptedTerminal;

    #[test]
    fn the_command_list_is_the_cpp_list() {
        let names: Vec<&str> = all_commands().iter().map(|c| c.name).collect();
        assert_eq!(
            names,
            vec![
                "help",
                "advanced",
                "exit",
                "status",
                "refresh",
                "swap_node",
                "address",
                "addr",
                "balance",
                "bal",
                "incoming_transfers",
                "in",
                "outgoing_transfers",
                "out",
                "list_transfers",
                "txs",
                "txs_full",
                "transfer",
                "ab_send",
                "send_all",
                "sweep",
                "sweep_all",
                "get_tx_private_key",
                "check_tx",
                "decode_integrated",
                "ab_add",
                "ab_delete",
                "ab_list",
                "make_integrated_address",
                "backup",
                "change_password",
                "save",
                "save_csv",
                "reset",
                "set_log_level",
            ]
        );
    }

    #[test]
    fn a_view_wallet_loses_exactly_the_spending_commands() {
        let full: Vec<&str> = all_commands().iter().map(|c| c.name).collect();
        let view: Vec<&str> = all_view_wallet_commands().iter().map(|c| c.name).collect();
        let missing: Vec<&str> = full.iter().copied().filter(|n| !view.contains(n)).collect();
        assert_eq!(
            missing,
            vec![
                "outgoing_transfers",
                "out",
                "list_transfers",
                "txs",
                "txs_full",
                "transfer",
                "ab_send",
                "send_all",
                "sweep",
                "sweep_all",
            ]
        );
    }

    #[test]
    fn the_startup_menu_is_the_cpp_menu() {
        let names: Vec<&str> = startup_commands().iter().map(|c| c.name).collect();
        assert_eq!(names, vec!["open", "create", "seed_restore", "key_restore", "view_wallet", "exit"]);
        let names: Vec<&str> = node_down_commands().iter().map(|c| c.name).collect();
        assert_eq!(names, vec!["try_again", "continue", "swap_node", "exit"]);
    }

    #[test]
    fn a_number_selects_by_position() {
        let commands = startup_commands();
        let mut term = ScriptedTerminal::new(["2"]);
        assert_eq!(
            parse_command(&mut term, &commands, &commands, "What would you like to do?: "),
            Selection::Command("create".into())
        );
    }

    #[test]
    fn a_name_is_matched_case_insensitively() {
        let commands = all_commands();
        let mut term = ScriptedTerminal::new(["BALANCE"]);
        assert_eq!(parse_command(&mut term, &commands, &commands, "> "), Selection::Command("balance".into()));
    }

    #[test]
    fn an_unknown_command_reprints_the_menu_and_asks_again() {
        let commands = startup_commands();
        let mut term = ScriptedTerminal::new(["nonsense", "open"]);
        assert_eq!(parse_command(&mut term, &commands, &commands, "> "), Selection::Command("open".into()));
        assert!(term.output.contains("Unknown command: nonsense"));
        assert!(term.output.contains("Open a wallet already on your system"));
    }

    #[test]
    fn a_number_out_of_range_says_the_range() {
        let commands = startup_commands();
        let mut term = ScriptedTerminal::new(["99", "exit"]);
        assert_eq!(parse_command(&mut term, &commands, &commands, "> "), Selection::Command("exit".into()));
        assert!(term.output.contains("Bad input, expected a command name, or number from 1 to 6"));
    }

    #[test]
    fn two_commands_keep_their_inline_argument() {
        let commands = all_commands();
        let mut term = ScriptedTerminal::new(["check_tx ABCdef123"]);
        assert_eq!(
            parse_command(&mut term, &commands, &commands, "> "),
            Selection::Command("check_tx ABCdef123".into())
        );

        // Everything else drops it, as the C++ does.
        let mut term = ScriptedTerminal::new(["balance now"]);
        assert_eq!(parse_command(&mut term, &commands, &commands, "> "), Selection::Command("balance".into()));
    }

    #[test]
    fn end_of_input_is_exit() {
        let commands = startup_commands();
        let mut term = ScriptedTerminal::new(Vec::<String>::new());
        assert_eq!(parse_command(&mut term, &commands, &commands, "> "), Selection::Exit);
    }
}
