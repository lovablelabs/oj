// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! `oj env`: the runtime env keys of the app's running dev server, read and
//! set over its `/@oj/env`.
//!
//! `get` prints every key with the value in force; `set` and `unset` change
//! the layer that wins over the values file and print the result; `--wait`
//! returns only once every key asked about has a value, for a script that
//! starts a backend and then the page. The dev server is found as
//! `oj dev-typecheck` finds it, through the address it advertised for its
//! root.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use clap::{Args, Subcommand};

/// The command answered; with `--wait`, every key asked about has a value.
pub const OK: i32 = 0;
/// No dev server runs for the root, it runs without the keys, or it refused
/// the request.
pub const UNAVAILABLE: i32 = 1;
/// The arguments made no request (clap's own code for a usage error).
pub const USAGE: i32 = 2;
/// `--wait` ran out of time; the last status was still printed.
pub const TIMED_OUT: i32 = 3;

/// How long one request to the dev server may take.
const REQUEST: Duration = Duration::from_secs(30);

/// How often `--wait` asks again.
const POLL: Duration = Duration::from_millis(250);

#[derive(Subcommand)]
pub enum EnvCommand {
    /// Print every runtime env key (or the ones named) with the value in
    /// force, as `KEY=value` lines; a key without one as `# KEY unset`.
    Get {
        #[command(flatten)]
        opts: Opts,
        /// Only these keys. With `--wait`, only they must have a value.
        keys: Vec<String>,
    },
    /// Set values, as `KEY=VALUE` or `KEY VALUE` pairs. A set value wins
    /// over the values file's until `unset`. A value that starts with `-`
    /// is written `KEY=-value`, or after `--` (`set -- KEY -value`), with
    /// the flags before it.
    Set {
        #[command(flatten)]
        opts: Opts,
        #[arg(required = true, value_name = "KEY=VALUE")]
        pairs: Vec<String>,
    },
    /// Drop set values, so each key falls back to the values file's or the
    /// `.env` literal.
    Unset {
        #[command(flatten)]
        opts: Opts,
        #[arg(required = true)]
        keys: Vec<String>,
    },
}

#[derive(Args)]
pub struct Opts {
    /// The app root the dev server serves (default: `playground` when it
    /// holds an index.html, else the current directory).
    #[arg(long)]
    pub root: Option<PathBuf>,
    /// Print the server's JSON status (the `GET /@oj/env` body) instead of
    /// `KEY=value` lines.
    #[arg(long)]
    pub json: bool,
    /// Return once the keys have a runtime value (set, or from the values
    /// file: a `.env` literal does not count), asking again until then: the
    /// keys `get` names, else every key (after `set` too, which waits for
    /// the ones it did not set); exit 3 when `--timeout` passes first.
    #[arg(long)]
    pub wait: bool,
    /// How long `--wait` waits, in seconds (`inf` for no limit).
    #[arg(long, default_value = "30", value_name = "SECONDS")]
    pub timeout: f64,
}

