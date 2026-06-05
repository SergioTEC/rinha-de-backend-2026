// Fast path heuristics for Rinha 2026 fraud detection.
// Aggressively resolves queries without touching the 3M vector index.

/// Result of fast path classification
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum FastResult {
    /// Definitely legit (approved=true, score=0.0)
    Legit,
    /// Definitely fraud (approved=false, score=1.0)
    Fraud,
    /// Cannot decide, need full search
    Borderline,
}

/// Aggressive heuristic fast path classification.
/// Covers ~90% of queries with simple threshold checks.
#[inline(always)]
pub fn fast_path(v: &[f32; 14]) -> FastResult {
    let amount = v[0];
    let amount_vs_avg = v[2];
    let km_home = v[7];
    let tx_count = v[8];
    let is_online = v[9];
    let card_present = v[10];
    let unknown_merchant = v[11];
    let mcc_risk = v[12];

    // === LEGIT (low risk signals) ===
    // Order checks by discriminative power: amount first, then merchant/mcc/geo.

    // Very low amount, close, known merchant, low MCC
    if amount < 0.03 && km_home < 0.05 && unknown_merchant < 0.5 && mcc_risk < 0.2 {
        return FastResult::Legit;
    }

    // Known merchant, low amount, close, not online
    if amount < 0.15 && unknown_merchant < 0.5 && km_home < 0.2 && is_online < 0.5 {
        return FastResult::Legit;
    }

    // Very low MCC risk, known merchant, reasonable distance
    if amount < 0.25 && mcc_risk < 0.15 && unknown_merchant < 0.5 {
        return FastResult::Legit;
    }

    // Low amount vs avg, few tx, close, card present
    if amount_vs_avg < 0.1 && tx_count < 0.2 && km_home < 0.15 && card_present > 0.5 {
        return FastResult::Legit;
    }

    // NEW: very small amount + known merchant + very low tx_count = clearly legit
    if amount < 0.05 && unknown_merchant < 0.5 && tx_count < 0.1 {
        return FastResult::Legit;
    }

    // NEW: low amount + very low mcc_risk + low amount_vs_avg = clearly legit
    if amount < 0.10 && mcc_risk < 0.2 && amount_vs_avg < 0.3 {
        return FastResult::Legit;
    }

    // NEW: zero amount vs avg (sentinel) + low tx count + card present = legit
    if amount_vs_avg < 0.05 && tx_count < 0.3 && card_present > 0.5 {
        return FastResult::Legit;
    }

    // === FRAUD (high risk signals) ===

    // Very high amount, far, unknown, high MCC
    if amount > 0.8 && km_home > 0.7 && unknown_merchant > 0.5 && mcc_risk > 0.6 {
        return FastResult::Fraud;
    }

    // Online + unknown + high risk + significant amount
    if amount > 0.5 && is_online > 0.5 && unknown_merchant > 0.5 && mcc_risk > 0.6 {
        return FastResult::Fraud;
    }

    // Card not present, far from home, unknown, high amount
    if amount > 0.4 && card_present < 0.5 && km_home > 0.5 && unknown_merchant > 0.5 {
        return FastResult::Fraud;
    }

    // Extreme amount vs avg, many tx, unknown merchant
    if amount_vs_avg > 0.9 && tx_count > 0.8 && unknown_merchant > 0.5 {
        return FastResult::Fraud;
    }

    // Many tx in 24h + far + unknown (ATM/pump pattern)
    if tx_count > 0.8 && km_home > 0.5 && unknown_merchant > 0.5 {
        return FastResult::Fraud;
    }

    // NEW: extreme amount + online + unknown = clear fraud
    if amount > 0.9 && is_online > 0.5 && unknown_merchant > 0.5 {
        return FastResult::Fraud;
    }

    // NEW: high amount + very far + no card = fraud
    if amount > 0.6 && km_home > 0.8 && card_present < 0.5 {
        return FastResult::Fraud;
    }

    // NEW: maxed tx_count + far + high mcc = fraud
    if tx_count > 0.9 && km_home > 0.4 && mcc_risk > 0.5 {
        return FastResult::Fraud;
    }

    // NEW: very high mcc_risk + online + unknown = fraud
    if mcc_risk > 0.7 && is_online > 0.5 && unknown_merchant > 0.5 && amount > 0.2 {
        return FastResult::Fraud;
    }

    FastResult::Borderline
}
