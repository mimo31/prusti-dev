// compile-flags: -Ptime_reasoning=true

use prusti_contracts::*;

#[pure]
#[requires(time_credits(1))]
fn func() {}

#[requires(time_credits(1))]
fn main() {
    func(); //~ ERROR Not enough time credits to call function.
}