/// Runs the command against the dev server for its root (`default_root`
/// when none is given) and returns the exit code.
pub async fn run(command: EnvCommand, default_root: fn() -> PathBuf) -> i32 {
    let (opts, only, updates) = match command {
        EnvCommand::Get { opts, keys } => {
            if keys.iter().any(String::is_empty) {
                eprintln!("oj env get: an empty argument names no key");
                return USAGE;
            }
            (opts, keys, None)
        }
        EnvCommand::Set { opts, pairs } => match parse_pairs(&pairs) {
            Ok(updates) => (opts, Vec::new(), Some(updates)),
            Err(e) => {
                eprintln!("oj env set: {e}");
                return USAGE;
            }
        },
        EnvCommand::Unset { opts, keys } => {
            let mut updates = BTreeMap::new();
            for key in keys {
                if key.is_empty() {
                    eprintln!("oj env unset: an empty argument names no key");
                    return USAGE;
                }
                if updates.insert(key.clone(), None).is_some() {
                    eprintln!("oj env unset: `{key}` is given twice");
                    return USAGE;
                }
            }
            (opts, Vec::new(), Some(updates))
        }
    };
    if opts.timeout.is_nan() || opts.timeout < 0.0 {
        eprintln!("oj env: --timeout takes a number of seconds, zero or more");
        return USAGE;
    }
    // None: no limit (`inf`, or more seconds than a clock holds).
    let deadline = Duration::try_from_secs_f64(opts.timeout)
        .ok()
        .and_then(|timeout| Instant::now().checked_add(timeout));
    let root = opts.root.clone().unwrap_or_else(default_root);
    // The updates go once: asked again, the command only reads.
    let mut updates = updates;
    let mut status: Option<serde_json::Value> = None;
    // Said once for each time the server is away, not for each request.
    let mut away = false;
    let mut code = OK;
    loop {
        let left = deadline.map(|d| d.saturating_duration_since(Instant::now()));
        // A request does not outlast the wait it is part of.
        let limit = match left {
            Some(left) if opts.wait => left.max(POLL).min(REQUEST),
            _ => REQUEST,
        };
        match oj_server::env_via_server(&root, updates.as_ref(), limit).await {
            Ok(answer) => {
                updates = None;
                away = false;
                // Each answer: a server that came back with another key
                // list has nothing to bind a key it no longer lists.
                if let Some(unknown) = only.iter().find(|k| !names_key(&answer, k)) {
                    eprintln!(
                        "oj env: unknown runtime env key {unknown}; the keys are {}",
                        key_names(&answer).join(", ")
                    );
                    return UNAVAILABLE;
                }
                let done = !opts.wait || bound(&answer, &only);
                status = Some(answer);
                if done {
                    break;
                }
            }
            // With `--wait`, a server that is not there is waited for: one
            // not up yet (`oj dev & oj env set .. --wait`), or restarting
            // (a config or `.env` edit), which keeps the values that were
            // set. What a server answered to refuse is final.
            Err(e) if opts.wait && e.retry => {
                if !away {
                    eprintln!("oj env: {e}; asking again");
                    away = true;
                }
            }
            Err(e) => {
                eprintln!("oj env: {e}");
                return UNAVAILABLE;
            }
        }
        let left = deadline.map(|d| d.saturating_duration_since(Instant::now()));
        if left.is_some_and(|left| left.is_zero()) {
            match &status {
                Some(status) => eprintln!(
                    "oj env: still {} after {}s",
                    status["state"].as_str().unwrap_or("unbound"),
                    opts.timeout
                ),
                None => eprintln!("oj env: no dev server answered in {}s", opts.timeout),
            }
            code = TIMED_OUT;
            break;
        }
        tokio::time::sleep(left.map_or(POLL, |left| left.min(POLL))).await;
    }
    // Nothing to print when no server ever answered.
    let Some(status) = status else {
        return code;
    };
    if !opts.json {
        for key in unfaithful(&status, &only) {
            eprintln!(
                "oj env: the value of {key} cannot be written as one dotenv line that reads back the same; `--json` has it as it is"
            );
        }
    }
    // A closed stdout (`oj env get | head -1`) ends the output, not the command.
    use std::io::Write as _;
    let mut stdout = std::io::stdout().lock();
    let _ = if opts.json {
        writeln!(stdout, "{}", filtered(status, &only))
    } else {
        write!(stdout, "{}", render(&status, &only))
    };
    let _ = stdout.flush();
    code
}

/// The updates `oj env set` names: each argument a `KEY=VALUE`, or a bare
/// `KEY` followed by its value as the next argument (so a value holding `=`
/// needs no quoting). An empty value is a value, as it is for the server.
pub(crate) fn parse_pairs(args: &[String]) -> Result<BTreeMap<String, Option<String>>, String> {
    let mut updates = BTreeMap::new();
    let mut args = args.iter();
    while let Some(arg) = args.next() {
        let (key, value) = match arg.split_once('=') {
            Some((key, value)) => (key, value.to_string()),
            None => match args.next() {
                Some(value) => (arg.as_str(), value.clone()),
                None => {
                    return Err(format!(
                        "no value for `{arg}`: pass `{arg}=VALUE` or `{arg} VALUE`"
                    ))
                }
            },
        };
        if key.is_empty() {
            return Err(format!("`{arg}` names no key"));
        }
        if updates.insert(key.to_string(), Some(value)).is_some() {
            return Err(format!("`{key}` is given twice"));
        }
    }
    Ok(updates)
}

/// The status's keys, in the server's order.
fn keys(status: &serde_json::Value) -> impl Iterator<Item = &serde_json::Value> {
    status["keys"].as_array().into_iter().flatten()
}

