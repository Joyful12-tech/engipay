//! Crediting observed on-chain deposits into the ledger.
//!
//! Two concerns live here, deliberately kept separate:
//!
//! * [`sequential`] orders whole [`crate::LedgerEvent`]s before they are
//!   applied, so money is posted in the order the network settled it (#172).
//! * [`deposit_creditor`] turns a single [`crate::ObservedDeposit`] into a
//!   ledger credit: it checks confirmations, resolves the address to a user,
//!   and commits idempotently (#171).

pub mod deposit_creditor;
pub mod sequential;

pub use deposit_creditor::{
    CreditOutcome, CreditorError, DepositCreditor, PlannedCredit, plan_credit,
};
pub use sequential::SequentialCreditor;
