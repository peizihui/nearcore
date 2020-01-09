// MOO block production scheduling needs to be changed
use near_crypto::Signer;
use near_primitives::block::Approval;
use near_primitives::hash::CryptoHash;
use near_primitives::types::{AccountId, Balance, BlockHeight, BlockHeightDelta, ValidatorStake};
use std::collections::HashMap;
use std::convert::TryFrom;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Have that many iterations in the timer instead of `loop` to prevent potential bugs from blocking
/// the node
const MAX_TIMER_ITERS: usize = 20;

/// The threshold for doomslug to create a block.
/// `HalfStake` means the block can only be produced if at least half of the stake is approvign it,
///             and is what should be used in production (and what guarantees doomslug finality)
/// `SingleApproval` means the block can be produced if it has at least one approval. This is used
///             in many tests (e.g. `cross_shard_tx`) to create lots of forkfulness.
#[derive(PartialEq, Eq, Debug, Clone, Copy)]
pub enum DoomslugThresholdMode {
    SingleApproval,
    HalfStake,
}

/// The result of processing an approval.
///
/// `None`
/// `PassedThreshold(when)` - after processing this approval the block has passed the threshold set by
///                     `threshold_mode` (either one half of the total stake, or a single approval).
///                     Once the threshold is hit, we want for `T(h - h_final) / 2` before producing
///                     a block
/// `ReadyToProduce`  - after processing this approval the block can be produced without waiting.
///                     We produce a block without further waiting if it has 2/3 of approvals AND has
///                     doomslug finality on the previous block
#[derive(PartialEq, Eq, Debug)]
pub enum DoomslugBlockProductionReadiness {
    None,
    PassedThreshold(Instant),
    ReadyToProduce,
}

struct DoomslugTimer {
    started: Instant,
    height: BlockHeight,
    min_delay: Duration,
    delay_step: Duration,
    max_delay: Duration,
}

struct DoomslugTip {
    block_hash: CryptoHash,
    reference_hash: Option<CryptoHash>,
    height: BlockHeight,
}

struct DoomslugApprovalsTracker {
    witness: HashMap<AccountId, Approval>,
    account_id_to_stake: HashMap<AccountId, Balance>,
    total_stake: Balance,
    approved_stake: Balance,
    endorsed_stake: Balance,
    time_passed_threshold: Option<Instant>,
    threshold_mode: DoomslugThresholdMode,
}

/// Contains all the logic for Doomslug, but no integration with chain or storage. The integration
/// happens via `PersistentDoomslug` struct. The split is to simplify testing of the logic separate
/// from the chain.
pub struct Doomslug {
    me: Option<AccountId>,
    approval_tracking: HashMap<(CryptoHash, BlockHeight), DoomslugApprovalsTracker>,
    /// Largest height that we promised to skip
    largest_promised_skip_height: BlockHeight,
    /// Largest height that we endorsed
    largest_endorsed_height: BlockHeight,
    /// Largest height for which we saw a block containing 1/2 endorsements in it
    largest_ds_final_height: BlockHeight,
    /// Largest height for which we saw threshold approvals (and thus can potentially create a block)
    largest_threshold_height: BlockHeight,
    /// Information Doomslug tracks about the chain tip
    tip: DoomslugTip,
    /// Information to track the timer (see `start_timer` routine in the paper)
    timer: DoomslugTimer,
    signer: Option<Arc<dyn Signer>>,
    /// How many approvals to have before producing a block. In production should be always `HalfStake`,
    ///    but for many tests we use `SingleApproval` to invoke more forkfulness
    threshold_mode: DoomslugThresholdMode,
}

impl DoomslugTimer {
    /// Computes the delay to sleep given the number of heights from the last block with doomslug
    /// finality. This is what represented by `T` in the paper.
    ///
    /// # Arguments
    /// * `n` - number of heights since the last block with doomslug finality
    ///
    /// # Returns
    /// Duration to sleep
    pub fn get_delay(&self, n: BlockHeightDelta) -> Duration {
        let n32 = u32::try_from(n).unwrap_or(std::u32::MAX);
        std::cmp::min(self.max_delay, self.min_delay + self.delay_step * n32.saturating_sub(1))
    }
}

