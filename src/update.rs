//! The platform independent half of auto-update: reading GitHub's release
//! description and deciding whether it is newer than the running version.

/// "v1.2.3" or "1.2.3" as numbers. Anything else, including pre-release
/// suffixes, is rejected so the updater never moves to something odd.
pub fn parse_version(s: &str) -> Option<(u32, u32, u32)> {
    let s = s.trim().trim_start_matches('v');
    let mut it = s.split('.');
    let v = (it.next()?.parse().ok()?, it.next()?.parse().ok()?, it.next()?.parse().ok()?);
    if it.next().is_some() {
        return None;
    }
    Some(v)
}

pub fn is_newer(remote: &str, local: &str) -> bool {
    match (parse_version(remote), parse_version(local)) {
        (Some(r), Some(l)) => r > l,
        _ => false,
    }
}

fn unescape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut it = s.chars();
    while let Some(c) = it.next() {
        if c == '\\' {
            match it.next() {
                Some('/') => out.push('/'),
                Some('"') => out.push('"'),
                Some('\\') => out.push('\\'),
                Some(o) => {
                    out.push('\\');
                    out.push(o);
                }
                None => {}
            }
        } else {
            out.push(c);
        }
    }
    out
}

/// Every string value stored under `key`, in order of appearance.
fn string_values(text: &str, key: &str) -> Vec<String> {
    let needle = format!("\"{key}\"");
    let mut out = Vec::new();
    let mut rest = text;
    while let Some(i) = rest.find(&needle) {
        rest = &rest[i + needle.len()..];
        let after = rest.trim_start();
        let Some(after) = after.strip_prefix(':') else { continue };
        let Some(after) = after.trim_start().strip_prefix('"') else { continue };
        let mut end = None;
        let mut escaped = false;
        for (j, c) in after.char_indices() {
            if escaped {
                escaped = false;
            } else if c == '\\' {
                escaped = true;
            } else if c == '"' {
                end = Some(j);
                break;
            }
        }
        if let Some(e) = end {
            out.push(unescape(&after[..e]));
            rest = &after[e..];
        }
    }
    out
}

pub fn tag_name(release_json: &str) -> Option<String> {
    string_values(release_json, "tag_name").into_iter().next()
}

/// Download address of the release file called `file_name`.
pub fn asset_url(release_json: &str, file_name: &str) -> Option<String> {
    let suffix = format!("/{file_name}");
    string_values(release_json, "browser_download_url").into_iter().find(|u| u.ends_with(&suffix))
}

/// The hash from a checksum file such as `<64 hex>  clip-a-pc.exe`.
pub fn expected_hash(text: &str) -> Option<String> {
    let h: String = text.trim_start().chars().take(64).collect();
    (h.len() == 64 && h.chars().all(|c| c.is_ascii_hexdigit())).then(|| h.to_ascii_lowercase())
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"{
  "url": "https://api.github.com/repos/o/r/releases/1",
  "tag_name": "v0.2.0",
  "name": "v0.2.0",
  "assets": [
    {
      "name": "clip-a-pc.exe.sha256",
      "browser_download_url": "https://github.com/o/r/releases/download/v0.2.0/clip-a-pc.exe.sha256"
    },
    {
      "name": "clip-a-pc.exe",
      "browser_download_url": "https://github.com/o/r/releases/download/v0.2.0/clip-a-pc.exe"
    }
  ]
}"#;

    #[test]
    fn versions_compare_numerically() {
        assert!(is_newer("v0.1.10", "0.1.9"));
        assert!(is_newer("1.0.0", "0.9.9"));
        assert!(!is_newer("v0.1.0", "0.1.0"));
        assert!(!is_newer("v0.0.9", "0.1.0"));
    }

    #[test]
    fn odd_versions_are_ignored() {
        assert_eq!(parse_version("v1.2"), None);
        assert_eq!(parse_version("v1.2.3-beta"), None);
        assert_eq!(parse_version("nightly"), None);
        assert!(!is_newer("nightly", "0.1.0"));
        assert!(!is_newer("v9.9.9", "dev"));
    }

    #[test]
    fn reads_tag_and_picks_the_right_asset() {
        assert_eq!(tag_name(SAMPLE).as_deref(), Some("v0.2.0"));
        assert_eq!(
            asset_url(SAMPLE, "clip-a-pc.exe").as_deref(),
            Some("https://github.com/o/r/releases/download/v0.2.0/clip-a-pc.exe")
        );
        assert_eq!(
            asset_url(SAMPLE, "clip-a-pc.exe.sha256").as_deref(),
            Some("https://github.com/o/r/releases/download/v0.2.0/clip-a-pc.exe.sha256")
        );
        assert_eq!(asset_url(SAMPLE, "other.zip"), None);
    }

    #[test]
    fn works_on_compact_json_and_escapes() {
        let j = r#"{"tag_name":"v1.0.0","assets":[{"browser_download_url":"https:\/\/x.io\/d\/clip-a-pc.exe"}]}"#;
        assert_eq!(tag_name(j).as_deref(), Some("v1.0.0"));
        assert_eq!(asset_url(j, "clip-a-pc.exe").as_deref(), Some("https://x.io/d/clip-a-pc.exe"));
        assert_eq!(tag_name("not json"), None);
        assert_eq!(tag_name(r#"{"message":"Not Found"}"#), None);
    }

    #[test]
    fn checksum_file_parsing() {
        let h = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";
        assert_eq!(expected_hash(&format!("{h}  clip-a-pc.exe\n")).as_deref(), Some(h));
        assert_eq!(expected_hash(&h.to_uppercase()).as_deref(), Some(h));
        assert_eq!(expected_hash("short"), None);
        assert_eq!(expected_hash("<html>not a checksum at all, sorry about that, really not one</html>"), None);
    }
}
