// Copyright © Aptos Foundation
// SPDX-License-Identifier: Apache-2.0

use crate::metrics::OTHER_TIMERS;
use anyhow::{ensure, Result};
use aptos_crypto::{hash::CryptoHash, HashValue};
use aptos_executor_types::{
    execution_output::ExecutionOutput, state_checkpoint_output::StateCheckpointOutput,
    transactions_with_output::TransactionsWithOutput, ProofReader,
};
use aptos_logger::info;
use aptos_metrics_core::TimerHelper;
use aptos_scratchpad::FrozenSparseMerkleTree;
use aptos_storage_interface::{
    cached_state_view::{ShardedStateCache, StateCache},
    state_delta::StateDelta,
};
use aptos_types::{
    state_store::{
        create_empty_sharded_state_updates, state_key::StateKey,
        state_storage_usage::StateStorageUsage, state_value::StateValue, ShardedStateUpdates,
    },
    transaction::Version,
    write_set::{TransactionWrite, WriteSet},
};
use arr_macro::arr;
use dashmap::DashMap;
use itertools::zip_eq;
use rayon::prelude::*;
use std::{collections::HashMap, ops::Deref, sync::Arc};

/// Helper class for calculating state changes after a block of transactions are executed.
pub struct InMemoryStateCalculatorV2 {}

impl InMemoryStateCalculatorV2 {
    pub fn calculate_for_transactions(
        execution_output: &ExecutionOutput,
        parent_state: &Arc<StateDelta>,
        known_state_checkpoints: Option<impl IntoIterator<Item = Option<HashValue>>>,
    ) -> Result<StateCheckpointOutput> {
        if execution_output.is_block {
            Self::validate_input_for_block(parent_state, &execution_output.to_commit)?;
        }

        let state_updates_vec = Self::get_sharded_state_updates(
            execution_output.to_commit.transaction_outputs(),
            |txn_output| txn_output.write_set(),
        );

        // If there are multiple checkpoints in the chunk, we only calculate the SMT (and its root
        // hash) for the last one.
        let last_checkpoint_index = execution_output.to_commit.get_last_checkpoint_index();

        Self::calculate_impl(
            parent_state,
            &execution_output.state_cache,
            state_updates_vec,
            last_checkpoint_index,
            execution_output.is_block,
            known_state_checkpoints,
        )
    }

    pub fn calculate_for_write_sets_after_snapshot(
        parent_state: &Arc<StateDelta>,
        state_cache: &StateCache,
        last_checkpoint_index: Option<usize>,
        write_sets: &[WriteSet],
    ) -> Result<StateCheckpointOutput> {
        let state_updates_vec = Self::get_sharded_state_updates(write_sets, |write_set| write_set);

        Self::calculate_impl(
            parent_state,
            state_cache,
            state_updates_vec,
            last_checkpoint_index,
            false,
            Option::<Vec<_>>::None,
        )
    }

