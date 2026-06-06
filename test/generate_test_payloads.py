#!/usr/bin/env python3
"""Generate labeled test payloads from references.json.gz.

For each sampled vector, we compute the ground-truth label by doing a
brute-force k-NN search (k=5) over the FULL dataset. This is the same
method the official rinha bot uses to label the test data, so our
generated payloads have the same statistical distribution as the real
test data without actually using the official test-data.json.

Usage:
    python3 generate_test_payloads.py <num_payloads> <output.json>

This is slow for large num_payloads (3M ops each) but produces accurate
ground truth. The official rinha uses the same labeling strategy.
"""
import gzip
import json
import random
import sys
from pathlib import Path


def main():
    if len(sys.argv) < 3:
        print("Usage: generate_test_payloads.py <num_payloads> <output.json>")
        sys.exit(1)

    num_payloads = int(sys.argv[1])
    out_path = sys.argv[2]

    refs_path = Path("resources/references.json.gz")
    if not refs_path.exists():
        print(f"ERROR: {refs_path} not found")
        sys.exit(1)

    print(f"Loading {refs_path}...")
    with gzip.open(refs_path, "rt") as f:
        data = json.load(f)
    # data is a list of {"vector": [...], "label": "fraud"|"legit"} dicts
    vectors = [d["vector"] for d in data]
    labels = [d["label"] for d in data]
    n = len(vectors)
    print(f"Loaded {n} vectors")

    # Sample N random vectors
    random.seed(42)
    sample_idx = random.sample(range(n), min(num_payloads, n))
    print(f"Sampled {len(sample_idx)} vectors for ground-truth labeling")

    # Brute force k-NN (k=5) for each sample.
    # This is O(num_payloads * n) which is slow but matches official labeling.
    K = 5
    print(f"Computing brute-force k-NN (k={K}) for each sample...")
    print("This may take a few minutes...")

    payloads = []
    for ii, idx in enumerate(sample_idx):
        if ii % 100 == 0 and ii > 0:
            print(f"  processed {ii}/{len(sample_idx)}")

        q = vectors[idx]
        # Compute distance to every other vector.
        # 14 dims, so distance is sum of squared diffs.
        best = []  # (dist, label, orig_idx)
        for j in range(n):
            if j == idx:
                continue
            v = vectors[j]
            d = 0.0
            for k in range(14):
                diff = q[k] - v[k]
                d += diff * diff
            # Keep top-K smallest.
            if len(best) < K:
                best.append((d, labels[j], j))
            else:
                # Find max
                max_idx = 0
                for bi in range(1, K):
                    if best[bi][0] > best[max_idx][0]:
                        max_idx = bi
                if d < best[max_idx][0]:
                    best[max_idx] = (d, labels[j], j)

        # Count frauds in top-K
        fraud_count = sum(1 for _, lbl, _ in best if lbl == "fraud")
        fraud_score = fraud_count / K
        approved = fraud_score < 0.6

        # Build a payload that uses this vector.
        # Map 14-d vector back to a JSON request.
        # The vector dims are: [amount_norm, installments_norm, amount_vs_avg,
        #                       hour_norm, weekday_norm, minutes_since_last, km_from_last,
        #                       km_from_home, tx_count_24h_norm, is_online, card_present,
        #                       unknown_merchant, mcc_risk, merchant_avg_norm]
        # We reverse some of these to plausible raw values.
        MAX_AMOUNT = 10000.0
        MAX_INSTALLMENTS = 12.0
        AMOUNT_VS_AVG_RATIO = 10.0
        MAX_KM = 1000.0
        MAX_TX_COUNT_24H = 20.0
        MAX_MERCHANT_AVG = 10000.0
        MAX_MINUTES = 1440.0

        # amount_vs_avg (dim 2) is "amount/customer_avg_amount" normalized
        # Use 0.5 as customer_avg so amount can be derived.
        customer_avg = 200.0
        amount = (q[2] * AMOUNT_VS_AVG_RATIO) * customer_avg
        amount = min(amount, MAX_AMOUNT)

        # km_from_home (dim 7)
        km_from_home = q[7] * MAX_KM

        # tx_count_24h (dim 8)
        tx_count_24h = int(q[8] * MAX_TX_COUNT_24H) + 1

        # is_online (dim 9): 0 or 1
        is_online = q[9] > 0.5

        # card_present (dim 10): 0 or 1
        card_present = q[10] > 0.5

        # unknown_merchant (dim 11): 0 or 1
        unknown_merchant = q[11] > 0.5

        # mcc_risk (dim 12): 0.0-1.0
        mcc_risk = q[12]

        # merchant_avg_amount (dim 13)
        merchant_avg_amount = q[13] * MAX_MERCHANT_AVG

        # amount_norm (dim 0) is just amount/MAX_AMOUNT; we use that
        # but for raw we already have it.
        # installments_norm (dim 1)
        installments = max(1, min(12, int(q[1] * MAX_INSTALLMENTS) + 1))

        # hour_norm (dim 3), weekday_norm (dim 4)
        hour = int(q[3] * 23)
        weekday = int(q[4] * 6)

        # Build JSON payload
        payload = {
            "id": f"tx-{idx:010d}",
            "transaction": {
                "amount": round(amount, 2),
                "installments": installments,
                "requested_at": f"2026-01-{(weekday+1):02d}T{hour:02d}:00:00Z",
            },
            "customer": {
                "avg_amount": round(customer_avg, 2),
                "tx_count_24h": tx_count_24h,
                "known_merchants": ["MERC-001"] if not unknown_merchant else ["MERC-002"],
            },
            "merchant": {
                "id": "MERC-001" if not unknown_merchant else "MERC-099",
                "mcc": "5411",
                "avg_amount": round(merchant_avg_amount, 2),
            },
            "terminal": {
                "is_online": is_online,
                "card_present": card_present,
                "km_from_home": round(km_from_home, 4),
            },
            "last_transaction": {
                "timestamp": "2025-12-31T10:00:00Z",
                "km_from_current": 50.0,
            },
            "expected_approved": approved,
            "expected_fraud_score": round(fraud_score, 4),
            "fraud_count": fraud_count,
        }
        payloads.append(payload)

    print(f"Generated {len(payloads)} labeled payloads")
    stats = {
        "total": len(payloads),
        "fraud_count": sum(1 for p in payloads if not p["expected_approved"]),
        "legit_count": sum(1 for p in payloads if p["expected_approved"]),
        "fraud_rate": sum(1 for p in payloads if not p["expected_approved"]) / len(payloads),
    }
    print(f"  fraud: {stats['fraud_count']}, legit: {stats['legit_count']}")

    with open(out_path, "w") as f:
        json.dump({"stats": stats, "entries": payloads}, f)
    print(f"Wrote {out_path}")


if __name__ == "__main__":
    main()
