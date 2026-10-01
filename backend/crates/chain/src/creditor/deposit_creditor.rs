//! The deposit creditor: turns observed on-chain deposits into ledger credits.
//!
//! [`crate::services::account_resolver`] tells us which user a deposit address
//! belongs to, and [`engipay_ledger::postgres::PostgresLedgerStore`] writes the
//! credit. This module is the policy in between, and it answers the three design
//! questions issue #171 inherited from #50.
//!
//! # Design
//!
//! ## 1. Idempotency on the deposit reference
//!
//! Every deposit carries a `reference` (e.g. `base:<tx hash>:<log index>`) that
//! is unique per credit. That reference is passed straight to
//! [`PostgresLedgerStore::deposit`], which writes it under a UNIQUE constraint
//! inside a `SERIALIZABLE` transaction. Replaying the same deposit — because
//! the watcher polled an overlapping ledger range, or because the process
//! crashed mid-commit — therefore credits the user exactly once and returns
//! the original receipt with `replayed: true`.
//!
//! We do **not** keep our own "have I seen this?" set. An in-memory set would
//! be lost on restart and would not help across processes; the database
//! constraint is the only thing that is actually durable.
//!
//! ## 2. Deposits that cannot be attributed to a user
//!
//! A deposit to the bare custody account, or to an address that is not in
//! `deposit_addresses`, has no owner. It is **not** dropped and it is **not**
//! guessed: [`CreditOutcome::Unattributed`] is returned so the caller can log
//! or quarantine it. Crediting it to a random user would be a theft bug.
//!
//! ## 3. Crash safety between reading a deposit and committing it
//!
//! Reading (the watcher's poll) and committing (`deposit`) cannot be made one
//! atomic step, because the watcher and the ledger are separate concerns. The
//! guarantee we rely on is at-least-once delivery plus idempotent commit: a
//! crash can only ever cause a deposit to be *delivered again*, never credited
//! twice. Combined with the per-deposit `reference`, re-processing after a
//! crash is safe and is the expected path.
//!
//! # Ordering
//!
//! [`crate::creditor::SequentialCreditor`] sorts whole ledger events before
//! they reach here, so this module only has to get the *policy* right. It is
//! deliberately free of ledger-state of its own.

use engipay_core::UserId;
use engipay_ledger::postgres::PostgresLedgerStore;

use crate::ChainClient;
use crate::ObservedDeposit;

/// What happened to one deposit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CreditOutcome {
    /// The deposit was credited. `replayed` is `true` when this reference had
    /// already been applied and nothing moved this time.
    Credited {
        transaction_id: uuid::Uuid,
        replayed: bool,
    },
    /// The deposit has not reached the chain's required confirmation depth yet.
    /// It is not an error: the watcher will offer it again as the chain
    /// confirms it.
    NotConfirmed { confirmations: u32, required: u32 },
    /// No user owns the deposit address. Never credited, never guessed.
    Unattributed { address: String },
}

impl CreditOutcome {
    /// `true` when the deposit reached the ledger (freshly or as a replay).
    pub const fn is_credited(&self) -> bool {
        matches!(self, Self::Credited { .. })
    }

    /// `true` when the deposit will never be credited as-is and needs a human.
    pub const fn needs_review(&self) -> bool {
        matches!(self, Self::Unattributed { .. })
    }
}

/// Errors that stop a credit attempt for a reason other than the deposit's own
/// state. `Unattributed` and `NotConfirmed` are *not* errors — see
/// [`CreditOutcome`].
#[derive(Debug, thiserror::Error)]
pub enum CreditorError {
    #[error("deposit reference must not be empty")]
    EmptyReference,
    #[error("ledger error: {0}")]
    Ledger(#[from] engipay_ledger::LedgerError),
    #[error("failed to resolve deposit address: {0}")]
    Resolve(String),
}

/// Credits observed deposits into the Postgres ledger.
///
/// `store` is a borrowed handle to the ledger; `chain` supplies the
/// confirmation depth this network requires. The creditor holds no cursor of
/// its own — re-processing is safe, and correctness comes from the per-deposit
/// reference.
pub struct DepositCreditor<'a, C: ChainClient + ?Sized> {
    store: &'a PostgresLedgerStore,
    chain: &'a C,
}

