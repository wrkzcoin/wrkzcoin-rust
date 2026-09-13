// Copyright (c) 2026, The WrkzCoin developers
//
// Please see the included LICENSE file for more information

//! `src/utilities/FormatTools.cpp`: the amount, percentage, hashrate and date
//! strings the interface prints.

use wrkz_primitives::constants::CRYPTONOTE_DISPLAY_DECIMAL_POINT;

/// `WalletConfig::ticker`.
pub const TICKER: &str = "WRKZ";

/// `WalletConfig::daemonName`.
pub const DAEMON_NAME: &str = "Wrkzd";

/// `WalletConfig::walletName`.
pub const WALLET_NAME: &str = "zedwallet";

/// `WalletConfig::csvFilename`.
pub const CSV_FILENAME: &str = "transactions.csv";

/// `WalletConfig::addressBookFilename`.
pub const ADDRESS_BOOK_FILENAME: &str = ".addressBook.json";

/// `WalletConfig::contactLink`.
pub const CONTACT_LINK: &str = "https://chat.wrkz.work";

/// `WalletConfig::numDecimalPlaces`.
pub const DECIMAL_PLACES: u32 = CRYPTONOTE_DISPLAY_DECIMAL_POINT;

/// `Utilities::getDivisor`.
pub fn divisor() -> u64 {
    10u64.pow(DECIMAL_PLACES)
}

