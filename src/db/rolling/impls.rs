// Copyright 2019-2023 ChainSafe Systems
// SPDX-License-Identifier: Apache-2.0, MIT

use crate::libp2p_bitswap::{BitswapStoreRead, BitswapStoreReadWrite};
use crate::utils::db::file_backed_obj::FileBackedObject;
use ahash::HashSet;
use cid::Cid;
use fvm_ipld_blockstore::Blockstore;
use human_repr::HumanCount;
use itertools::Itertools;
use parking_lot::RwLock;
use uuid::Uuid;

use super::*;
use crate::db::*;

impl Blockstore for RollingDB {
    fn has(&self, k: &Cid) -> anyhow::Result<bool> {
        for db in self.db_queue() {
            if Blockstore::has(&db, k)? {
                return Ok(true);
            }
        }

        Ok(false)
    }

    fn get(&self, k: &Cid) -> anyhow::Result<Option<Vec<u8>>> {
        for db in self.db_queue() {
            if let Some(v) = Blockstore::get(&db, k)? {
                return Ok(Some(v));
            }
        }

        Ok(None)
    }

    fn put<D>(
        &self,
        mh_code: cid::multihash::Code,
        block: &fvm_ipld_blockstore::Block<D>,
    ) -> anyhow::Result<Cid>
    where
        Self: Sized,
        D: AsRef<[u8]>,
    {
        Blockstore::put(&self.current(), mh_code, block)
    }

    fn put_many<D, I>(&self, blocks: I) -> anyhow::Result<()>
    where
        Self: Sized,
        D: AsRef<[u8]>,
        I: IntoIterator<Item = (cid::multihash::Code, fvm_ipld_blockstore::Block<D>)>,
    {
        Blockstore::put_many(&self.current(), blocks)
    }

    fn put_many_keyed<D, I>(&self, blocks: I) -> anyhow::Result<()>
    where
        Self: Sized,
        D: AsRef<[u8]>,
        I: IntoIterator<Item = (Cid, D)>,
    {
        Blockstore::put_many_keyed(&self.current(), blocks)
    }

    fn put_keyed(&self, k: &Cid, block: &[u8]) -> anyhow::Result<()> {
        Blockstore::put_keyed(&self.current(), k, block)
    }
}

impl SettingsStore for RollingDB {
    fn read_bin(&self, key: &str) -> anyhow::Result<Option<Vec<u8>>> {
        for db in self.db_queue() {
            if let Some(v) = SettingsStore::read_bin(db.as_ref(), key)? {
                return Ok(Some(v));
            }
        }

        Ok(None)
    }

    fn write_bin(&self, key: &str, value: &[u8]) -> anyhow::Result<()> {
        SettingsStore::write_bin(self.current.read().as_ref(), key, value)
    }

    fn exists(&self, key: &str) -> anyhow::Result<bool> {
        for db in self.db_queue() {
            if SettingsStore::exists(db.as_ref(), key)? {
                return Ok(true);
            }
        }

        Ok(false)
    }

    fn setting_keys(&self) -> anyhow::Result<Vec<String>> {
        let mut set = HashSet::default();
        for db in self.db_queue() {
            set.extend(SettingsStore::setting_keys(db.as_ref())?);
        }
        Ok(set.into_iter().collect_vec())
    }
}

impl BitswapStoreRead for RollingDB {
    fn contains(&self, cid: &Cid) -> anyhow::Result<bool> {
        for db in self.db_queue() {
            if BitswapStoreRead::contains(&db, cid)? {
                return Ok(true);
            }
        }

        Ok(false)
    }

    fn get(&self, cid: &Cid) -> anyhow::Result<Option<Vec<u8>>> {
        for db in self.db_queue() {
            if let Some(v) = BitswapStoreRead::get(&db, cid)? {
                return Ok(Some(v));
            }
        }

        Ok(None)
    }
}

impl BitswapStoreReadWrite for RollingDB {
    type Params = <Db as BitswapStoreReadWrite>::Params;

    fn insert(&self, block: &libipld::Block<Self::Params>) -> anyhow::Result<()> {
        BitswapStoreReadWrite::insert(self.current().as_ref(), block)
    }
}

impl DBStatistics for RollingDB {
    fn get_statistics(&self) -> Option<String> {
        DBStatistics::get_statistics(self.current.read().as_ref())
    }
}

impl FileBackedObject for DbIndex {
    fn serialize(&self) -> anyhow::Result<Vec<u8>> {
        Ok(serde_yaml::to_string(self)?.as_bytes().to_vec())
    }

    fn deserialize(bytes: &[u8]) -> anyhow::Result<Self> {
        Ok(serde_yaml::from_slice(bytes)?)
    }
}

impl RollingDB {
    pub fn load_or_create(db_root: PathBuf, db_config: DbConfig) -> anyhow::Result<Self> {
        if !db_root.exists() {
            std::fs::create_dir_all(db_root.as_path())?;
        }
        let (db_index, current, old) = load_dbs(&db_root, &db_config)?;

        Ok(Self {
            db_root,
            db_config,
            db_index: RwLock::new(db_index),
            current: RwLock::new(current.into()),
            old: RwLock::new(old.into()),
        })
    }