impl DoomslugApprovalsTracker {
    fn new(stakes: &Vec<ValidatorStake>, threshold_mode: DoomslugThresholdMode) -> Self {
        let account_id_to_stake =
            stakes.iter().map(|x| (x.account_id.clone(), x.amount)).collect::<HashMap<_, _>>();
        assert!(account_id_to_stake.len() == stakes.len());
        let total_stake = account_id_to_stake.values().sum::<Balance>();

        DoomslugApprovalsTracker {
            witness: Default::default(),
            account_id_to_stake,
            total_stake,
            approved_stake: 0,
            endorsed_stake: 0,
            time_passed_threshold: None,
            threshold_mode,
        }
    }

    /// Given a single approval (either an endorsement or a skip-message) updates the approved
    /// stake on the block that is being approved, and returns the largest threshold that is crossed
    /// This method only returns `ReadyToProduce` if the block has 2/3 approvals and 1/2 endorsements.
    /// The production readiness due to enough time passing since crossing the threshold should be
    /// handled by the caller.
    ///
    /// # Arguments
    /// * now      - the current timestamp
    /// * approval - the approval to process
    ///
    /// # Returns
    /// `None` is the block doesn't have enough approvals yet to cross the doomslug threshold
    /// `PassedThreshold` if the block has enough approvals to pass the threshold, but can't be
    ///     produced bypassing the timeout
    /// `ReadyToProduce` if the block can be produced bypassing the timeout (i.e. has 2/3+ of
    ///     approvals and 1/2+ of endorsements)
    fn process_approval(
        &mut self,
        now: Instant,
        approval: &Approval,
    ) -> DoomslugBlockProductionReadiness {
        let mut increment_approved_stake = false;
        let mut increment_endorsed_stake = false;
        self.witness.entry(approval.account_id.clone()).or_insert_with(|| {
            increment_approved_stake = true;
            if approval.is_endorsement {
                increment_endorsed_stake = true;
            }
            approval.clone()
        });

        if increment_approved_stake {
            self.approved_stake +=
                self.account_id_to_stake.get(&approval.account_id).map_or(0, |x| *x);
        }

        if increment_endorsed_stake {
            self.endorsed_stake +=
                self.account_id_to_stake.get(&approval.account_id).map_or(0, |x| *x);
        }

        // We call to `get_block_production_readiness` here so that if the number of approvals crossed
        // the threshold, the timer for block production starts.
        self.get_block_production_readiness(now)
    }

    /// Returns whether the block is ready to be produced without waiting for any extra time,
    /// has crossed the doomslug threshold (and thus can be produced given enough time passed),
    /// or is not ready to be produced at all.
    /// This method only returns `ReadyToProduce` if the block has 2/3 approvals and 1/2 endorsements.
    /// The production readiness due to enough time passing since crossing the threshold should be
    /// handled by the caller.
    ///
    /// # Arguments
    /// * now - the current timestamp
    ///
    /// # Returns
    /// `None` is the block doesn't have enough approvals yet to cross the doomslug threshold
    /// `PassedThreshold` if the block has enough approvals to pass the threshold, but can't be
    ///     produced bypassing the timeout
    /// `ReadyToProduce` if the block can be produced bypassing the timeout (i.e. has 2/3+ of
    ///     approvals and 1/2+ of endorsements)
    fn get_block_production_readiness(&mut self, now: Instant) -> DoomslugBlockProductionReadiness {
        if self.approved_stake > self.total_stake * 2 / 3
            && self.endorsed_stake > self.total_stake / 2
        {
            DoomslugBlockProductionReadiness::ReadyToProduce
        } else if self.approved_stake > self.total_stake / 2
            || self.threshold_mode == DoomslugThresholdMode::SingleApproval
        {
            if self.time_passed_threshold == None {
                self.time_passed_threshold = Some(now);
            }
            DoomslugBlockProductionReadiness::PassedThreshold(self.time_passed_threshold.unwrap())
        } else {
            DoomslugBlockProductionReadiness::None
        }
    }
}

