//! The reference link a hover carries.
//!
//! In `render/` rather than `dictionary/`: which external site a version maps to
//! is protocol presentation, not something the dictionary knows. `documentLink`
//! and `fix.tagUrl` (work-order item 4d) will read the same mapping.

use percent_encoding::{utf8_percent_encode, AsciiSet, NON_ALPHANUMERIC};

const SEGMENT: &AsciiSet = &NON_ALPHANUMERIC
    .remove(b'-')
    .remove(b'.')
    .remove(b'_')
    .remove(b'~');

/// How the *default* reference site spells a FIX version in a path, or `None`
/// when it publishes no dictionary for one.
///
/// Onixs's numbering, not the protocol's, and it is lossy: `FIX.5.0SP1`,
/// `FIX.5.0SP2` and `FIXT.1.1` are all `5.0` there, because the site keeps one
/// page for the family. That is why `{fixVersion}` exists beside `{version}` —
/// a reference that does distinguish them cannot be addressed through this
/// mapping.
///
/// A table rather than a derivation: the site's own numbering is what this
/// has to match, and it is not a function of the version string.
///
/// `FIXT.1.1` maps to `/5.0/`, which is easy to get wrong in either direction:
/// the site publishes no FIXT page, and the session layer's fields are
/// documented under 5.0.
fn segment(version: &str) -> Option<&'static str> {
    Some(match version {
        "FIX.2.7" => "2.7",
        "FIX.3.0" => "3.0",
        "FIX.4.0" => "4.0",
        "FIX.4.1" => "4.1",
        "FIX.4.2" => "4.2",
        "FIX.4.3" => "4.3",
        "FIX.4.4" => "4.4",
        "FIX.5.0" | "FIX.5.0SP1" | "FIX.5.0SP2" | "FIXT.1.1" => "5.0",
        _ => return None,
    })
}

/// Where a tag's reference page lives, as `fix.documentLink.url` says.
///
/// A template rather than a constant because the reference need not be Onixs:
/// a desk with its own dictionary browser points this at that. The schema
/// reserved the key from the start, so hover reads it rather than hardcoding
/// what `fix.tagUrl` in 4(d) would then have to hardcode again.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TagUrl {
    template: String,
    valid: bool,
}

impl TagUrl {
    pub fn validate(template: &str) -> Result<(), &'static str> {
        if template.is_empty() {
            return Ok(());
        }
        let sample = template
            .replace(VERSION, "4.4")
            .replace(FIX_VERSION, "FIX.4.4")
            .replace(TAG, "35");
        let authority = sample
            .split_once("://")
            .map(|(_, rest)| rest.split(['/', '?', '#']).next().unwrap_or_default());
        if authority.is_none_or(str::is_empty) {
            return Err("documentLink.url must name a host");
        }
        let parsed = lsp_types::Uri::parse(&sample).map_err(|_| "invalid URL template")?;
        if !matches!(parsed.scheme(), "http" | "https") || parsed.host_str().is_none() {
            return Err("documentLink.url must be an http or https URL, or empty");
        }
        if template.chars().any(char::is_whitespace) {
            return Err("documentLink.url must not contain whitespace");
        }
        Ok(())
    }

    pub fn new(template: impl Into<String>) -> TagUrl {
        let template = template.into();
        let valid = Self::validate(&template).is_ok();
        TagUrl { template, valid }
    }

    /// What to call the site in a link's text: its host, without `www.`.
    ///
    /// Derived rather than configured. The URL is a template a user may point
    /// anywhere, and a label that still said `onixs.biz` would be a lie the
    /// user cannot correct without a second setting. `None` when the template
    /// names no host, and the link then goes unlabelled rather than guessing.
    pub fn host(&self) -> Option<&str> {
        if !self.valid {
            return None;
        }
        let after_scheme = self.template.split_once("://")?.1;
        let host = after_scheme.split('/').next()?;
        // Credentials and ports stay off the label: neither names the site.
        let host = host.rsplit_once('@').map_or(host, |(_, after)| after);
        let host = host.split_once(':').map_or(host, |(before, _)| before);
        let host = host.strip_prefix("www.").unwrap_or(host);
        (!host.is_empty()).then_some(host)
    }

    /// `None` when the template needs a version segment the default site does
    /// not publish, or when the template is empty — which is how a user turns
    /// the link off without turning off hover.
    pub fn of(&self, version: &str, tag: u32) -> Option<String> {
        if self.template.is_empty() || !self.valid {
            return None;
        }
        let mut url = self.template.clone();
        if url.contains(VERSION) {
            // Only resolved when the template asks for it. A template built on
            // `{fixVersion}` or on the tag alone answers for any version,
            // including the ones the default site does not publish.
            url = url.replace(
                VERSION,
                &utf8_percent_encode(segment(version)?, SEGMENT).to_string(),
            );
        }
        url = url.replace(
            FIX_VERSION,
            &utf8_percent_encode(version, SEGMENT).to_string(),
        );
        Some(url.replace(TAG, &tag.to_string()))
    }
}