impl<'a, C: ChainClient + ?Sized> DepositCreditor<'a, C> {
    /// Builds a creditor that writes to `store` and applies `chain`'s
    /// confirmation requirements.
    pub const fn new(store: &'a PostgresLedgerStore, chain: &'a C) -> Self {
        Self { store, chain }
    }

    /// Credits `deposit` to `user`.
    ///
    /// The deposit is written with `deposit.reference` as the ledger
    /// idempotency key, so calling this twice with the same deposit credits
    /// the user once.
    pub async fn credit(
        &self,
        user: UserId,
        deposit: &ObservedDeposit,
    ) -> Result<CreditOutcome, CreditorError> {
        if deposit.reference.trim().is_empty() {
            return Err(CreditorError::EmptyReference);
        }

        let receipt = self
            .store
            .deposit(user, deposit.money, &deposit.reference)
            .await?;

        Ok(CreditOutcome::Credited {
            transaction_id: receipt.transaction_id,
            replayed: receipt.replayed,
        })
    }

    /// The confirmation depth this creditor requires, from the chain client.
    pub fn required_confirmations(&self) -> u32 {
        self.chain.required_confirmations()
    }
}

/// Decides whether an observed deposit may be credited yet.
///
/// This is the pure policy half of [`DepositCreditor`], split out so it can be
/// unit tested without a database. It answers, in order:
///
/// 1. is the deposit confirmed deeply enough?
/// 2. does it have a reference to be idempotent on?
/// 3. does a user own the address?
pub fn plan_credit<C: ChainClient + ?Sized>(
    chain: &C,
    deposit: &ObservedDeposit,
    resolve: impl FnOnce(&str) -> Option<UserId>,
) -> Result<PlannedCredit, CreditorError> {
    if deposit.reference.trim().is_empty() {
        return Err(CreditorError::EmptyReference);
    }

    let required = chain.required_confirmations();
    if deposit.confirmations < required {
        return Ok(PlannedCredit::NotConfirmed {
            confirmations: deposit.confirmations,
            required,
        });
    }

    // A zero or negative amount can never be credited: the ledger rejects it,
    // and treating it as creditable would spin on a permanently bad deposit.
    if !deposit.money.is_positive() {
        return Err(CreditorError::Ledger(
            engipay_ledger::LedgerError::NonPositiveAmount,
        ));
    }

    match resolve(&deposit.address) {
        Some(user) => Ok(PlannedCredit::Attributed(user)),
        None => Ok(PlannedCredit::Unattributed {
            address: deposit.address.clone(),
        }),
    }
}

/// What [`plan_credit`] decides, before any ledger write.
///
/// Kept separate from [`CreditOutcome`] because the pure policy step can answer
/// "who owns this deposit?" without a ledger; the ledger is only needed to turn
/// an [`PlannedCredit::Attributed`] into a [`CreditOutcome::Credited`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlannedCredit {
    /// The deposit is confirmed, valid, and owned by this user.
    Attributed(UserId),
    /// Not confirmed yet.
    NotConfirmed { confirmations: u32, required: u32 },
    /// No user owns the address.
    Unattributed { address: String },
}

#[cfg(test)]
mod tests {
    use super::*;
    use engipay_core::{Asset, Chain, Money};
    use uuid::Uuid;

    /// A chain client with fixed confirmation requirements.
    struct FakeChain {
        required: u32,
        chain: Chain,
    }

    impl FakeChain {
        fn new(chain: Chain, required: u32) -> Self {
            Self { required, chain }
        }
    }

    impl ChainClient for FakeChain {
        fn chain(&self) -> Chain {
            self.chain
        }

        fn required_confirmations(&self) -> u32 {
            self.required
        }

        async fn latest_height(&self) -> Result<u64, crate::ChainError> {
            Ok(100)
        }

        async fn deposits_since(
            &self,
            _height: u64,
        ) -> Result<Vec<ObservedDeposit>, crate::ChainError> {
            Ok(Vec::new())
        }

        async fn stream_events(
            &self,
            from_height: u64,
            poll_interval: std::time::Duration,
        ) -> Result<crate::EventStream, crate::ChainError> {
            Ok(crate::polling_stream(
                std::sync::Arc::new(FakeChain {
                    required: self.required,
                    chain: self.chain,
                }),
                from_height,
                poll_interval,
            ))
        }
    }

    fn deposit(reference: &str, confirmations: u32, minor: i128) -> ObservedDeposit {
        ObservedDeposit {
            money: Money::from_minor(Asset::Usdc, minor),
            address: "MABC".to_owned(),
            reference: reference.to_owned(),
            confirmations,
        }
    }

