// Copyright (c) 2026, The WrkzCoin developers
//
// Please see the included LICENSE file for more information

//! `src/zedwallet++/AddressBook.cpp`: a JSON array in `.addressBook.json` in
//! the working directory, with `friendlyName`, `address` and `paymentID`.
//!
//! The file is read and written with the same shape the C++ writes
//! (`nlohmann::json` at `setw(2)`), so the two wallets share one address book.

use wrkz_rpc::json::{Json, Obj, ParseLimits};

use super::term::{information, success, warning, Terminal};

/// `AddressBookEntry` (`AddressBook.h:12`).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AddressBookEntry {
    pub friendly_name: String,
    pub address: String,
    pub payment_id: String,
}

impl AddressBookEntry {
    fn to_json(&self) -> Json {
        let mut o = Obj::new();
        o.set("friendlyName", self.friendly_name.clone())
            .set("address", self.address.clone())
            .set("paymentID", self.payment_id.clone());
        o.build()
    }

    /// `fromJSON`: every field required, and a missing one skips the entry.
    fn from_json(value: &Json) -> Option<AddressBookEntry> {
        Some(AddressBookEntry {
            friendly_name: value.get("friendlyName")?.as_str()?.to_string(),
            address: value.get("address")?.as_str()?.to_string(),
            payment_id: value.get("paymentID")?.as_str()?.to_string(),
        })
    }
}

/// `getAddressBook()` (`AddressBook.cpp:319`): a missing file is an empty book;
/// a malformed one warns and is treated as empty; a malformed *entry* warns and
/// is skipped.
pub fn load(term: &mut dyn Terminal, path: &str) -> Vec<AddressBookEntry> {
    let Ok(bytes) = std::fs::read(path) else {
        return Vec::new();
    };

    let Ok(value) = wrkz_rpc::json::parse(&bytes, ParseLimits::default()) else {
        term.line(&warning("Failed to parse address book JSON. Using empty address book."));
        return Vec::new();
    };

    let Some(items) = value.as_array() else {
        term.line(&warning("Address book file has invalid format. Using empty address book."));
        return Vec::new();
    };

    let mut book = Vec::with_capacity(items.len());
    for item in items {
        if !item.is_object() {
            term.line(&warning("Skipping invalid address book entry (expected JSON object)."));
            continue;
        }
        match AddressBookEntry::from_json(item) {
            Some(entry) => book.push(entry),
            None => term.line(&warning("Skipping malformed address book entry.")),
        }
    }
    book
}

/// `saveAddressBook()` (`AddressBook.cpp:368`).
pub fn save(term: &mut dyn Terminal, path: &str, book: &[AddressBookEntry]) -> bool {
    let array = Json::Array(book.iter().map(AddressBookEntry::to_json).collect());
    // `output << std::setw(2) << arr` — two-space indentation, no trailing
    // newline.
    let text = indent_two(&array);

    if std::fs::write(path, text.as_bytes()).is_err() {
        term.line(&warning("Failed to save address book to disk!"));
        term.line(&format!(
            "{}{}",
            warning("Check you are able to write files to your "),
            warning("current directory.")
        ));
        return false;
    }
    true
}