    fn calculate_impl(
        parent_state: &Arc<StateDelta>,
        state_cache: &StateCache,
        state_updates_vec: Vec<ShardedStateUpdates>,
        last_checkpoint_index: Option<usize>,
        is_block: bool,
        known_state_checkpoints: Option<impl IntoIterator<Item = Option<HashValue>>>,
    ) -> Result<StateCheckpointOutput> {
        let StateCache {
            // This makes sure all in-mem nodes seen while proofs were fetched stays in mem during the
            // calculation
            frozen_base,
            sharded_state_cache,
            proofs,
        } = state_cache;
        assert!(frozen_base.smt.is_the_same(&parent_state.current));

        let (updates_before_last_checkpoint, updates_after_last_checkpoint) =
            if let Some(index) = last_checkpoint_index {
                (
                    Self::calculate_updates(&state_updates_vec[..=index]),
                    Self::calculate_updates(&state_updates_vec[index + 1..]),
                )
            } else {
                (
                    create_empty_sharded_state_updates(),
                    Self::calculate_updates(&state_updates_vec),
                )
            };

        let num_txns = state_updates_vec.len();

        let usage = Self::calculate_usage(parent_state.current.usage(), sharded_state_cache, &[
            &updates_before_last_checkpoint,
            &updates_after_last_checkpoint,
        ]);

        let first_version = parent_state.current_version.map_or(0, |v| v + 1);
        let proof_reader = ProofReader::new(proofs);
        let latest_checkpoint = if let Some(index) = last_checkpoint_index {
            Self::make_checkpoint(
                parent_state.current.freeze(&frozen_base.base_smt),
                &updates_before_last_checkpoint,
                if index == num_txns - 1 {
                    usage
                } else {
                    StateStorageUsage::new_untracked()
                },
                &proof_reader,
            )?
        } else {
            // If there is no checkpoint in this chunk, the latest checkpoint will be the existing
            // one.
            parent_state.base.freeze(&frozen_base.base_smt)
        };

        let mut latest_checkpoint_version = parent_state.base_version;
        let mut state_checkpoint_hashes = known_state_checkpoints
            .map_or_else(|| vec![None; num_txns], |v| v.into_iter().collect());
        ensure!(
            state_checkpoint_hashes.len() == num_txns,
            "Bad number of known hashes."
        );
        if let Some(index) = last_checkpoint_index {
            if let Some(h) = state_checkpoint_hashes[index] {
                ensure!(
                    h == latest_checkpoint.root_hash(),
                    "Last checkpoint not expected."
                );
            } else {
                state_checkpoint_hashes[index] = Some(latest_checkpoint.root_hash());
            }
            latest_checkpoint_version = Some(first_version + index as u64);
        }

        let current_version = first_version + num_txns as u64 - 1;
        // We need to calculate the SMT at the end of the chunk, if it is not already calculated.
        let current_tree = if last_checkpoint_index == Some(num_txns - 1) {
            latest_checkpoint.smt.clone()
        } else {
            ensure!(!is_block, "Block must have the checkpoint at the end.");
            // The latest tree is either the last checkpoint in current chunk, or the tree at the
            // end of previous chunk if there is no checkpoint in the current chunk.
            let latest_tree = if last_checkpoint_index.is_some() {
                latest_checkpoint.clone()
            } else {
                parent_state.current.freeze(&frozen_base.base_smt)
            };
            Self::make_checkpoint(
                latest_tree,
                &updates_after_last_checkpoint,
                usage,
                &proof_reader,
            )?
            .smt
        };

        let updates_since_latest_checkpoint = if last_checkpoint_index.is_some() {
            updates_after_last_checkpoint
        } else {
            let mut updates_since_latest_checkpoint =
                parent_state.updates_since_base.deref().deref().clone();
            zip_eq(
                updates_since_latest_checkpoint.iter_mut(),
                updates_after_last_checkpoint,
            )
            .for_each(|(base, delta)| base.extend(delta));
            updates_since_latest_checkpoint
        };

        info!(
            "last_checkpoint_index {last_checkpoint_index:?}, result_state: {latest_checkpoint_version:?} {:?} {:?} {current_version} {:?} {:?}",
            latest_checkpoint.root_hash(),
            latest_checkpoint.usage(),
            current_tree.root_hash(),
            current_tree.usage(),
        );

        let result_state = StateDelta::new(
            latest_checkpoint.smt,
            latest_checkpoint_version,
            current_tree,
            Some(current_version),
            updates_since_latest_checkpoint,
        );

        Ok(StateCheckpointOutput::new(
            parent_state.clone(),
            Arc::new(result_state),
            last_checkpoint_index.map(|_| updates_before_last_checkpoint),
            state_updates_vec,
            state_checkpoint_hashes,
        ))
    }