impl Doomslug {
    pub fn new(
        me: Option<AccountId>,
        largest_previously_skipped_height: BlockHeight,
        largest_previously_endorsed_height: BlockHeight,
        min_delay: Duration,
        delay_step: Duration,
        max_delay: Duration,
        signer: Option<Arc<dyn Signer>>,
        threshold_mode: DoomslugThresholdMode,
    ) -> Self {
        assert_eq!(me.is_some(), signer.is_some());
        Doomslug {
            me,
            approval_tracking: HashMap::new(),
            largest_promised_skip_height: largest_previously_skipped_height,
            largest_endorsed_height: largest_previously_endorsed_height,
            largest_ds_final_height: 0,
            largest_threshold_height: 0,
            tip: DoomslugTip { block_hash: CryptoHash::default(), reference_hash: None, height: 0 },
            timer: DoomslugTimer {
                started: Instant::now(),
                height: 0,
                min_delay,
                delay_step,
                max_delay,
            },
            signer,
            threshold_mode,
        }
    }

    /// Returns the `(hash, height)` of the current tip. Currently is only used by tests.
    pub fn get_tip(&self) -> (CryptoHash, BlockHeight) {
        (self.tip.block_hash, self.tip.height)
    }

    /// Returns the largest height for which we have enough approvals to be theoretically able to
    ///     produce a block (in practice a blocks might not be produceable yet if not enough time
    ///     passed since it accumulated enough approvals)
    pub fn get_largest_height_crossing_threshold(&self) -> BlockHeight {
        self.largest_threshold_height
    }

    pub fn get_largest_height_with_doomslug_finality(&self) -> BlockHeight {
        self.largest_ds_final_height
    }

    pub fn get_largest_skipped_height(&self) -> BlockHeight {
        self.largest_promised_skip_height
    }

    pub fn get_largest_endorsed_height(&self) -> BlockHeight {
        self.largest_endorsed_height
    }

    pub fn get_timer_height(&self) -> BlockHeight {
        self.timer.height
    }

    pub fn get_timer_start(&self) -> Instant {
        self.timer.started
    }

    /// Is expected to be called periodically and processed the timer (`start_timer` in the paper)
    /// If the `cur_time` way ahead of last time the `process_timer` was called, will only process
    /// a bounded number of steps, to avoid an infinite loop in case of some bugs.
    /// Processes sending delayed approvals or skip messages
    ///
    /// # Arguments
    /// * `cur_time` - is expected to receive `now`. Doesn't directly use `now` to simplify testing
    ///
    /// # Returns
    /// A vector of approvals that need to be sent to other block producers as a result of processing
    /// the timers
    #[must_use]
    pub fn process_timer(&mut self, cur_time: Instant) -> Vec<Approval> {
        let mut ret = vec![];
        for _i in 0..MAX_TIMER_ITERS {
            let delay = self
                .timer
                .get_delay(self.timer.height.saturating_sub(self.largest_ds_final_height));
            if cur_time >= self.timer.started + delay {
                if self.timer.height > self.largest_endorsed_height
                    && self.timer.height > self.tip.height
                {
                    self.largest_promised_skip_height =
                        std::cmp::max(self.timer.height, self.largest_promised_skip_height);
                    if let Some(approval) = self.create_approval(self.timer.height + 1, false) {
                        ret.push(approval);
                    }
                }

                // Restart the timer
                self.timer.started += delay;
                self.timer.height += 1;
            } else {
                break;
            }
        }

        ret
    }

    pub fn create_approval(
        &self,
        target_height: BlockHeight,
        is_endorsement: bool,
    ) -> Option<Approval> {
        self.me.as_ref().map(|me| {
            Approval::new(
                self.tip.block_hash,
                self.tip.reference_hash,
                target_height,
                is_endorsement,
                &**self.signer.as_ref().unwrap(),
                me.clone(),
            )
        })
    }