fn key_names(status: &serde_json::Value) -> Vec<&str> {
    keys(status).filter_map(|k| k["key"].as_str()).collect()
}

fn names_key(status: &serde_json::Value, key: &str) -> bool {
    keys(status).any(|k| k["key"] == key)
}

/// Whether the server reports every key bound, or, with `only` named, every
/// named key has a runtime value (set or from the file: the `.env` literal
/// a key falls back to does not bind it, as it does not for the server).
pub(crate) fn bound(status: &serde_json::Value, only: &[String]) -> bool {
    if only.is_empty() {
        return status["state"] == "bound";
    }
    keys(status)
        .filter(|k| only.iter().any(|name| k["key"] == name.as_str()))
        .all(|k| k["source"] == "set" || k["source"] == "file")
}

/// The `KEY=value` lines for the keys in `only` (every key when empty); a
/// key without a value is a `# KEY unset` comment, so the output stays a
/// dotenv file.
pub(crate) fn render(status: &serde_json::Value, only: &[String]) -> String {
    let mut out = String::new();
    for key in keys(status) {
        let name = key["key"].as_str().unwrap_or_default();
        if !only.is_empty() && !only.iter().any(|k| k == name) {
            continue;
        }
        match key["value"].as_str() {
            Some(value) => {
                out.push_str(name);
                out.push('=');
                out.push_str(&match dotenv_value(value) {
                    Ok(line) | Err(line) => line,
                });
            }
            None => {
                out.push_str("# ");
                out.push_str(name);
                out.push_str(" unset");
            }
        }
        out.push('\n');
    }
    out
}

/// The keys among `only` (every key when empty) whose value no dotenv line
/// reads back as ([`dotenv_value`]).
pub(crate) fn unfaithful<'a>(status: &'a serde_json::Value, only: &[String]) -> Vec<&'a str> {
    keys(status)
        .filter_map(|key| Some((key["key"].as_str()?, key["value"].as_str()?)))
        .filter(|(name, _)| only.is_empty() || only.iter().any(|k| k == name))
        .filter(|(_, value)| dotenv_value(value).is_err())
        .map(|(name, _)| name)
        .collect()
}

/// `value` as a `.env` file holds it on one line, such that oj reading the
/// line back (as Vite does: `oj_env::parse`) gives the value again:
///
/// - as it is, when the parser takes it whole: it trims an unquoted value,
///   ends it at a `#`, expands a `$` and strips a quote it starts with;
/// - in single quotes, which keep every character but `'` and a line break;
/// - in double quotes, the only ones to hold a line break (`\n`), with `$`
///   escaped; they cannot hold `"`, a carriage return, or the two characters
///   `\n` or `\t`, which read back as the line break or the tab.
///
/// `Err` holds the nearest double-quoted line for a value none of them
/// keeps (a line break beside a `"`, both kinds of quote).
fn dotenv_value(value: &str) -> Result<String, String> {
    let line_break = value.contains(['\n', '\r']);
    let plain = !line_break
        && !value.contains(['#', '$'])
        && value.trim() == value
        && !value.starts_with(['\'', '"', '`', '\u{feff}']);
    if plain {
        return Ok(value.to_string());
    }
    if !line_break && !value.contains('\'') {
        return Ok(format!("'{value}'"));
    }
    let mut out = String::from("\"");
    for c in value.chars() {
        match c {
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '$' => out.push_str("\\$"),
            '"' => out.push_str("\\\""),
            c => out.push(c),
        }
    }
    out.push('"');
    let kept = !value.contains(['"', '\r']) && !value.contains("\\n") && !value.contains("\\t");
    if kept {
        Ok(out)
    } else {
        Err(out)
    }
}