    fn get_sharded_state_updates<'a, T, F>(
        outputs: &'a [T],
        write_set_fn: F,
    ) -> Vec<ShardedStateUpdates>
    where
        T: Sync + 'a,
        F: Fn(&'a T) -> &'a WriteSet + Sync,
    {
        let _timer = OTHER_TIMERS.timer_with(&["get_sharded_state_updates"]);

        outputs
            .par_iter()
            .map(|output| {
                let mut updates = arr![HashMap::new(); 16];
                write_set_fn(output)
                    .iter()
                    .for_each(|(state_key, write_op)| {
                        updates[state_key.get_shard_id() as usize]
                            .insert(state_key.clone(), write_op.as_state_value());
                    });
                updates
            })
            .collect()
    }

    fn calculate_updates(state_updates_vec: &[ShardedStateUpdates]) -> ShardedStateUpdates {
        let _timer = OTHER_TIMERS.timer_with(&["calculate_updates"]);
        let mut updates: ShardedStateUpdates = create_empty_sharded_state_updates();
        updates
            .par_iter_mut()
            .enumerate()
            .for_each(|(i, per_shard_update)| {
                per_shard_update.extend(
                    state_updates_vec
                        .iter()
                        .flat_map(|hms| &hms[i])
                        .map(|(k, v)| (k.clone(), v.clone()))
                        .collect::<Vec<_>>(),
                )
            });
        updates
    }

    fn add_to_delta(
        k: &StateKey,
        v: &Option<StateValue>,
        state_cache: &DashMap<StateKey, (Option<Version>, Option<StateValue>)>,
        items_delta: &mut i64,
        bytes_delta: &mut i64,
    ) {
        let key_size = k.size();
        if let Some(ref value) = v {
            *items_delta += 1;
            *bytes_delta += (key_size + value.size()) as i64;
        }

        // n.b. all updated state items must be read and recorded in the state cache,
        // otherwise we can't calculate the correct usage.
        let old_entry = state_cache.get(k).expect("Must cache read");
        if let (_, Some(old_v)) = old_entry.value() {
            *items_delta -= 1;
            *bytes_delta -= (key_size + old_v.size()) as i64;
        }
    }

    fn calculate_usage(
        old_usage: StateStorageUsage,
        sharded_state_cache: &ShardedStateCache,
        updates: &[&ShardedStateUpdates; 2],
    ) -> StateStorageUsage {
        let _timer = OTHER_TIMERS
            .with_label_values(&["calculate_usage"])
            .start_timer();
        if old_usage.is_untracked() {
            return StateStorageUsage::new_untracked();
        }
        let (items_delta, bytes_delta) = updates[0]
            .par_iter()
            .zip_eq(updates[1].par_iter())
            .enumerate()
            .map(
                |(i, (shard_updates_before_checkpoint, shard_updates_after_checkpoint))| {
                    let mut items_delta = 0i64;
                    let mut bytes_delta = 0i64;
                    let num_updates_before_checkpoint = shard_updates_before_checkpoint.len();
                    for (index, (k, v)) in shard_updates_before_checkpoint
                        .iter()
                        .chain(shard_updates_after_checkpoint.iter())
                        .enumerate()
                    {
                        // Ignore updates before the checkpoint if there is an update for the same
                        // key after the checkpoint.
                        if index < num_updates_before_checkpoint
                            && shard_updates_after_checkpoint.contains_key(k)
                        {
                            continue;
                        }
                        Self::add_to_delta(
                            k,
                            v,
                            sharded_state_cache.shard(i as u8),
                            &mut items_delta,
                            &mut bytes_delta,
                        );
                    }
                    (items_delta, bytes_delta)
                },
            )
            .reduce(
                || (0i64, 0i64),
                |(items_now, bytes_now), (items_delta, bytes_delta)| {
                    (items_now + items_delta, bytes_now + bytes_delta)
                },
            );
        StateStorageUsage::new(
            (old_usage.items() as i64 + items_delta) as usize,
            (old_usage.bytes() as i64 + bytes_delta) as usize,
        )
    }

    fn make_checkpoint(
        latest_checkpoint: FrozenSparseMerkleTree<StateValue>,
        updates: &ShardedStateUpdates,
        usage: StateStorageUsage,
        proof_reader: &ProofReader,
    ) -> Result<FrozenSparseMerkleTree<StateValue>> {
        let _timer = OTHER_TIMERS.timer_with(&["make_checkpoint"]);

        // Update SMT.
        //
        // TODO(grao): Consider use the sharded updates directly instead of flatten.
        let smt_updates: Vec<_> = updates
            .iter()
            .flatten()
            .map(|(key, value)| (key.hash(), value.as_ref()))
            .collect();
        let new_checkpoint = latest_checkpoint.batch_update(smt_updates, usage, proof_reader)?;
        Ok(new_checkpoint)
    }

    fn validate_input_for_block(
        base: &StateDelta,
        to_commit: &TransactionsWithOutput,
    ) -> Result<()> {
        let num_txns = to_commit.len();
        ensure!(num_txns != 0, "Empty block is not allowed.");
        ensure!(
            base.base_version == base.current_version,
            "Block base state is not a checkpoint. base_version {:?}, current_version {:?}",
            base.base_version,
            base.current_version,
        );
        ensure!(
            base.updates_since_base.iter().all(|shard| shard.is_empty()),
            "Base state is corrupted, updates_since_base is not empty at a checkpoint."
        );

        for (i, (txn, _txn_out, is_reconfig)) in to_commit.iter().enumerate() {
            ensure!(
                TransactionsWithOutput::need_checkpoint(txn, is_reconfig) ^ (i != num_txns - 1),
                "Checkpoint is allowed iff it's the last txn in the block. index: {i}, num_txns: {num_txns}, is_last: {}, txn: {txn:?}, is_reconfig: {}",
                i == num_txns - 1,
                is_reconfig,
            );
        }
        Ok(())
    }
}