/// `nlohmann::json` at `setw(2)`.
fn indent_two(value: &Json) -> String {
    // The pretty printer is fixed at four spaces (that is what the API needs);
    // halving its indentation is exact, because it only ever emits runs of four
    // spaces at the start of a line.
    let four = crate::api::pretty::dump(value);
    four.lines()
        .map(|line| {
            let leading = line.len() - line.trim_start_matches(' ').len();
            format!("{}{}", " ".repeat(leading / 2), &line[leading..])
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// `isAddressBookEmpty` (`AddressBook.cpp:264`).
pub fn is_empty(term: &mut dyn Terminal, book: &[AddressBookEntry]) -> bool {
    if book.is_empty() {
        term.line(&format!("{}{}", warning("Your address book is empty! Add some people "), warning("to it first.")));
        return true;
    }
    false
}

/// `listAddressBook()` (`AddressBook.cpp:333`).
pub fn list(term: &mut dyn Terminal, book: &[AddressBookEntry]) {
    if is_empty(term, book) {
        return;
    }

    for (i, entry) in book.iter().enumerate() {
        term.line(&format!(
            "{}{}{}{}",
            information("Address Book Entry: "),
            information((i + 1).to_string()),
            information(" | "),
            success(&entry.friendly_name)
        ));
        term.line(&format!("{}{}", information("Address: "), success(&entry.address)));
        if entry.payment_id.is_empty() {
            term.write("\n");
        } else {
            term.line(&format!("{}{}", information("Payment ID: "), success(&entry.payment_id)));
            term.write("\n");
        }
    }
}

/// `getAddressBookEntry` (`AddressBook.cpp:159`): a name or a one-based number,
/// and `cancel` to give up.
pub fn pick(term: &mut dyn Terminal, book: &[AddressBookEntry]) -> Option<AddressBookEntry> {
    loop {
        term.write(&information("Who do you want to send to from your "));
        term.write(&information("address book?: "));
        term.flush();

        let name = term.read_line()?.trim().to_string();

        if name.is_empty() {
            continue;
        }
        if name == "cancel" {
            return None;
        }

        if let Ok(n) = name.parse::<i64>() {
            let index = n - 1;
            if index < 0 || index >= book.len() as i64 {
                term.line(&format!(
                    "{}{}{}{}{}",
                    warning("Bad input, expected a friendly name, "),
                    warning("or number from "),
                    information("1"),
                    warning(" to "),
                    information(book.len().to_string())
                ));
                term.write("\n");
                continue;
            }
            return Some(book[index as usize].clone());
        }

        if let Some(entry) = book.iter().find(|e| e.friendly_name == name) {
            return Some(entry.clone());
        }

        term.write("\n");
        term.line(&format!(
            "{}{}{}",
            warning("Could not find a user with the name of "),
            information(&name),
            warning(" in your address book!")
        ));
        term.write("\n");

        if super::prompt::confirm(term, "Would you like to list everyone in your address book?") {
            term.write("\n");
            list(term, book);
        } else {
            term.write("\n");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::term::ScriptedTerminal;

    fn entry(name: &str) -> AddressBookEntry {
        AddressBookEntry { friendly_name: name.into(), address: "Wrkz…".into(), payment_id: String::new() }
    }

    #[test]
    fn the_file_shape_is_the_cpp_shape() {
        let book = [AddressBookEntry {
            friendly_name: "bob".into(),
            address: "WrkzBob".into(),
            payment_id: "0102030405060708".into(),
        }];
        let text = indent_two(&Json::Array(book.iter().map(AddressBookEntry::to_json).collect()));
        assert_eq!(
            text,
            [
                "[",
                "  {",
                "    \"address\": \"WrkzBob\",",
                "    \"friendlyName\": \"bob\",",
                "    \"paymentID\": \"0102030405060708\"",
                "  }",
                "]",
            ]
            .join("\n")
        );
    }

    #[test]
    fn an_empty_book_is_reported_once() {
        let mut term = ScriptedTerminal::new(Vec::<String>::new());
        assert!(is_empty(&mut term, &[]));
        assert!(term.output.contains("Your address book is empty!"));
    }

    #[test]
    fn picking_accepts_a_number_or_a_name() {
        let book = vec![entry("alice"), entry("bob")];
        let mut term = ScriptedTerminal::new(["2"]);
        assert_eq!(pick(&mut term, &book).map(|e| e.friendly_name), Some("bob".into()));

        let mut term = ScriptedTerminal::new(["alice"]);
        assert_eq!(pick(&mut term, &book).map(|e| e.friendly_name), Some("alice".into()));

        let mut term = ScriptedTerminal::new(["cancel"]);
        assert!(pick(&mut term, &book).is_none());
    }

    #[test]
    fn an_unknown_name_offers_the_list() {
        let book = vec![entry("alice")];
        let mut term = ScriptedTerminal::new(["nobody", "y", "alice"]);
        assert_eq!(pick(&mut term, &book).map(|e| e.friendly_name), Some("alice".into()));
        assert!(term.output.contains("Could not find a user with the name of nobody in your address book!"));
        assert!(term.output.contains("Address Book Entry: 1 | alice"));
    }
}