/// The status with its `keys` narrowed to `only` (unchanged when empty).
pub(crate) fn filtered(mut status: serde_json::Value, only: &[String]) -> serde_json::Value {
    if only.is_empty() {
        return status;
    }
    if let Some(keys) = status.get_mut("keys").and_then(|k| k.as_array_mut()) {
        keys.retain(|k| only.iter().any(|name| k["key"] == name.as_str()));
    }
    status
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    fn pairs(list: &[&str]) -> Vec<(String, Option<String>)> {
        parse_pairs(&args(list)).unwrap().into_iter().collect()
    }

    fn pair(key: &str, value: &str) -> (String, Option<String>) {
        (key.to_string(), Some(value.to_string()))
    }

    #[test]
    fn pairs_come_as_key_equals_value_or_as_two_arguments() {
        assert_eq!(
            pairs(&["A=1", "B", "2", "C=three=3"]),
            [pair("A", "1"), pair("B", "2"), pair("C", "three=3")]
        );
    }

    #[test]
    fn a_bare_key_takes_the_whole_next_argument_as_its_value() {
        // `oj env set URL /x?a=b` must not read `/x?a=b` as a pair of its own.
        assert_eq!(pairs(&["URL", "/x?a=b"]), [pair("URL", "/x?a=b")]);
        assert_eq!(pairs(&["A", "B=2"]), [pair("A", "B=2")]);
    }

    #[test]
    fn an_empty_value_is_a_value() {
        assert_eq!(pairs(&["A=", "B", ""]), [pair("A", ""), pair("B", "")]);
    }

    #[test]
    fn a_key_without_a_value_is_a_usage_error() {
        let err = parse_pairs(&args(&["A=1", "B"])).unwrap_err();
        assert!(err.contains("no value for `B`"), "{err}");
        assert!(parse_pairs(&args(&["B"])).is_err());
    }

    #[test]
    fn an_empty_key_and_a_repeated_key_are_usage_errors() {
        let err = parse_pairs(&args(&["=1"])).unwrap_err();
        assert!(err.contains("names no key"), "{err}");
        let err = parse_pairs(&args(&["A=1", "A", "2"])).unwrap_err();
        assert!(err.contains("`A` is given twice"), "{err}");
    }

    #[test]
    fn no_arguments_make_no_updates() {
        assert!(parse_pairs(&[]).unwrap().is_empty());
    }

    fn status(state: &str, keys: &[(&str, Option<&str>)]) -> serde_json::Value {
        status_from(state, keys, "file")
    }

    fn status_from(state: &str, keys: &[(&str, Option<&str>)], source: &str) -> serde_json::Value {
        serde_json::json!({
            "state": state,
            "generation": 2,
            "keys": keys.iter().map(|(k, v)| serde_json::json!({
                "key": k, "field": k, "value": v, "source": v.map(|_| source),
            })).collect::<Vec<_>>(),
        })
    }

    #[test]
    fn render_prints_dotenv_lines_and_comments_out_unset_keys() {
        let status = status("partial", &[("A", Some("1")), ("B", None), ("C", Some(""))]);
        assert_eq!(render(&status, &[]), "A=1\n# B unset\nC=\n");
    }

    #[test]
    fn a_value_with_a_line_break_stays_on_one_dotenv_line() {
        let status = status(
            "bound",
            &[("A", Some("one\ntwo 'q' \\ $x")), ("B", Some("x=\"y\""))],
        );
        assert_eq!(
            render(&status, &[]),
            "A=\"one\\ntwo 'q' \\ \\$x\"\nB=x=\"y\"\n"
        );
        assert!(unfaithful(&status, &[]).is_empty());
    }

    // Unquoted, the parser trims the value, ends it at a `#`, expands a `$`
    // and strips a quote it starts with: such a value is printed in the
    // quotes that keep it whole.
    #[test]
    fn a_value_a_parser_would_cut_is_quoted() {
        for (value, line) in [
            ("abc #def", "'abc #def'"),
            ("a#b", "'a#b'"),
            (" padded ", "' padded '"),
            ("\"x\"", "'\"x\"'"),
            ("p$word", "'p$word'"),
            ("`x`", "'`x`'"),
            ("it's #1", "\"it's #1\""),
            ("it's $x", "\"it's \\$x\""),
            ("plain=\"y\" 'z'", "plain=\"y\" 'z'"),
            ("", ""),
        ] {
            let status = status("bound", &[("A", Some(value))]);
            assert_eq!(render(&status, &[]), format!("A={line}\n"), "{value:?}");
            assert!(unfaithful(&status, &[]).is_empty(), "{value:?}");
        }
    }

    // A value no single line keeps is still printed, and named: both kinds
    // of quote where it needs quoting, a line break beside a `"`, a
    // carriage return, the characters `\n` beside a `'`.
    #[test]
    fn a_value_no_line_keeps_is_named() {
        let status = status(
            "bound",
            &[
                ("A", Some("it's \"x\" #1")),
                ("B", Some("one\ntwo \"q\"")),
                ("C", Some("a\rb")),
                ("D", Some("it's C:\\new #1")),
                ("E", Some("fine")),
            ],
        );
        assert_eq!(unfaithful(&status, &[]), ["A", "B", "C", "D"]);
        assert_eq!(unfaithful(&status, &args(&["B", "E"])), ["B"]);
        assert_eq!(
            render(&status, &args(&["B"])),
            "B=\"one\\ntwo \\\"q\\\"\"\n"
        );
    }

    // Every line the command calls faithful is read back as the value by
    // oj's `.env` parser, over every string of up to five of the characters
    // a parser treats specially.
    #[test]
    fn a_faithful_line_reads_back_as_the_value() {
        const ALPHABET: [&str; 12] = [
            "a", " ", "'", "\"", "`", "#", "$", "\\", "n", "t", "\n", "=",
        ];
        let mut faithful = 0;
        let mut values = vec![String::new()];
        for _ in 0..5 {
            let longer: Vec<String> = values
                .iter()
                .flat_map(|v| ALPHABET.iter().map(move |c| format!("{v}{c}")))
                .collect();
            for value in &longer {
                let Ok(line) = dotenv_value(value) else {
                    continue;
                };
                faithful += 1;
                assert!(!line.contains('\n'), "{value:?}: {line:?}");
                let read = oj_env::parse(&format!("A={line}\nB=after\n"), &BTreeMap::new());
                assert_eq!(
                    read,
                    [
                        ("A".to_string(), value.clone()),
                        ("B".into(), "after".into())
                    ],
                    "{value:?} as {line:?}"
                );
            }
            values = longer;
        }
        // Most values have a line: the ones named are the exception.
        assert!(faithful > 200_000, "{faithful}");
    }

    #[test]
    fn render_keeps_only_the_named_keys_in_the_servers_order() {
        let status = status(
            "bound",
            &[("A", Some("1")), ("B", Some("2")), ("C", Some("3"))],
        );
        assert_eq!(render(&status, &args(&["C", "A"])), "A=1\nC=3\n");
        assert_eq!(render(&status, &args(&["nope"])), "");
    }

    #[test]
    fn bound_follows_the_servers_state_unless_keys_are_named() {
        let partial = status("partial", &[("A", Some("1")), ("B", None)]);
        assert!(!bound(&partial, &[]));
        assert!(bound(&partial, &args(&["A"])));
        assert!(!bound(&partial, &args(&["A", "B"])));
        // An empty string is a value; a key the server does not list never binds.
        let empty = status("bound", &[("A", Some(""))]);
        assert!(bound(&empty, &[]));
        assert!(bound(&empty, &args(&["A"])));
        assert!(bound(&empty, &args(&["Z"])), "no named key is pending");
    }

    #[test]
    fn a_literal_fallback_does_not_bind_a_named_key() {
        // The server reports `unbound` for the same reason: the value in
        // force is the compile's, not the backend's.
        let literal = status_from("unbound", &[("A", Some("https://baked.test"))], "fallback");
        assert!(!bound(&literal, &args(&["A"])));
        assert!(!bound(&literal, &[]));
        let set = status_from("bound", &[("A", Some("/_port_1"))], "set");
        assert!(bound(&set, &args(&["A"])));
        // Rendering shows the literal all the same: it is what the page reads.
        assert_eq!(render(&literal, &[]), "A=https://baked.test\n");
    }

    #[test]
    fn filtered_narrows_the_json_keys_and_keeps_the_rest() {
        let status = status("bound", &[("A", Some("1")), ("B", Some("2"))]);
        let narrowed = filtered(status.clone(), &args(&["B"]));
        assert_eq!(narrowed["keys"].as_array().unwrap().len(), 1);
        assert_eq!(narrowed["keys"][0]["key"], "B");
        assert_eq!(narrowed["state"], "bound");
        assert_eq!(filtered(status.clone(), &[]), status);
    }

    #[test]
    fn a_status_without_keys_renders_nothing_and_is_not_bound() {
        let bare = serde_json::json!({"state": "unbound"});
        assert_eq!(render(&bare, &[]), "");
        assert!(!bound(&bare, &[]));
        assert!(!names_key(&bare, "A"));
        assert!(key_names(&bare).is_empty());
    }
}
