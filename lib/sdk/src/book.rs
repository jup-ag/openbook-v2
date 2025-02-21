use std::collections::HashMap;

use anchor_lang::prelude::Pubkey;
use anyhow::Result;
pub use fixed::types::I80F48;
use openbook_v2::state::{
    Market, Orderbook, Side, DROP_EXPIRED_ORDER_LIMIT, FILL_EVENT_REMAINING_LIMIT,
};

// TODO Adjust this number after doing some calculations
pub const MAXIMUM_TAKEN_ORDERS: u8 = 50;
pub const MAXIMUM_REMAINING_ACCOUNTS: usize = 3;

pub struct Amounts {
    pub total_base_taken_native: u64,
    pub total_quote_taken_native: u64,
    pub fee: u64,
    pub not_enough_liquidity: bool,
}

pub fn remaining_accounts_to_crank(
    book: Orderbook,
    side: Side,
    max_base_lots: i64,
    max_quote_lots_including_fees: i64,
    market: &Market,
    oracle_price: Option<I80F48>,
    now_ts: u64,
) -> Result<Vec<Pubkey>> {
    let oracle_price_lots = oracle_price
        .map(|price| market.native_price_to_lot(price))
        .transpose()?;

    // Pre-allocate accounts vec with maximum possible size
    let max_accounts = FILL_EVENT_REMAINING_LIMIT as usize + DROP_EXPIRED_ORDER_LIMIT as usize;
    let mut accounts = Vec::with_capacity(max_accounts);

    iterate_book(
        book,
        side,
        max_base_lots,
        max_quote_lots_including_fees,
        market,
        oracle_price_lots,
        now_ts,
        &mut accounts,
    );

    // Pre-allocate HashMap with known capacity
    let mut frequency_map = HashMap::with_capacity(accounts.len());
    for &value in &accounts {
        *frequency_map.entry(value).or_insert(0) += 1;
    }

    // Sort by occurrences in descending order
    let mut sorted_pairs: Vec<(Pubkey, usize)> = frequency_map.into_iter().collect();
    sorted_pairs.sort_unstable_by(|a, b| b.1.cmp(&a.1));

    // Pre-allocate final vector with exact size needed
    let mut common_accounts = Vec::with_capacity(MAXIMUM_REMAINING_ACCOUNTS);
    for (value, _) in sorted_pairs.iter().take(MAXIMUM_REMAINING_ACCOUNTS) {
        common_accounts.push(*value);
    }

    Ok(common_accounts)
}

#[allow(clippy::too_many_arguments)]
pub fn iterate_book_amounts(
    book: Orderbook,
    side: Side,
    max_base_lots: i64,
    max_quote_lots_including_fees: i64,
    market: &Market,
    oracle_price_lots: Option<i64>,
    now_ts: u64,
) -> (i64, i64, u64, bool) {
    let mut limit = MAXIMUM_TAKEN_ORDERS;

    // Pre-calculate constants and avoid repeated access
    let quote_lot_size = market.quote_lot_size;
    let order_max_base_lots = max_base_lots;
    let order_max_quote_lots = match side {
        Side::Bid => market.subtract_taker_fees(max_quote_lots_including_fees),
        Side::Ask => max_quote_lots_including_fees,
    };

    // Early exit conditions
    if order_max_base_lots <= 0 || order_max_quote_lots <= 0 {
        return (0, 0, 0, true);
    }

    let mut maker_rebates_acc = 0;
    let mut remaining_base_lots = order_max_base_lots;
    let mut remaining_quote_lots = order_max_quote_lots;
    let mut total_base_lots_taken = 0;
    let mut total_quote_lots_taken = 0;

    let opposing_bookside = book.bookside(side.invert_side());
    let mut iter = opposing_bookside.iter_all_including_invalid(now_ts, oracle_price_lots);

    // Process orders in chunks to improve cache locality
    while let Some(best_opposing) = iter.next() {
        if !best_opposing.is_valid() {
            continue;
        }

        if limit == 0 {
            break;
        }

        let best_opposing_price = best_opposing.price_lots;
        let max_match_by_quote = remaining_quote_lots / best_opposing_price;
        if max_match_by_quote <= 0 {
            break;
        }

        let match_base_lots = remaining_base_lots
            .min(best_opposing.node.quantity)
            .min(max_match_by_quote);

        // Early continue if no match
        if match_base_lots <= 0 {
            continue;
        }

        let match_quote_lots = match_base_lots * best_opposing_price;

        // Calculate maker rebates and update totals in one pass
        maker_rebates_acc += market.maker_rebate_floor((match_quote_lots * quote_lot_size) as u64);

        total_base_lots_taken += match_base_lots;
        total_quote_lots_taken += match_quote_lots;
        remaining_base_lots -= match_base_lots;
        remaining_quote_lots -= match_quote_lots;

        limit -= 1;

        // Early exit if we've matched everything we need
        if remaining_base_lots <= 0 || remaining_quote_lots <= 0 {
            break;
        }
    }

    let not_enough_liquidity = match side {
        Side::Ask => remaining_base_lots > 0,
        Side::Bid => remaining_quote_lots > 0,
    };

    (
        total_base_lots_taken,
        total_quote_lots_taken,
        maker_rebates_acc,
        not_enough_liquidity,
    )
}

