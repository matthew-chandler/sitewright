//! Dashboard panels over a site's own data: the bridge between `sw-metrics`, which
//! defines dashboards, and `sw-backend`, which owns the site databases.

use futures_util::future::BoxFuture;
use serde_json::Value;
use sw_backend::SiteBackends;
use sw_core::SiteName;
use sw_metrics::QueryResult;
use sw_metrics::dashboard::{SiteData, prepare_site_sql};
use sw_metrics::sandbox::MAX_ROWS;

/// Read-only queries on one site's backend database, for its dashboard.
///
/// The site is fixed when this is made, and the queries go to that site's own database
/// file under the site database's authorizer: no writes, no `ATTACH` or `PRAGMA`, no
/// sessions or password hashes. Neither Sitewright's main database nor another site's
/// can be reached from here.
#[derive(Debug, Clone)]
pub struct SiteQueries {
    backends: SiteBackends,
    site: SiteName,
}

impl SiteQueries {
    pub fn new(backends: SiteBackends, site: SiteName) -> Self {
        Self { backends, site }
    }
}

impl SiteData for SiteQueries {
    fn query<'a>(
        &'a self,
        sql: &'a str,
        from: &'a str,
        to: &'a str,
    ) -> BoxFuture<'a, Result<QueryResult, String>> {
        Box::pin(async move {
            let (sql, params) = prepare_site_sql(sql, from, to)?;
            let params: Vec<Value> = params.into_iter().map(Value::from).collect();
            let read = self
                .backends
                .query_read_only(&self.site, &sql, &params, MAX_ROWS)
                .await?;
            Ok(QueryResult {
                columns: read.columns,
                rows: read.rows,
                truncated: read.truncated,
            })
        })
    }
}
