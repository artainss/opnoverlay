//! Looks up the latest obs-lite release on GitHub once at start-up. Settings › General can turn
//! it off. Nothing is downloaded or installed: the user is only told, with a link.

const LATEST: &str = "https://api.github.com/repos/25Dark25/ObsLite/releases/latest";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Release {
    pub version: String,
    pub url: String,
}

/// The latest release, if it is newer than this build. Errors (no network, rate limit…) are
/// returned so the caller can ignore them.
pub fn newer_release() -> Result<Option<Release>, String> {
    let json = crate::platform::sys::http_get(LATEST)?;
    let release = parse(&json).ok_or("unexpected response")?;
    Ok(is_newer(&release.version, env!("CARGO_PKG_VERSION")).then_some(release))
}

/// Release tag and page from the GitHub API response.
fn parse(json: &str) -> Option<Release> {
    let value: serde_json::Value = serde_json::from_str(json).ok()?;
    let tag = value.get("tag_name")?.as_str()?;
    let url = value.get("html_url")?.as_str()?;
    Some(Release { version: tag.trim_start_matches('v').to_owned(), url: url.to_owned() })
}

/// Whether `candidate` ("0.3.0") is a later version than `current`.
fn is_newer(candidate: &str, current: &str) -> bool {
    let parts = |v: &str| -> Option<Vec<u64>> { v.split('.').map(|p| p.parse().ok()).collect() };
    match (parts(candidate), parts(current)) {
        (Some(a), Some(b)) => a > b,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions_compare_numerically() {
        assert!(is_newer("0.3.0", "0.2.1"));
        assert!(is_newer("0.10.0", "0.9.9"));
        assert!(!is_newer("0.2.1", "0.2.1"));
        assert!(!is_newer("0.2.0", "0.2.1"));
        assert!(!is_newer("0.3.0-beta", "0.2.1")); // not a plain version: ignored
    }

    #[test]
    fn release_is_read_from_the_github_response() {
        let json = r#"{"tag_name": "v0.3.0", "html_url": "https://github.com/25Dark25/ObsLite/releases/tag/v0.3.0", "assets": []}"#;
        let release = parse(json).unwrap();
        assert_eq!(release.version, "0.3.0");
        assert_eq!(release.url, "https://github.com/25Dark25/ObsLite/releases/tag/v0.3.0");
        assert_eq!(parse(r#"{"message": "Not Found"}"#), None);
    }
}
