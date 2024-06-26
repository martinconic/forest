// Copyright 2019-2024 ChainSafe Systems
// SPDX-License-Identifier: Apache-2.0, MIT

mod partition;
mod state;

use cid::Cid;
use fil_actor_interface::miner::State;
use fil_actors_shared::fvm_ipld_bitfield::BitField;
use fvm_ipld_blockstore::Blockstore;

use crate::rpc::types::{SectorOnChainInfo, SectorPreCommitOnChainInfo};
use crate::utils::db::CborStoreExt as _;

pub trait MinerStateExt {
    /// Loads sectors corresponding to the bitfield. If no bitfield is passed
    /// in, return all.
    fn load_sectors_ext<BS: Blockstore>(
        &self,
        store: &BS,
        sectors: Option<&BitField>,
    ) -> anyhow::Result<Vec<SectorOnChainInfo>>;

    /// Loads the allocated sector numbers
    fn load_allocated_sector_numbers<BS: Blockstore>(&self, store: &BS)
        -> anyhow::Result<BitField>;

    /// Loads the precommit-on-chain info
    fn load_precommit_on_chain_info<BS: Blockstore>(
        &self,
        store: &BS,
        sector_number: u64,
    ) -> anyhow::Result<Option<SectorPreCommitOnChainInfo>>;
}

pub trait PartitionExt {
    /// Terminated sectors
    fn terminated(&self) -> &BitField;

    // Maps epochs sectors that expire in or before that epoch.
    // An expiration may be an "on-time" scheduled expiration, or early "faulty" expiration.
    // Keys are quantized to last-in-deadline epochs.
    fn expirations_epochs(&self) -> Cid;
}