    /// Sets `current` as `old`, and sets a new DB as `current`, finally delete
    /// the dangling `old` DB.
    pub(super) fn next_current(&self, current_epoch: i64) -> anyhow::Result<()> {
        let new_db_name = Uuid::new_v4().simple().to_string();
        info!("Setting {new_db_name} as current db");
        let db = open_db(&self.db_root.join(&new_db_name), &self.db_config)?;
        *self.old.write() = std::mem::replace(&mut self.current.write(), db.into());

        let mut db_index = self.db_index.write();
        let db_index_inner_mut = db_index.inner_mut();
        let old_db_path = self.db_root.join(&db_index_inner_mut.old);
        db_index_inner_mut.old = db_index_inner_mut.current.clone();
        db_index_inner_mut.current = new_db_name;
        db_index_inner_mut.current_creation_epoch = current_epoch;
        db_index.sync()?;

        delete_db(&old_db_path);

        self.transfer_settings()?;

        Ok(())
    }

    pub(super) fn current_creation_epoch(&self) -> i64 {
        self.db_index.read().inner().current_creation_epoch
    }

    pub fn total_size_in_bytes(&self) -> anyhow::Result<u64> {
        // Sum old and current in case forest CAR files are stored under DB root
        Ok(self.current_size_in_bytes()? + self.old_size_in_bytes()?)
    }

    pub fn old_size_in_bytes(&self) -> anyhow::Result<u64> {
        Ok(fs_extra::dir::get_size(
            self.db_root
                .as_path()
                .join(self.db_index.read().inner().old.as_str()),
        )?)
    }

    pub fn current_size_in_bytes(&self) -> anyhow::Result<u64> {
        Ok(fs_extra::dir::get_size(
            self.db_root
                .as_path()
                .join(self.db_index.read().inner().current.as_str()),
        )?)
    }

    pub fn current(&self) -> Arc<Db> {
        self.current.read().clone()
    }

    fn db_queue(&self) -> [Arc<Db>; 2] {
        [self.current.read().clone(), self.old.read().clone()]
    }

    fn transfer_settings(&self) -> anyhow::Result<()> {
        let current = self.current.read();
        for key in self.setting_keys()? {
            if !current.exists(&key)? {
                if let Some(v) = self.read_bin(&key)? {
                    current.write_bin(&key, &v)?;
                }
            }
        }

        Ok(())
    }
}

fn load_dbs(db_root: &Path, db_config: &DbConfig) -> anyhow::Result<(FileBacked<DbIndex>, Db, Db)> {
    let mut db_index =
        FileBacked::load_from_file_or_create(db_root.join("db_index.yaml"), Default::default)?;
    let db_index_mut: &mut DbIndex = db_index.inner_mut();
    if db_index_mut.current.is_empty() {
        db_index_mut.current = Uuid::new_v4().simple().to_string();
    }
    if db_index_mut.old.is_empty() {
        db_index_mut.old = Uuid::new_v4().simple().to_string();
    }
    let current = open_db(&db_root.join(&db_index_mut.current), db_config)?;
    let old = open_db(&db_root.join(&db_index_mut.old), db_config)?;
    db_index.sync()?;
    Ok((db_index, current, old))
}

fn delete_db(db_path: &Path) {
    let size = fs_extra::dir::get_size(db_path).unwrap_or_default();
    if let Err(err) = std::fs::remove_dir_all(db_path) {
        warn!(
            "Error deleting database under {}, size: {}. {err}",
            db_path.display(),
            size.human_count_bytes()
        );
    } else {
        info!(
            "Deleted database under {}, size: {}",
            db_path.display(),
            size.human_count_bytes()
        );
    }
}

#[cfg(test)]
mod tests {
    use std::{thread::sleep, time::Duration};

    use cid::{multihash::MultihashDigest, Cid};
    use fvm_ipld_blockstore::Blockstore;
    use pretty_assertions::assert_eq;
    use rand::Rng;
    use tempfile::TempDir;

    use super::*;
    use crate::libp2p_bitswap::BitswapStoreRead;

    #[test]
    fn rolling_db_behaviour_tests() {
        let db_root = TempDir::new().unwrap();
        let rolling_db =
            RollingDB::load_or_create(db_root.path().into(), Default::default()).unwrap();
        println!("Generating random blocks");
        let pairs: Vec<_> = (0..1000)
            .map(|_| {
                let mut bytes = [0; 1024];
                rand::rngs::OsRng.fill(&mut bytes);
                let cid =
                    Cid::new_v0(cid::multihash::Code::Sha2_256.digest(bytes.as_slice())).unwrap();
                (cid, bytes.to_vec())
            })
            .collect();

        let split_index = 500;

        for (i, (k, block)) in pairs.iter().enumerate() {
            if i == split_index {
                sleep(Duration::from_millis(1));
                println!("Creating a new current db");
                rolling_db.next_current(0).unwrap();
                println!("Created a new current db");
            }
            rolling_db.put_keyed(k, block).unwrap();
        }

        for (i, (k, block)) in pairs.iter().enumerate() {
            assert!(rolling_db.contains(k).unwrap(), "{i}");
            assert_eq!(
                Blockstore::get(&rolling_db, k).unwrap().unwrap().as_slice(),
                block,
                "{i}"
            );
        }

        rolling_db.next_current(0).unwrap();

        for (i, (k, _)) in pairs.iter().enumerate() {
            if i < split_index {
                assert!(!rolling_db.contains(k).unwrap(), "{i}");
            } else {
                assert!(rolling_db.contains(k).unwrap(), "{i}");
            }
        }

        drop(rolling_db);

        let rolling_db =
            RollingDB::load_or_create(db_root.path().into(), Default::default()).unwrap();
        for (i, (k, _)) in pairs.iter().enumerate() {
            if i < split_index {
                assert!(!rolling_db.contains(k).unwrap());
            } else {
                assert!(rolling_db.contains(k).unwrap());
            }
        }
    }
}
