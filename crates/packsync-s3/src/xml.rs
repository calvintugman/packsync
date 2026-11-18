//! The little XML S3 sends back: ListObjectsV2 pages and error documents. Not a general XML parser:
//! elements are found by name, without attributes or nesting of the same element, which is all these
//! documents use. Text is unescaped.

pub(crate) struct ListedFile {
    pub key: String,
    pub size: u64,
    pub etag: String,
    pub last_modified: String,
}

pub(crate) struct ListPage {
    pub files: Vec<ListedFile>,
    pub prefixes: Vec<String>,
    pub truncated: bool,
    pub next_token: Option<String>,
}

/// Every `<tag ...>…</tag>` body in `xml`, in order.
fn blocks<'a>(xml: &'a str, tag: &str) -> Vec<&'a str> {
    let prefix = format!("<{tag}");
    let close = format!("</{tag}>");
    let mut out = vec![];
    let mut rest = xml;
    while let Some(i) = rest.find(&prefix) {
        let after_name = &rest[i + prefix.len()..];
        match after_name.as_bytes().first() {
            Some(b'>' | b' ' | b'\t' | b'\r' | b'\n' | b'/') => {}
            _ => {
                rest = after_name;
                continue;
            }
        }
        let Some(gt) = after_name.find('>') else { break };
        if gt > 0 && after_name.as_bytes()[gt - 1] == b'/' {
            out.push("");
            rest = &after_name[gt + 1..];
            continue;
        }
        let body_start = &after_name[gt + 1..];
        let Some(j) = body_start.find(&close) else { break };
        out.push(&body_start[..j]);
        rest = &body_start[j + close.len()..];
    }
    out
}

/// The text of the first `<tag>` element, unescaped.
pub(crate) fn text(xml: &str, tag: &str) -> Option<String> {
    blocks(xml, tag).first().map(|s| unescape(s))
}

fn unescape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(i) = rest.find('&') {
        out.push_str(&rest[..i]);
        let after = &rest[i..];
        let Some(end) = after.find(';') else {
            out.push_str(after);
            return out;
        };
        let ent = &after[1..end];
        let ch = match ent {
            "amp" => Some('&'),
            "lt" => Some('<'),
            "gt" => Some('>'),
            "quot" => Some('"'),
            "apos" => Some('\''),
            _ => ent
                .strip_prefix("#x")
                .and_then(|h| u32::from_str_radix(h, 16).ok())
                .or_else(|| ent.strip_prefix('#').and_then(|d| d.parse().ok()))
                .and_then(char::from_u32),
        };
        match ch {
            Some(c) => out.push(c),
            None => out.push_str(&after[..=end]),
        }
        rest = &after[end + 1..];
    }
    out.push_str(rest);
    out
}

pub(crate) fn parse_list(xml: &str) -> Result<ListPage, String> {
    if !xml.contains("<ListBucketResult") {
        return Err("the listing response is not a ListBucketResult document".into());
    }
    let mut files = vec![];
    for c in blocks(xml, "Contents") {
        let key = text(c, "Key").ok_or("a listed object has no key")?;
        let size = text(c, "Size").and_then(|s| s.trim().parse().ok()).ok_or("a listed object has no size")?;
        files.push(ListedFile {
            key,
            size,
            etag: text(c, "ETag").unwrap_or_default(),
            last_modified: text(c, "LastModified").unwrap_or_default(),
        });
    }
    let prefixes = blocks(xml, "CommonPrefixes").iter().filter_map(|c| text(c, "Prefix")).collect();
    Ok(ListPage {
        files,
        prefixes,
        truncated: text(xml, "IsTruncated").is_some_and(|t| t.trim() == "true"),
        next_token: text(xml, "NextContinuationToken"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_listing_page_and_an_error() {
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<ListBucketResult xmlns="http://s3.amazonaws.com/doc/2006-03-01/"><Name>b</Name><Prefix>packsync/packs/</Prefix>
<KeyCount>2</KeyCount><MaxKeys>1000</MaxKeys><Delimiter>/</Delimiter><IsTruncated>true</IsTruncated>
<Contents><Key>packsync/packs/a.pack</Key><LastModified>2026-11-10T05:00:00.000Z</LastModified>
<ETag>&quot;9e107d9d372bb6826bd81d3542a419d6&quot;</ETag><Size>16777216</Size><StorageClass>STANDARD</StorageClass></Contents>
<CommonPrefixes><Prefix>packsync/packs/sub/</Prefix></CommonPrefixes>
<NextContinuationToken>1ueGcxLPRx1Tr/XYExHnhbYLgveDs2J/wm36Hy4vbOwM=</NextContinuationToken></ListBucketResult>"#;
        let p = parse_list(xml).unwrap();
        assert_eq!(p.files.len(), 1);
        assert_eq!(p.files[0].key, "packsync/packs/a.pack");
        assert_eq!(p.files[0].size, 16777216);
        assert_eq!(p.files[0].etag, "\"9e107d9d372bb6826bd81d3542a419d6\"");
        assert_eq!(p.prefixes, vec!["packsync/packs/sub/".to_string()]);
        assert!(p.truncated);
        assert_eq!(p.next_token.as_deref(), Some("1ueGcxLPRx1Tr/XYExHnhbYLgveDs2J/wm36Hy4vbOwM="));
        let err = "<Error><Code>SignatureDoesNotMatch</Code><Message>a &amp; b</Message></Error>";
        assert_eq!(text(err, "Code").as_deref(), Some("SignatureDoesNotMatch"));
        assert_eq!(text(err, "Message").as_deref(), Some("a & b"));
        assert!(parse_list(err).is_err());

        let with_attrs = r#"<ListBucketResult xmlns="http://s3.amazonaws.com/doc/2006-03-01/"><Contents id="1"><Key attr="k">a/b</Key><Size type="int">42</Size></Contents><Contents/><Keyring>ignored</Keyring></ListBucketResult>"#;
        assert_eq!(blocks(with_attrs, "Contents"), vec![r#"<Key attr="k">a/b</Key><Size type="int">42</Size>"#, ""]);
        assert_eq!(text(with_attrs, "Key").as_deref(), Some("a/b"));
        assert_eq!(text(with_attrs, "Size").as_deref(), Some("42"));
    }
}
