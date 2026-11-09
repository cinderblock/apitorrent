//! A `dns-provider` plugin as ACME's DNS-01 provider.
//!
//! The ACME manager keeps the state machine, the account and the keys; the plugin only
//! publishes and removes TXT records, through the methods it provided:
//!
//! ```ts
//! api.roles.provide("dns-provider", {
//!   async setTxt({ domain, name, value }) { … },
//!   async removeTxt({ domain, name, value }) { … },
//! });
//! ```
//!
//! The provider is made before the plugin host starts (TLS is set up first) and attached
//! to the host afterwards. Until the plugin runs, calls wait a while for it, so a daemon
//! that starts ACME right after the host doesn't fail its first order.

use std::sync::{Arc, OnceLock};
use std::time::Duration;

use apitorrent_plugin_api::{PluginError, PluginRole};
use apitorrent_tls::acme::{BoxFuture, DnsError, DnsProvider, TxtRecord};

use crate::PluginHost;

/// How long a call waits for the plugin to be running.
const WAIT_FOR_PLUGIN: Duration = Duration::from_secs(60);

/// ACME DNS-01 through one `dns-provider` plugin.
pub struct PluginDnsProvider {
    plugin: String,
    host: OnceLock<PluginHost>,
}

impl std::fmt::Debug for PluginDnsProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PluginDnsProvider")
            .field("plugin", &self.plugin)
            .field("attached", &self.host.get().is_some())
            .finish()
    }
}

impl PluginDnsProvider {
    /// A provider for the plugin with this id, not yet attached to a host.
    pub fn new(plugin: impl Into<String>) -> Arc<Self> {
        Arc::new(Self {
            plugin: plugin.into(),
            host: OnceLock::new(),
        })
    }

    pub fn plugin(&self) -> &str {
        &self.plugin
    }

    /// Connect the provider to the running host. Later calls are ignored.
    pub fn attach(&self, host: PluginHost) {
        let _ = self.host.set(host);
    }

    async fn call(&self, method: &str, record: &TxtRecord) -> Result<(), DnsError> {
        let host = self.host.get().ok_or_else(|| {
            DnsError(format!(
                "the DNS provider plugin `{}` can't run: plugins are turned off",
                self.plugin
            ))
        })?;
        host.wait_running(&self.plugin, WAIT_FOR_PLUGIN)
            .await
            .map_err(|e| self.error(&e))?;
        if !host
            .providers(PluginRole::DnsProvider)
            .iter()
            .any(|(id, _)| *id == self.plugin)
        {
            return Err(DnsError(format!(
                "plugin `{}` doesn't provide the `dns-provider` role (it must declare it and \
                 call api.roles.provide(\"dns-provider\", …))",
                self.plugin
            )));
        }
        let arg = serde_json::json!([{
            "domain": record.domain,
            "name": record.name,
            "value": record.value,
        }]);
        host.call_role(&self.plugin, PluginRole::DnsProvider, method, arg)
            .await
            .map(|_| ())
            .map_err(|e| self.error(&e))
    }

    fn error(&self, e: &PluginError) -> DnsError {
        DnsError(format!("DNS provider plugin `{}`: {e}", self.plugin))
    }
}

impl DnsProvider for PluginDnsProvider {
    fn publish_txt<'a>(&'a self, record: &'a TxtRecord) -> BoxFuture<'a, Result<(), DnsError>> {
        Box::pin(self.call("setTxt", record))
    }

    fn remove_txt<'a>(&'a self, record: &'a TxtRecord) -> BoxFuture<'a, Result<(), DnsError>> {
        Box::pin(self.call("removeTxt", record))
    }
}
