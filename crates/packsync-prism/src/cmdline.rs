//! Prism runs hook commands through `QProcess::splitCommand` (no shell). This is a faithful port of
//! that function (Qt 6) plus a quoter that only produces strings the port reads back exactly.
//!
//! Qt's rules: tokens are separated by white space; a token may be wrapped in double quotes to
//! contain spaces; **three** consecutive double quotes stand for one literal quote; an empty token
//! cannot be expressed (`""` vanishes). Backslashes are ordinary characters (Windows paths are fine).
//! Before splitting, Prism replaces `$INST_NAME`, `$INST_ID`, `$INST_DIR`, … anywhere in the string,
//! so a literal `$` in a path could be mangled: such paths are refused.

use packsync_core::error::{Error, Result};

/// One token with its byte span in the source string.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Token {
    pub text: String,
    pub start: usize,
    pub end: usize,
}

/// `QProcess::splitCommand`, keeping spans.
pub fn tokenize(command: &str) -> Vec<Token> {
    let mut out = Vec::new();
    let mut tmp = String::new();
    let mut tmp_start: Option<usize> = None;
    let mut quote_count = 0;
    let mut in_quote = false;
    let mut last_end = 0;
    for (i, ch) in command.char_indices() {
        let ch_end = i + ch.len_utf8();
        if ch == '"' {
            quote_count += 1;
            if tmp_start.is_none() {
                tmp_start = Some(i);
            }
            if quote_count == 3 {
                quote_count = 0;
                tmp.push(ch);
            }
            last_end = ch_end;
            continue;
        }
        if quote_count != 0 {
            if quote_count == 1 {
                in_quote = !in_quote;
            }
            quote_count = 0;
        }
        if !in_quote && ch.is_whitespace() {
            if !tmp.is_empty() {
                out.push(Token { text: std::mem::take(&mut tmp), start: tmp_start.take().unwrap_or(i), end: last_end });
            }
            tmp_start = None;
        } else {
            if tmp_start.is_none() {
                tmp_start = Some(i);
            }
            tmp.push(ch);
            last_end = ch_end;
        }
    }
    if !tmp.is_empty() {
        out.push(Token { text: tmp, start: tmp_start.unwrap_or(0), end: last_end });
    }
    out
}

pub fn split_command(command: &str) -> Vec<String> {
    tokenize(command).into_iter().map(|t| t.text).collect()
}

/// Quote one argument so that [`split_command`] returns exactly `arg`. Refuses what cannot be
/// represented safely: empty strings, double quotes, `$`, control characters.
pub fn quote_arg(arg: &str) -> Result<String> {
    if arg.is_empty() {
        return Err(Error::invalid("hook command", "an empty argument cannot be expressed in a Prism command line"));
    }
    if arg.contains('"') {
        return Err(Error::invalid(
            "hook command",
            format!("{arg:?} contains a double quote, which Prism's command-line syntax cannot carry safely"),
        ));
    }
    if arg.contains('$') {
        return Err(Error::invalid(
            "hook command",
            format!(
                "{arg:?} contains '$', which Prism would try to substitute as a variable (e.g. $INST_DIR). Install PackSync in a path without '$'"
            ),
        ));
    }
    if arg.chars().any(|c| c.is_control()) {
        return Err(Error::invalid("hook command", "control characters are not allowed in a hook command"));
    }
    Ok(if arg.chars().any(|c| c.is_whitespace()) { format!("\"{arg}\"") } else { arg.to_string() })
}

/// The text after the first standalone `--` token (the chained original command), verbatim.
pub fn chain_suffix(command: &str) -> Option<&str> {
    let toks = tokenize(command);
    let t = toks.iter().find(|t| t.text == "--")?;
    let rest = command[t.end..].trim_start();
    if rest.is_empty() { None } else { Some(rest) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_qts_documented_behaviour() {
        assert_eq!(split_command("a b  c"), ["a", "b", "c"]);
        assert_eq!(split_command("\"hello world\" x"), ["hello world", "x"]);
        assert_eq!(split_command("\"/opt/My Pack/ünï\" hook pre-launch"), ["/opt/My Pack/ünï", "hook", "pre-launch"]);
        // three quotes = one literal quote
        assert_eq!(split_command("say \"\"\"hi\"\"\""), ["say", "\"hi\""]);
        // backslashes are ordinary
        assert_eq!(
            split_command(r#""C:\Program Files\PackSync\packsync.exe" hook"#),
            [r"C:\Program Files\PackSync\packsync.exe", "hook"]
        );
        // an empty token disappears
        assert_eq!(split_command("a \"\" b"), ["a", "b"]);
        // a quote in the middle of a word joins
        assert_eq!(split_command("pre\"fix ed\"post"), ["prefix edpost"]);
    }

    #[test]
    fn quote_roundtrips_or_refuses() {
        for ok in [
            "/usr/bin/packsync",
            "/opt/My Pack/ünï/packsync",
            r"C:\Program Files\PackSync\packsync.exe",
            "a-b_c.d",
            "日本語 パス/x",
        ] {
            let q = quote_arg(ok).unwrap();
            assert_eq!(split_command(&q), [ok], "{q}");
        }
        for bad in ["", "has\"quote", "cost$inst", "line\nbreak", "tab\there"] {
            assert!(quote_arg(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn spans_and_chain_suffix() {
        let cmd = "\"/opt/x y/packsync\" hook pre-launch --instance abc --marker packsync-hook -- \"/usr/bin/notify send\" --flag  value";
        let toks = tokenize(cmd);
        assert_eq!(toks[0].text, "/opt/x y/packsync");
        assert_eq!(&cmd[toks[0].start..toks[0].end], "\"/opt/x y/packsync\"");
        assert_eq!(chain_suffix(cmd), Some("\"/usr/bin/notify send\" --flag  value"));
        assert_eq!(chain_suffix("a b c"), None);
        assert_eq!(chain_suffix("a --"), None);
    }
}
