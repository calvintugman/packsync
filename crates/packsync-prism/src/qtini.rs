//! Prism's `instance.cfg` is read **and written by Qt's `QSettings` in `IniFormat`** (see
//! `launcher/settings/INIFile.cpp`), not by a generic INI library. Everything that matters for
//! round-tripping through the real launcher is therefore Qt's behaviour, ported here from the Qt
//! 6.11 sources (`src/corelib/io/qsettings.cpp`):
//!
//! * **logical lines** (`readIniLine`): `;` starts a comment (also *after* a value, outside quotes),
//!   a backslash-newline continues a line, a newline inside `"…"` does not end it, blank lines and
//!   comment lines are invisible, `\r` alone is a line terminator;
//! * **values** (`iniUnescapedStringList` / `iniEscapedString`): `"` toggles a quoted run and is
//!   *consumed* (so a literal quote must be written `\"`), an unquoted `,` turns the value into a
//!   *list*, `\\`, `\"`, `\n`, `\t`, `\xHH`, octal escapes, and a backslash before any other
//!   character silently drops both;
//! * **keys** (`iniUnescapedKey`): `\` is the group separator (`mods_Page\Columns` is the key
//!   `mods_Page/Columns`), `%XX` / `%UXXXX` are escapes; lookups in `QSettings` are
//!   case-insensitive.
//!
//! The editor below changes single logical lines **in place** and keeps every other byte (comments,
//! ordering, `[UI]` blobs, line endings, BOM) exactly as found, because Prism may have written the
//! file and we must not make it look hand-edited more than necessary.

use std::ops::Range;

use packsync_core::error::{Error, Result};

/// Instance files are small (a few KiB). Anything bigger is not a Prism `instance.cfg`.
pub const MAX_INI_BYTES: usize = 2 * 1024 * 1024;

const BOM: &[u8] = b"\xEF\xBB\xBF";

fn is_space(b: u8) -> bool {
    matches!(b, b'\t' | b'\n' | b'\r' | b' ')
}

fn is_special(b: u8) -> bool {
    matches!(b, b'\n' | b'\r' | b'"' | b';' | b'=' | b'\\')
}

/// One logical line as Qt sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogicalLine {
    /// First non-blank byte of the line.
    pub start: usize,
    /// End of the logical line: at the terminator, at an inline `;` comment, or at EOF.
    pub end: usize,
    /// First `=` outside quotes.
    pub equals: Option<usize>,
}

/// Port of `QConfFileSettingsPrivate::readIniLine`. `pos` is the scan position and is advanced.
fn read_line(d: &[u8], pos: &mut usize) -> Option<LogicalLine> {
    let len = d.len();
    let mut in_quotes = false;
    let mut equals = None;
    let mut line_start = *pos;
    while line_start < len && is_space(d[line_start]) {
        line_start += 1;
    }
    let mut i = line_start;
    'outer: while i < len {
        let mut ch = d[i];
        while !is_special(ch) {
            i += 1;
            if i == len {
                break 'outer;
            }
            ch = d[i];
        }
        i += 1;
        match ch {
            b'=' => {
                if !in_quotes && equals.is_none() {
                    equals = Some(i - 1);
                }
            }
            b'\n' | b'\r' => {
                if i == line_start + 1 {
                    line_start += 1;
                } else if !in_quotes {
                    i -= 1;
                    break 'outer;
                }
            }
            b'\\' => {
                if i < len {
                    let c1 = d[i];
                    i += 1;
                    if i < len {
                        let c2 = d[i];
                        if (c1 == b'\n' && c2 == b'\r') || (c1 == b'\r' && c2 == b'\n') {
                            i += 1;
                        }
                    }
                }
            }
            b'"' => in_quotes = !in_quotes,
            _ => {
                // ';'
                if i == line_start + 1 {
                    while i < len && d[i] != b'\n' && d[i] != b'\r' {
                        i += 1;
                    }
                    while i < len && is_space(d[i]) {
                        i += 1;
                    }
                    line_start = i;
                } else if !in_quotes {
                    i -= 1;
                    break 'outer;
                }
            }
        }
    }
    *pos = i;
    if i > line_start { Some(LogicalLine { start: line_start, end: i, equals }) } else { None }
}

// ---------------------------------------------------------------------------------------- values

/// What `QSettings::value()` hands to Prism for one INI value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum QtValue {
    /// An ordinary string (the only thing Prism ever writes for its string settings).
    Str(String),
    /// The value contained an unquoted comma, which Qt reads as a `QStringList`. Prism's
    /// `QVariant::toString()` on such a value yields an *empty string*.
    List(Vec<String>),
    /// `@Variant(…)`, `@ByteArray(…)`, `@Invalid()` … Qt's serialised types; never produced for
    /// the settings we touch.
    Special(String),
}

