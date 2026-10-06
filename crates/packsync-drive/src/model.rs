//! Drive API v3 JSON shapes. Everything the server sends is untrusted: raw structs accept any
//! shape serde can map, and [`DriveFile::from_raw`] validates IDs, sizes and checksums before
//! anything is used in a URL, a query or a comparison.

use std::cmp::Ordering;
use std::collections::BTreeMap;

use serde::{Deserialize, Deserializer};

use crate::config::FOLDER_MIME;
use crate::util::{is_hex_len, is_valid_drive_id, parse_rfc3339_ms};

/// Fields requested for every file resource.
pub(crate) const FILE_FIELDS: &str =
    "id,name,mimeType,size,md5Checksum,sha256Checksum,createdTime,modifiedTime,parents,appProperties,trashed";

/// Fields requested for `files.list`.
pub(crate) fn list_fields() -> String {
    format!("nextPageToken,incompleteSearch,files({FILE_FIELDS})")
}

/// appProperties key marking objects created by PackSync (retention only touches marked objects).
pub(crate) const PROP_MARKER: &str = "ps";
/// appProperties key identifying the root folder (`psk=root`).
pub(crate) const PROP_KIND: &str = "psk";
/// Value of [`PROP_KIND`] on the root folder.
pub(crate) const KIND_ROOT: &str = "root";