/// `Utilities::formatDollars`: comma separators every three digits, which the
/// C++ gets from a hand-rolled `std::numpunct` rather than the user's locale.
pub fn format_dollars(amount: u64) -> String {
    let digits = amount.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// `Utilities::formatCents`: zero padded to the decimal places.
pub fn format_cents(amount: u64) -> String {
    format!("{:0width$}", amount, width = DECIMAL_PLACES as usize)
}

/// `Utilities::formatAmount`: `1,234.56 WRKZ`.
pub fn format_amount(amount: u64) -> String {
    let d = divisor();
    format!("{}.{} {TICKER}", format_dollars(amount / d), format_cents(amount % d))
}

/// `Utilities::formatAmountBasic`: no separators, no ticker — the CSV column.
pub fn format_amount_basic(amount: u64) -> String {
    let d = divisor();
    format!("{}.{}", amount / d, format_cents(amount % d))
}

/// `Utilities::get_sync_percentage`: two decimal places, never 100 unless it
/// really is, never above 100.
pub fn sync_percentage(height: u64, target_height: u64) -> String {
    if height == 0 || target_height == 0 {
        return "0.00".to_string();
    }
    let height = height.min(target_height);
    let mut percent = 100.0f64 * height as f64 / target_height as f64;
    if height < target_height && percent > 99.99 {
        percent = 99.99;
    }
    format!("{percent:.2}")
}

/// `Utilities::get_mining_speed`.
pub fn mining_speed(hashrate: u64) -> String {
    let h = hashrate as f64;
    if h > 1e9 {
        format!("{:.2} GH/s", h / 1e9)
    } else if h > 1e6 {
        format!("{:.2} MH/s", h / 1e6)
    } else if h > 1e3 {
        format!("{:.2} KH/s", h / 1e3)
    } else {
        format!("{h:.2} H/s")
    }
}

/// `Utilities::unixTimeToDate`: `%F %R`, i.e. `2024-01-31 15:04`.
///
/// The C++ uses `std::localtime`. There is no timezone database here and
/// nothing to read one with, so this is UTC and says so nowhere the C++ does
/// not — a wallet showing a timestamp an hour out is a cosmetic difference, and
/// inventing a timezone would be worse.
pub fn unix_time_to_date(timestamp: u64) -> String {
    let (year, month, day, hour, minute) = civil_from_unix(timestamp);
    format!("{year:04}-{month:02}-{day:02} {hour:02}:{minute:02}")
}

/// Howard Hinnant's `civil_from_days`, which is exact for every day this can
/// see and needs no table.
fn civil_from_unix(timestamp: u64) -> (i64, u32, u32, u32, u32) {
    let secs = timestamp as i64;
    let days = secs.div_euclid(86_400);
    let secs_of_day = secs.rem_euclid(86_400);

    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if m <= 2 { y + 1 } else { y };

    (year, m as u32, d as u32, (secs_of_day / 3600) as u32, ((secs_of_day % 3600) / 60) as u32)
}

/// Parse a user-typed amount into atomic units, the way
/// `getAmountToAtomic` does: commas removed, at most
/// [`DECIMAL_PLACES`] decimals, then read as an integer.
///
/// `Err` carries the message the C++ prints for that failure.
pub fn parse_amount(input: &str) -> Result<u64, String> {
    let cleaned: String = input.chars().filter(|c| *c != ',').collect();
    let cleaned = cleaned.trim();

    let decimal_length = match cleaned.rfind('.') {
        Some(pos) => cleaned.len() - pos - 1,
        None => 0,
    };

    if decimal_length > DECIMAL_PLACES as usize {
        return Err(format!(
            "{} transfers can have a max of {DECIMAL_PLACES} decimal places.\n",
            wrkz_primitives::constants::CRYPTONOTE_NAME
        ));
    }

    let mut digits: String = cleaned.chars().filter(|c| *c != '.').collect();
    for _ in decimal_length..DECIMAL_PLACES as usize {
        digits.push('0');
    }

    if digits.is_empty() || !digits.bytes().all(|c| c.is_ascii_digit()) {
        return Err("Failed to parse amount! Ensure you entered the value correctly.\n".to_string());
    }

    digits.parse::<u64>().map_err(|_| "Input is too large or too small!".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn amounts_print_with_separators_and_two_decimals() {
        assert_eq!(format_amount(0), "0.00 WRKZ");
        assert_eq!(format_amount(5), "0.05 WRKZ");
        assert_eq!(format_amount(1_234_567), "12,345.67 WRKZ");
        assert_eq!(format_amount_basic(1_234_567), "12345.67");
    }

    #[test]
    fn an_amount_round_trips_through_the_prompt_parser() {
        assert_eq!(parse_amount("1000"), Ok(100_000));
        assert_eq!(parse_amount("12,345.67"), Ok(1_234_567));
        assert_eq!(parse_amount("0.05"), Ok(5));
        assert_eq!(parse_amount("1.5"), Ok(150));
        assert!(parse_amount("1.234").is_err());
        assert!(parse_amount("abc").is_err());
    }

    #[test]
    fn the_sync_percentage_never_reads_a_hundred_early() {
        assert_eq!(sync_percentage(0, 100), "0.00");
        assert_eq!(sync_percentage(100, 0), "0.00");
        assert_eq!(sync_percentage(50, 100), "50.00");
        assert_eq!(sync_percentage(999_999, 1_000_000), "99.99");
        assert_eq!(sync_percentage(1_000_000, 1_000_000), "100.00");
        // Above the target is clamped, as `get_sync_percentage` clamps it.
        assert_eq!(sync_percentage(1_000_001, 1_000_000), "100.00");
    }

    #[test]
    fn hashrate_picks_the_right_unit() {
        assert_eq!(mining_speed(500), "500.00 H/s");
        assert_eq!(mining_speed(1_500), "1.50 KH/s");
        assert_eq!(mining_speed(2_500_000), "2.50 MH/s");
        assert_eq!(mining_speed(3_000_000_000), "3.00 GH/s");
    }

    #[test]
    fn dates_match_strftime_f_r() {
        assert_eq!(unix_time_to_date(0), "1970-01-01 00:00");
        // The WrkzCoin genesis timestamp, in UTC.
        assert_eq!(unix_time_to_date(1_529_831_318), "2018-06-24 09:08");
        assert_eq!(unix_time_to_date(1_700_000_000), "2023-11-14 22:13");
    }
}