    /// Determines whether a block that precedes the block containing approvals has doomslug
    /// finality, i.e. if the sum of stakes of block producers who produced endorsements (approvals
    /// with `is_endorsed = true`) exceeds half of the total stake
    ///
    /// # Arguments
    /// * `approvals` - the set of approvals in the current block
    /// * `stakes`    - the vector of validator stakes in the current epoch
    pub fn is_approved_block_ds_final(
        // MOO make sure approvals are dedupped by now
        approvals: &Vec<Approval>,
        account_id_to_stake: &HashMap<AccountId, u128>,
    ) -> bool {
        let threshold = account_id_to_stake.values().sum::<Balance>() / 2;

        let endorsed_stake = approvals
            .iter()
            .map(|approval| {
                { account_id_to_stake.get(&approval.account_id) }.map_or(0, |stake| {
                    if approval.is_endorsement {
                        *stake
                    } else {
                        0
                    }
                })
            })
            .sum::<Balance>();

        endorsed_stake > threshold
    }

    /// Determines whether the block `prev_hash` has doomslug finality from perspective of a block
    /// that has it as its previous, and is at height `target_height`.
    /// Internally just pulls approvals from the `approval_tracking` and calls to
    /// `is_approved_block_ds_final`
    /// This method is presently only used by tests, and is not efficient (specifically, it
    /// recomputes `endorsed_state` and `total_stake`, both of which are/can be maintained)
    ///
    /// # Arguments
    /// * `prev_hash`     - the hash of the previous block (for which the ds finality is tested)
    /// * `target_height` - the height of the current block
    pub fn is_prev_block_ds_final(
        &self,
        prev_hash: CryptoHash,
        target_height: BlockHeight,
    ) -> bool {
        let approvals_tracker = self.approval_tracking.get(&(prev_hash, target_height));
        match approvals_tracker {
            None => false,
            Some(approvals_tracker) => Doomslug::is_approved_block_ds_final(
                &approvals_tracker.witness.values().cloned().collect::<Vec<_>>(),
                &approvals_tracker.account_id_to_stake,
            ),
        }
    }

    /// Determines whether a block has enough approvals to be produced.
    /// In production (with `mode == HalfStake`) we require the total stake of all the approvals to
    /// be strictly more than half of the total stake. For many non-doomslug specific tests
    /// (with `mode == SingleApproval`) just one approval is sufficient.
    ///
    /// # Arguments
    /// * `mode`      - whether we want half of the total stake or just a single approval
    /// * `approvals` - the set of approvals in the current block
    /// * `stakes`    - the vector of validator stakes in the current epoch
    pub fn can_approved_block_be_produced(
        mode: DoomslugThresholdMode,
        approvals: &Vec<Approval>,
        account_id_to_stake: &HashMap<AccountId, u128>,
    ) -> bool {
        if mode == DoomslugThresholdMode::SingleApproval {
            return !approvals.is_empty();
        }

        let threshold = account_id_to_stake.values().sum::<Balance>() / 2;

        let approved_stake = approvals
            .iter()
            .map(|approval| {
                { account_id_to_stake.get(&approval.account_id) }.map_or(0, |stake| *stake)
            })
            .sum::<Balance>();

        approved_stake > threshold
    }

    pub fn remove_witness(
        &mut self,
        prev_hash: CryptoHash,
        target_height: BlockHeight,
    ) -> Vec<Approval> {
        let approvals_tracker = self.approval_tracking.remove(&(prev_hash, target_height));
        match approvals_tracker {
            None => vec![],
            Some(approvals_tracker) => {
                approvals_tracker.witness.into_iter().map(|(_, v)| v).collect::<Vec<_>>()
            }
        }
    }

