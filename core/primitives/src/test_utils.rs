use std::sync::Mutex;

use log::LevelFilter;

use lazy_static::lazy_static;
use near_crypto::{EmptySigner, PublicKey, Signer};

use crate::account::{AccessKey, AccessKeyPermission};
use crate::block::{Approval, Block};
use crate::hash::CryptoHash;
use crate::transaction::{
    Action, AddKeyAction, CreateAccountAction, SignedTransaction, StakeAction, Transaction,
    TransferAction,
};
use crate::types::{AccountId, Balance, BlockIndex, EpochId, Nonce};
use crate::utils::to_timestamp;
use chrono::{DateTime, Utc};

lazy_static! {
    static ref HEAVY_TESTS_LOCK: Mutex<()> = Mutex::new(());
}

pub fn heavy_test<F>(f: F)
where
    F: FnOnce() -> (),
{
    let _guard = HEAVY_TESTS_LOCK.lock();
    f();
}

pub fn init_test_logger() {
    let _ = env_logger::Builder::new()
        .filter_module("tokio_reactor", LevelFilter::Info)
        .filter_module("tokio_core", LevelFilter::Info)
        .filter_module("hyper", LevelFilter::Info)
        .filter(None, LevelFilter::Debug)
        .try_init();
}

pub fn init_test_module_logger(module: &str) {
    let _ = env_logger::Builder::new()
        .filter_module("tokio_reactor", LevelFilter::Info)
        .filter_module("tokio_core", LevelFilter::Info)
        .filter_module("hyper", LevelFilter::Info)
        .filter_module("cranelift_wasm", LevelFilter::Warn)
        .filter_module(module, LevelFilter::Info)
        .filter(None, LevelFilter::Info)
        .try_init();
}

pub fn init_integration_logger() {
    let _ = env_logger::Builder::new()
        .filter(None, LevelFilter::Info)
        .filter(Some("actix_web"), LevelFilter::Warn)
        .try_init();
}

impl Transaction {
    pub fn sign(self, signer: &dyn Signer) -> SignedTransaction {
        let signature = signer.sign(self.get_hash().as_ref());
        SignedTransaction::new(signature, self)
    }
}

impl SignedTransaction {
    pub fn send_money(
        nonce: Nonce,
        signer_id: AccountId,
        receiver_id: AccountId,
        signer: &dyn Signer,
        deposit: Balance,
        block_hash: CryptoHash,
    ) -> Self {
        Self::from_actions(
            nonce,
            signer_id,
            receiver_id,
            signer,
            vec![Action::Transfer(TransferAction { deposit })],
            block_hash,
        )
    }

    pub fn stake(
        nonce: Nonce,
        signer_id: AccountId,
        signer: &dyn Signer,
        stake: Balance,
        public_key: PublicKey,
        block_hash: CryptoHash,
    ) -> Self {
        Self::from_actions(
            nonce,
            signer_id.clone(),
            signer_id,
            signer,
            vec![Action::Stake(StakeAction { stake, public_key })],
            block_hash,
        )
    }

    pub fn create_account(
        nonce: Nonce,
        originator: AccountId,
        new_account_id: AccountId,
        amount: Balance,
        public_key: PublicKey,
        signer: &dyn Signer,
        block_hash: CryptoHash,
    ) -> Self {
        Self::from_actions(
            nonce,
            originator,
            new_account_id,
            signer,
            vec![
                Action::CreateAccount(CreateAccountAction {}),
                Action::AddKey(AddKeyAction {
                    public_key,
                    access_key: AccessKey { nonce: 0, permission: AccessKeyPermission::FullAccess },
                }),
                Action::Transfer(TransferAction { deposit: amount }),
            ],
            block_hash,
        )
    }

    pub fn empty(block_hash: CryptoHash) -> Self {
        Self::from_actions(0, "".to_string(), "".to_string(), &EmptySigner {}, vec![], block_hash)
    }
}

impl Block {
    pub fn empty_with_epoch(
        prev: &Block,
        height: BlockIndex,
        epoch_id: EpochId,
        signer: &dyn Signer,
    ) -> Self {
        Self::empty_with_approvals(prev, height, epoch_id, vec![], signer)
    }

    pub fn empty_with_height(prev: &Block, height: BlockIndex, signer: &dyn Signer) -> Self {
        Self::empty_with_epoch(prev, height, prev.header.inner_lite.epoch_id.clone(), signer)
    }

    pub fn empty(prev: &Block, signer: &dyn Signer) -> Self {
        Self::empty_with_height(prev, prev.header.inner_lite.height + 1, signer)
    }

    pub fn empty_with_timestamp(
        prev: &Block,
        signer: &dyn Signer,
        timestamp: DateTime<Utc>,
    ) -> Self {
        Self::empty_with_height_and_timestamp(
            prev,
            prev.header.inner_lite.height + 1,
            signer,
            timestamp,
        )
    }

    pub fn empty_with_height_and_timestamp(
        prev: &Block,
        height: BlockIndex,
        signer: &dyn Signer,
        timestamp: DateTime<Utc>,
    ) -> Self {
        let mut block = Self::empty_with_height(prev, height, signer);
        block.header.inner_lite.timestamp = to_timestamp(timestamp);
        block.header.init();
        block.header.signature = signer.sign(block.header.hash.as_ref());
        block
    }

    /// This is not suppose to be used outside of chain tests, because this doesn't refer to correct chunks.
    /// Done because chain tests don't have a good way to store chunks right now.
    pub fn empty_with_approvals(
        prev: &Block,
        height: BlockIndex,
        epoch_id: EpochId,
        approvals: Vec<Approval>,
        signer: &dyn Signer,
    ) -> Self {
        Block::produce(
            &prev.header,
            height,
            prev.chunks.clone(),
            epoch_id,
            approvals,
            0,
            Some(0),
            vec![],
            vec![],
            signer,
            0.into(),
            CryptoHash::default(),
            CryptoHash::default(),
        )
    }
}
