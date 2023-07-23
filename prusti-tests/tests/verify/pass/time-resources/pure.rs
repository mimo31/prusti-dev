// compile-flags: -Ptime_reasoning=true

use prusti_contracts::*;

#[pure]
#[requires(time_credits(1))]
#[ensures(time_receipts(1))]
fn do_nothing() {}

#[pure]
#[requires(time_credits(10))]
fn do_costly_nothing() -> bool {
    true
}

#[requires(time_credits(1) & do_costly_nothing())]
fn costly_in_precond() {}

#[requires(time_credits(1))]
#[ensures(time_receipts(1))]
fn main() {}
