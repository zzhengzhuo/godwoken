use anyhow::Result;
use gw_common::state::State;
use gw_store::{
    state_db::{CheckPoint, StateDBMode, StateDBTransaction, SubState},
    transaction::StoreTransaction,
};
use gw_types::prelude::Unpack;
use std::collections::{BinaryHeap, HashMap};

/// Max queue size
const MAX_QUEUE_SIZE: usize = 10000;
/// Drop size when queue is full
const DROP_SIZE: usize = 100;

use super::types::FeeEntry;

/// Txs & withdrawals queue sorted by fee rate
pub struct FeeQueue {
    // priority queue to store tx and withdrawal
    queue: BinaryHeap<FeeEntry>,
}

impl FeeQueue {
    pub fn new() -> Self {
        Self {
            queue: BinaryHeap::with_capacity(MAX_QUEUE_SIZE + DROP_SIZE),
        }
    }

    pub fn len(&self) -> usize {
        self.queue.len()
    }

    pub fn is_empty(&self) -> bool {
        self.queue.is_empty()
    }

    /// Add item to queue
    pub fn add(&mut self, entry: FeeEntry) {
        // push to queue
        self.queue.push(entry);

        // drop items if full
        if self.is_full() {
            let mut new_queue = BinaryHeap::with_capacity(MAX_QUEUE_SIZE + DROP_SIZE);
            let expected_len = self.queue.len().saturating_sub(DROP_SIZE);
            while let Some(entry) = self.queue.pop() {
                new_queue.push(entry);
                if new_queue.len() >= expected_len {
                    break;
                }
            }
            self.queue = new_queue;
            log::debug!(
                "Fee queue is full, drop {} items, new size: {}",
                DROP_SIZE,
                expected_len
            );
        }
    }

    pub fn is_full(&self) -> bool {
        self.queue.len() > MAX_QUEUE_SIZE
    }

    #[allow(clippy::needless_lifetimes)]
    fn mem_pool_state_db<'a>(&self, db: &'a StoreTransaction) -> Result<StateDBTransaction<'a>> {
        let tip = db.get_tip_block()?;
        let tip_block_number = tip.raw().number().unpack();
        let checkpoint = CheckPoint::new(tip_block_number, SubState::MemBlock);
        StateDBTransaction::from_checkpoint(db, checkpoint, StateDBMode::ReadOnly)
            .map_err(Into::into)
    }

    /// Fetch items by fee sort
    pub fn fetch(&mut self, db: &StoreTransaction, count: usize) -> Result<Vec<FeeEntry>> {
        let state_db = self.mem_pool_state_db(db)?;
        let state = state_db.state_tree()?;
        // sorted fee items
        let mut fetched_items = Vec::with_capacity(count as usize);
        let mut fetched_senders: HashMap<u32, u32> = Default::default();
        // future items, we will push back this queue
        let mut future_queue = Vec::default();

        // Fetch item from PQ
        while let Some(entry) = self.queue.pop() {
            let nonce = match fetched_senders.get(&entry.sender) {
                Some(&nonce) => nonce,
                None => state.get_nonce(entry.sender)?,
            };
            match entry.item.nonce().cmp(&nonce) {
                std::cmp::Ordering::Equal => {
                    // update nonce
                    fetched_senders.insert(entry.sender, nonce.saturating_add(1));
                    // fetch this item
                    fetched_items.push(entry);
                }
                std::cmp::Ordering::Greater => {
                    // push item back if it still has change to get fetched
                    future_queue.push(entry);
                }
                _ => {}
            }

            if fetched_items.len() >= count {
                break;
            }
        }

        // Add back future items
        for entry in future_queue {
            // Only add back if we fetched another item from the same sender
            if fetched_senders.contains_key(&entry.sender) {
                self.add(entry);
            }
        }

        Ok(fetched_items)
    }
}

impl Default for FeeQueue {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use gw_common::{h256_ext::H256Ext, state::State, H256};
    use gw_config::GenesisConfig;
    use gw_generator::{genesis::init_genesis, traits::StateExt};
    use gw_store::Store;
    use gw_types::{
        bytes::Bytes,
        packed::{L2BlockCommittedInfo, L2Transaction, RawL2Transaction, RollupConfig},
        prelude::{Builder, Entity, Pack, Unpack},
    };