/// int64 values arrive as JSON strings (`"size": "123"`); accept numbers too.
fn de_opt_u64<'de, D: Deserializer<'de>>(d: D) -> Result<Option<u64>, D::Error> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Num {
        S(String),
        N(u64),
    }
    Ok(match Option::<Num>::deserialize(d)? {
        None => None,
        Some(Num::N(n)) => Some(n),
        Some(Num::S(s)) => Some(s.trim().parse::<u64>().map_err(serde::de::Error::custom)?),
    })
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct RawFile {
    pub id: Option<String>,
    pub name: Option<String>,
    pub mime_type: Option<String>,
    #[serde(default, deserialize_with = "de_opt_u64")]
    pub size: Option<u64>,
    pub md5_checksum: Option<String>,
    pub sha256_checksum: Option<String>,
    pub created_time: Option<String>,
    pub modified_time: Option<String>,
    pub parents: Option<Vec<String>>,
    pub app_properties: Option<BTreeMap<String, serde_json::Value>>,
    pub trashed: Option<bool>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct RawFileList {
    #[serde(default)]
    pub files: Vec<RawFile>,
    pub next_page_token: Option<String>,
    #[serde(default)]
    pub incomplete_search: bool,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub(crate) struct RawGeneratedIds {
    #[serde(default)]
    pub ids: Vec<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct RawQuota {
    #[serde(default, deserialize_with = "de_opt_u64")]
    pub limit: Option<u64>,
    #[serde(default, deserialize_with = "de_opt_u64")]
    pub usage: Option<u64>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct RawUser {
    pub display_name: Option<String>,
    pub email_address: Option<String>,
    pub permission_id: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct RawAbout {
    pub storage_quota: Option<RawQuota>,
    pub user: Option<RawUser>,
}

/// A validated file or folder resource.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DriveFile {
    pub id: String,
    pub name: String,
    pub is_folder: bool,
    pub size: Option<u64>,
    /// Lowercase hex.
    pub md5: Option<String>,
    /// Lowercase hex.
    pub sha256: Option<String>,
    pub created_ms: Option<i64>,
    pub modified_ms: Option<i64>,
    pub parents: Vec<String>,
    pub app_properties: BTreeMap<String, String>,
    pub trashed: bool,
}

impl DriveFile {
    /// Validate a raw resource. Malformed checksums are dropped (treated as "not reported"),
    /// malformed IDs reject the whole resource.
    pub fn from_raw(raw: RawFile) -> Result<DriveFile, String> {
        let id = raw.id.ok_or("the server omitted the file ID")?;
        if !is_valid_drive_id(&id) {
            return Err("the server returned a malformed file ID".into());
        }
        let name = raw.name.unwrap_or_default();
        if name.len() > 32 * 1024 {
            return Err("the server returned an oversized file name".into());
        }
        let parents = raw.parents.unwrap_or_default();
        if parents.len() > 64 || !parents.iter().all(|p| is_valid_drive_id(p)) {
            return Err("the server returned malformed parent IDs".into());
        }
        let md5 = raw.md5_checksum.filter(|s| is_hex_len(s, 32)).map(|s| s.to_ascii_lowercase());
        let sha256 = raw.sha256_checksum.filter(|s| is_hex_len(s, 64)).map(|s| s.to_ascii_lowercase());
        let app_properties = raw
            .app_properties
            .unwrap_or_default()
            .into_iter()
            .filter_map(|(k, v)| v.as_str().map(|s| (k, s.to_string())))
            .collect();
        Ok(DriveFile {
            id,
            name,
            is_folder: raw.mime_type.as_deref() == Some(FOLDER_MIME),
            size: raw.size,
            md5,
            sha256,
            created_ms: raw.created_time.as_deref().and_then(parse_rfc3339_ms),
            modified_ms: raw.modified_time.as_deref().and_then(parse_rfc3339_ms),
            parents,
            app_properties,
            trashed: raw.trashed.unwrap_or(false),
        })
    }

    /// Created by PackSync (`appProperties.ps == "1"`).
    pub fn is_ours(&self) -> bool {
        self.app_properties.get(PROP_MARKER).map(String::as_str) == Some("1")
    }

    /// The tags callers see: everything except the adapter's private folder marker. The `ps=1`
    /// marker is reported (every object the adapter lists carries it: listings are filtered on it),
    /// which is how retention knows an object was created by PackSync.
    pub fn user_tags(&self) -> BTreeMap<String, String> {
        self.app_properties
            .iter()
            .filter(|(k, _)| k.as_str() != PROP_KIND)
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect()
    }
}

/// Keys PackSync reserves for itself in `appProperties`.
pub(crate) fn is_reserved_tag(k: &str) -> bool {
    k == PROP_MARKER || k == PROP_KIND
}

/// Deterministic canonical order among objects sharing one name: oldest `createdTime` first
/// (unknown times last), ties broken by the lowest file ID.
pub(crate) fn canonical_order(a_created: Option<i64>, a_id: &str, b_created: Option<i64>, b_id: &str) -> Ordering {
    a_created.unwrap_or(i64::MAX).cmp(&b_created.unwrap_or(i64::MAX)).then_with(|| a_id.cmp(b_id))
}

/// Sort files canonically (in place).
pub(crate) fn sort_canonical(files: &mut [DriveFile]) {
    files.sort_by(|a, b| canonical_order(a.created_ms, &a.id, b.created_ms, &b.id));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_int64_strings_and_validates() {
        let raw: RawFile = serde_json::from_str(
            r#"{"id":"1abc","name":"x.pack","mimeType":"application/octet-stream","size":"12345",
                "md5Checksum":"0123456789ABCDEF0123456789abcdef","sha256Checksum":"zz",
                "createdTime":"2026-10-06T12:00:00.123Z","parents":["p1"],"appProperties":{"ps":"1","k":"v","n":null},
                "trashed":false}"#,
        )
        .unwrap();
        let f = DriveFile::from_raw(raw).unwrap();
        assert_eq!(f.size, Some(12345));
        assert_eq!(f.md5.as_deref(), Some("0123456789abcdef0123456789abcdef"));
        assert_eq!(f.sha256, None, "malformed checksum is dropped");
        assert!(f.is_ours());
        // the `ps` marker is reported (retention relies on it); null-valued properties are dropped
        assert_eq!(
            f.user_tags(),
            [("ps".to_string(), "1".to_string()), ("k".to_string(), "v".to_string())].into_iter().collect()
        );
        assert!(!f.is_folder);

        let raw: RawFile = serde_json::from_str(r#"{"id":"1abc","size":7}"#).unwrap();
        assert_eq!(DriveFile::from_raw(raw).unwrap().size, Some(7));
        assert!(serde_json::from_str::<RawFile>(r#"{"id":"1","size":"-1"}"#).is_err());
        for bad in [r#"{"id":"../x"}"#, r#"{"name":"no id"}"#, r#"{"id":"a","parents":["a/b"]}"#] {
            let raw: RawFile = serde_json::from_str(bad).unwrap();
            assert!(DriveFile::from_raw(raw).is_err(), "{bad}");
        }
    }

    #[test]
    fn canonical_choice_is_deterministic() {
        let mk = |id: &str, c: Option<i64>| DriveFile {
            id: id.into(),
            name: "n".into(),
            is_folder: false,
            size: None,
            md5: None,
            sha256: None,
            created_ms: c,
            modified_ms: None,
            parents: vec![],
            app_properties: BTreeMap::new(),
            trashed: false,
        };
        let mut v = vec![mk("b", Some(5)), mk("z", None), mk("a", Some(5)), mk("c", Some(1))];
        sort_canonical(&mut v);
        let ids: Vec<_> = v.iter().map(|f| f.id.as_str()).collect();
        assert_eq!(ids, ["c", "a", "b", "z"]);
    }
}