    /// Updates the current tip of the chain. Restarts the timer accordingly.
    /// If the tip can be endorsed, produces the endorsement, otherwise a non-endorsing approval.
    ///
    /// # Arguments
    /// * `now`            - current time. Doesn't call to `Utc::now()` directly to simplify testing
    /// * `block_hash`     - the hash of the new tip
    /// * `reference_hash` - is expected to come from the finality gadget and represents the
    ///                      reference hash (if any) to be included in the approvals being sent for
    ///                      this tip (whether endorsements or skip messages)
    /// * `height`         - the height of the tip
    /// * `last_ds_final_height` - last height at which a block in this chain has doomslug finality
    ///
    /// # Returns
    /// The approval to send
    #[must_use]
    pub fn set_tip(
        &mut self,
        now: Instant,
        block_hash: CryptoHash,
        reference_hash: Option<CryptoHash>,
        height: BlockHeight,
        last_ds_final_height: BlockHeight,
    ) -> Option<Approval> {
        self.tip = DoomslugTip { block_hash, reference_hash, height };

        self.largest_ds_final_height = last_ds_final_height;
        self.timer.height = height + 1;
        self.timer.started = now;

        let is_endorsement =
            height > self.largest_promised_skip_height && height > self.largest_endorsed_height;

        if is_endorsement {
            self.largest_endorsed_height = height;
        }

        self.create_approval(height + 1, is_endorsement)
    }

    /// Records an approval message, and return whether the block has passed the threshold / ready
    /// to be produced without waiting any further. See the comment for `DoomslugApprovalTracker::process_approval`
    /// for details
    #[must_use]
    fn on_approval_message_internal(
        &mut self,
        now: Instant,
        approval: &Approval,
        stakes: &Vec<ValidatorStake>,
    ) -> DoomslugBlockProductionReadiness {
        // MOO handle spammin
        // MOO make sure only called for our heights
        let threshold_mode = self.threshold_mode;
        let ret = self
            .approval_tracking
            .entry((approval.parent_hash, approval.target_height))
            .or_insert_with(|| DoomslugApprovalsTracker::new(stakes, threshold_mode))
            .process_approval(now, approval);

        if ret != DoomslugBlockProductionReadiness::None {
            if approval.target_height > self.largest_threshold_height {
                self.largest_threshold_height = approval.target_height;
            }
        }

        ret
    }

    /// Processes single approval
    pub fn on_approval_message(
        &mut self,
        now: Instant,
        approval: &Approval,
        stakes: &Vec<ValidatorStake>,
    ) {
        let _ = self.on_approval_message_internal(now, approval, stakes);
    }