    use crate::fee::{
        queue::MAX_QUEUE_SIZE,
        types::{FeeEntry, FeeItem},
    };

    use super::FeeQueue;

    #[test]
    fn test_sort_txs_by_fee() {
        let mut queue = FeeQueue::new();

        let store = Store::open_tmp().expect("open store");
        setup_genesis(&store);
        {
            let db = store.begin_transaction();
            let genesis = db.get_tip_block().expect("tip");
            assert_eq!(genesis.raw().number().unpack(), 0);
            db.set_mem_block_account_count(genesis.raw().post_account().count().unpack());
            db.set_mem_block_account_smt_root(genesis.raw().post_account().merkle_root().unpack());
            let state_db = queue.mem_pool_state_db(&db).unwrap();
            let mut state = state_db.state_tree().unwrap();

            // create accounts
            for i in 0..4 {
                state.create_account(H256::from_u32(i));
            }

            db.commit().expect("commit");
        }

        let entry1 = FeeEntry {
            item: FeeItem::Tx(Default::default()),
            fee_rate: 100,
            cycles_limit: 1000,
            sender: 2,
        };

        let entry2 = FeeEntry {
            item: FeeItem::Tx(Default::default()),
            fee_rate: 101,
            cycles_limit: 1000,
            sender: 3,
        };

        let entry3 = FeeEntry {
            item: FeeItem::Tx(Default::default()),
            fee_rate: 100,
            cycles_limit: 1001,
            sender: 4,
        };

        let entry4 = FeeEntry {
            item: FeeItem::Withdrawal(Default::default()),
            fee_rate: 101,
            cycles_limit: 1001,
            sender: 5,
        };

        queue.add(entry1);
        queue.add(entry2);
        queue.add(entry3);
        queue.add(entry4);

        let db = store.begin_transaction();

        // fetch 3
        {
            let items = queue.fetch(&db, 3).expect("fetch");
            assert_eq!(items.len(), 3);
            assert_eq!(items[0].sender, 3);
            assert_eq!(items[1].sender, 5);
            assert_eq!(items[2].sender, 2);
        }
        // fetch 3
        {
            let items = queue.fetch(&db, 3).expect("fetch");
            assert_eq!(items.len(), 1);
            assert_eq!(items[0].sender, 4);
        }
        // fetch 3
        {
            let items = queue.fetch(&db, 3).expect("fetch");
            assert_eq!(items.len(), 0);
        }
    }

    #[test]
    fn test_insert_distinct_nonce() {
        let mut queue = FeeQueue::new();

        let store = Store::open_tmp().expect("open store");
        setup_genesis(&store);
        {
            let db = store.begin_transaction();
            let genesis = db.get_tip_block().expect("tip");
            assert_eq!(genesis.raw().number().unpack(), 0);
            db.set_mem_block_account_count(genesis.raw().post_account().count().unpack());
            db.set_mem_block_account_smt_root(genesis.raw().post_account().merkle_root().unpack());
            let state_db = queue.mem_pool_state_db(&db).unwrap();
            let mut state = state_db.state_tree().unwrap();

            // create accounts
            for i in 0..4 {
                state.create_account(H256::from_u32(i));
            }

            db.commit().expect("commit");
        }

        let entry1 = FeeEntry {
            item: FeeItem::Tx(
                L2Transaction::new_builder()
                    .raw(RawL2Transaction::new_builder().nonce(1u32.pack()).build())
                    .build(),
            ),
            fee_rate: 100,
            cycles_limit: 1000,
            sender: 2,
        };

        let entry2 = FeeEntry {
            item: FeeItem::Tx(
                L2Transaction::new_builder()
                    .raw(RawL2Transaction::new_builder().nonce(0u32.pack()).build())
                    .build(),
            ),
            fee_rate: 100,
            cycles_limit: 1000,
            sender: 2,
        };

        queue.add(entry1);
        queue.add(entry2);

        let db = store.begin_transaction();

        // fetch
        {
            let items = queue.fetch(&db, 3).expect("fetch");
            assert_eq!(items.len(), 2);
            assert_eq!(items[0].item.nonce(), 0);
            assert_eq!(items[1].item.nonce(), 1);
        }
    }
    #[test]
    fn test_replace_by_fee() {
        let mut queue = FeeQueue::new();

        let store = Store::open_tmp().expect("open store");
        setup_genesis(&store);
        {
            let db = store.begin_transaction();
            let genesis = db.get_tip_block().expect("tip");
            assert_eq!(genesis.raw().number().unpack(), 0);
            db.set_mem_block_account_count(genesis.raw().post_account().count().unpack());
            db.set_mem_block_account_smt_root(genesis.raw().post_account().merkle_root().unpack());
            let state_db = queue.mem_pool_state_db(&db).unwrap();
            let mut state = state_db.state_tree().unwrap();

            // create accounts
            for i in 0..4 {
                state.create_account(H256::from_u32(i));
            }

            db.commit().expect("commit");
        }

        let entry1 = FeeEntry {
            item: FeeItem::Tx(
                L2Transaction::new_builder()
                    .raw(RawL2Transaction::new_builder().nonce(0u32.pack()).build())
                    .build(),
            ),
            fee_rate: 100,
            cycles_limit: 1000,
            sender: 2,
        };

        let entry2 = FeeEntry {
            item: FeeItem::Tx(
                L2Transaction::new_builder()
                    .raw(RawL2Transaction::new_builder().nonce(0u32.pack()).build())
                    .build(),
            ),
            fee_rate: 101,
            cycles_limit: 1000,
            sender: 2,
        };

        queue.add(entry1);
        queue.add(entry2);

        let db = store.begin_transaction();

        // fetch
        {
            let items = queue.fetch(&db, 3).expect("fetch");
            assert_eq!(items.len(), 1);
            assert_eq!(items[0].fee_rate, 101);
            // try fetch remain items
            let items = queue.fetch(&db, 1).expect("fetch");
            assert_eq!(items.len(), 0);
        }
    }