pub fn amounts_from_book(
    book: Orderbook,
    side: Side,
    max_base_lots: i64,
    max_quote_lots_including_fees: i64,
    market: &Market,
    oracle_price: Option<I80F48>,
    now_ts: u64,
) -> Result<Amounts> {
    let oracle_price_lots = oracle_price
        .map(|price| market.native_price_to_lot(price))
        .transpose()?;

    let (total_base_lots_taken, total_quote_lots_taken, makers_rebates, not_enough_liquidity) =
        iterate_book_amounts(
            book,
            side,
            max_base_lots,
            max_quote_lots_including_fees,
            market,
            oracle_price_lots,
            now_ts,
        );

    // Avoid multiple multiplications by pre-calculating lot sizes
    let base_lot_size = market.base_lot_size;
    let quote_lot_size = market.quote_lot_size;

    Ok(Amounts {
        total_base_taken_native: (total_base_lots_taken * base_lot_size) as u64,
        total_quote_taken_native: (total_quote_lots_taken * quote_lot_size) as u64,
        fee: makers_rebates,
        not_enough_liquidity,
    })
}

#[allow(clippy::too_many_arguments)]
pub fn iterate_book(
    book: Orderbook,
    side: Side,
    max_base_lots: i64,
    max_quote_lots_including_fees: i64,
    market: &Market,
    oracle_price_lots: Option<i64>,
    now_ts: u64,
    accounts: &mut Vec<Pubkey>,
) -> (i64, i64, u64, bool) {
    let mut limit = MAXIMUM_TAKEN_ORDERS;
    let mut number_of_processed_fill_events = 0;
    let mut number_of_dropped_expired_orders = 0;

    // Pre-calculate constants
    let fill_event_limit = FILL_EVENT_REMAINING_LIMIT;
    let drop_expired_limit = DROP_EXPIRED_ORDER_LIMIT;
    let quote_lot_size = market.quote_lot_size;

    let order_max_base_lots = max_base_lots;
    let order_max_quote_lots = match side {
        Side::Bid => market.subtract_taker_fees(max_quote_lots_including_fees),
        Side::Ask => max_quote_lots_including_fees,
    };

    // Early exit conditions
    if order_max_base_lots <= 0 || order_max_quote_lots <= 0 {
        return (0, 0, 0, true);
    }

    let mut maker_rebates_acc = 0;
    let mut remaining_base_lots = order_max_base_lots;
    let mut remaining_quote_lots = order_max_quote_lots;

    let opposing_bookside = book.bookside(side.invert_side());

    for best_opposing in opposing_bookside.iter_all_including_invalid(now_ts, oracle_price_lots) {
        if !best_opposing.is_valid() {
            // Remove the order from the book unless we've done that enough
            if number_of_dropped_expired_orders < drop_expired_limit {
                accounts.push(best_opposing.node.owner);
                number_of_dropped_expired_orders += 1;
            }
            continue;
        }

        if limit == 0 {
            break;
        }

        let best_opposing_price = best_opposing.price_lots;
        let max_match_by_quote = remaining_quote_lots / best_opposing_price;
        if max_match_by_quote <= 0 {
            break;
        }

        let match_base_lots = remaining_base_lots
            .min(best_opposing.node.quantity)
            .min(max_match_by_quote);

        // Early continue if no match
        if match_base_lots <= 0 {
            continue;
        }

        let match_quote_lots = match_base_lots * best_opposing_price;

        // Calculate maker rebates only if we need to track fill events
        if number_of_processed_fill_events < fill_event_limit {
            maker_rebates_acc +=
                market.maker_rebate_floor((match_quote_lots * quote_lot_size) as u64);
            accounts.push(best_opposing.node.owner);
            number_of_processed_fill_events += 1;
        }

        remaining_base_lots -= match_base_lots;
        remaining_quote_lots -= match_quote_lots;

        limit -= 1;

        // Early exit if we've matched everything we need
        if remaining_base_lots <= 0 || remaining_quote_lots <= 0 {
            break;
        }
    }

    let total_base_lots_taken = order_max_base_lots - remaining_base_lots;
    let total_quote_lots_taken = order_max_quote_lots - remaining_quote_lots;

    let not_enough_liquidity = match side {
        Side::Ask => remaining_base_lots > 0,
        Side::Bid => remaining_quote_lots > 0,
    };

    (
        total_base_lots_taken,
        total_quote_lots_taken,
        maker_rebates_acc,
        not_enough_liquidity,
    )
}