    #[must_use]
    pub fn ready_to_produce_block(&mut self, now: Instant, target_height: BlockHeight) -> bool {
        let approval_tracker =
            self.approval_tracking.get_mut(&(self.tip.block_hash, target_height));
        match approval_tracker {
            None => false,
            Some(approval_tracker) => {
                let block_production_readiness =
                    approval_tracker.get_block_production_readiness(now);
                match block_production_readiness {
                    DoomslugBlockProductionReadiness::None => false,
                    DoomslugBlockProductionReadiness::PassedThreshold(when) => {
                        let delay = self.timer.get_delay(
                            self.timer.height.saturating_sub(self.largest_ds_final_height),
                        ) / 2;

                        now > when + delay
                    }
                    DoomslugBlockProductionReadiness::ReadyToProduce => true,
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::doomslug::{DoomslugBlockProductionReadiness, DoomslugThresholdMode};
    use crate::Doomslug;
    use near_crypto::{InMemorySigner, KeyType, SecretKey};
    use near_primitives::block::Approval;
    use near_primitives::hash::hash;
    use near_primitives::types::ValidatorStake;
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    #[test]
    fn test_endorsements_and_skips_basic() {
        let mut now = Instant::now(); // For the test purposes the absolute value of the initial instant doesn't matter

        let mut ds = Doomslug::new(
            Some("test".to_string()),
            0,
            0,
            Duration::from_millis(1000),
            Duration::from_millis(100),
            Duration::from_millis(3000),
            Some(Arc::new(InMemorySigner::from_seed("test", KeyType::ED25519, "test"))),
            DoomslugThresholdMode::HalfStake,
        );

        // Set a new tip, must produce an endorsement
        let approval = ds.set_tip(now, hash(&[1]), None, 1, 1).unwrap();
        assert_eq!(approval.parent_hash, hash(&[1]));
        assert_eq!(approval.target_height, 2);
        assert!(approval.is_endorsement);

        // Same tip => no endorsement, but still expect an approval (it is for the cases when a block
        // at lower height is received after a block at a higher height, e.g. due to finality gadget)
        let approval = ds.set_tip(now, hash(&[1]), None, 1, 1).unwrap();
        assert_eq!(approval.parent_hash, hash(&[1]));
        assert_eq!(approval.target_height, 2);
        assert!(!approval.is_endorsement);

        // The block was `ds_final` and therefore started the timer. Try checking before one second expires
        assert_eq!(ds.process_timer(now + Duration::from_millis(999)), vec![]);

        // But one second should trigger the skip
        match ds.process_timer(now + Duration::from_millis(1000)) {
            approvals if approvals.len() == 0 => assert!(false),
            approvals => {
                assert_eq!(approvals[0].parent_hash, hash(&[1]));
                assert_eq!(approvals[0].target_height, 3);
                assert!(!approvals[0].is_endorsement);
            }
        }

        // Shift now 1 second forward
        now += Duration::from_millis(1000);

        // Not processing a block at height 2 should not produce an endorsement (but still an approval)
        let approval = ds.set_tip(now, hash(&[2]), None, 2, 1).unwrap();
        assert_eq!(approval.parent_hash, hash(&[2]));
        assert_eq!(approval.target_height, 3);
        assert!(!approval.is_endorsement);

        // Shift now 1 second forward
        now += Duration::from_millis(1000);

        // But at height 3 should (also neither block has ds_finality set, keep last ds_final at 1 for now)
        let approval = ds.set_tip(now, hash(&[3]), None, 3, 1).unwrap();
        assert_eq!(approval.parent_hash, hash(&[3]));
        assert_eq!(approval.target_height, 4);
        assert!(approval.is_endorsement);

        // Move 1 second further
        now += Duration::from_millis(1000);

        assert_eq!(ds.process_timer(now + Duration::from_millis(199)), vec![]);

        match ds.process_timer(now + Duration::from_millis(200)) {
            approvals if approvals.len() == 0 => assert!(false),
            approvals if approvals.len() == 1 => {
                assert_eq!(approvals[0].parent_hash, hash(&[3]));
                assert_eq!(approvals[0].target_height, 5);
                assert!(!approvals[0].is_endorsement);
            }
            _ => assert!(false),
        }

        // Move 1 second further
        now += Duration::from_millis(1000);

        // Now skip 5 (the extra delay is 200+300 = 500)
        assert_eq!(ds.process_timer(now + Duration::from_millis(499)), vec![]);

        match ds.process_timer(now + Duration::from_millis(500)) {
            approvals if approvals.len() == 0 => assert!(false),
            approvals => {
                assert_eq!(approvals[0].parent_hash, hash(&[3]));
                assert_eq!(approvals[0].target_height, 6);
                assert!(!approvals[0].is_endorsement);
            }
        }

        // Move 1 second further
        now += Duration::from_millis(1000);

        // Skip 6 (the extra delay is 0+200+300+400 = 900)
        assert_eq!(ds.process_timer(now + Duration::from_millis(899)), vec![]);

        match ds.process_timer(now + Duration::from_millis(900)) {
            approvals if approvals.len() == 0 => assert!(false),
            approvals => {
                assert_eq!(approvals[0].parent_hash, hash(&[3]));
                assert_eq!(approvals[0].target_height, 7);
                assert!(!approvals[0].is_endorsement);
            }
        }

        // Move 1 second further
        now += Duration::from_millis(1000);

        // Accept block at 5 with ds finality, expect it to produce an approval, but not an endorsement
        let approval = ds.set_tip(now, hash(&[5]), None, 5, 5).unwrap();
        assert_eq!(approval.parent_hash, hash(&[5]));
        assert_eq!(approval.target_height, 6);
        assert!(!approval.is_endorsement);

        // Skip a whole bunch of heights by moving 100 seconds ahead
        now += Duration::from_millis(100_000);
        assert!(ds.process_timer(now).len() > 10);

        // Add some random small number of milliseconds to test that when the next block is added, the
        // timer is reset
        now += Duration::from_millis(17);

        // That approval should not be an endorsement, since we skipped 6
        let approval = ds.set_tip(now, hash(&[6]), None, 6, 5).unwrap();
        assert_eq!(approval.parent_hash, hash(&[6]));
        assert_eq!(approval.target_height, 7);
        assert!(!approval.is_endorsement);

        // The block height was less than the timer height, and thus the timer was reset.
        // The wait time for height 7 with last ds final block at 5 is 1100
        assert_eq!(ds.process_timer(now + Duration::from_millis(1099)), vec![]);

        match ds.process_timer(now + Duration::from_millis(1100)) {
            approvals if approvals.len() == 0 => assert!(false),
            approvals => {
                assert_eq!(approvals[0].parent_hash, hash(&[6]));
                assert_eq!(approvals[0].target_height, 8);
                assert!(!approvals[0].is_endorsement);
            }
        }
    }

    #[test]
    fn test_doomslug_approvals() {
        let stakes = vec![("test1", 2), ("test2", 1), ("test3", 3), ("test4", 2)]
            .into_iter()
            .map(|(account_id, amount)| ValidatorStake {
                account_id: account_id.to_string(),
                amount,
                public_key: SecretKey::from_seed(KeyType::ED25519, account_id).public_key(),
            })
            .collect::<Vec<_>>();

        let signer = Arc::new(InMemorySigner::from_seed("test", KeyType::ED25519, "test"));
        let mut ds = Doomslug::new(
            Some("test".to_string()),
            0,
            0,
            Duration::from_millis(1000),
            Duration::from_millis(100),
            Duration::from_millis(3000),
            Some(signer.clone()),
            DoomslugThresholdMode::HalfStake,
        );

        let mut now = Instant::now();

        // In the comments below the format is
        // account, height -> approved stake
        // The total stake is 8, so the thresholds are 5 and 7

        // "test1", 2 -> 2
        assert_eq!(
            ds.on_approval_message_internal(
                now,
                &Approval::new(hash(&[1]), None, 2, true, &*signer, "test1".to_string()),
                &stakes,
            ),
            DoomslugBlockProductionReadiness::None,
        );

        // "test3", 4 -> 3
        assert_eq!(
            ds.on_approval_message_internal(
                now,
                &Approval::new(hash(&[1]), None, 4, true, &*signer, "test3".to_string()),
                &stakes,
            ),
            DoomslugBlockProductionReadiness::None,
        );

        // "test1", 4 -> 5
        assert_eq!(
            ds.on_approval_message_internal(
                now,
                &Approval::new(hash(&[1]), None, 4, true, &*signer, "test1".to_string()),
                &stakes,
            ),
            DoomslugBlockProductionReadiness::PassedThreshold(now),
        );

        // "test1", 4 -> same account, still 5
        assert_eq!(
            ds.on_approval_message_internal(
                now + Duration::from_millis(100),
                &Approval::new(hash(&[1]), None, 4, true, &*signer, "test1".to_string()),
                &stakes,
            ),
            DoomslugBlockProductionReadiness::PassedThreshold(now),
        );

        // "test4", 4 -> 7
        assert_eq!(
            ds.on_approval_message_internal(
                now,
                &Approval::new(hash(&[1]), None, 4, true, &*signer, "test4".to_string()),
                &stakes,
            ),
            DoomslugBlockProductionReadiness::ReadyToProduce,
        );

        // "test4", 2 -> 4
        assert_eq!(
            ds.on_approval_message_internal(
                now,
                &Approval::new(hash(&[1]), None, 2, true, &*signer, "test4".to_string()),
                &stakes,
            ),
            DoomslugBlockProductionReadiness::None,
        );

        now += Duration::from_millis(200);

        // "test2", 2 -> 5
        assert_eq!(
            ds.on_approval_message_internal(
                now,
                &Approval::new(hash(&[1]), None, 2, true, &*signer, "test2".to_string()),
                &stakes,
            ),
            DoomslugBlockProductionReadiness::PassedThreshold(now),
        );

        // A different parent hash
        assert_eq!(
            ds.on_approval_message_internal(
                now,
                &Approval::new(hash(&[2]), None, 4, true, &*signer, "test2".to_string()),
                &stakes,
            ),
            DoomslugBlockProductionReadiness::None,
        );
    }
    // MOO test that honest actors don't get slashed
}