impl Default for TagUrl {
    fn default() -> TagUrl {
        TagUrl::new(crate::config::DocumentLink::default().url)
    }
}

/// The version as the reference site spells it — lossy, and required by the
/// default URL.
const VERSION: &str = "{version}";
/// The line's own version string, verbatim. Substituted after `{version}` so
/// that a template using both gets each in its own form.
const FIX_VERSION: &str = "{fixVersion}";
const TAG: &str = "{tag}";

#[cfg(test)]
mod tests {
    use super::*;

    fn onixs() -> TagUrl {
        TagUrl::default()
    }

    #[test]
    fn the_site_uses_its_own_version_segments() {
        assert_eq!(
            onixs().of("FIX.4.4", 35).as_deref(),
            Some("https://www.onixs.biz/fix-dictionary/4.4/tagNum_35.html")
        );
        for version in ["FIX.5.0", "FIX.5.0SP1", "FIX.5.0SP2"] {
            assert_eq!(
                onixs().of(version, 1).as_deref(),
                Some("https://www.onixs.biz/fix-dictionary/5.0/tagNum_1.html"),
                "{version}"
            );
        }
    }

    /// FIXT.1.1 links to the 5.0 page — the transport version documents the
    /// session layer of the same edition. Asserted separately from the 5.0
    /// family because its table entry is the one that is easy to miss.
    #[test]
    fn fixt_links_to_the_five_oh_page() {
        assert_eq!(
            onixs().of("FIXT.1.1", 8).as_deref(),
            Some("https://www.onixs.biz/fix-dictionary/5.0/tagNum_8.html")
        );
    }

    #[test]
    fn a_version_the_site_does_not_publish_has_no_link() {
        assert_eq!(onixs().of("FIX.9.9", 35), None);
        assert_eq!(onixs().of("", 35), None);
    }

    #[test]
    fn a_configured_template_is_used_as_written() {
        let own = TagUrl::new("https://fix.internal/{version}/t{tag}");
        assert_eq!(
            own.of("FIX.4.2", 54).as_deref(),
            Some("https://fix.internal/4.2/t54")
        );
    }

    /// A template that never asks for a version answers for any version,
    /// including the ones the segment table does not name.
    #[test]
    fn a_template_without_a_version_needs_none() {
        let flat = TagUrl::new("https://fix.internal/tag/{tag}");
        assert_eq!(
            flat.of("FIX.9.9", 54).as_deref(),
            Some("https://fix.internal/tag/54")
        );
    }

