// Convert TransactionData into a 14-dimension float vector for vector search.
// All operations are inlined and use fast math.

use crate::json_parser::{TransactionData, get_mcc_risk};

// Constants from normalization.json
pub const MAX_AMOUNT: f32 = 10000.0;
pub const MAX_INSTALLMENTS: f32 = 12.0;
pub const AMOUNT_VS_AVG_RATIO: f32 = 10.0;
pub const MAX_MINUTES: f32 = 1440.0;
pub const MAX_KM: f32 = 1000.0;
pub const MAX_TX_COUNT_24H: f32 = 20.0;
pub const MAX_MERCHANT_AVG_AMOUNT: f32 = 10000.0;
pub const QSCALE: f32 = 10000.0;

/// Sentinel value for absent last_transaction
pub const NO_LAST_TX: f32 = -1.0;

/// Clamp x to [0.0, 1.0]
#[inline(always)]
pub fn clamp(x: f32) -> f32 {
    if x < 0.0 { 0.0 } else if x > 1.0 { 1.0 } else { x }
}

/// Normalize a float to quantized int16
#[inline(always)]
pub fn quantize(v: f32) -> i16 {
    let scaled = v * QSCALE;
    if scaled > 32767.0 {
        32767
    } else if scaled < -32768.0 {
        -32768
    } else {
        scaled as i16
    }
}

/// Check if merchant is unknown (not in known_merchants list)
#[inline(always)]
pub fn is_unknown_merchant(tx: &TransactionData) -> f32 {
    let mut found = false;
    for i in 0..tx.known_merchant_count as usize {
        if tx.known_merchants[i] == tx.merchant_id_hash {
            found = true;
            break;
        }
    }
    if found { 0.0 } else { 1.0 }
}

/// Vectorize a transaction into 14 dimensions.
/// Returns [0..13] float values.
#[inline(always)]
pub fn vectorize(tx: &TransactionData, out: &mut [f32; 14]) {
    // 0: amount
    out[0] = clamp(tx.amount / MAX_AMOUNT);
    // 1: installments
    out[1] = clamp(tx.installments as f32 / MAX_INSTALLMENTS);
    // 2: amount_vs_avg
    out[2] = clamp((tx.amount / tx.customer_avg_amount) / AMOUNT_VS_AVG_RATIO);
    // 3: hour_of_day
    out[3] = tx.hour as f32 / 23.0;
    // 4: day_of_week
    out[4] = tx.weekday as f32 / 6.0;
    // 5: minutes_since_last_tx
    if tx.has_last_tx {
        out[5] = clamp(tx.last_tx_minutes as f32 / MAX_MINUTES);
    } else {
        out[5] = NO_LAST_TX;
    }
    // 6: km_from_last_tx
    if tx.has_last_tx {
        out[6] = clamp(tx.km_from_last_tx / MAX_KM);
    } else {
        out[6] = NO_LAST_TX;
    }
    // 7: km_from_home
    out[7] = clamp(tx.km_from_home / MAX_KM);
    // 8: tx_count_24h
    out[8] = clamp(tx.tx_count_24h as f32 / MAX_TX_COUNT_24H);
    // 9: is_online
    out[9] = if tx.is_online { 1.0 } else { 0.0 };
    // 10: card_present
    out[10] = if tx.card_present { 1.0 } else { 0.0 };
    // 11: unknown_merchant
    out[11] = is_unknown_merchant(tx);
    // 12: mcc_risk
    out[12] = get_mcc_risk(tx.mcc);
    // 13: merchant_avg_amount
    out[13] = clamp(tx.merchant_avg_amount / MAX_MERCHANT_AVG_AMOUNT);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::json_parser::TransactionData;

    #[test]
    fn test_vectorize_null_last_tx() {
        let tx = TransactionData {
            id: 1,
            amount: 41.12,
            installments: 2,
            hour: 18,
            weekday: 2, // Tue
            customer_avg_amount: 82.24,
            tx_count_24h: 3,
            merchant_id_hash: 123,
            known_merchants: [0; 32],
            known_merchant_count: 0,
            mcc: 5411,
            merchant_avg_amount: 60.25,
            is_online: false,
            card_present: true,
            km_from_home: 29.23,
            has_last_tx: false,
            last_tx_minutes: 0,
            km_from_last_tx: 0.0,
        };
        let mut v = [0.0f32; 14];
        vectorize(&tx, &mut v);
        assert!((v[0] - 0.004112).abs() < 0.0001);
        assert_eq!(v[1], 2.0 / 12.0);
        assert!((v[2] - 0.05).abs() < 0.0001);
        assert!((v[3] - 18.0 / 23.0).abs() < 0.0001);
        assert_eq!(v[5], NO_LAST_TX);
        assert_eq!(v[6], NO_LAST_TX);
        assert_eq!(v[11], 1.0); // unknown merchant
    }

    #[test]
    fn test_vectorize_fraud() {
        let tx = TransactionData {
            id: 2,
            amount: 9505.97,
            installments: 10,
            hour: 5,
            weekday: 5, // Fri
            customer_avg_amount: 81.28,
            tx_count_24h: 20,
            merchant_id_hash: 456,
            known_merchants: {
                let mut arr = [0u32; 32];
                arr[0] = 789;
                arr[1] = 101112;
                arr[2] = 131415;
                arr
            },
            known_merchant_count: 3,
            mcc: 7802,
            merchant_avg_amount: 54.86,
            is_online: false,
            card_present: true,
            km_from_home: 952.27,
            has_last_tx: false,
            last_tx_minutes: 0,
            km_from_last_tx: 0.0,
        };
        let mut v = [0.0f32; 14];
        vectorize(&tx, &mut v);
        assert!((v[0] - 0.9506).abs() < 0.001);
        assert!((v[1] - 0.8333).abs() < 0.001);
        assert_eq!(v[2], 1.0); // clamped
        assert!((v[7] - 0.9523).abs() < 0.001);
        assert_eq!(v[8], 1.0); // clamped
        assert_eq!(v[11], 1.0); // unknown merchant
    }
}