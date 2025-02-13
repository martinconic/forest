// Copyright 2019-2025 ChainSafe Systems
// SPDX-License-Identifier: Apache-2.0, MIT

use crate::db::PersistentStore;
use crate::utils::net::{download_file_with_cache, DownloadFileOption};
use crate::{
    networks::{ActorBundleInfo, NetworkChain, ACTOR_BUNDLES},
    utils::db::car_stream::{CarBlock, CarStream},
};
use ahash::HashSet;
use cid::Cid;
use directories::ProjectDirs;
use futures::{stream::FuturesUnordered, TryStreamExt};
use once_cell::sync::Lazy;
use std::mem::discriminant;
use std::path::PathBuf;
use std::sync::Arc;
use std::{io::Cursor, path::Path};
use tokio::sync::Semaphore;
use tracing::{info, warn};

/// Tries to load the missing actor bundles to the blockstore. If the bundle is
/// not present, it will be downloaded.
pub async fn load_actor_bundles(
    db: &impl PersistentStore,
    network: &NetworkChain,
) -> anyhow::Result<()> {
    if let Some(bundle_path) = match std::env::var("FOREST_ACTOR_BUNDLE_PATH") {
        Ok(path) if !path.is_empty() => Some(path),
        _ => None,
    } {
        info!("Loading actor bundle from {bundle_path} set by FOREST_ACTOR_BUNDLE_PATH environment variable");
        load_actor_bundles_from_path(db, network, &bundle_path).await?;
    } else {
        load_actor_bundles_from_server(db, network, &ACTOR_BUNDLES).await?;
    }

    Ok(())
}

pub async fn load_actor_bundles_from_path(
    db: &impl PersistentStore,
    network: &NetworkChain,
    bundle_path: impl AsRef<Path>,
) -> anyhow::Result<()> {
    anyhow::ensure!(
        bundle_path.as_ref().is_file(),
        "Bundle file not found at {}",
        bundle_path.as_ref().display()
    );
    let mut car_stream = CarStream::new(tokio::io::BufReader::new(
        tokio::fs::File::open(bundle_path.as_ref()).await?,
    ))
    .await?;

    // Validate the bundle
    let roots = HashSet::from_iter(car_stream.header_v1.roots.iter());
    for ActorBundleInfo {
        manifest, network, ..
    } in ACTOR_BUNDLES.iter().filter(|bundle| {
        // Comparing only the discriminant is enough. All devnets share the same
        // actor bundle.
        discriminant(network) == discriminant(&bundle.network)
    }) {
        anyhow::ensure!(
                roots.contains(manifest),
                "actor {manifest} for {network} is missing from {}, try regenerating the bundle with `forest-tool state-migration actor-bundle`", bundle_path.as_ref().display());
    }

    // Load into DB
    while let Some(CarBlock { cid, data }) = car_stream.try_next().await? {
        db.put_keyed_persistent(&cid, &data)?;
    }

    Ok(())
}

pub static ACTOR_BUNDLE_CACHE_DIR: Lazy<PathBuf> = Lazy::new(|| {
    let project_dir = ProjectDirs::from("com", "ChainSafe", "Forest");
    project_dir
        .map(|d| d.cache_dir().to_path_buf())
        .unwrap_or_else(std::env::temp_dir)
        .join("actor-bundles")
});

/// Loads the missing actor bundle, returns the CIDs of the loaded bundles.
pub async fn load_actor_bundles_from_server(
    db: &impl PersistentStore,
    network: &NetworkChain,
    bundles: &[ActorBundleInfo],
) -> anyhow::Result<Vec<Cid>> {
    let semaphore = Arc::new(Semaphore::new(4));
    FuturesUnordered::from_iter(
        bundles
            .iter()
            .filter(|bundle| {
                !db.has(&bundle.manifest).unwrap_or(false) &&
                // Comparing only the discriminant is enough. All devnets share the same
                // actor bundle.
                discriminant(network) == discriminant(&bundle.network)
            })
            .map(
                |ActorBundleInfo {
                     manifest: root,
                     url,
                     alt_url,
                     network,
                     version,
                 }| {
                    let semaphore = semaphore.clone();
                    async move {
                        let _permit = semaphore.acquire().await?;
                        let result = if let Ok(response) =
                            download_file_with_cache(url, &ACTOR_BUNDLE_CACHE_DIR, DownloadFileOption::NonResumable).await
                        {
                            response
                        } else {
                            warn!("failed to download bundle {network}-{version} from primary URL, trying alternative URL");
                            download_file_with_cache(alt_url, &ACTOR_BUNDLE_CACHE_DIR, DownloadFileOption::NonResumable).await?
                        };

                        let bytes = std::fs::read(&result.path)?;
                        let mut stream = CarStream::new(Cursor::new(bytes)).await?;
                        while let Some(block) = stream.try_next().await? {
                            db.put_keyed_persistent(&block.cid, &block.data)?;
                        }
                        let header = stream.header_v1;
                        anyhow::ensure!(header.roots.len() == 1);
                        anyhow::ensure!(header.roots.first() == root);
                        Ok(*root)
                    }
                }
            ),
    )
    .try_collect::<Vec<_>>()
    .await
}
