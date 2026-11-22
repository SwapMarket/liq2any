//! Low-balance alarms: USDT on either chain below the largest trade we offer, or gas on either chain for fewer than a
//! few swaps. A maker that runs dry only stops quoting, which nobody notices from outside. From liq2sol's, with POL in
//! SOL's place.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct BalanceAlarms {
    /// Page when gas would last fewer swaps than this.
    pub gas_swaps: u64,
    /// POL one swap spends, in wei: a lock and a refund, or a claim, at about 400k gas together (measured on Amoy: lock
    /// 244–273k, claim 110–128k), plus an L→P swap's gas drop. The default, 0.2 POL, is 400k gas at 250 gwei, or 400k at
    /// 100 gwei and a 0.16 POL drop. Set it from your own gas drop and the fees you see.
    pub pol_wei_per_swap: u128,
    /// L-BTC one swap spends: the lock, the taker's fee coin, the claim or refund, and a share of fee-coin top-ups.
    pub lbtc_sats_per_swap: u64,
    pub every_secs: u64,
    /// While an alarm lasts, page again this often.
    pub repeat_secs: u64,
}

impl Default for BalanceAlarms {
    fn default() -> Self {
        Self {
            gas_swaps: 10,
            pol_wei_per_swap: 200_000_000_000_000_000,
            lbtc_sats_per_swap: 200,
            every_secs: 600,
            repeat_secs: 6 * 3600,
        }
    }
}

/// What the signer holds, in each chain's base units: Liquid USDt has 8 decimals, USDT0 6, POL 18.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Holdings {
    pub liquid_usdt: u64,
    pub liquid_lbtc_sats: u64,
    pub polygon_usdt: u128,
    pub pol_wei: u128,
}

fn usdt(units: u128) -> String {
    format!("{:.2}", units as f64 / 1e6)
}

/// Each alarm that holds, by name, with what to tell the operator. `max_trade` is in USDT0 units.
pub fn low(h: &Holdings, max_trade: u64, a: &BalanceAlarms) -> Vec<(&'static str, String)> {
    let mut out = vec![];
    let max_trade = u128::from(max_trade);
    let liquid_usdt = u128::from(h.liquid_usdt / 100);
    if h.polygon_usdt < max_trade {
        let what = format!("USDT0 is {}, below the largest trade, {}", usdt(h.polygon_usdt), usdt(max_trade));
        out.push(("polygon_usdt", what));
    }
    if liquid_usdt < max_trade {
        let what = format!("L-USDt is {}, below the largest trade, {}", usdt(liquid_usdt), usdt(max_trade));
        out.push(("liquid_usdt", what));
    }
    let pol_needed = a.pol_wei_per_swap.saturating_mul(u128::from(a.gas_swaps));
    if h.pol_wei < pol_needed {
        let what = format!(
            "POL is {:.4}, enough for fewer than {} swaps ({:.4})",
            h.pol_wei as f64 / 1e18,
            a.gas_swaps,
            pol_needed as f64 / 1e18
        );
        out.push(("pol", what));
    }
    let lbtc_needed = a.lbtc_sats_per_swap.saturating_mul(a.gas_swaps);
    if h.liquid_lbtc_sats < lbtc_needed {
        let what = format!(
            "L-BTC is {} sats, enough for fewer than {} swaps ({} sats)",
            h.liquid_lbtc_sats, a.gas_swaps, lbtc_needed
        );
        out.push(("lbtc", what));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const FULL: Holdings = Holdings {
        liquid_usdt: 5_000_000_000,
        liquid_lbtc_sats: 2_000,
        polygon_usdt: 50_000_000,
        pol_wei: 2_000_000_000_000_000_000,
    };

    fn names(h: Holdings) -> Vec<&'static str> {
        low(&h, 50_000_000, &BalanceAlarms::default()).into_iter().map(|(n, _)| n).collect()
    }

    #[test]
    fn exactly_enough_is_no_alarm() {
        assert!(names(FULL).is_empty());
    }

    #[test]
    fn each_balance_alarms_on_its_own() {
        assert_eq!(names(Holdings { polygon_usdt: 49_999_999, ..FULL }), ["polygon_usdt"]);
        // Liquid's two extra decimals: 49.99999999 L-USDt is short of 50.
        assert_eq!(names(Holdings { liquid_usdt: 4_999_999_999, ..FULL }), ["liquid_usdt"]);
        assert_eq!(names(Holdings { pol_wei: 1_999_999_999_999_999_999, ..FULL }), ["pol"]);
        assert_eq!(names(Holdings { liquid_lbtc_sats: 1_999, ..FULL }), ["lbtc"]);
        let empty = Holdings { liquid_usdt: 0, liquid_lbtc_sats: 0, polygon_usdt: 0, pol_wei: 0 };
        assert_eq!(names(empty), ["polygon_usdt", "liquid_usdt", "pol", "lbtc"]);
    }

    #[test]
    fn huge_balances_and_settings_do_not_overflow() {
        let a = BalanceAlarms { pol_wei_per_swap: u128::MAX, gas_swaps: u64::MAX, ..BalanceAlarms::default() };
        let rich = Holdings { polygon_usdt: u128::MAX, pol_wei: u128::MAX, ..FULL };
        assert!(low(&rich, u64::MAX, &a).iter().all(|(n, _)| *n == "liquid_usdt" || *n == "lbtc"));
    }

    #[test]
    fn the_message_says_how_much_and_against_what() {
        let alarms = low(&Holdings { polygon_usdt: 12_340_000, ..FULL }, 50_000_000, &BalanceAlarms::default());
        assert_eq!(alarms[0].1, "USDT0 is 12.34, below the largest trade, 50.00");
    }

    #[test]
    fn config_fields_default_one_by_one() {
        let a: BalanceAlarms = serde_json::from_str(r#"{"gas_swaps": 3}"#).unwrap();
        assert_eq!(a, BalanceAlarms { gas_swaps: 3, ..BalanceAlarms::default() });
    }
}