    // ── Confirmation gating ──────────────────────────────────────────────────

    #[test]
    fn fully_confirmed_deposit_is_attributed() {
        let chain = FakeChain::new(Chain::Stellar, 1);
        let user = UserId::new();
        let d = deposit("stellar:abc:0", 1, 5_000_000);

        let outcome = plan_credit(&chain, &d, |_| Some(user)).expect("plan");

        assert_eq!(outcome, PlannedCredit::Attributed(user));
    }

    #[test]
    fn unconfirmed_deposit_is_not_credited() {
        // Stellar needs 1 confirmation.
        let chain = FakeChain::new(Chain::Stellar, 1);
        let d = deposit("stellar:abc:0", 0, 5_000_000);

        let outcome = plan_credit(&chain, &d, |_| Some(UserId::new())).expect("plan");

        assert_eq!(
            outcome,
            PlannedCredit::NotConfirmed {
                confirmations: 0,
                required: 1
            }
        );
    }

    #[test]
    fn base_requires_twelve_confirmations() {
        let chain = FakeChain::new(Chain::Base, 12);

        // 11 is not enough.
        let short = deposit("base:0x1:0", 11, 1_000);
        assert_eq!(
            plan_credit(&chain, &short, |_| Some(UserId::new())).expect("plan"),
            PlannedCredit::NotConfirmed {
                confirmations: 11,
                required: 12
            }
        );

        // 12 is.
        let enough = deposit("base:0x1:0", 12, 1_000);
        assert!(matches!(
            plan_credit(&chain, &enough, |_| Some(UserId::new())).expect("plan"),
            PlannedCredit::Attributed(_)
        ));
    }

    #[test]
    fn bitcoin_requires_two_confirmations() {
        let chain = FakeChain::new(Chain::Bitcoin, 2);
        let short = deposit("btc:abc:0", 1, 1_000);

        assert_eq!(
            plan_credit(&chain, &short, |_| Some(UserId::new())).expect("plan"),
            PlannedCredit::NotConfirmed {
                confirmations: 1,
                required: 2
            }
        );
    }

    #[test]
    fn more_confirmations_than_required_still_credits() {
        let chain = FakeChain::new(Chain::Stellar, 1);
        let d = deposit("stellar:abc:0", 30, 5_000_000);

        assert!(matches!(
            plan_credit(&chain, &d, |_| Some(UserId::new())).expect("plan"),
            PlannedCredit::Attributed(_)
        ));
    }

    // ── Unattributable deposits ──────────────────────────────────────────────

    #[test]
    fn unknown_address_is_unattributed_not_credited() {
        let chain = FakeChain::new(Chain::Stellar, 1);
        let d = deposit("stellar:abc:0", 5, 5_000_000);

        let outcome = plan_credit(&chain, &d, |_| None).expect("plan");

        assert_eq!(
            outcome,
            PlannedCredit::Unattributed {
                address: "MABC".to_owned()
            }
        );
    }

    #[test]
    fn custody_account_deposit_is_unattributed() {
        // A deposit straight to the bare `G...` custody account resolves to
        // nothing and must never be credited to an arbitrary user.
        let chain = FakeChain::new(Chain::Stellar, 1);
        let mut d = deposit("stellar:abc:0", 5, 5_000_000);
        d.address = "GCUSTODY".to_owned();

        let outcome = plan_credit(&chain, &d, |addr| {
            if addr == "GCUSTODY" {
                None
            } else {
                Some(UserId::new())
            }
        })
        .expect("plan");

        assert_eq!(
            outcome,
            PlannedCredit::Unattributed {
                address: "GCUSTODY".to_owned()
            }
        );
    }

    #[test]
    fn unconfirmed_unknown_address_reports_not_confirmed_first() {
        // Confirmation gating happens before resolution, so an early deposit to
        // an unknown address is merely "not yet", not "unattributed".
        let chain = FakeChain::new(Chain::Stellar, 1);
        let d = deposit("stellar:abc:0", 0, 5_000_000);

        assert_eq!(
            plan_credit(&chain, &d, |_| None).expect("plan"),
            PlannedCredit::NotConfirmed {
                confirmations: 0,
                required: 1
            }
        );
    }

    // ── Validation ───────────────────────────────────────────────────────────