    #[test]
    fn test_drop_items() {
        let mut queue = FeeQueue::new();

        let store = Store::open_tmp().expect("open store");
        setup_genesis(&store);
        {
            let db = store.begin_transaction();
            let genesis = db.get_tip_block().expect("tip");
            assert_eq!(genesis.raw().number().unpack(), 0);
            db.set_mem_block_account_count(genesis.raw().post_account().count().unpack());
            db.set_mem_block_account_smt_root(genesis.raw().post_account().merkle_root().unpack());
            let state_db = queue.mem_pool_state_db(&db).unwrap();
            let mut state = state_db.state_tree().unwrap();

            // create accounts
            for i in 0..4 {
                state.create_account(H256::from_u32(i));
            }

            db.commit().expect("commit");
        }

        for i in 0..(MAX_QUEUE_SIZE as u32) {
            let entry1 = FeeEntry {
                item: FeeItem::Tx(
                    L2Transaction::new_builder()
                        .raw(RawL2Transaction::new_builder().nonce(i.pack()).build())
                        .build(),
                ),
                fee_rate: 100,
                cycles_limit: 1000,
                sender: 2,
            };
            queue.add(entry1);
        }

        assert_eq!(queue.len(), MAX_QUEUE_SIZE);

        // add 1 more item
        {
            let entry1 = FeeEntry {
                item: FeeItem::Tx(
                    L2Transaction::new_builder()
                        .raw(
                            RawL2Transaction::new_builder()
                                .nonce(10001u32.pack())
                                .build(),
                        )
                        .build(),
                ),
                fee_rate: 100,
                cycles_limit: 1000,
                sender: 2,
            };
            queue.add(entry1);
        }

        // we should trigger the drop
        assert!(queue.len() < MAX_QUEUE_SIZE);
    }

    const ALWAYS_SUCCESS_CODE_HASH: [u8; 32] = [42u8; 32];

    fn setup_genesis(store: &Store) {
        let rollup_type_hash = H256::from_u32(42);
        let rollup_config = RollupConfig::new_builder()
            .allowed_eoa_type_hashes(vec![ALWAYS_SUCCESS_CODE_HASH].pack())
            .finality_blocks(0.pack())
            .build();
        let genesis_config = GenesisConfig {
            timestamp: 0,
            meta_contract_validator_type_hash: Default::default(),
            rollup_config: rollup_config.into(),
            rollup_type_hash: {
                let h: [u8; 32] = rollup_type_hash.into();
                h.into()
            },
            secp_data_dep: Default::default(),
        };
        let genesis_committed_info = L2BlockCommittedInfo::default();
        init_genesis(
            store,
            &genesis_config,
            genesis_committed_info,
            Bytes::default(),
        )
        .unwrap();
    }
}