impl QtValue {
    /// The string Prism's `value.toString()` would see.
    pub fn prism_string(&self) -> String {
        match self {
            QtValue::Str(s) => s.clone(),
            QtValue::List(_) => String::new(),
            QtValue::Special(s) => s.clone(),
        }
    }

    pub fn as_plain(&self) -> Option<&str> {
        match self {
            QtValue::Str(s) => Some(s),
            _ => None,
        }
    }
}

fn from_hex(b: u8) -> Option<u16> {
    match b {
        b'0'..=b'9' => Some((b - b'0') as u16),
        b'a'..=b'f' => Some((b - b'a' + 10) as u16),
        b'A'..=b'F' => Some((b - b'A' + 10) as u16),
        _ => None,
    }
}

fn from_oct(b: u8) -> Option<u16> {
    matches!(b, b'0'..=b'7').then(|| (b - b'0') as u16)
}

fn escape_code(ch: u8) -> Option<u16> {
    Some(match ch {
        b'a' => 0x07,
        b'b' => 0x08,
        b'f' => 0x0C,
        b'n' => 0x0A,
        b'r' => 0x0D,
        b't' => 0x09,
        b'v' => 0x0B,
        b'"' => b'"' as u16,
        b'?' => b'?' as u16,
        b'\'' => b'\'' as u16,
        b'\\' => b'\\' as u16,
        _ => return None,
    })
}

fn push_utf8(out: &mut Vec<u16>, bytes: &[u8]) {
    out.extend(String::from_utf8_lossy(bytes).encode_utf16());
}

fn chop_trailing_spaces(s: &mut Vec<u16>, limit: usize) {
    while s.len() > limit && matches!(s.last(), Some(&0x20) | Some(&0x09)) {
        s.pop();
    }
}

/// Port of `QSettingsPrivate::iniUnescapedStringList` followed by `stringToVariant` /
/// `stringListToVariantList`.
pub fn unescape_value(raw: &[u8]) -> QtValue {
    let n = raw.len();
    let mut i = 0usize;
    let mut result: Vec<u16> = Vec::with_capacity(n);
    let mut list: Vec<Vec<u16>> = Vec::new();
    let mut is_list = false;
    let mut in_quoted = false;
    let mut cur_quoted = false;
    let mut chop_limit: usize;
    let mut chop_at_end = false;

    'skip_spaces: loop {
        while i < n && (raw[i] == b' ' || raw[i] == b'\t') {
            i += 1;
        }
        'normal: loop {
            chop_limit = result.len();
            while i < n {
                match raw[i] {
                    b'\\' => {
                        i += 1;
                        if i >= n {
                            break 'skip_spaces;
                        }
                        let ch = raw[i];
                        i += 1;
                        if let Some(c) = escape_code(ch) {
                            result.push(c);
                            continue 'normal;
                        }
                        if ch == b'x' {
                            if i >= n {
                                break 'skip_spaces;
                            }
                            if let Some(first) = from_hex(raw[i]) {
                                let mut v: u16 = first;
                                i += 1;
                                while i < n {
                                    match from_hex(raw[i]) {
                                        Some(h) => {
                                            v = (v << 4).wrapping_add(h);
                                            i += 1;
                                        }
                                        None => break,
                                    }
                                }
                                result.push(v);
                                if i >= n {
                                    break 'skip_spaces;
                                }
                                continue 'normal;
                            }
                        } else if let Some(first) = from_oct(ch) {
                            let mut v: u16 = first;
                            while i < n {
                                match from_oct(raw[i]) {
                                    Some(o) => {
                                        v = (v << 3).wrapping_add(o);
                                        i += 1;
                                    }
                                    None => break,
                                }
                            }
                            result.push(v);
                            if i >= n {
                                break 'skip_spaces;
                            }
                            continue 'normal;
                        } else if ch == b'\n' || ch == b'\r' {
                            if i < n {
                                let ch2 = raw[i];
                                if (ch2 == b'\n' || ch2 == b'\r') && ch2 != ch {
                                    i += 1;
                                }
                            }
                        }
                        // any other escaped character is dropped together with its backslash
                        chop_limit = result.len();
                    }
                    b'"' => {
                        i += 1;
                        cur_quoted = true;
                        in_quoted = !in_quoted;
                        if !in_quoted {
                            continue 'skip_spaces;
                        }
                    }
                    b',' if !in_quoted => {
                        if !cur_quoted {
                            chop_trailing_spaces(&mut result, chop_limit);
                        }
                        if !is_list {
                            is_list = true;
                            list.clear();
                        }
                        list.push(std::mem::take(&mut result));
                        cur_quoted = false;
                        i += 1;
                        continue 'skip_spaces;
                    }
                    _ => {
                        let mut j = i + 1;
                        while j < n && !matches!(raw[j], b'\\' | b'"' | b',') {
                            j += 1;
                        }
                        push_utf8(&mut result, &raw[i..j]);
                        i = j;
                    }
                }
            }
            chop_at_end = true;
            break 'skip_spaces;
        }
    }
    if chop_at_end && !cur_quoted {
        chop_trailing_spaces(&mut result, chop_limit);
    }
    let to_string = |u: &[u16]| String::from_utf16_lossy(u);
    if is_list {
        list.push(result);
        let strs: Vec<String> = list.iter().map(|u| to_string(u)).collect();
        let mut out = strs.clone();
        for (idx, s) in strs.iter().enumerate() {
            if s.starts_with('@') {
                if s.len() < 2 || !s[1..].starts_with('@') {
                    return QtValue::Special(strs.join(","));
                }
                out[idx] = s[1..].to_string();
            }
        }
        return QtValue::List(out);
    }
    let s = to_string(&result);
    if let Some(rest) = s.strip_prefix('@') {
        if rest.starts_with('@') {
            return QtValue::Str(rest.to_string());
        }
        return QtValue::Special(s);
    }
    QtValue::Str(s)
}