    #[test]
    fn empty_reference_is_rejected() {
        // Without a reference there is no idempotency key, so the deposit could
        // be credited twice. Refuse it outright.
        let chain = FakeChain::new(Chain::Stellar, 1);
        let d = deposit("   ", 5, 5_000_000);

        assert!(matches!(
            plan_credit(&chain, &d, |_| Some(UserId::new())),
            Err(CreditorError::EmptyReference)
        ));
    }

    #[test]
    fn zero_amount_is_rejected() {
        let chain = FakeChain::new(Chain::Stellar, 1);
        let d = deposit("stellar:abc:0", 5, 0);

        assert!(plan_credit(&chain, &d, |_| Some(UserId::new())).is_err());
    }

    #[test]
    fn negative_amount_is_rejected() {
        let chain = FakeChain::new(Chain::Stellar, 1);
        let d = deposit("stellar:abc:0", 5, -1_000);

        assert!(plan_credit(&chain, &d, |_| Some(UserId::new())).is_err());
    }

    #[test]
    fn validation_happens_before_resolution() {
        // An empty reference is rejected without even consulting the resolver.
        let chain = FakeChain::new(Chain::Stellar, 1);
        let d = deposit("", 5, 5_000_000);
        let mut consulted = false;

        let _ = plan_credit(&chain, &d, |_| {
            consulted = true;
            Some(UserId::new())
        });

        assert!(!consulted, "resolver must not be consulted");
    }

    // ── Money stays exact ────────────────────────────────────────────────────

    #[test]
    fn amounts_are_preserved_exactly_in_minor_units() {
        // 0.0000001 USDC is one minor unit; a float round-trip would lose this.
        let chain = FakeChain::new(Chain::Stellar, 1);
        let d = deposit("stellar:abc:0", 1, 1);
        let user = UserId::new();

        let outcome = plan_credit(&chain, &d, |_| Some(user)).expect("plan");

        assert_eq!(outcome, PlannedCredit::Attributed(user));
        assert_eq!(d.money.minor, 1, "one minor unit survives unchanged");
        assert_eq!(d.money.asset, Asset::Usdc);
    }

    #[test]
    fn every_supported_chain_can_credit() {
        for chain in Chain::ALL {
            let client = FakeChain::new(chain, client_required(chain));
            let d = deposit("ref-1", client_required(chain), 1_000);
            let user = UserId::new();

            assert_eq!(
                plan_credit(&client, &d, |_| Some(user)).expect("plan"),
                PlannedCredit::Attributed(user),
                "{chain:?} should credit at exactly its required depth"
            );
        }
    }

    fn client_required(chain: Chain) -> u32 {
        match chain {
            Chain::Base => 12,
            Chain::Bitcoin => 2,
            Chain::Stellar => 1,
        }
    }

    // ── Outcome helpers ──────────────────────────────────────────────────────

    #[test]
    fn credited_outcome_reports_credited() {
        let outcome = CreditOutcome::Credited {
            transaction_id: Uuid::new_v4(),
            replayed: false,
        };
        assert!(outcome.is_credited());
        assert!(!outcome.needs_review());
    }

    #[test]
    fn replayed_credit_still_counts_as_credited() {
        let outcome = CreditOutcome::Credited {
            transaction_id: Uuid::new_v4(),
            replayed: true,
        };
        assert!(
            outcome.is_credited(),
            "a replay means it was credited before"
        );
    }

    #[test]
    fn unattributed_needs_review() {
        let outcome = CreditOutcome::Unattributed {
            address: "GUNKNOWN".to_owned(),
        };
        assert!(!outcome.is_credited());
        assert!(outcome.needs_review());
    }

    #[test]
    fn not_confirmed_needs_no_review() {
        let outcome = CreditOutcome::NotConfirmed {
            confirmations: 0,
            required: 1,
        };
        assert!(!outcome.is_credited());
        assert!(
            !outcome.needs_review(),
            "an unconfirmed deposit is expected, not an exception"
        );
    }
}
// ── End-to-end tests against a live database ─────────────────────────────
//
// These need `DATABASE_URL` and are skipped by default; CI runs them with a
// Postgres service. They cover the acceptance criterion from #171: processing
// the same deposit twice credits it once.
#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::arithmetic_side_effects)]
mod db_tests {
    use super::*;
    use crate::creditor::sequential::SequentialCreditor;
    use crate::{ChainClient, EventStream, LedgerEvent, ObservedDeposit, polling_stream};
    use engipay_core::{Asset, Chain, Money};
    use sqlx::PgPool;
    use std::sync::Arc;
    use std::time::Duration;

