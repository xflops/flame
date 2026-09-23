"""
Monte Carlo Estimation of PI using Flame App API

This example uses the Monte Carlo method to estimate the value of PI by
randomly sampling points in a unit square and checking if they fall inside
a quarter circle.
"""

import math
import os

import numpy as np

import flamepy.app as app

app.init("pi-example")


@app.service()
def estimate_batch(num_samples: int) -> int:
    """Count random points that fall inside a unit quarter circle."""
    x = np.random.rand(num_samples)
    y = np.random.rand(num_samples)
    return int(np.sum(x * x + y * y <= 1.0))


def main():
    """Run Monte Carlo PI estimation using distributed computing."""

    print("=" * 60)
    print("Monte Carlo Estimation of PI using Flame App")
    print("=" * 60)

    # Configuration
    num_batches = int(os.getenv("PI_NUM_BATCHES", "10"))
    samples_per_batch = int(os.getenv("PI_SAMPLES_PER_BATCH", "1000000"))
    if num_batches <= 0:
        raise ValueError("PI_NUM_BATCHES must be greater than 0")
    if samples_per_batch <= 0:
        raise ValueError("PI_SAMPLES_PER_BATCH must be greater than 0")
    total_samples = num_batches * samples_per_batch

    print("\nConfiguration:")
    print(f"  Batches: {num_batches}")
    print(f"  Samples per batch: {samples_per_batch:,}")
    print(f"  Total samples: {total_samples:,}")
    print("\nRunning distributed Monte Carlo simulation...")

    try:
        # Submit all batch computations
        results = [estimate_batch(samples_per_batch) for _ in range(num_batches)]

        # Collect results
        insides = app.get(results)
    finally:
        app.destroy()

    # Calculate final PI estimate
    pi_estimate = 4.0 * sum(insides) / total_samples

    error = abs(pi_estimate - math.pi)
    error_percent = (error / math.pi) * 100

    print("\nResults:")
    print(f"  Estimated PI: {pi_estimate:.10f}")
    print(f"  Actual PI:    {math.pi:.10f}")
    print(f"  Error:        {error:.10f} ({error_percent:.6f}%)")
    print("=" * 60)


if __name__ == "__main__":
    main()