/// Port of `QSettingsPrivate::iniEscapedString` (+ the `@` prefix rule of `variantToString`).
///
/// The result is exactly what Qt would write for this string value, including the surrounding
/// quotes it adds when the text contains `;` `,` `=` or starts/ends with a space.
pub fn escape_value(value: &str) -> Result<String> {
    if value.contains('\0') {
        return Err(Error::invalid("INI value", "values containing NUL cannot be stored in instance.cfg"));
    }
    let prefixed;
    let value = if value.starts_with('@') {
        prefixed = format!("@{value}");
        prefixed.as_str()
    } else {
        value
    };
    let mut out = String::with_capacity(value.len() + 8);
    let mut needs_quotes = false;
    let mut escape_next_if_digit = false;
    for ch in value.chars() {
        if matches!(ch, ';' | ',' | '=') {
            needs_quotes = true;
        }
        if escape_next_if_digit && ch.is_ascii_hexdigit() {
            out.push_str(&format!("\\x{:x}", ch as u32));
            continue;
        }
        escape_next_if_digit = false;
        match ch {
            '\u{7}' => out.push_str("\\a"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{b}' => out.push_str("\\v"),
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            c if (c as u32) <= 0x1F => {
                out.push_str(&format!("\\x{:x}", c as u32));
                escape_next_if_digit = true;
            }
            c => out.push(c),
        }
    }
    if needs_quotes || out.starts_with(' ') || out.ends_with(' ') {
        out.insert(0, '"');
        out.push('"');
    }
    Ok(out)
}

/// Port of `QSettingsPrivate::iniUnescapedKey`.
pub fn unescape_key(raw: &[u8]) -> String {
    let decoded: Vec<char> = String::from_utf8_lossy(raw).chars().collect();
    let size = decoded.len();
    let mut out = String::with_capacity(size);
    let mut i = 0;
    while i < size {
        let ch = decoded[i];
        if ch == '\\' {
            out.push('/');
            i += 1;
            continue;
        }
        if ch != '%' || i == size - 1 {
            out.push(ch);
            i += 1;
            continue;
        }
        let mut num_digits = 2;
        let mut first = i + 1;
        if decoded[i + 1] == 'U' {
            first += 1;
            num_digits = 4;
        }
        if first + num_digits > size {
            out.push('%');
            i += 1;
            continue;
        }
        let hex: String = decoded[first..first + num_digits].iter().collect();
        match u16::from_str_radix(&hex, 16).ok().and_then(|u| char::from_u32(u as u32)) {
            Some(c) => {
                out.push(c);
                i = first + num_digits;
            }
            None => {
                out.push('%');
                i += 1;
            }
        }
    }
    out
}

/// Port of `QSettingsPrivate::iniEscapedKey`.
pub fn escape_key(key: &str) -> String {
    let mut out = String::with_capacity(key.len());
    for ch in key.chars() {
        let c = ch as u32;
        if ch == '/' {
            out.push('\\');
        } else if ch.is_ascii_alphanumeric() || matches!(ch, '_' | '-' | '.') {
            out.push(ch);
        } else if c <= 0xFF {
            out.push_str(&format!("%{c:02X}"));
        } else {
            let mut buf = [0u16; 2];
            for unit in ch.encode_utf16(&mut buf) {
                out.push_str(&format!("%U{unit:04X}"));
            }
        }
    }
    out
}

/// Qt's `QVariant::toBool()` for a string setting.
pub fn parse_bool(value: &str) -> bool {
    let v = value.trim();
    !(v.is_empty() || v == "0" || v.eq_ignore_ascii_case("false"))
}

// ------------------------------------------------------------------------------------ the document

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EntryKind {
    /// `[Section]`
    Header,
    /// `key=value`
    Pair { key_text: Range<usize>, value: Range<usize> },
    /// A line without `=` that is not a header (Qt reports a format error but keeps going).
    Other,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub line: LogicalLine,
    /// `""` for the General section, `"UI/"` style prefix otherwise (as Qt keeps it).
    pub section: String,
    /// Full key as `QSettings` sees it (`section + unescaped key`). Empty for headers.
    pub key: String,
    pub kind: EntryKind,
}

/// A parsed view of an INI byte buffer. Borrowing keeps edits explicit: they produce a new `Vec`.
#[derive(Debug)]
pub struct IniDoc<'a> {
    pub data: &'a [u8],
    pub entries: Vec<Entry>,
    /// Number of leading BOM bytes (never touched).
    pub bom: usize,
}

