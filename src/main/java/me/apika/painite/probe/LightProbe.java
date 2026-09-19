package me.apika.painite.probe;

import java.util.concurrent.atomic.AtomicLong;
import java.util.concurrent.atomic.LongAccumulator;

/** Busy time of the light engine's serial update loop, so its utilisation over a burst is visible. */
public final class LightProbe {
	private static final AtomicLong CALLS = new AtomicLong();
	private static final AtomicLong BUSY_NANOS = new AtomicLong();
	private static final LongAccumulator MAX_NANOS = new LongAccumulator(Math::max, 0);
	private static final LongAccumulator FIRST_START = new LongAccumulator(Math::min, Long.MAX_VALUE);
	private static final LongAccumulator LAST_END = new LongAccumulator(Math::max, 0);

	private LightProbe() {}

	public static long started() {
		long now = System.nanoTime();
		FIRST_START.accumulate(now);
		return now;
	}

	public static void finished(long t0) {
		long now = System.nanoTime();
		long ran = now - t0;
		CALLS.incrementAndGet();
		BUSY_NANOS.addAndGet(ran);
		MAX_NANOS.accumulate(ran);
		LAST_END.accumulate(now);
	}

	public static String report() {
		long n = CALLS.get();
		double span = n == 0 ? 0 : (LAST_END.get() - FIRST_START.get()) / 1e9;
		double busy = BUSY_NANOS.get() / 1e9;
		return String.format("[painite] light probe (runUpdate)%n  updates=%d busy=%.2fs span=%.1fs utilisation=%.0f%% meanUpdate=%.3fms maxUpdate=%.1fms%n",
				n, busy, span, span == 0 ? 0 : 100 * busy / span, n == 0 ? 0 : busy * 1e3 / n, MAX_NANOS.get() / 1e6);
	}

	public static void reset() {
		CALLS.set(0);
		BUSY_NANOS.set(0);
		MAX_NANOS.reset();
		FIRST_START.reset();
		LAST_END.reset();
	}
}