    /// The reason `{fixVersion}` exists: Onixs keeps one page for the whole 5.0
    /// family, so `{version}` cannot tell SP1 from SP2. A reference that does
    /// distinguish them is unreachable through the site's own numbering.
    #[test]
    fn fix_version_keeps_what_the_sites_numbering_loses() {
        let onixs = onixs();
        assert_eq!(
            onixs.of("FIX.5.0SP1", 1),
            onixs.of("FIX.5.0SP2", 1),
            "the default site collapses them"
        );

        let own = TagUrl::new("https://fix.internal/{fixVersion}/{tag}");
        assert_eq!(
            own.of("FIX.5.0SP1", 1).as_deref(),
            Some("https://fix.internal/FIX.5.0SP1/1")
        );
        assert_eq!(
            own.of("FIX.5.0SP2", 1).as_deref(),
            Some("https://fix.internal/FIX.5.0SP2/1")
        );
        assert_eq!(
            own.of("FIXT.1.1", 1).as_deref(),
            Some("https://fix.internal/FIXT.1.1/1"),
            "and FIXT keeps its own name rather than the tables it reads"
        );
    }

    /// A version the default site does not publish is still addressable, as
    /// long as the template does not ask for the site's own spelling.
    #[test]
    fn fix_version_answers_where_the_segment_table_cannot() {
        let own = TagUrl::new("https://fix.internal/{fixVersion}/{tag}");
        assert_eq!(
            own.of("FIX.9.9", 1).as_deref(),
            Some("https://fix.internal/FIX.9.9/1")
        );
    }

    /// Both in one template, each in its own form.
    #[test]
    fn a_template_may_use_both() {
        let both = TagUrl::new("https://fix.internal/{version}/{fixVersion}/{tag}");
        assert_eq!(
            both.of("FIX.5.0SP2", 8).as_deref(),
            Some("https://fix.internal/5.0/FIX.5.0SP2/8")
        );
    }

    /// Emptying the URL removes the link without touching `hover.enabled` —
    /// the only way to say "hover, but no outbound link".
    #[test]
    fn an_empty_template_is_how_the_link_is_turned_off() {
        assert_eq!(TagUrl::new("").of("FIX.4.4", 35), None);
    }

    /// The label follows the URL, so pointing the template elsewhere does not
    /// leave a link claiming to be Onixs.
    #[test]
    fn the_label_is_the_hosts_own_name() {
        assert_eq!(onixs().host(), Some("onixs.biz"), "www. is not a name");
        assert_eq!(
            TagUrl::new("https://fix.internal/{tag}").host(),
            Some("fix.internal")
        );
        assert_eq!(
            TagUrl::new("http://fix.internal:8080/x/{tag}").host(),
            Some("fix.internal"),
            "a port names a socket, not a site"
        );
        assert_eq!(
            TagUrl::new("https://me@fix.internal/{tag}").host(),
            Some("fix.internal")
        );
    }

    #[test]
    fn a_template_that_names_no_host_has_no_label() {
        assert_eq!(TagUrl::new("").host(), None);
        assert_eq!(TagUrl::new("/local/{tag}.html").host(), None);
        assert_eq!(TagUrl::new("https:///{tag}").host(), None);
    }

    #[test]
    fn only_http_urls_are_allowed() {
        for template in [
            "file:///tmp/{tag}",
            "javascript:alert(1)",
            "/relative/{tag}",
            "ftp://host/{tag}",
        ] {
            assert!(TagUrl::validate(template).is_err());
            assert_eq!(TagUrl::new(template).of("FIX.4.4", 35), None);
        }
        assert!(TagUrl::validate("").is_ok());
        assert!(TagUrl::validate("https://host/{fixVersion}/{tag}").is_ok());
    }

    #[test]
    fn substituted_values_cannot_escape_their_url_segment() {
        let link = TagUrl::new("https://host/{fixVersion}/{tag}");
        assert_eq!(
            link.of("FIX/4 ?#%é", 35).as_deref(),
            Some("https://host/FIX%2F4%20%3F%23%25%C3%A9/35")
        );
    }
}
