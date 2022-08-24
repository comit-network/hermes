use crate::olivia;
use crate::payouts;
use crate::CompleteFee;
use crate::Contracts;
use crate::Leverage;
use crate::Position;
use crate::Price;
use crate::Role;
use anyhow::bail;
use anyhow::Result;
use itertools::Itertools;
use maia_core::generate_payouts;
use maia_core::Announcement;
use maia_core::Payout;
use rust_decimal::Decimal;
use std::collections::HashMap;

mod inverse;
#[cfg(test)]
mod prop_compose;
mod quanto;

/// Payout combinations associated with the oracle events that may
/// trigger them.
#[derive(Debug)]
pub struct OraclePayouts(HashMap<Announcement, Vec<Payout>>);

impl OraclePayouts {
    pub fn new(payouts: Payouts, announcements: Vec<olivia::Announcement>) -> Result<Self> {
        let announcements = Announcements::new(announcements)?;

        let settlement = (announcements.settlement, payouts.settlement);
        let liquidations = announcements.liquidation.into_iter().map(|announcement| {
            (
                announcement,
                vec![
                    payouts.long_liquidation.clone(),
                    payouts.short_liquidation.clone(),
                ],
            )
        });

        Ok(Self(HashMap::from_iter(
            [settlement].into_iter().chain(liquidations),
        )))
    }
}

impl From<OraclePayouts> for HashMap<Announcement, Vec<Payout>> {
    fn from(from: OraclePayouts) -> Self {
        from.0
    }
}

pub struct Payouts {
    /// The full range of payout combinations by which a CFD can be
    /// settled.
    settlement: Vec<Payout>,
    /// The payout combination which corresponds to the party with the
    /// long position being liquidated.
    long_liquidation: Payout,
    /// The payout combination which corresponds to the party with the
    /// short position being liquidated.
    short_liquidation: Payout,
}

impl Payouts {
    #[allow(clippy::too_many_arguments)]
    #[tracing::instrument(err)]
    pub fn new_inverse(
        position: Position,
        role: Role,
        price: Price,
        quantity: Contracts,
        long_leverage: Leverage,
        short_leverage: Leverage,
        n_payouts: usize,
        fee: CompleteFee,
    ) -> Result<Self> {
        let mut payouts = payouts::inverse::calculate(
            price,
            quantity,
            long_leverage,
            short_leverage,
            n_payouts,
            fee,
        )?;

        // Overwrite the short liquidation upper bound with the maximum price that Olivia can attest
        // to
        {
            let n_payouts = payouts.len() - 1;
            let short_liquidation = payouts.get_mut(n_payouts).expect("several payouts");
            short_liquidation.range =
                *short_liquidation.range.start()..=maia_core::interval::MAX_PRICE_DEC;
        }

        let settlement: Vec<_> = match (position, role) {
            (Position::Long, Role::Taker) | (Position::Short, Role::Maker) => payouts
                .into_iter()
                .map(|payout| generate_payouts(payout.range, payout.short, payout.long))
                .flatten_ok()
                .try_collect()?,
            (Position::Short, Role::Taker) | (Position::Long, Role::Maker) => payouts
                .into_iter()
                .map(|payout| generate_payouts(payout.range, payout.long, payout.short))
                .flatten_ok()
                .try_collect()?,
        };

        let long_liquidation = settlement.first().expect("several payouts").clone();
        let short_liquidation = settlement.last().expect("several payouts").clone();

        Ok(Self {
            settlement,
            long_liquidation,
            short_liquidation,
        })
    }

    pub fn new_quanto(
        (position, role): (Position, Role),
        initial_price: u64,
        n_contracts: u64,
        (leverage_long, leverage_short): (Leverage, Leverage),
        n_payouts: usize,
        multiplier: Decimal,
        fee_offset: CompleteFee,
    ) -> Result<Self> {
        let payouts = quanto::Payouts::new(
            initial_price,
            n_contracts,
            leverage_long,
            leverage_short,
            n_payouts,
            multiplier,
            fee_offset,
        )?;

        let settlement: Vec<_> = match (position, role) {
            (Position::Long, Role::Taker) | (Position::Short, Role::Maker) => payouts
                .into_inner()
                .into_iter()
                .map(|payout| generate_payouts(payout.interval, payout.short, payout.long))
                .flatten_ok()
                .try_collect()?,
            (Position::Short, Role::Taker) | (Position::Long, Role::Maker) => payouts
                .into_inner()
                .into_iter()
                .map(|payout| generate_payouts(payout.interval, payout.long, payout.short))
                .flatten_ok()
                .try_collect()?,
        };

        let long_liquidation = settlement.first().expect("several payouts").clone();
        let short_liquidation = settlement.last().expect("several payouts").clone();

        Ok(Self {
            settlement,
            long_liquidation,
            short_liquidation,
        })
    }

    pub fn settlement(&self) -> Vec<Payout> {
        self.settlement.clone()
    }

    pub fn long_liquidation(&self) -> &Payout {
        &self.long_liquidation
    }

    pub fn short_liquidation(&self) -> &Payout {
        &self.short_liquidation
    }
}

struct Announcements {
    /// The announcement which corresponds to the oracle event that
    /// will mark the end of an epoch for a CFD.
    settlement: Announcement,
    /// All the intermediate oracle announcements between the start
    /// and end of an epoch for a CFD.
    liquidation: Vec<Announcement>,
}

