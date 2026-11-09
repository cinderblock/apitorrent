//! The first-party plugins this build ships (the `first-party-plugins` feature; see
//! `build.rs`). At startup the daemon offers them to the plugin host, which installs a new
//! one (or a newer version) for an admin to approve; nothing runs until it is approved.

use apitorrent_plugin_api::PluginBundle;

include!(concat!(env!("OUT_DIR"), "/bundled.rs"));

/// The embedded bundles that parse. One that doesn't is a broken build; it is logged and
/// left out.
pub fn bundles() -> Vec<PluginBundle> {
    BUNDLES
        .iter()
        .filter_map(|text| match serde_json::from_str::<PluginBundle>(text) {
            Ok(b) => Some(b),
            Err(e) => {
                tracing::error!(error = %e, "a first-party plugin bundle in this build doesn't parse");
                None
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    #[test]
    fn every_embedded_bundle_parses_and_validates() {
        if cfg!(feature = "first-party-plugins") {
            assert!(
                !super::BUNDLES.is_empty(),
                "a build with the feature ships bundles"
            );
        }
        assert_eq!(super::bundles().len(), super::BUNDLES.len());
        for b in super::bundles() {
            apitorrent_plugin_api::validate::bundle(&b).unwrap();
        }
    }
}
