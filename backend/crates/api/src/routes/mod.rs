pub mod assets;
pub mod auth;
pub mod balances;
pub mod deposits;
pub mod health;
pub mod me;
pub mod payment_requests;
pub mod requests;
pub mod transactions;
pub mod transfers;
// `withdrawals` targets a removed `crate::ledger`/`crate::state` architecture and
// does not compile against the current API crate. It is kept out of the build
// (as `auth::evm`/`auth::stellar` also are) until it is rewritten; see #183/#184.

use axum::Router;

use crate::AppState;

/// Versioned routes. A breaking change becomes /v2 instead of surprising the app.
pub fn v1() -> Router<AppState> {
    Router::new()
        .merge(assets::routes())
        .merge(auth::routes())
        .merge(balances::routes())
        .merge(deposits::routes())
        .merge(payment_requests::routes())
        .merge(requests::routes())
        .merge(transactions::routes())
        .merge(transfers::routes())
}