impl Announcements {
    fn new(announcements: Vec<olivia::Announcement>) -> Result<Self> {
        let announcements = announcements
            .into_iter()
            .sorted_by(|a, b| a.id.cmp(&b.id))
            .map(|announcement| Announcement {
                id: announcement.id.to_string(),
                nonce_pks: announcement.nonce_pks,
            })
            .collect_vec();

        let (liquidation, settlement) = match announcements.as_slice() {
            [] => bail!("Need at least one announcement to construct"),
            [beginning @ .., last] => (beginning.to_vec(), last.clone()),
        };

        Ok(Self {
            settlement,
            liquidation,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::olivia::Announcement;
    use crate::olivia::BitMexPriceEventId;
    use crate::payouts::prop_compose::arb_contracts;
    use crate::payouts::prop_compose::arb_fee_flow;
    use crate::payouts::prop_compose::arb_leverage;
    use crate::payouts::prop_compose::arb_price;
    use crate::payouts::quanto;
    use proptest::prelude::*;
    use rust_decimal_macros::dec;
    use std::ops::Add;
    use time::ext::NumericalDuration;
    use time::macros::datetime;

    proptest! {
        #[test]
        fn given_generated_inverse_payouts_then_can_build_oracle_payouts(
            position in prop_oneof![Just(Position::Long), Just(Position::Short)],
            role in prop_oneof![Just(Role::Maker), Just(Role::Taker)],
            price in arb_price(1000.0, 100_000.0),
            n_contracts in arb_contracts(100, 10_000_000),
            short_leverage in arb_leverage(1, 100),
            fee_flow in arb_fee_flow(-100_000_000, 100_000_000),
        ) {
            let payouts = Payouts::new_inverse(
                position,
                role,
                price,
                n_contracts,
                Leverage::ONE,
                short_leverage,
                200,
                fee_flow,
            )
                .unwrap();

            let n_events = 24;
            let announcements = (0..n_events)
                .map(|i| {
                    let timestamp = datetime!(2022-07-29 13:00:00).assume_utc().add(i.hours());

                    Announcement {
                        id: BitMexPriceEventId::new(timestamp, 1),
                        expected_outcome_time: timestamp,
                        nonce_pks: vec![
                            "d02d163cf9623f567c4e3faf851a9266ac1ede13da4ca4141f3a7717fba9a739"
                                .parse()
                                .unwrap(),
                        ],
                    }
                })
                .collect_vec();

            let mut oracle_payouts = OraclePayouts::new(payouts, announcements.clone()).unwrap();
            assert_eq!(oracle_payouts.0.len() as i64, n_events);

            {
                let settlement_announcement = {
                    let settlement_announcement = announcements.last().unwrap();
                    maia_core::Announcement { id: settlement_announcement.id.to_string(), nonce_pks: settlement_announcement.nonce_pks.clone() }
                };

                oracle_payouts.0.remove(&settlement_announcement);
            }

            let has_long_and_short_liquidation_payouts = oracle_payouts
                .0
                .iter()
                .all(|(_, payouts)| payouts.len() == 2);
            assert!(has_long_and_short_liquidation_payouts)
        }
    }

    proptest! {
        #[test]
        fn given_generated_quanto_payouts_then_can_build_oracle_payouts(
            position in prop_oneof![Just(Position::Long), Just(Position::Short)],
            role in prop_oneof![Just(Role::Maker), Just(Role::Taker)],
            initial_price in 1u64..100_000,
            n_contracts in 1u64..10_000,
            leverage_long in arb_leverage(1, 100),
            leverage_short in arb_leverage(1, 100),
            n_payouts in 10usize..2000,
            fee_offset in arb_fee_flow(-100_000, 100_000)
        ) {
            let payouts = match Payouts::new_quanto(
                (position, role),
                initial_price,
                n_contracts,
                (leverage_long, leverage_short),
                n_payouts,
                dec!(0.000001),
                fee_offset
            ) {
                Ok(payouts) => payouts,
                Err(e) => {
                    let e = match e.downcast_ref::<quanto::Error>() {
                        Some(quanto::Error::LongOwesTooMuch { .. } | quanto::Error::ShortOwesTooMuch { .. }) => {
                            TestCaseError::reject("The fee_offset was too high, given the other parameters")
                        },
                        Some(_) | None => TestCaseError::fail(format!("{e}")),
                    };

                    return Err(e);
                }
            };

            let n_events = 24;
            let announcements = (0..n_events)
                .map(|i| {
                    let timestamp = datetime!(2022-07-29 13:00:00).assume_utc().add(i.hours());

                    Announcement {
                        id: BitMexPriceEventId::new(timestamp, 1),
                        expected_outcome_time: timestamp,
                        nonce_pks: vec![
                            "d02d163cf9623f567c4e3faf851a9266ac1ede13da4ca4141f3a7717fba9a739"
                                .parse()
                                .unwrap(),
                        ],
                    }
                })
                .collect_vec();

            let mut oracle_payouts = OraclePayouts::new(payouts, announcements.clone()).unwrap();
            assert_eq!(oracle_payouts.0.len() as i64, n_events);

            {
                let settlement_announcement = {
                    let settlement_announcement = announcements.last().unwrap();
                    maia_core::Announcement { id: settlement_announcement.id.to_string(), nonce_pks: settlement_announcement.nonce_pks.clone() }
                };

                oracle_payouts.0.remove(&settlement_announcement);
            }

            let has_long_and_short_liquidation_payouts = oracle_payouts
                .0
                .iter()
                .all(|(_, payouts)| payouts.len() == 2);
            assert!(has_long_and_short_liquidation_payouts)
        }
    }
}