impl<'a> IniDoc<'a> {
    pub fn parse(data: &'a [u8]) -> Result<IniDoc<'a>> {
        if data.len() > MAX_INI_BYTES {
            return Err(Error::invalid("instance.cfg", format!("larger than {MAX_INI_BYTES} bytes")));
        }
        let bom = if data.starts_with(BOM) { BOM.len() } else { 0 };
        let mut entries = Vec::new();
        let mut pos = bom;
        let mut section = String::new();
        while let Some(line) = read_line(data, &mut pos) {
            let text = &data[line.start..line.end];
            if text.first() == Some(&b'[') {
                let close = text.iter().position(|&b| b == b']');
                let name_bytes = match close {
                    Some(c) => &text[1..c],
                    None => &text[1..],
                };
                let name = String::from_utf8_lossy(name_bytes).trim().to_string();
                section = if name.eq_ignore_ascii_case("general") {
                    String::new()
                } else if name.eq_ignore_ascii_case("%general") {
                    format!("{}/", &name[1..])
                } else {
                    format!("{}/", unescape_key(name.as_bytes()))
                };
                entries.push(Entry { line, section: section.clone(), key: String::new(), kind: EntryKind::Header });
                continue;
            }
            match line.equals {
                Some(eq) => {
                    let key_text = line.start..eq;
                    let key_bytes = &data[key_text.clone()];
                    let trimmed = trim_ascii(key_bytes);
                    let key = format!("{}{}", section, unescape_key(trimmed));
                    let value = eq + 1..line.end;
                    entries.push(Entry {
                        line,
                        section: section.clone(),
                        key,
                        kind: EntryKind::Pair { key_text, value },
                    });
                }
                None => {
                    entries.push(Entry { line, section: section.clone(), key: String::new(), kind: EntryKind::Other })
                }
            }
        }
        Ok(IniDoc { data, entries, bom })
    }

    /// Pairs whose full key equals `key` ignoring ASCII case (Qt's INI lookups are case-insensitive).
    pub fn find(&self, key: &str) -> Vec<&Entry> {
        self.entries
            .iter()
            .filter(|e| matches!(e.kind, EntryKind::Pair { .. }) && e.key.eq_ignore_ascii_case(key))
            .collect()
    }

    pub fn value_of(&self, e: &Entry) -> Option<QtValue> {
        match &e.kind {
            EntryKind::Pair { value, .. } => Some(unescape_value(&self.data[value.clone()])),
            _ => None,
        }
    }

    /// The value Prism sees for `key`: the **last** occurrence wins (QMap insert semantics).
    pub fn get(&self, key: &str) -> Option<QtValue> {
        self.find(key).last().and_then(|e| self.value_of(e))
    }

    /// Raw text of an entry's logical line.
    pub fn raw_line(&self, e: &Entry) -> &'a [u8] {
        &self.data[e.line.start..e.line.end]
    }

    /// The line terminator style of the file (first terminator found; `\n` when there is none).
    pub fn eol(&self) -> &'static str {
        let mut i = 0;
        while i < self.data.len() {
            match self.data[i] {
                b'\r' if self.data.get(i + 1) == Some(&b'\n') => return "\r\n",
                b'\r' => return "\r",
                b'\n' => return "\n",
                _ => i += 1,
            }
        }
        if cfg!(windows) { "\r\n" } else { "\n" }
    }

    /// Start of the physical line holding `pos` (just after the previous terminator).
    fn physical_line_start(&self, pos: usize) -> usize {
        let mut s = pos;
        while s > self.bom && !matches!(self.data[s - 1], b'\n' | b'\r') {
            s -= 1;
        }
        s
    }

    /// End of the physical line holding `pos`, *excluding* its terminator.
    fn physical_line_end(&self, pos: usize) -> usize {
        let mut e = pos;
        while e < self.data.len() && !matches!(self.data[e], b'\n' | b'\r') {
            e += 1;
        }
        e
    }

    /// End of the terminator that follows `pos` (handles `\r\n`); `pos` itself if at EOF.
    fn after_terminator(&self, pos: usize) -> usize {
        match self.data.get(pos) {
            Some(b'\r') if self.data.get(pos + 1) == Some(&b'\n') => pos + 2,
            Some(b'\r') | Some(b'\n') => pos + 1,
            _ => pos,
        }
    }
}