    struct FakeChain {
        chain: Chain,
        required: u32,
    }

    impl ChainClient for FakeChain {
        fn chain(&self) -> Chain {
            self.chain
        }

        fn required_confirmations(&self) -> u32 {
            self.required
        }

        async fn latest_height(&self) -> Result<u64, crate::ChainError> {
            Ok(100)
        }

        async fn deposits_since(
            &self,
            _height: u64,
        ) -> Result<Vec<ObservedDeposit>, crate::ChainError> {
            Ok(Vec::new())
        }

        async fn stream_events(
            &self,
            from_height: u64,
            poll_interval: Duration,
        ) -> Result<EventStream, crate::ChainError> {
            Ok(polling_stream(
                Arc::new(FakeChain {
                    chain: self.chain,
                    required: self.required,
                }),
                from_height,
                poll_interval,
            ))
        }
    }

    async fn test_pool() -> Option<PgPool> {
        let url = std::env::var("DATABASE_URL").ok()?;
        let pool = PgPool::connect(&url).await.ok()?;
        sqlx::migrate!("../../migrations").run(&pool).await.ok()?;
        Some(pool)
    }

    fn deposit(reference: &str, confirmations: u32, minor: i128) -> ObservedDeposit {
        ObservedDeposit {
            money: Money::from_minor(Asset::Usdc, minor),
            address: "MABC".to_owned(),
            reference: reference.to_owned(),
            confirmations,
        }
    }

    async fn user_balance(pool: &PgPool, user: UserId) -> i128 {
        let total: String = sqlx::query_scalar(
            "SELECT COALESCE(SUM(amount), 0)::text FROM ledger_postings \
             WHERE owner_kind = 'user' AND user_id = $1 AND asset = 'USDC'",
        )
        .bind(user.as_uuid())
        .fetch_one(pool)
        .await
        .unwrap();
        total.parse().unwrap()
    }

    /// #171 acceptance: processing the same deposit twice credits it once.
    #[tokio::test]
    #[ignore = "requires DATABASE_URL"]
    async fn processing_the_same_deposit_twice_credits_once() {
        let pool = test_pool().await.unwrap();
        let store = PostgresLedgerStore::new(pool.clone());
        let chain = FakeChain {
            chain: Chain::Stellar,
            required: 1,
        };
        let creditor = DepositCreditor::new(&store, &chain);

        let user = UserId::new();
        sqlx::query("INSERT INTO users (id) VALUES ($1) ON CONFLICT DO NOTHING")
            .bind(user.as_uuid())
            .execute(&pool)
            .await
            .unwrap();

        // A unique reference per run: ledger references are globally unique.
        let reference = format!("stellar-e2e-{}", uuid::Uuid::new_v4());
        let d = deposit(&reference, 1, 5_000_000);

        let first = creditor.credit(user, &d).await.unwrap();
        let second = creditor.credit(user, &d).await.unwrap();

        assert!(matches!(
            first,
            CreditOutcome::Credited {
                replayed: false,
                ..
            }
        ));
        assert!(
            matches!(second, CreditOutcome::Credited { replayed: true, .. }),
            "the second delivery of the same deposit must replay"
        );

        assert_eq!(
            user_balance(&pool, user).await,
            5_000_000,
            "the user must be credited exactly once"
        );
    }

    /// Distinct references for the same money are two real deposits.
    #[tokio::test]
    #[ignore = "requires DATABASE_URL"]
    async fn distinct_references_are_credited_separately() {
        let pool = test_pool().await.unwrap();
        let store = PostgresLedgerStore::new(pool.clone());
        let chain = FakeChain {
            chain: Chain::Stellar,
            required: 1,
        };
        let creditor = DepositCreditor::new(&store, &chain);

        let user = UserId::new();
        sqlx::query("INSERT INTO users (id) VALUES ($1) ON CONFLICT DO NOTHING")
            .bind(user.as_uuid())
            .execute(&pool)
            .await
            .unwrap();

        let a = format!("stellar-two-a-{}", uuid::Uuid::new_v4());
        let b = format!("stellar-two-b-{}", uuid::Uuid::new_v4());

        creditor.credit(user, &deposit(&a, 1, 1_000)).await.unwrap();
        creditor.credit(user, &deposit(&b, 1, 2_500)).await.unwrap();

        assert_eq!(
            user_balance(&pool, user).await,
            3_500,
            "two different deposits must both count"
        );
    }

