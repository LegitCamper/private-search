pub mod duckduckgo;
mod google;

use crate::WebSiteResult;

pub trait Engine: Sized {
    type Error;

    async fn search(
        query: &str,
        start: usize,
        count: usize,
    ) -> Result<Vec<WebSiteResult>, Self::Error>;
}
