//! The in-memory routing table: which hostname serves which release directory.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use sw_core::SiteName;

/// What a hostname currently serves.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Route {
    pub site_id: i64,
    pub site: SiteName,
    pub release_id: i64,
    pub sha: String,
    /// The site's primary hostname (the same for all of the site's routes).
    pub primary_hostname: String,
    /// The immutable release directory files are served from.
    pub root: PathBuf,
}

/// A snapshot of all routes. Rebuilt and swapped atomically whenever a site changes.
#[derive(Debug, Default)]
pub struct RouteTable {
    by_host: HashMap<String, Arc<Route>>,
}

impl RouteTable {
    pub(crate) fn new(by_host: HashMap<String, Arc<Route>>) -> Self {
        Self { by_host }
    }

    /// Look up a normalized hostname (lowercase, no port).
    pub fn get(&self, hostname: &str) -> Option<&Arc<Route>> {
        self.by_host.get(hostname)
    }

    pub fn len(&self) -> usize {
        self.by_host.len()
    }

    pub fn is_empty(&self) -> bool {
        self.by_host.is_empty()
    }

    /// One route per site, whichever hostname it was reached by.
    pub fn sites(&self) -> Vec<Arc<Route>> {
        let mut seen = std::collections::HashSet::new();
        let mut sites: Vec<Arc<Route>> = self
            .by_host
            .values()
            .filter(|r| seen.insert(r.site_id))
            .cloned()
            .collect();
        sites.sort_by_key(|r| r.site_id);
        sites
    }

    pub fn hostnames(&self) -> impl Iterator<Item = &str> {
        self.by_host.keys().map(String::as_str)
    }
}