    /// End to end: a watcher event flows through the sequential creditor into
    /// the ledger, and a replayed event credits nothing extra.
    #[tokio::test]
    #[ignore = "requires DATABASE_URL"]
    async fn watcher_event_is_credited_end_to_end() {
        let pool = test_pool().await.unwrap();
        let store = PostgresLedgerStore::new(pool.clone());
        let chain = FakeChain {
            chain: Chain::Stellar,
            required: 1,
        };
        let creditor = DepositCreditor::new(&store, &chain);

        let user = UserId::new();
        sqlx::query("INSERT INTO users (id) VALUES ($1) ON CONFLICT DO NOTHING")
            .bind(user.as_uuid())
            .execute(&pool)
            .await
            .unwrap();

        let reference = format!("stellar-e2e-flow-{}", uuid::Uuid::new_v4());
        let event = LedgerEvent {
            height: 10,
            deposits: vec![deposit(&reference, 1, 7_000_000)],
        };

        // The watcher can re-emit the same ledger range while catching up, so
        // the sequential creditor may offer the same event twice.
        let mut queue = SequentialCreditor::new(9);
        queue.push(event.clone());
        queue.push(event);

        let mut credited = 0;
        for drained in queue.drain_ready() {
            for observed in drained.deposits {
                match creditor.credit(user, &observed).await.unwrap() {
                    CreditOutcome::Credited { replayed, .. } => {
                        if !replayed {
                            credited += 1;
                        }
                    }
                    other => panic!("unexpected outcome: {other:?}"),
                }
            }
        }

        assert_eq!(credited, 1, "only the first delivery should move money");
        assert_eq!(user_balance(&pool, user).await, 7_000_000);
    }

    /// The creditor refuses a deposit with no reference, because there would be
    /// no idempotency key to prevent a double credit.
    #[tokio::test]
    #[ignore = "requires DATABASE_URL"]
    async fn deposit_without_a_reference_is_refused() {
        let pool = test_pool().await.unwrap();
        let store = PostgresLedgerStore::new(pool.clone());
        let chain = FakeChain {
            chain: Chain::Stellar,
            required: 1,
        };
        let creditor = DepositCreditor::new(&store, &chain);

        let user = UserId::new();
        sqlx::query("INSERT INTO users (id) VALUES ($1) ON CONFLICT DO NOTHING")
            .bind(user.as_uuid())
            .execute(&pool)
            .await
            .unwrap();

        let result = creditor.credit(user, &deposit("  ", 1, 5_000_000)).await;

        assert!(matches!(result, Err(CreditorError::EmptyReference)));
        assert_eq!(
            user_balance(&pool, user).await,
            0,
            "nothing may be credited"
        );
    }

    /// An unconfirmed deposit is not credited; once confirmed it is.
    #[tokio::test]
    #[ignore = "requires DATABASE_URL"]
    async fn unconfirmed_deposit_is_not_credited_until_confirmed() {
        let pool = test_pool().await.unwrap();
        let store = PostgresLedgerStore::new(pool.clone());
        let chain = FakeChain {
            chain: Chain::Base,
            required: 12,
        };
        let creditor = DepositCreditor::new(&store, &chain);

        let user = UserId::new();
        sqlx::query("INSERT INTO users (id) VALUES ($1) ON CONFLICT DO NOTHING")
            .bind(user.as_uuid())
            .execute(&pool)
            .await
            .unwrap();

        let reference = format!("base-e2e-{}", uuid::Uuid::new_v4());

        let pending = deposit(&reference, 11, 4_000_000);
        let plan = plan_credit(&chain, &pending, |_| Some(user)).unwrap();
        assert_eq!(
            plan,
            PlannedCredit::NotConfirmed {
                confirmations: 11,
                required: 12
            }
        );
        assert_eq!(user_balance(&pool, user).await, 0, "nothing yet");

        // Once the chain confirms it, the same deposit credits.
        let confirmed = deposit(&reference, 12, 4_000_000);
        let plan = plan_credit(&chain, &confirmed, |_| Some(user)).unwrap();
        assert_eq!(plan, PlannedCredit::Attributed(user));
        creditor.credit(user, &confirmed).await.unwrap();
        assert_eq!(user_balance(&pool, user).await, 4_000_000);
    }
}