fn trim_ascii(b: &[u8]) -> &[u8] {
    let mut s = 0;
    let mut e = b.len();
    while s < e && is_space(b[s]) {
        s += 1;
    }
    while e > s && is_space(b[e - 1]) {
        e -= 1;
    }
    &b[s..e]
}

/// One planned modification of a General-group key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KeyEdit {
    /// Set the key to this *string* value (replacing the line if present, appending to the General
    /// section otherwise).
    Set { key: String, value: String },
    /// Replace the key's line with these exact bytes (used to restore a saved original verbatim).
    SetRaw { key: String, line: String },
    /// Delete every line of the key.
    Remove { key: String },
}

impl KeyEdit {
    pub fn key(&self) -> &str {
        match self {
            KeyEdit::Set { key, .. } | KeyEdit::SetRaw { key, .. } | KeyEdit::Remove { key } => key,
        }
    }
}

/// Apply `edits` to `data`, touching only the affected logical lines. Keys are matched like Qt does
/// (case-insensitive, General group). Returns the new bytes.
pub fn apply_edits(data: &[u8], edits: &[KeyEdit]) -> Result<Vec<u8>> {
    let doc = IniDoc::parse(data)?;
    let eol = doc.eol();
    let mut replacements: Vec<(Range<usize>, String)> = Vec::new();
    let mut appended: Vec<String> = Vec::new();

    for edit in edits {
        let key = edit.key();
        if key.contains('/') || key.contains('=') || key.contains('\n') || key.trim() != key || key.is_empty() {
            return Err(Error::invalid("INI key", format!("{key:?} is not an editable General key")));
        }
        let general: Vec<&Entry> = doc.find(key).into_iter().filter(|e| e.section.is_empty()).collect();
        let canonical = |value: &str| -> Result<String> { Ok(format!("{}={}", escape_key(key), escape_value(value)?)) };
        match edit {
            KeyEdit::Remove { .. } => {
                for e in general {
                    let start = doc.physical_line_start(e.line.start);
                    let end = doc.after_terminator(doc.physical_line_end(e.line.end));
                    replacements.push((start..end, String::new()));
                }
            }
            KeyEdit::Set { value, .. } => {
                let line = canonical(value)?;
                match general.as_slice() {
                    [] => appended.push(line),
                    [e] => replacements.push((e.line.start..doc.physical_line_end(e.line.end), line)),
                    _ => {
                        return Err(Error::invalid(
                            "instance.cfg",
                            format!("key {key} appears more than once; refusing to guess which line Prism uses"),
                        ));
                    }
                }
            }
            KeyEdit::SetRaw { line, .. } => match general.as_slice() {
                [] => appended.push(line.clone()),
                [e] => replacements.push((e.line.start..doc.physical_line_end(e.line.end), line.clone())),
                _ => {
                    return Err(Error::invalid(
                        "instance.cfg",
                        format!("key {key} appears more than once; refusing to guess which line Prism uses"),
                    ));
                }
            },
        }
    }

    // Insertion point for new General keys: after the last General pair (or the General header, or
    // at the top of a file that has none).
    let mut insert: Option<(usize, String)> = None;
    if !appended.is_empty() {
        let last_general = doc
            .entries
            .iter()
            .filter(|e| e.section.is_empty() && matches!(e.kind, EntryKind::Pair { .. } | EntryKind::Other))
            .next_back();
        let header_general = doc
            .entries
            .iter()
            .find(|e| matches!(e.kind, EntryKind::Header) && e.section.is_empty())
            .filter(|_| last_general.is_none());
        let mut text = String::new();
        match (last_general, header_general) {
            (Some(e), _) | (None, Some(e)) => {
                let eol_pos = doc.physical_line_end(e.line.end);
                let after = doc.after_terminator(eol_pos);
                let needs_leading_eol = after == eol_pos; // last line without a terminator
                if needs_leading_eol {
                    text.push_str(eol);
                }
                for l in &appended {
                    text.push_str(l);
                    text.push_str(eol);
                }
                insert = Some((after, text));
            }
            (None, None) => {
                // No General content at all: a fresh section at the very top (after a BOM).
                text.push_str("[General]");
                text.push_str(eol);
                for l in &appended {
                    text.push_str(l);
                    text.push_str(eol);
                }
                if doc.entries.first().is_some() {
                    text.push_str(eol);
                }
                insert = Some((doc.bom, text));
            }
        }
    }

    let mut all: Vec<(Range<usize>, String)> = replacements;
    if let Some((pos, text)) = insert {
        all.push((pos..pos, text));
    }
    all.sort_by_key(|(r, _)| (r.start, r.end));
    for w in all.windows(2) {
        if w[0].0.end > w[1].0.start {
            return Err(Error::invalid("instance.cfg", "overlapping edits"));
        }
    }
    let mut out = Vec::with_capacity(data.len() + 128);
    let mut cursor = 0;
    for (range, text) in all {
        out.extend_from_slice(&data[cursor..range.start]);
        out.extend_from_slice(text.as_bytes());
        cursor = range.end;
    }
    out.extend_from_slice(&data[cursor..]);
    if out.len() > MAX_INI_BYTES {
        return Err(Error::invalid("instance.cfg", "result would be too large"));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(v: &QtValue) -> String {
        v.prism_string()
    }

    #[test]
    fn escape_matches_qt_for_the_characters_that_matter() {
        assert_eq!(escape_value("plain").unwrap(), "plain");
        assert_eq!(escape_value("a b").unwrap(), "a b");
        assert_eq!(escape_value("\"/opt/x y/pack sync\" hook").unwrap(), "\\\"/opt/x y/pack sync\\\" hook");
        assert_eq!(escape_value("C:\\Program Files\\x").unwrap(), "C:\\\\Program Files\\\\x");
        assert_eq!(escape_value("a=b").unwrap(), "\"a=b\"");
        assert_eq!(escape_value("a;b").unwrap(), "\"a;b\"");
        assert_eq!(escape_value("a,b").unwrap(), "\"a,b\"");
        assert_eq!(escape_value(" lead").unwrap(), "\" lead\"");
        assert_eq!(escape_value("trail ").unwrap(), "\"trail \"");
        assert_eq!(escape_value("l1\nl2\ttab").unwrap(), "l1\\nl2\\ttab");
        assert_eq!(escape_value("ünï 日本 🎮").unwrap(), "ünï 日本 🎮");
        assert_eq!(escape_value("@x").unwrap(), "@@x");
        assert_eq!(escape_value("\u{1}a").unwrap(), "\\x1\\x61", "a hex digit after \\x escape is escaped too");
        assert!(escape_value("a\0b").is_err());
        // the real instance.cfg base64 blobs end in '=' and are therefore quoted by Qt itself
        assert_eq!(escape_value("AAAA/wAAAAE=").unwrap(), "\"AAAA/wAAAAE=\"");
    }

    #[test]
    fn unescape_is_the_inverse_for_everything_escape_produces() {
        let corpus = [
            "",
            "plain",
            "with spaces",
            " leading",
            "trailing ",
            "\"quoted\" arg",
            "C:\\Program Files\\Prism\\prism.exe",
            "a=b;c,d",
            "\\\\server\\share",
            "tab\there\nnewline\r\n",
            "ünï 日本 🎮 Мир",
            "\u{1}\u{2}12 abc \u{1f}",
            "@@double",
            "@single",
            "#hash # not a comment",
            "'single' \"double\"",
            "\\n literally",
            "trailing backslash\\",
            "\"/home/u/Pack Sync/ünï/packsync\" hook pre-launch --instance 00000000-0000-4000-8000-000000000000 --marker packsync-hook -- \"/usr/bin/echo\" 'x y' a=b",
        ];
        for c in corpus {
            let esc = escape_value(c).unwrap();
            assert!(!esc.contains('\n') && !esc.contains('\r'), "single line: {esc:?}");
            let back = unescape_value(esc.as_bytes());
            assert_eq!(back, QtValue::Str(c.to_string()), "value {c:?} escaped as {esc:?}");
        }
    }

    #[test]
    fn random_strings_round_trip() {
        let mut rng = fastrand::Rng::with_seed(11);
        let alphabet: Vec<char> = "ab c\"\\=;,'#@\t\nü日🎮0123456789xXfF\u{1}\u{7}%$-_./".chars().collect();
        for _ in 0..5000 {
            let len = rng.usize(0..24);
            let v: String = (0..len).map(|_| alphabet[rng.usize(0..alphabet.len())]).collect();
            let esc = escape_value(&v).unwrap();
            let line = format!("k={esc}\n");
            let doc = IniDoc::parse(line.as_bytes()).unwrap();
            let got = doc.get("k").unwrap_or_else(|| panic!("no key for {v:?} -> {line:?}"));
            assert_eq!(got, QtValue::Str(v.clone()), "{v:?} via {esc:?}");
        }
    }

    #[test]
    fn quotes_are_consumed_and_spaces_after_a_closing_quote_vanish() {
        // This is what the real Prism does to a hand-written `PreLaunchCommand="path with spaces" arg`
        // (observed with Prism 11.0.3): the quotes disappear and the space after the closing quote is
        // swallowed, which is why quotes MUST be written as \" in instance.cfg.
        assert_eq!(s(&unescape_value(b"\"/p q/exe\" --flag")), "/p q/exe--flag");
        assert_eq!(s(&unescape_value(b"\\\"/p q/exe\\\" --flag")), "\"/p q/exe\" --flag");
    }

    #[test]
    fn commas_make_lists_and_lists_stringify_to_empty() {
        assert_eq!(unescape_value(b"a,b"), QtValue::List(vec!["a".into(), "b".into()]));
        assert_eq!(s(&unescape_value(b"a,b")), "");
        assert_eq!(unescape_value(b"\"a,b\""), QtValue::Str("a,b".into()));
        assert_eq!(unescape_value(b"a, b ,c"), QtValue::List(vec!["a".into(), "b".into(), "c".into()]));
    }

    #[test]
    fn escapes_and_special_values() {
        assert_eq!(unescape_value(b"a\\nb\\tc\\\\d\\\"e"), QtValue::Str("a\nb\tc\\d\"e".into()));
        assert_eq!(unescape_value(b"\\x41\\x42z"), QtValue::Str("ABz".into()));
        assert_eq!(unescape_value(b"\\101"), QtValue::Str("A".into()));
        assert_eq!(unescape_value(b"C:\\Program"), QtValue::Str("C:rogram".into()), "unknown escapes drop the char");
        assert_eq!(unescape_value(b"  padded  "), QtValue::Str("padded".into()));
        assert_eq!(unescape_value(b"\"  padded  \""), QtValue::Str("  padded  ".into()));
        assert_eq!(unescape_value(b"@@x"), QtValue::Str("@x".into()));
        assert!(matches!(unescape_value(b"@Invalid()"), QtValue::Special(_)));
        assert!(matches!(unescape_value(b"@ByteArray(abc)"), QtValue::Special(_)));
        assert_eq!(unescape_value("ünï".as_bytes()), QtValue::Str("ünï".into()));
    }

    #[test]
    fn keys_use_backslash_as_group_separator_and_percent_escapes() {
        assert_eq!(unescape_key(b"mods_Page\\Columns"), "mods_Page/Columns");
        assert_eq!(unescape_key(b"a%20b"), "a b");
        assert_eq!(unescape_key(b"x%U00FCz"), "xüz");
        assert_eq!(escape_key("UI/mods_Page"), "UI\\mods_Page");
        assert_eq!(escape_key("a b"), "a%20b");
        assert_eq!(unescape_key(escape_key("weird key/ü日").as_bytes()), "weird key/ü日");
    }

    #[test]
    fn logical_lines_follow_qts_reader() {
        let text = b"; leading comment\n[General]\nname=My Pack ; inline comment\nnotes=line1\\\nline2\n\n  \nquoted=\"a\nb\"\nlast=1";
        let doc = IniDoc::parse(text).unwrap();
        let keys: Vec<(String, String)> = doc
            .entries
            .iter()
            .filter(|e| matches!(e.kind, EntryKind::Pair { .. }))
            .map(|e| (e.key.clone(), doc.value_of(e).unwrap().prism_string()))
            .collect();
        assert_eq!(
            keys,
            vec![
                ("name".into(), "My Pack".into()),
                ("notes".into(), "line1line2".into()),
                ("quoted".into(), "a\nb".into()),
                ("last".into(), "1".into()),
            ]
        );
    }

    #[test]
    fn keys_in_ui_section_are_prefixed_and_general_is_case_insensitive() {
        let text = b"[general]\nA=1\n[UI]\nmods_Page\\Columns=\"AAAA/wAAAAE=\"\n[%General]\nB=2\n";
        let doc = IniDoc::parse(text).unwrap();
        assert_eq!(s(&doc.get("A").unwrap()), "1");
        assert_eq!(s(&doc.get("UI/mods_Page/Columns").unwrap()), "AAAA/wAAAAE=");
        assert_eq!(s(&doc.get("General/B").unwrap()), "2", "[%General] is a real group named General");
        assert!(doc.get("B").is_none());
        assert_eq!(s(&doc.get("a").unwrap()), "1", "lookups are case-insensitive");
    }

    #[test]
    fn bom_crlf_and_missing_final_newline_are_preserved() {
        let mut text = Vec::new();
        text.extend_from_slice(b"\xEF\xBB\xBF[General]\r\nname=Pack\r\nOverrideCommands=false");
        let out = apply_edits(
            &text,
            &[
                KeyEdit::Set { key: "OverrideCommands".into(), value: "true".into() },
                KeyEdit::Set { key: "PreLaunchCommand".into(), value: "\"x y\" a".into() },
            ],
        )
        .unwrap();
        let expect = "\u{feff}[General]\r\nname=Pack\r\nOverrideCommands=true\r\nPreLaunchCommand=\\\"x y\\\" a\r\n";
        assert_eq!(String::from_utf8(out).unwrap(), expect);
    }

    #[test]
    fn edits_touch_only_their_own_lines() {
        let text = "[General]\nConfigVersion=1.3\nInstanceType=OneSix\nname=Weird ünï name ; keep\nOverrideCommands=false\nPostExitCommand=old\nzzz=last\n\n[UI]\nmods_Page\\Columns=\"AAAA/wAAAAE=\"\nPostExitCommand=ui-section-is-not-general\n";
        let out = apply_edits(
            text.as_bytes(),
            &[
                KeyEdit::Set { key: "OverrideCommands".into(), value: "true".into() },
                KeyEdit::Set { key: "PostExitCommand".into(), value: "new = value".into() },
                KeyEdit::Set { key: "PreLaunchCommand".into(), value: "pre".into() },
            ],
        )
        .unwrap();
        let expect = "[General]\nConfigVersion=1.3\nInstanceType=OneSix\nname=Weird ünï name ; keep\nOverrideCommands=true\nPostExitCommand=\"new = value\"\nzzz=last\nPreLaunchCommand=pre\n\n[UI]\nmods_Page\\Columns=\"AAAA/wAAAAE=\"\nPostExitCommand=ui-section-is-not-general\n";
        assert_eq!(String::from_utf8(out).unwrap(), expect);
    }

    #[test]
    fn remove_deletes_whole_lines_and_set_raw_restores_verbatim() {
        let text = "[General]\nA=1\n  B = two ; c\nC=3\n";
        let out = apply_edits(text.as_bytes(), &[KeyEdit::Remove { key: "B".into() }]).unwrap();
        assert_eq!(String::from_utf8(out).unwrap(), "[General]\nA=1\nC=3\n");
        let out =
            apply_edits(text.as_bytes(), &[KeyEdit::SetRaw { key: "B".into(), line: "B=restored".into() }]).unwrap();
        assert_eq!(String::from_utf8(out).unwrap(), "[General]\nA=1\n  B=restored\nC=3\n");
    }

    #[test]
    fn keys_in_a_file_without_sections_or_empty_files_work() {
        let out = apply_edits(b"", &[KeyEdit::Set { key: "name".into(), value: "x".into() }]).unwrap();
        assert_eq!(out, b"[General]\nname=x\n");
        let out = apply_edits(b"a=1\n", &[KeyEdit::Set { key: "b".into(), value: "2".into() }]).unwrap();
        assert_eq!(out, b"a=1\nb=2\n");
        let out = apply_edits(b"[UI]\nx=1\n", &[KeyEdit::Set { key: "b".into(), value: "2".into() }]).unwrap();
        assert_eq!(String::from_utf8(out).unwrap(), "[General]\nb=2\n\n[UI]\nx=1\n");
    }

    #[test]
    fn duplicate_keys_are_refused_for_set() {
        let err = apply_edits(b"a=1\na=2\n", &[KeyEdit::Set { key: "a".into(), value: "3".into() }]).unwrap_err();
        assert!(err.to_string().contains("more than once"), "{err}");
    }

    #[test]
    fn garbage_never_panics() {
        let mut rng = fastrand::Rng::with_seed(3);
        let pieces: [&[u8]; 12] = [b"[", b"]", b"=", b"\"", b"\\", b";", b"\n", b"\r", b"a", b" ", b"\xff", b"%U"];
        for _ in 0..3000 {
            let mut buf = Vec::new();
            for _ in 0..rng.usize(0..40) {
                buf.extend_from_slice(pieces[rng.usize(0..pieces.len())]);
            }
            if let Ok(doc) = IniDoc::parse(&buf) {
                for e in &doc.entries {
                    let _ = doc.value_of(e);
                }
                let _ = apply_edits(&buf, &[KeyEdit::Set { key: "k".into(), value: "v".into() }]);
                let _ = apply_edits(&buf, &[KeyEdit::Remove { key: "a".into() }]);
            }
        }
    }

    #[test]
    fn oversized_input_is_rejected() {
        let big = vec![b'a'; MAX_INI_BYTES + 1];
        assert!(IniDoc::parse(&big).is_err());
    }

    #[test]
    fn bool_parsing_follows_qvariant() {
        assert!(parse_bool("true") && parse_bool("1") && parse_bool("yes"));
        assert!(!parse_bool("false") && !parse_bool("FALSE") && !parse_bool("0") && !parse_bool(""));
    }
}
