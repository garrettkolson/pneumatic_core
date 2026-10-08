//! Shared CLI helpers for this package's binaries (`pneumatic_testnet_gen`,
//! `mesh-probe`).
//!
//! One implementation, because two copies of flag parsing drift: the same shape
//! of problem as the duplicated listen-IP rule that let one binary bind loopback
//! while the other did not.

/// One CLI flag value (`--flag value` or `--flag=value`), same convention as the
/// data service binary.
pub fn flag(args: &[String], name: &str) -> Option<String> {
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        if let Some(rest) = arg.strip_prefix(&format!("--{name}=")) {
            return Some(rest.to_string());
        }
        if arg == &format!("--{name}") {
            return iter.next().map(|value| value.to_string());
        }
    }
    None
}

/// A flag parsed into a number, with the flag name in the error so the operator
/// sees which one they mistyped.
pub fn parsed<T: std::str::FromStr>(args: &[String], name: &str) -> Result<Option<T>, String> {
    match flag(args, name) {
        None => Ok(None),
        Some(raw) => raw
            .parse::<T>()
            .map(Some)
            .map_err(|_| format!("--{name} expected a number, got {raw:?}")),
    }
}

/// Every occurrence of a repeatable string flag (`--flag a --flag b`), in
/// command-line order. Repeatable because a genesis with two test tokens is
/// one keystroke more than a flag returning only the first value would force
/// into two runs — and a silently-dropped second `--tx-token` is exactly the
/// quiet surprise the generator's own docs say it exists to remove.
pub fn flag_all(args: &[String], name: &str) -> Vec<String> {
    let mut values = Vec::new();
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        if let Some(rest) = arg.strip_prefix(&format!("--{name}=")) {
            values.push(rest.to_string());
        } else if arg == &format!("--{name}") {
            if let Some(value) = iter.next() {
                values.push(value.clone());
            }
        }
    }
    values
}
