//! Manga jobs: scrape (read ComicInfo + pages from a CBZ), cache (download the
//! archive), build (assemble from loose images or SAF URIs), pack (write a CBZ
//! and hand it to the upload queue).
//!
//! Owner: T5. Stubs for now.

use async_trait::async_trait;

use wdmm_queue::model::TaskKind;

use crate::{Job, JobCtx, JobOutcome};

macro_rules! stub_job {
    ($name:ident, $kind:expr, $label:expr) => {
        pub struct $name;

        #[async_trait]
        impl Job for $name {
            fn kind(&self) -> TaskKind {
                $kind
            }

            async fn run(&self, _ctx: JobCtx) -> JobOutcome {
                JobOutcome::unimplemented($label)
            }
        }
    };
}

stub_job!(MangaScrapeJob, TaskKind::MangaScrape, "manga_scrape");
stub_job!(MangaCacheJob, TaskKind::MangaCache, "manga_cache");
stub_job!(MangaBuildJob, TaskKind::MangaBuild, "manga_build");
stub_job!(MangaPackJob, TaskKind::MangaPack, "manga_pack");